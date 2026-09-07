use std::{future::Future, pin::Pin};

use rusqlite::{Connection, Transaction, params};
use thiserror::Error;
use wheat_core::{
    ContentId, ContentSetChild, ContentSetId, ContentSetNode, ObjectId, RetainedSetChild,
    RetainedSetId, RetainedSetNode,
};
use wheat_store::{CollectionError, ContentSetStore, ImmutableObjectStore, RetainedSetStore};

const CHUNK: usize = 1024;
type BuildFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ContentAccumulatorError>> + 'a>>;

#[derive(Debug, Error)]
pub enum ContentAccumulatorError {
    #[error("finalization accumulator database failed")]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Collection(#[from] CollectionError),
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
             CREATE TABLE content_manifest_roots (id BLOB PRIMARY KEY) WITHOUT ROWID;
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
            .prepare_cached(table.insert_sql())?
            .execute(params![id.as_bytes().as_slice()])?;
        Ok(())
    }

    pub(crate) fn insert_content(
        transaction: &Transaction<'_>,
        id: ContentId,
    ) -> Result<(), ContentAccumulatorError> {
        transaction
            .prepare_cached("INSERT OR IGNORE INTO content_ids (id) VALUES (?1)")?
            .execute(params![id.as_bytes().as_slice()])?;
        Ok(())
    }

    /// Records a per-view manifest root. Its authenticated contents are merged structurally at
    /// the end rather than enumerated into SQLite.
    pub(crate) fn insert_manifest(
        transaction: &Transaction<'_>,
        root: ContentSetId,
    ) -> Result<(), ContentAccumulatorError> {
        transaction
            .prepare_cached("INSERT OR IGNORE INTO content_manifest_roots (id) VALUES (?1)")?
            .execute(params![root.as_bytes().as_slice()])?;
        Ok(())
    }

    pub(crate) async fn build_content<S: ImmutableObjectStore>(
        &mut self,
        store: &ContentSetStore<'_, S>,
    ) -> Result<ContentSetId, ContentAccumulatorError> {
        // Read every authenticated input manifest once into the existing disk-backed unique
        // inventory. Building once from its sorted order avoids materializing an intermediate
        // radix after every manifest while keeping memory independent of repository size.
        let mut after_manifest = None;
        let mut chunk = Vec::with_capacity(CHUNK);
        loop {
            let manifests = self.manifest_root_chunk(after_manifest.as_ref())?;
            if manifests.is_empty() {
                break;
            }
            after_manifest = manifests.last().map(|id| *id.as_bytes());
            for manifest in manifests {
                let mut cursor = store.cursor(manifest).await?;
                loop {
                    while chunk.len() < CHUNK {
                        let Some(id) = cursor.next().await? else {
                            break;
                        };
                        chunk.push(id);
                    }
                    if chunk.is_empty() {
                        break;
                    }
                    let complete = chunk.len() < CHUNK;
                    let transaction = self.connection.transaction()?;
                    for id in chunk.drain(..) {
                        Self::insert_content(&transaction, id)?;
                    }
                    transaction.commit()?;
                    if complete {
                        break;
                    }
                }
            }
        }

        let Some((first, last)) = self.content_bounds()? else {
            return Ok(store.empty_root().await?);
        };
        self.build_content_range(store, first, last, 0).await
    }

    fn build_content_range<'a, S: ImmutableObjectStore>(
        &'a self,
        store: &'a ContentSetStore<'a, S>,
        first: ContentId,
        last: ContentId,
        depth: usize,
    ) -> BuildFuture<'a, ContentSetId> {
        Box::pin(async move {
            if first == last {
                let node = ContentSetNode::new(first.as_bytes()[depth..].to_vec(), true, vec![])
                    .expect("sorted builder emits a canonical content leaf");
                store.write_node(node).await.map_err(Into::into)
            } else {
                let common = common_prefix(&first.as_bytes()[depth..], &last.as_bytes()[depth..]);
                let end = depth + common;
                let mut children = Vec::new();
                for (edge, child_first, child_last) in self.content_groups(first, last, end)? {
                    let child = self
                        .build_content_range(store, child_first, child_last, end + 1)
                        .await?;
                    children.push(ContentSetChild { edge, child });
                }
                let node =
                    ContentSetNode::new(first.as_bytes()[depth..end].to_vec(), false, children)
                        .expect("sorted builder emits a canonical content branch");
                store.write_node(node).await.map_err(Into::into)
            }
        })
    }

    fn manifest_root_chunk(
        &self,
        after: Option<&[u8; 32]>,
    ) -> Result<Vec<ContentSetId>, ContentAccumulatorError> {
        let mut statement = self.connection.prepare(
            "SELECT id FROM content_manifest_roots WHERE (?1 IS NULL OR id > ?1) \
             ORDER BY id LIMIT 1024",
        )?;
        let rows = statement.query_map(params![after.map(|id| id.as_slice())], |row| {
            let bytes: Vec<u8> = row.get(0)?;
            let digest: [u8; 32] = bytes.try_into().map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    32,
                    rusqlite::types::Type::Blob,
                    "invalid digest length".into(),
                )
            })?;
            Ok(ContentSetId::from_digest(digest))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub(crate) async fn build_retained<S: ImmutableObjectStore>(
        &self,
        store: &RetainedSetStore<'_, S>,
        table: RetainedTable,
    ) -> Result<RetainedSetId, ContentAccumulatorError> {
        let Some((first, last)) = self.retained_bounds(table)? else {
            return Ok(store.empty_root().await?);
        };
        self.build_retained_range(store, table, first, last, 0)
            .await
    }

    fn build_retained_range<'a, S: ImmutableObjectStore>(
        &'a self,
        store: &'a RetainedSetStore<'a, S>,
        table: RetainedTable,
        first: ObjectId,
        last: ObjectId,
        depth: usize,
    ) -> BuildFuture<'a, RetainedSetId> {
        Box::pin(async move {
            if first == last {
                let node = RetainedSetNode::new(first.as_bytes()[depth..].to_vec(), true, vec![])
                    .expect("sorted builder emits a canonical retained leaf");
                store.write_node(node).await.map_err(Into::into)
            } else {
                let common = common_prefix(&first.as_bytes()[depth..], &last.as_bytes()[depth..]);
                let end = depth + common;
                let mut children = Vec::new();
                for (edge, child_first, child_last) in
                    self.retained_groups(table, first, last, end)?
                {
                    let child = self
                        .build_retained_range(store, table, child_first, child_last, end + 1)
                        .await?;
                    children.push(RetainedSetChild { edge, child });
                }
                let node =
                    RetainedSetNode::new(first.as_bytes()[depth..end].to_vec(), false, children)
                        .expect("sorted builder emits a canonical retained branch");
                store.write_node(node).await.map_err(Into::into)
            }
        })
    }

    fn content_bounds(&self) -> Result<Option<(ContentId, ContentId)>, ContentAccumulatorError> {
        let mut statement = self.connection.prepare(
            "SELECT (SELECT id FROM content_ids ORDER BY id LIMIT 1), \
             (SELECT id FROM content_ids ORDER BY id DESC LIMIT 1)",
        )?;
        let (first, last) = statement.query_row([], |row| {
            Ok((
                row.get::<_, Option<Vec<u8>>>(0)?,
                row.get::<_, Option<Vec<u8>>>(1)?,
            ))
        })?;
        match (first, last) {
            (Some(first), Some(last)) => Ok(Some((
                ContentId::from_digest(decode_digest(first)?),
                ContentId::from_digest(decode_digest(last)?),
            ))),
            (None, None) => Ok(None),
            _ => unreachable!("both aggregate bounds come from the same table"),
        }
    }

    fn content_groups(
        &self,
        first: ContentId,
        last: ContentId,
        byte: usize,
    ) -> Result<Vec<(u8, ContentId, ContentId)>, ContentAccumulatorError> {
        let mut statement = self.connection.prepare(
            "SELECT substr(id, ?1, 1), MIN(id), MAX(id) FROM content_ids \
             WHERE id >= ?2 AND id <= ?3 GROUP BY substr(id, ?1, 1) ORDER BY 1",
        )?;
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
                ContentId::from_digest(decode_digest(first)?),
                ContentId::from_digest(decode_digest(last)?),
            ))
        })
        .collect()
    }

    fn retained_bounds(
        &self,
        table: RetainedTable,
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

    fn retained_groups(
        &self,
        table: RetainedTable,
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

impl RetainedTable {
    fn name(self) -> &'static str {
        match self {
            Self::Changes => "retained_changes",
            Self::Views => "retained_views",
            Self::Revisions => "retained_revisions",
            Self::Applications => "retained_applications",
        }
    }

    fn insert_sql(self) -> &'static str {
        match self {
            Self::Changes => "INSERT OR IGNORE INTO retained_changes (id) VALUES (?1)",
            Self::Views => "INSERT OR IGNORE INTO retained_views (id) VALUES (?1)",
            Self::Revisions => "INSERT OR IGNORE INTO retained_revisions (id) VALUES (?1)",
            Self::Applications => "INSERT OR IGNORE INTO retained_applications (id) VALUES (?1)",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::memory::InMemory;
    use wheat_store::ObjectStoreAdapter;

    use super::*;

    #[tokio::test]
    async fn content_build_merges_many_overlapping_roots_in_one_canonical_pass() {
        let objects = ObjectStoreAdapter::new(Arc::new(InMemory::new()), "multi-root-content");
        let sets = ContentSetStore::new(&objects);
        let empty = sets.empty_root().await.unwrap();
        let common = ContentId::from_digest([7; 32]);
        let mut accumulator = ContentAccumulator::new().unwrap();

        for index in 0..40_u8 {
            let mut bytes = [index; 32];
            bytes[31] = index.wrapping_mul(17);
            let unique = ContentId::from_digest(bytes);
            let root = sets
                .update_batch(empty, [(common, true), (unique, true)])
                .await
                .unwrap();
            let transaction = accumulator.begin_page().unwrap();
            ContentAccumulator::insert_manifest(&transaction, root).unwrap();
            transaction.commit().unwrap();
        }
        let historical = ContentId::from_digest([0xfe; 32]);
        let transaction = accumulator.begin_page().unwrap();
        ContentAccumulator::insert_content(&transaction, historical).unwrap();
        transaction.commit().unwrap();

        let actual = accumulator.build_content(&sets).await.unwrap();
        let mut expected_entries = vec![(common, true), (historical, true)];
        for index in 0..40_u8 {
            let mut bytes = [index; 32];
            bytes[31] = index.wrapping_mul(17);
            expected_entries.push((ContentId::from_digest(bytes), true));
        }
        let expected = sets.update_batch(empty, expected_entries).await.unwrap();
        assert_eq!(actual, expected);
    }
}
