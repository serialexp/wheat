use std::{future::Future, pin::Pin};

use rusqlite::{Connection, Transaction, params};
use thiserror::Error;
use wheat_core::{
    ContentId, ContentMapId, ContentSetChild, ContentSetId, ContentSetNode, ObjectId,
    RetainedSetChild, RetainedSetId, RetainedSetNode,
};
use wheat_store::{
    CollectionError, ContentMapError, ContentMapStore, ContentSetStore, ImmutableObjectStore,
    RetainedSetStore,
};

type BuildFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ContentAccumulatorError>> + 'a>>;

#[derive(Debug, Error)]
pub enum ContentAccumulatorError {
    #[error("finalization accumulator database failed")]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Collection(#[from] CollectionError),
    #[error(transparent)]
    ContentMap(#[from] ContentMapError),
    #[error("content-set value occurs before terminal depth")]
    NonterminalValue,
}

/// A disk-backed unique inventory used while finalization authenticates mappings.
///
/// SQLite's empty path creates a private temporary on-disk database which is removed when the
/// connection closes. Only radix traversal state (at most 33 nodes) and output chunks are held in
/// memory.
pub(crate) struct ContentAccumulator {
    connection: Connection,
}

impl ContentAccumulator {
    pub(crate) fn new() -> Result<Self, ContentAccumulatorError> {
        let connection = Connection::open("")?;
        connection.execute_batch(
            "PRAGMA temp_store = FILE;
             PRAGMA cache_size = -8192;
             CREATE TABLE content_ids (id BLOB PRIMARY KEY) WITHOUT ROWID;
             CREATE TABLE retained_changes (id BLOB PRIMARY KEY) WITHOUT ROWID;
             CREATE TABLE retained_views (id BLOB PRIMARY KEY) WITHOUT ROWID;
             CREATE TABLE retained_revisions (id BLOB PRIMARY KEY) WITHOUT ROWID;
             CREATE TABLE retained_applications (id BLOB PRIMARY KEY) WITHOUT ROWID;",
        )?;
        Ok(Self { connection })
    }

    pub(crate) fn begin_page(&mut self) -> Result<Transaction<'_>, ContentAccumulatorError> {
        Ok(self.connection.transaction()?)
    }

    pub(crate) fn insert_retained(
        transaction: &Transaction<'_>,
        table: RetainedTable,
        id: ObjectId,
    ) -> Result<(), ContentAccumulatorError> {
        transaction
            .prepare_cached(Table::Retained(table).insert_sql())?
            .execute(params![id.as_bytes().as_slice()])?;
        Ok(())
    }

    pub(crate) fn insert_content(
        transaction: &Transaction<'_>,
        id: ContentId,
    ) -> Result<(), ContentAccumulatorError> {
        transaction
            .prepare_cached(Table::Content.insert_sql())?
            .execute(params![id.as_bytes().as_slice()])?;
        Ok(())
    }

    /// Records the blobs a view's content map names that its composition parent's does
    /// not. Every view's parent is retained too, so starting from the empty base view
    /// this covers every blob any view names, while each view costs only what its
    /// commit changed rather than a read of its whole map.
    pub(crate) async fn insert_view_content<S: ImmutableObjectStore>(
        transaction: &Transaction<'_>,
        maps: &ContentMapStore<'_, S>,
        parent: ContentMapId,
        view: ContentMapId,
    ) -> Result<(), ContentAccumulatorError> {
        for content in maps.added(parent, view).await? {
            Self::insert_content(transaction, content)?;
        }
        Ok(())
    }

    pub(crate) async fn build_content<S: ImmutableObjectStore>(
        &self,
        store: &ContentSetStore<'_, S>,
    ) -> Result<ContentSetId, ContentAccumulatorError> {
        self.build_set(store, Table::Content).await
    }

    pub(crate) async fn build_retained<S: ImmutableObjectStore>(
        &self,
        store: &RetainedSetStore<'_, S>,
        table: RetainedTable,
    ) -> Result<RetainedSetId, ContentAccumulatorError> {
        self.build_set(store, Table::Retained(table)).await
    }

    async fn build_set<St: SortedSet>(
        &self,
        store: &St,
        table: Table,
    ) -> Result<St::Id, ContentAccumulatorError> {
        let Some((first, last)) = self.bounds(table)? else {
            return store.empty().await;
        };
        self.build_range(store, table, first, last, 0).await
    }

