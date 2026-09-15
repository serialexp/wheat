use rusqlite::{Connection, params};
use tempfile::NamedTempFile;
use thiserror::Error;
use wheat_core::{
    ApplicationMapId, ChangeId, ChangeObjectId, ConflictSetId, ContentId, ContentSetId, ObjectId,
    RetainedSetId, RevisionMapId, TreeId,
};

#[derive(Clone, Copy)]
pub(crate) enum RadixReceiptKind {
    VisibleContent = 1,
    RevisionMap = 2,
}

const SCHEMA: &str = include_str!("verification_sql/schema.sql");
const INSERT_CHANGE: &str = include_str!("verification_sql/insert_change.sql");
const CONTAINS_CHANGE: &str = include_str!("verification_sql/contains_change.sql");
const INSERT_CONTENT: &str = include_str!("verification_sql/insert_content.sql");
const CONTAINS_CONTENT: &str = include_str!("verification_sql/contains_content.sql");
const INSERT_RETAINED: &str = include_str!("verification_sql/insert_retained.sql");
const CONTAINS_RETAINED: &str = include_str!("verification_sql/contains_retained.sql");
const INSERT_TREE_AUTH: &str = include_str!("verification_sql/insert_tree_auth.sql");
const CONTAINS_TREE_AUTH: &str = include_str!("verification_sql/contains_tree_auth.sql");
const INSERT_TREE_SEMANTICS: &str = include_str!("verification_sql/insert_tree_semantics.sql");
const CONTAINS_TREE_SEMANTICS: &str = include_str!("verification_sql/contains_tree_semantics.sql");
const INSERT_RADIX_NODE: &str = include_str!("verification_sql/insert_radix_node.sql");
const CONTAINS_RADIX_NODE: &str = include_str!("verification_sql/contains_radix_node.sql");
const INSERT_MAP_PAIR: &str = include_str!("verification_sql/insert_map_pair.sql");
const CONTAINS_MAP_PAIR: &str = include_str!("verification_sql/contains_map_pair.sql");
const INSERT_TREE_CONTENT_COUNT: &str =
    include_str!("verification_sql/insert_tree_content_count.sql");
const CONTENT_COUNT: &str = include_str!("verification_sql/content_count.sql");
const UPDATE_TREE_CONTENT_COUNT: &str =
    include_str!("verification_sql/update_tree_content_count.sql");
const DELETE_TREE_CONTENT_COUNT: &str =
    include_str!("verification_sql/delete_tree_content_count.sql");

