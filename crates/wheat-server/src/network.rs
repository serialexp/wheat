//! Fork networks: repositories sharing one store.
//!
//! The repositories of a fork network share its store (see
//! [`wheat_store::RepositoriesDirectory`]), so every repository of a network open at
//! once uses one handle to it: a packed store is locked against a second open. A
//! [`Network`] is that handle, together with the record of who writes to the store,
//! so holding one repository still ([`Registry::quiesce`]) holds the whole network.
//!
//! An opener keeps the networks it has open in [`OpenNetworks`], which opens a
//! network's store with the first of its repositories and closes it with the last.
//!
//! [`Registry::quiesce`]: crate::registry::Registry::quiesce

use std::collections::HashMap;
use std::hash::Hash;
use std::ops::Deref;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};

use async_trait::async_trait;
use wheat_core::ObjectId;
use wheat_store::{
    ContentItem, ContentStore, ImmutableObjectStore, ObjectStoreError, RepositoryStore,
    RepositoryStoreError, StoredObject, StoredWrite,
};

use crate::naming::ContentNaming;
use crate::quiesce::Writers;

/// A fork network's stores, shared by the repositories of it that are open.
#[derive(Clone)]
pub struct Network(Arc<NetworkStores>);

impl Network {
    /// A network of its own for `store`, which no other [`Network`] uses.
    pub fn new(store: Box<dyn RepositoryStore>) -> Self {
        Self(Arc::new(NetworkStores::new(store, None, None)))
    }

    /// As [`Self::new`], telling `naming` the content each publish relies on.
    pub fn with_naming(store: Box<dyn RepositoryStore>, naming: Arc<dyn ContentNaming>) -> Self {
        Self(Arc::new(NetworkStores::new(store, Some(naming), None)))
    }
}

impl Deref for Network {
    type Target = NetworkStores;

    fn deref(&self) -> &NetworkStores {
        &self.0
    }
}

// The network as its store, so work handed to another thread can own a handle to it
// and keep the network open meanwhile.
#[async_trait]
impl ImmutableObjectStore for Network {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.store.put_if_absent(id, canonical_bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        self.store.put_batch(objects, concurrency).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.store.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.store.exists_many(ids).await
    }

    async fn seal(&self) -> Result<(), ObjectStoreError> {
        self.store.seal().await
    }
}

#[async_trait]
impl RepositoryStore for Network {
    fn content_store(&self) -> ContentStore {
        self.store.content_store()
    }

    async fn put_publication(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        content: &[ContentItem<'_>],
    ) -> Result<StoredWrite, RepositoryStoreError> {
        self.store.put_publication(objects, content).await
    }
}

/// What a [`Network`] holds.
pub struct NetworkStores {
    pub store: Box<dyn RepositoryStore>,
    /// `store`'s content.
    pub content: ContentStore,
    /// The publishes and committers writing to the store, held still by
    /// [`crate::registry::Registry::quiesce`].
    pub(crate) writers: Writers,
    /// Told the content each publish relies on, when the content store is
    /// collected by age.
    pub naming: Option<Arc<dyn ContentNaming>>,
    /// Tells [`OpenNetworks`] the network closed. Fields drop in order, so it does
    /// so once the store is closed.
    _closed: Option<Closed>,
}

impl NetworkStores {
    /// Whether a repository of the network is quiesced, so none takes publishes.
    #[must_use]
    pub fn is_quiesced(&self) -> bool {
        self.writers.is_quiesced()
    }