    fn build_range<'a, St: SortedSet>(
        &'a self,
        store: &'a St,
        table: Table,
        first: ObjectId,
        last: ObjectId,
        depth: usize,
    ) -> BuildFuture<'a, St::Id> {
        Box::pin(async move {
            if first == last {
                store
                    .write(first.as_bytes()[depth..].to_vec(), true, vec![], vec![])
                    .await
            } else {
                let common = common_prefix(&first.as_bytes()[depth..], &last.as_bytes()[depth..]);
                let end = depth + common;
                let mut children = Vec::new();
                let mut inline_entries = Vec::new();
                for (edge, child_first, child_last) in self.groups(table, first, last, end)? {
                    if child_first == child_last {
                        // Single-entry group: inline instead of creating a child.
                        inline_entries.push(child_first.as_bytes()[end..].to_vec());
                    } else {
                        let child = self
                            .build_range(store, table, child_first, child_last, end + 1)
                            .await?;
                        children.push((edge, child));
                    }
                }
                store
                    .write(
                        first.as_bytes()[depth..end].to_vec(),
                        false,
                        children,
                        inline_entries,
                    )
                    .await
            }
        })
    }

    fn bounds(
        &self,
        table: Table,
    ) -> Result<Option<(ObjectId, ObjectId)>, ContentAccumulatorError> {
        let sql = format!(
            "SELECT (SELECT id FROM {} ORDER BY id LIMIT 1), \
             (SELECT id FROM {} ORDER BY id DESC LIMIT 1)",
            table.name(),
            table.name()
        );
        let (first, last) = self.connection.query_row(&sql, [], |row| {
            Ok((
                row.get::<_, Option<Vec<u8>>>(0)?,
                row.get::<_, Option<Vec<u8>>>(1)?,
            ))
        })?;
        match (first, last) {
            (Some(first), Some(last)) => Ok(Some((
                ObjectId::from_digest(decode_digest(first)?),
                ObjectId::from_digest(decode_digest(last)?),
            ))),
            (None, None) => Ok(None),
            _ => unreachable!("both aggregate bounds come from the same table"),
        }
    }

    fn groups(
        &self,
        table: Table,
        first: ObjectId,
        last: ObjectId,
        byte: usize,
    ) -> Result<Vec<(u8, ObjectId, ObjectId)>, ContentAccumulatorError> {
        let sql = format!(
            "SELECT substr(id, ?1, 1), MIN(id), MAX(id) FROM {} \
             WHERE id >= ?2 AND id <= ?3 GROUP BY substr(id, ?1, 1) ORDER BY 1",
            table.name()
        );
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(
            params![
                byte as i64 + 1,
                first.as_bytes().as_slice(),
                last.as_bytes().as_slice()
            ],
            |row| {
                let edge: Vec<u8> = row.get(0)?;
                let first: Vec<u8> = row.get(1)?;
                let last: Vec<u8> = row.get(2)?;
                Ok((edge, first, last))
            },
        )?;
        rows.map(|row| {
            let (edge, first, last) = row?;
            if edge.len() != 1 {
                return Err(ContentAccumulatorError::NonterminalValue);
            }
            Ok((
                edge[0],
                ObjectId::from_digest(decode_digest(first)?),
                ObjectId::from_digest(decode_digest(last)?),
            ))
        })
        .collect()
    }
}

/// A radix set of 32-byte keys that the accumulator builds from one sorted table.
trait SortedSet {
    type Id: Copy;
    async fn empty(&self) -> Result<Self::Id, ContentAccumulatorError>;
    /// Writes one canonical node, as the sorted builder lays it out.
    async fn write(
        &self,
        prefix: Vec<u8>,
        present: bool,
        children: Vec<(u8, Self::Id)>,
        entries: Vec<Vec<u8>>,
    ) -> Result<Self::Id, ContentAccumulatorError>;
}

impl<S: ImmutableObjectStore> SortedSet for RetainedSetStore<'_, S> {
    type Id = RetainedSetId;
    async fn empty(&self) -> Result<RetainedSetId, ContentAccumulatorError> {
        Ok(self.empty_root().await?)
    }
    async fn write(
        &self,
        prefix: Vec<u8>,
        present: bool,
        children: Vec<(u8, RetainedSetId)>,
        entries: Vec<Vec<u8>>,
    ) -> Result<RetainedSetId, ContentAccumulatorError> {
        let children = children
            .into_iter()
            .map(|(edge, child)| RetainedSetChild { edge, child })
            .collect();
        let node = RetainedSetNode::new(prefix, present, children, entries)
            .expect("sorted builder emits a canonical retained node");
        Ok(self.write_node(node).await?)
    }
}

impl<S: ImmutableObjectStore> SortedSet for ContentSetStore<'_, S> {
    type Id = ContentSetId;
    async fn empty(&self) -> Result<ContentSetId, ContentAccumulatorError> {
        Ok(self.empty_root().await?)
    }
    async fn write(
        &self,
        prefix: Vec<u8>,
        present: bool,
        children: Vec<(u8, ContentSetId)>,
        entries: Vec<Vec<u8>>,
    ) -> Result<ContentSetId, ContentAccumulatorError> {
        let children = children
            .into_iter()
            .map(|(edge, child)| ContentSetChild { edge, child })
            .collect();
        let node = ContentSetNode::new(prefix, present, children, entries)
            .expect("sorted builder emits a canonical content-set node");
        Ok(self.write_node(node).await?)
    }
}

fn decode_digest(bytes: Vec<u8>) -> Result<[u8; 32], rusqlite::Error> {
    bytes.try_into().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            32,
            rusqlite::types::Type::Blob,
            "invalid digest length".into(),
        )
    })
}

