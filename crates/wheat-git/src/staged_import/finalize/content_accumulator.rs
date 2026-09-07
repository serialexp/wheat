use rusqlite::{Connection, OptionalExtension, Transaction, params};
use thiserror::Error;
use wheat_core::{ContentId, ContentSetId, ObjectId, RetainedSetId};
use wheat_store::{CollectionError, ContentSetStore, ImmutableObjectStore, RetainedSetStore};

const CHUNK: usize = 1024;

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
             CREATE TABLE visited_content_nodes (
                 id BLOB NOT NULL,
                 prefix BLOB NOT NULL,
                 PRIMARY KEY (id, prefix)
             ) WITHOUT ROWID;
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

    pub(crate) async fn insert_manifest<S: ImmutableObjectStore>(
        transaction: &Transaction<'_>,
        store: &ContentSetStore<'_, S>,
        root: ContentSetId,
    ) -> Result<(), ContentAccumulatorError> {
        let node = store.read_node(root).await?;
        if node.depth != 0 {
            return Err(CollectionError::InvalidRootDepth.into());
        }
        let key = [0_u8; 32];
        let mut stack = vec![(root, node, 0_usize, key)];
        while let Some((id, node, next_child, node_key)) = stack.last_mut() {
            if *next_child == 0 && node.present {
                if node.depth != 32 {
                    return Err(ContentAccumulatorError::NonterminalValue);
                }
                transaction
                    .prepare_cached("INSERT OR IGNORE INTO content_ids (id) VALUES (?1)")?
                    .execute(params![node_key.as_slice()])?;
            }
            if *next_child == node.children.len() {
                transaction
                    .prepare_cached(
                        "INSERT OR IGNORE INTO visited_content_nodes (id, prefix) VALUES (?1, ?2)",
                    )?
                    .execute(params![id.as_bytes().as_slice(), node_key.as_slice()])?;
                stack.pop();
                continue;
            }

            let child = node.children[*next_child];
            *next_child += 1;
            if usize::from(node.depth) >= node_key.len() {
                return Err(CollectionError::InvalidChildDepth.into());
            }
            let mut child_key = *node_key;
            child_key[usize::from(node.depth)] = child.edge;
            if is_visited(transaction, child.child, &child_key)? {
                continue;
            }
            let child_node = store.read_node(child.child).await?;
            if child_node.depth != node.depth + 1 {
                return Err(CollectionError::InvalidChildDepth.into());
            }
            stack.push((child.child, child_node, 0, child_key));
        }
        Ok(())
    }

    pub(crate) async fn build_content<S: ImmutableObjectStore>(
        &self,
        store: &ContentSetStore<'_, S>,
    ) -> Result<ContentSetId, ContentAccumulatorError> {
        let mut root = store.empty_root().await?;
        let mut after = None;
        loop {
            let chunk = self.content_chunk(after.as_ref())?;
            if chunk.is_empty() {
                return Ok(root);
            }
            after = chunk.last().map(|id| *id.as_bytes());
            root = store
                .update_batch(root, chunk.into_iter().map(|id| (id, true)))
                .await?;
        }
    }

    pub(crate) async fn build_retained<S: ImmutableObjectStore>(
        &self,
        store: &RetainedSetStore<'_, S>,
        table: RetainedTable,
    ) -> Result<RetainedSetId, ContentAccumulatorError> {
        let mut root = store.empty_root().await?;
        let mut after = None;
        loop {
            let chunk = self.retained_chunk(table, after.as_ref())?;
            if chunk.is_empty() {
                return Ok(root);
            }
            after = chunk.last().map(|id| *id.as_bytes());
            root = store
                .update_batch(root, chunk.into_iter().map(|id| (id, true)))
                .await?;
        }
    }

    fn content_chunk(
        &self,
        after: Option<&[u8; 32]>,
    ) -> Result<Vec<ContentId>, ContentAccumulatorError> {
        let mut statement = self.connection.prepare(
            "SELECT id FROM content_ids WHERE (?1 IS NULL OR id > ?1) ORDER BY id LIMIT 1024",
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
            Ok(ContentId::from_digest(digest))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn retained_chunk(
        &self,
        table: RetainedTable,
        after: Option<&[u8; 32]>,
    ) -> Result<Vec<ObjectId>, ContentAccumulatorError> {
        let sql = format!(
            "SELECT id FROM {} WHERE (?1 IS NULL OR id > ?1) ORDER BY id LIMIT {CHUNK}",
            table.name()
        );
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(params![after.map(|id| id.as_slice())], |row| {
            let bytes: Vec<u8> = row.get(0)?;
            let digest: [u8; 32] = bytes.try_into().map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    32,
                    rusqlite::types::Type::Blob,
                    "invalid digest length".into(),
                )
            })?;
            Ok(ObjectId::from_digest(digest))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

fn is_visited(
    transaction: &Transaction<'_>,
    id: ContentSetId,
    prefix: &[u8; 32],
) -> Result<bool, rusqlite::Error> {
    Ok(transaction
        .prepare_cached("SELECT 1 FROM visited_content_nodes WHERE id = ?1 AND prefix = ?2")?
        .query_row(params![id.as_bytes().as_slice(), prefix.as_slice()], |_| {
            Ok(())
        })
        .optional()?
        .is_some())
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