    fn new(
        store: Box<dyn RepositoryStore>,
        naming: Option<Arc<dyn ContentNaming>>,
        closed: Option<Closed>,
    ) -> Self {
        Self {
            content: store.content_store(),
            store,
            writers: Writers::default(),
            naming,
            _closed: closed,
        }
    }
}

/// The networks an opener has open, by key (a network ID, say).
pub struct OpenNetworks<K> {
    slots: Arc<Mutex<HashMap<K, Arc<Slot>>>>,
    /// Set on every network opened.
    naming: Option<Arc<dyn ContentNaming>>,
}

impl<K> Default for OpenNetworks<K> {
    fn default() -> Self {
        Self {
            slots: Arc::default(),
            naming: None,
        }
    }
}

impl<K> Clone for OpenNetworks<K> {
    fn clone(&self) -> Self {
        Self {
            slots: Arc::clone(&self.slots),
            naming: self.naming.clone(),
        }
    }
}

impl<K> OpenNetworks<K> {
    /// Networks that tell `naming` the content each publish relies on.
    #[must_use]
    pub fn with_naming(naming: Arc<dyn ContentNaming>) -> Self {
        Self {
            slots: Arc::default(),
            naming: Some(naming),
        }
    }
}

/// One network: `None` while closed, otherwise its stores, which are closing once
/// they no longer upgrade.
#[derive(Default)]
struct Slot {
    state: Mutex<Option<Weak<NetworkStores>>>,
    /// Signalled when the network has closed.
    closed: Condvar,
}

impl Slot {
    fn state(&self) -> MutexGuard<'_, Option<Weak<NetworkStores>>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Run once a network's store is closed.
struct Closed(Option<Box<dyn FnOnce() + Send + Sync>>);

impl Drop for Closed {
    fn drop(&mut self) {
        if let Some(closed) = self.0.take() {
            closed();
        }
    }
}

impl<K: Clone + Eq + Hash + Send + Sync + 'static> OpenNetworks<K> {
    /// The network `key`: the one open already, or one over the store `open` opens.
    /// Blocks while another thread opens `key` or `key` is still closing, so call it
    /// where blocking is fine, as a [`crate::registry::RepositoryOpener`] does its
    /// disk work.
    pub fn open<E>(
        &self,
        key: &K,
        open: impl FnOnce() -> Result<Box<dyn RepositoryStore>, E>,
    ) -> Result<Network, E> {
        let slot = Arc::clone(lock(&self.slots).entry(key.clone()).or_default());
        let mut state = slot.state();
        while let Some(stores) = state.as_ref() {
            if let Some(stores) = stores.upgrade() {
                return Ok(Network(stores));
            }
            // Its last repository was just closed: wait for the store to be, so it
            // can be opened again.
            state = slot
                .closed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        let store = match open() {
            Ok(store) => store,
            Err(error) => {
                drop(state);
                // Forget the slot unless someone else is about to open it.
                let mut map = lock(&self.slots);
                if Arc::strong_count(&slot) == 2 && slot.state().is_none() {
                    map.remove(key);
                }
                return Err(error);
            }
        };
        let closed = Closed(Some(Box::new({
            let slots = Arc::downgrade(&self.slots);
            let slot = Arc::clone(&slot);
            let key = key.clone();
            move || close(&slots, &slot, &key)
        })));
        let stores = Arc::new(NetworkStores::new(store, self.naming.clone(), Some(closed)));
        *state = Some(Arc::downgrade(&stores));
        Ok(Network(stores))
    }

    /// How many networks are open or closing.
    pub fn len(&self) -> usize {
        lock(&self.slots).len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Marks `slot`, `key`'s, closed and wakes those waiting to open it again. Forgets
/// it unless someone is.
fn close<K: Eq + Hash>(slots: &Weak<Mutex<HashMap<K, Arc<Slot>>>>, slot: &Arc<Slot>, key: &K) {
    let slots = slots.upgrade();
    // The map before the slot, as `open` takes them.
    let mut map = slots.as_deref().map(lock);
    *slot.state() = None;
    slot.closed.notify_all();
    // Held by the map and by this closure alone, nobody is about to open it.
    if let Some(map) = map.as_mut()
        && Arc::strong_count(slot) == 2
    {
        map.remove(key);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Each lock is held over consistent updates only.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use wheat_core::ObjectId;
    use wheat_store::{
        ContentItem, ImmutableObjectStore, ObjectStoreError, PackedObjectStore,
        RepositoryStoreError, StoredObject, StoredWrite,
    };

    use super::*;

    fn opener(
        directory: &std::path::Path,
        opens: &Cell<usize>,
    ) -> impl FnOnce() -> Result<Box<dyn RepositoryStore>, ObjectStoreError> {
        opens.set(opens.get() + 1);
        let directory = directory.to_path_buf();
        move || Ok(Box::new(PackedObjectStore::open(directory)?) as Box<dyn RepositoryStore>)
    }

    #[test]
    fn a_network_open_already_is_shared_and_closed_with_its_last_user() {
        let directory = tempfile::tempdir().unwrap();
        let networks = OpenNetworks::default();
        let opens = Cell::new(0);

        let first = networks.open(&1, opener(directory.path(), &opens)).unwrap();
        let second = networks
            .open(&1, || -> Result<_, ObjectStoreError> {
                panic!("the open network is shared")
            })
            .unwrap();
        assert!(Arc::ptr_eq(&first.0, &second.0));
        assert_eq!(networks.len(), 1);

        drop(first);
        assert_eq!(networks.len(), 1, "one user is left");
        drop(second);
        assert!(networks.is_empty(), "the closed network is forgotten");
        // Closed means the store's lock is released.
        drop(PackedObjectStore::open(directory.path()).unwrap());

        let again = networks.open(&1, opener(directory.path(), &opens)).unwrap();
        assert_eq!(opens.get(), 2);
        drop(again);
    }

    #[test]
    fn a_network_that_fails_to_open_is_not_kept() {
        let networks = OpenNetworks::default();

        let failed = networks.open(&1, || Err::<Box<dyn RepositoryStore>, _>("refused"));

        assert_eq!(failed.err(), Some("refused"));
        assert!(networks.is_empty());
    }

    /// A store that, once dropped, says so on `dropping` and closes only when
    /// `close` says it may; `open` counts the stores not closed yet.
    struct SlowToClose {
        inner: PackedObjectStore,
        open: Arc<AtomicUsize>,
        dropping: std::sync::mpsc::SyncSender<()>,
        close: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl Drop for SlowToClose {
        fn drop(&mut self) {
            self.dropping.send(()).unwrap();
            lock(&self.close).recv().unwrap();
            self.open.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl ImmutableObjectStore for SlowToClose {
        async fn put_if_absent(
            &self,
            id: ObjectId,
            canonical_bytes: &[u8],
        ) -> Result<(), ObjectStoreError> {
            self.inner.put_if_absent(id, canonical_bytes).await
        }

        async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
            self.inner.get(id).await
        }

        async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
            self.inner.exists_many(ids).await
        }
    }

    #[async_trait::async_trait]
    impl RepositoryStore for SlowToClose {
        fn content_store(&self) -> ContentStore {
            self.inner.content_store()
        }

        async fn put_publication(
            &self,
            objects: &[(ObjectId, Vec<u8>)],
            content: &[ContentItem<'_>],
        ) -> Result<StoredWrite, RepositoryStoreError> {
            self.inner.put_publication(objects, content).await
        }
    }

    #[test]
    fn a_network_reopened_while_closing_waits_for_its_store_to_close() {
        let directory = tempfile::tempdir().unwrap();
        let networks = OpenNetworks::default();
        let open = Arc::new(AtomicUsize::new(0));
        let (dropping_tx, dropping) = std::sync::mpsc::sync_channel(1);
        let (close, close_rx) = std::sync::mpsc::sync_channel(1);
        let network = networks
            .open(&1, || {
                open.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ObjectStoreError>(Box::new(SlowToClose {
                    inner: PackedObjectStore::open(directory.path())?,
                    open: Arc::clone(&open),
                    dropping: dropping_tx,
                    close: Mutex::new(close_rx),
                }) as Box<dyn RepositoryStore>)
            })
            .unwrap();

        // The last user is gone and its store is closing.
        let closing = std::thread::spawn(move || drop(network));
        dropping.recv().unwrap();
        let reopening = std::thread::spawn({
            let networks = networks.clone();
            let open = Arc::clone(&open);
            let directory = directory.path().to_path_buf();
            move || {
                networks
                    .open(&1, || {
                        assert_eq!(open.load(Ordering::SeqCst), 0, "the old store is open");
                        PackedObjectStore::open(directory)
                            .map(|store| Box::new(store) as Box<dyn RepositoryStore>)
                    })
                    .unwrap()
            }
        });
        // Give the reopen time to reach the network; only then let the store close.
        std::thread::sleep(std::time::Duration::from_millis(50));
        close.send(()).unwrap();
        closing.join().unwrap();
        drop(reopening.join().expect("the reopen waited for the close"));
        assert!(networks.is_empty());
    }
}
