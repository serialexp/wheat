//! Per-staged-import authenticated object read cache.
//!
//! Entries are populated only after the immutable store has successfully returned
//! an object, so this is merely an invocation-local performance cache, never a
//! source of object authenticity or durability.

use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
};

use async_trait::async_trait;
use wheat_core::ObjectId;
use wheat_store::{ImmutableObjectStore, ObjectStoreError, StoredObject};

/// A bounded LRU of successful immutable-store reads.
pub(super) struct CachedObjects<'a, O> {
    inner: &'a O,
    max_entries: usize,
    max_bytes: usize,
    state: Mutex<State>,
}

struct State {
    objects: HashMap<ObjectId, StoredObject>,
    /// Least recently used at the front.
    order: VecDeque<ObjectId>,
    bytes: usize,
}

impl<'a, O> CachedObjects<'a, O> {
    pub(super) fn new(inner: &'a O, max_entries: usize, max_bytes: usize) -> Self {
        Self {
            inner,
            max_entries,
            max_bytes,
            state: Mutex::new(State {
                objects: HashMap::new(),
                order: VecDeque::new(),
                bytes: 0,
            }),
        }
    }

    fn remove(state: &mut State, id: ObjectId) {
        if let Some(object) = state.objects.remove(&id) {
            state.bytes -= object.canonical_bytes.len();
        }
        if let Some(position) = state.order.iter().position(|candidate| *candidate == id) {
            state.order.remove(position);
        }
    }

    fn touch(state: &mut State, id: ObjectId) {
        if let Some(position) = state.order.iter().position(|candidate| *candidate == id) {
            state.order.remove(position);
        }
        state.order.push_back(id);
    }
}

#[async_trait]
impl<O: ImmutableObjectStore> ImmutableObjectStore for CachedObjects<'_, O> {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, canonical_bytes).await?;
        // Do not retain a possibly stale entry if an unusual backend changes the
        // object while satisfying put-if-absent.
        Self::remove(
            &mut self.state.lock().expect("staged import cache poisoned"),
            id,
        );
        Ok(())
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        if let Some(object) = {
            let mut state = self.state.lock().expect("staged import cache poisoned");
            let object = state.objects.get(&id).cloned();
            if object.is_some() {
                Self::touch(&mut state, id);
            }
            object
        } {
            return Ok(object);
        }

        // Errors, including corrupt objects, are deliberately never memoized.
        let object = self.inner.get(id).await?;
        let object_bytes = object.canonical_bytes.len();
        if self.max_entries == 0 || object_bytes > self.max_bytes {
            return Ok(object);
        }

        let mut state = self.state.lock().expect("staged import cache poisoned");
        // Another task may have inserted this object while the backend request was
        // in flight. Retaining the just-authenticated value is harmless.
        Self::remove(&mut state, id);
        while (state.objects.len() >= self.max_entries
            || state.bytes + object_bytes > self.max_bytes)
            && let Some(oldest) = state.order.pop_front()
        {
            if let Some(evicted) = state.objects.remove(&oldest) {
                state.bytes -= evicted.canonical_bytes.len();
            }
        }
        state.bytes += object_bytes;
        state.objects.insert(id, object.clone());
        state.order.push_back(id);
        Ok(object)
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use super::*;
    use wheat_store::ObjectKind;

    struct CountingStore {
        objects: HashMap<ObjectId, StoredObject>,
        gets: Mutex<HashMap<ObjectId, usize>>,
    }

    #[async_trait]
    impl ImmutableObjectStore for CountingStore {
        async fn put_if_absent(
            &self,
            _id: ObjectId,
            _canonical_bytes: &[u8],
        ) -> Result<(), ObjectStoreError> {
            Ok(())
        }

        async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
            *self.gets.lock().unwrap().entry(id).or_default() += 1;
            self.objects
                .get(&id)
                .cloned()
                .ok_or(ObjectStoreError::Missing(id))
        }

        async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
            Ok(ids.iter().map(|id| self.objects.contains_key(id)).collect())
        }
    }

    fn id(value: u8) -> ObjectId {
        ObjectId::from_digest([value; 32])
    }

    fn store() -> CountingStore {
        let objects = (1..=3)
            .map(|value| {
                let id = id(value);
                (
                    id,
                    StoredObject {
                        id,
                        kind: ObjectKind::Tree,
                        canonical_bytes: vec![value; 4],
                    },
                )
            })
            .collect();
        CountingStore {
            objects,
            gets: Mutex::new(HashMap::new()),
        }
    }

    #[tokio::test]
    async fn lru_is_bounded_and_reloads_evicted_objects() {
        let store = store();
        let cache = CachedObjects::new(&store, 2, 8);
        cache.get(id(1)).await.unwrap();
        cache.get(id(2)).await.unwrap();
        cache.get(id(1)).await.unwrap(); // make 2 the LRU
        cache.get(id(3)).await.unwrap();
        cache.get(id(2)).await.unwrap();
        let gets = store.gets.lock().unwrap();
        assert_eq!(gets[&id(1)], 1);
        assert_eq!(gets[&id(2)], 2);
        assert_eq!(gets[&id(3)], 1);
        let state = cache.state.lock().unwrap();
        assert!(state.objects.len() <= 2);
        assert!(state.bytes <= 8);
    }

    #[tokio::test]
    async fn failures_are_not_cached() {
        let store = store();
        let cache = CachedObjects::new(&store, 2, 8);
        assert!(cache.get(id(9)).await.is_err());
        assert!(cache.get(id(9)).await.is_err());
        assert_eq!(store.gets.lock().unwrap()[&id(9)], 2);
    }
}