fn common_prefix(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

#[derive(Clone, Copy)]
pub(crate) enum RetainedTable {
    Changes,
    Views,
    Revisions,
    Applications,
}

/// The accumulator's sorted, unique key tables.
#[derive(Clone, Copy)]
enum Table {
    Retained(RetainedTable),
    Content,
}

impl Table {
    fn name(self) -> &'static str {
        match self {
            Self::Retained(RetainedTable::Changes) => "retained_changes",
            Self::Retained(RetainedTable::Views) => "retained_views",
            Self::Retained(RetainedTable::Revisions) => "retained_revisions",
            Self::Retained(RetainedTable::Applications) => "retained_applications",
            Self::Content => "content_ids",
        }
    }

    fn insert_sql(self) -> &'static str {
        match self {
            Self::Retained(RetainedTable::Changes) => {
                "INSERT OR IGNORE INTO retained_changes (id) VALUES (?1)"
            }
            Self::Retained(RetainedTable::Views) => {
                "INSERT OR IGNORE INTO retained_views (id) VALUES (?1)"
            }
            Self::Retained(RetainedTable::Revisions) => {
                "INSERT OR IGNORE INTO retained_revisions (id) VALUES (?1)"
            }
            Self::Retained(RetainedTable::Applications) => {
                "INSERT OR IGNORE INTO retained_applications (id) VALUES (?1)"
            }
            Self::Content => "INSERT OR IGNORE INTO content_ids (id) VALUES (?1)",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;
    use object_store::memory::InMemory;
    use wheat_store::{ObjectKind, ObjectStoreAdapter, ObjectStoreError, StoredObject};

    use super::*;

    struct CountingStore {
        inner: ObjectStoreAdapter,
        content_map_reads: AtomicUsize,
    }

    #[async_trait]
    impl ImmutableObjectStore for CountingStore {
        async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
            let object = self.inner.get(id).await?;
            if object.kind == ObjectKind::ContentMapNode {
                self.content_map_reads.fetch_add(1, Ordering::Relaxed);
            }
            Ok(object)
        }

        async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
            self.inner.put_if_absent(id, bytes).await
        }

        async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
            self.inner.exists_many(ids).await
        }
    }

    fn content(index: u32) -> ContentId {
        ContentId::of(&index.to_le_bytes())
    }

    #[tokio::test]
    async fn content_build_matches_the_canonical_builder() {
        let objects = ObjectStoreAdapter::new(Arc::new(InMemory::new()), "content-build");
        let sets = ContentSetStore::new(&objects);
        for count in [0_u32, 1, 2, 300] {
            let mut accumulator = ContentAccumulator::new().unwrap();
            let transaction = accumulator.begin_page().unwrap();
            for index in 0..count {
                ContentAccumulator::insert_content(&transaction, content(index)).unwrap();
                // Duplicates are ignored.
                ContentAccumulator::insert_content(&transaction, content(index)).unwrap();
            }
            transaction.commit().unwrap();
            let actual = accumulator.build_content(&sets).await.unwrap();
            let expected = wheat_core::build_content_set((0..count).map(content), |_| {});
            assert_eq!(actual, expected, "{count} blobs");
        }
    }

    #[tokio::test]
    async fn each_view_reads_only_what_its_commit_changed() {
        const VIEWS: u32 = 40;
        let objects = CountingStore {
            inner: ObjectStoreAdapter::new(Arc::new(InMemory::new()), "view-content"),
            content_map_reads: AtomicUsize::new(0),
        };
        let maps = ContentMapStore::new(&objects);
        let empty = maps.empty_root().await.unwrap();
        let base = maps
            .update_batch(empty, (0..512).map(|index| (content(index), Some(1))))
            .await
            .unwrap();
        let mut views = Vec::new();
        for index in 0..VIEWS {
            let view = maps
                .update_batch(base, [(content(10_000 + index), Some(1))])
                .await
                .unwrap();
            views.push(view);
        }
        let mut accumulator = ContentAccumulator::new().unwrap();
        let transaction = accumulator.begin_page().unwrap();
        ContentAccumulator::insert_view_content(&transaction, &maps, empty, base)
            .await
            .unwrap();
        objects.content_map_reads.store(0, Ordering::Relaxed);
        for view in views {
            ContentAccumulator::insert_view_content(&transaction, &maps, base, view)
                .await
                .unwrap();
        }
        let reads = objects.content_map_reads.load(Ordering::Relaxed);
        transaction.commit().unwrap();

        let sets = ContentSetStore::new(&objects);
        let actual = accumulator.build_content(&sets).await.unwrap();
        let expected = wheat_core::build_content_set(
            (0..512)
                .chain((0..VIEWS).map(|index| 10_000 + index))
                .map(content),
            |_| {},
        );
        assert_eq!(actual, expected);
        // A diff reads the two roots and the one child on each side that changed; a
        // whole map of 512 blobs is over a hundred nodes.
        assert!(
            reads <= VIEWS as usize * 4,
            "{reads} content-map reads for {VIEWS} one-blob views"
        );
    }
}