#[derive(Debug, Error)]
pub enum VerificationScratchError {
    #[error("verification scratch file failed")]
    File(#[from] std::io::Error),
    #[error("verification scratch database failed")]
    Database(#[from] rusqlite::Error),
    #[error("tree content count underflow for {content}")]
    TreeContentCountUnderflow { content: ContentId },
    #[error("tree content count overflow for {content}")]
    TreeContentCountOverflow { content: ContentId },
}

/// A fresh, bounded-cache, automatically deleted index for one verification run.
pub(crate) struct VerificationScratch {
    connection: Connection,
    _file: NamedTempFile,
}

impl VerificationScratch {
    pub(crate) fn new() -> Result<Self, VerificationScratchError> {
        let file = NamedTempFile::new()?;
        let connection = Connection::open(file.path())?;
        connection.execute_batch(SCHEMA)?;
        Ok(Self {
            connection,
            _file: file,
        })
    }

    pub(crate) fn insert_change(
        &self,
        logical: ChangeId,
        object: ChangeObjectId,
    ) -> Result<bool, VerificationScratchError> {
        Ok(self
            .connection
            .prepare_cached(INSERT_CHANGE)?
            .execute(params![
                logical.as_bytes().as_slice(),
                object.as_bytes().as_slice()
            ])?
            == 1)
    }

    pub(crate) fn contains_change(&self, id: ChangeId) -> Result<bool, VerificationScratchError> {
        Ok(self
            .connection
            .prepare_cached(CONTAINS_CHANGE)?
            .query_row([id.as_bytes().as_slice()], |row| row.get(0))?)
    }

    pub(crate) fn insert_content(&self, id: ContentId) -> Result<(), VerificationScratchError> {
        self.connection
            .prepare_cached(INSERT_CONTENT)?
            .execute([id.as_bytes().as_slice()])?;
        Ok(())
    }

    pub(crate) fn contains_content(&self, id: ContentId) -> Result<bool, VerificationScratchError> {
        Ok(self
            .connection
            .prepare_cached(CONTAINS_CONTENT)?
            .query_row([id.as_bytes().as_slice()], |row| row.get(0))?)
    }

    pub(crate) fn insert_retained(
        &self,
        family: u8,
        id: ObjectId,
    ) -> Result<(), VerificationScratchError> {
        self.connection
            .prepare_cached(INSERT_RETAINED)?
            .execute(params![family, id.as_bytes().as_slice()])?;
        Ok(())
    }

    pub(crate) fn contains_retained(
        &self,
        family: u8,
        id: ObjectId,
    ) -> Result<bool, VerificationScratchError> {
        Ok(self
            .connection
            .prepare_cached(CONTAINS_RETAINED)?
            .query_row(params![family, id.as_bytes().as_slice()], |row| row.get(0))?)
    }

    fn visible_bytes(visible: Option<ContentSetId>) -> Vec<u8> {
        visible.map_or_else(Vec::new, |id| id.as_bytes().to_vec())
    }

    pub(crate) fn tree_height(
        &self,
        tree: TreeId,
    ) -> Result<Option<usize>, VerificationScratchError> {
        match self
            .connection
            .prepare_cached(CONTAINS_TREE_AUTH)?
            .query_row([tree.as_bytes().as_slice()], |row| row.get(0))
        {
            Ok(height) => Ok(Some(height)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn record_tree_height(
        &self,
        tree: TreeId,
        height: usize,
    ) -> Result<(), VerificationScratchError> {
        self.connection
            .prepare_cached(INSERT_TREE_AUTH)?
            .execute(params![tree.as_bytes().as_slice(), height])?;
        Ok(())
    }

    pub(crate) fn tree_semantics_verified(
        &self,
        tree: TreeId,
        aggregate: ContentSetId,
        visible: Option<ContentSetId>,
    ) -> Result<bool, VerificationScratchError> {
        Ok(self
            .connection
            .prepare_cached(CONTAINS_TREE_SEMANTICS)?
            .query_row(
                params![
                    tree.as_bytes().as_slice(),
                    aggregate.as_bytes().as_slice(),
                    Self::visible_bytes(visible)
                ],
                |row| row.get(0),
            )?)
    }

    pub(crate) fn record_tree_semantics(
        &self,
        tree: TreeId,
        aggregate: ContentSetId,
        visible: Option<ContentSetId>,
    ) -> Result<(), VerificationScratchError> {
        self.connection
            .prepare_cached(INSERT_TREE_SEMANTICS)?
            .execute(params![
                tree.as_bytes().as_slice(),
                aggregate.as_bytes().as_slice(),
                Self::visible_bytes(visible)
            ])?;
        Ok(())
    }

    pub(crate) fn clear_tree_content_counts(&self) -> Result<(), VerificationScratchError> {
        self.connection
            .execute("DELETE FROM tree_content_counts", [])?;
        Ok(())
    }

    pub(crate) fn adjust_tree_content_count(
        &self,
        content: ContentId,
        delta: i64,
    ) -> Result<(), VerificationScratchError> {
        let count = self.tree_content_count(content)? as i64;
        let next = count
            .checked_add(delta)
            .ok_or(VerificationScratchError::TreeContentCountOverflow { content })?;
        if next < 0 {
            return Err(VerificationScratchError::TreeContentCountUnderflow { content });
        }
        if next == 0 {
            self.connection
                .prepare_cached(DELETE_TREE_CONTENT_COUNT)?
                .execute([content.as_bytes().as_slice()])?;
        } else if count == 0 {
            self.connection
                .prepare_cached(INSERT_TREE_CONTENT_COUNT)?
                .execute(params![content.as_bytes().as_slice(), next])?;
        } else {
            self.connection
                .prepare_cached(UPDATE_TREE_CONTENT_COUNT)?
                .execute(params![content.as_bytes().as_slice(), delta])?;
        }
        Ok(())
    }

    pub(crate) fn tree_content_count(
        &self,
        content: ContentId,
    ) -> Result<u64, VerificationScratchError> {
        match self
            .connection
            .prepare_cached(CONTENT_COUNT)?
            .query_row([content.as_bytes().as_slice()], |row| row.get(0))
        {
            Ok(count) => Ok(count),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn begin_tree_content_savepoint(
        &self,
        depth: usize,
    ) -> Result<(), VerificationScratchError> {
        self.connection
            .execute_batch(&format!("SAVEPOINT tree_content_{depth}"))?;
        Ok(())
    }

    pub(crate) fn rollback_tree_content_savepoint(
        &self,
        depth: usize,
    ) -> Result<(), VerificationScratchError> {
        self.connection.execute_batch(&format!(
            "ROLLBACK TO tree_content_{depth}; RELEASE tree_content_{depth}"
        ))?;
        Ok(())
    }

    /// A contextual receipt is valid only for the exact prefix at which this
    /// canonical node was reached. A node ID alone is insufficient because the
    /// same path-compressed node can occur beneath multiple incoming edges.
    pub(crate) fn radix_node_verified(
        &self,
        kind: RadixReceiptKind,
        context: &[u8],
        node: ObjectId,
        prefix: &[u8],
    ) -> Result<bool, VerificationScratchError> {
        Ok(self
            .connection
            .prepare_cached(CONTAINS_RADIX_NODE)?
            .query_row(
                params![kind as u8, context, node.as_bytes().as_slice(), prefix],
                |row| row.get(0),
            )?)
    }

    pub(crate) fn record_radix_node(
        &self,
        kind: RadixReceiptKind,
        context: &[u8],
        node: ObjectId,
        prefix: &[u8],
    ) -> Result<(), VerificationScratchError> {
        self.connection
            .prepare_cached(INSERT_RADIX_NODE)?
            .execute(params![
                kind as u8,
                context,
                node.as_bytes().as_slice(),
                prefix
            ])?;
        Ok(())
    }

    pub(crate) fn map_pair_verified(
        &self,
        application: ApplicationMapId,
        revision: RevisionMapId,
        retained: RetainedSetId,
        conflicts: ConflictSetId,
    ) -> Result<bool, VerificationScratchError> {
        Ok(self
            .connection
            .prepare_cached(CONTAINS_MAP_PAIR)?
            .query_row(
                params![
                    application.as_bytes().as_slice(),
                    revision.as_bytes().as_slice(),
                    retained.as_bytes().as_slice(),
                    conflicts.as_bytes().as_slice()
                ],
                |row| row.get(0),
            )?)
    }

    pub(crate) fn record_map_pair(
        &self,
        application: ApplicationMapId,
        revision: RevisionMapId,
        retained: RetainedSetId,
        conflicts: ConflictSetId,
    ) -> Result<(), VerificationScratchError> {
        self.connection
            .prepare_cached(INSERT_MAP_PAIR)?
            .execute(params![
                application.as_bytes().as_slice(),
                revision.as_bytes().as_slice(),
                retained.as_bytes().as_slice(),
                conflicts.as_bytes().as_slice()
            ])?;
        Ok(())
    }
}
