use rusqlite::{Connection, params};
use tempfile::NamedTempFile;
use thiserror::Error;
use wheat_core::{
    ApplicationMapId, ChangeId, ChangeObjectId, ConflictId, ConflictSetId, ContentId, ContentSetId,
    ObjectId, RetainedSetId, RevisionMapId, TreeId,
};

#[derive(Clone, Copy)]
pub(crate) enum RadixReceiptKind {
    // VisibleContent was used by the radix-trie content set walker. With flat sorted
    // blobs, visible_set() resolves the blob directly. Kept for schema stability.
    #[allow(dead_code)]
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
const INSERT_PRODUCED_CONFLICT: &str =
    include_str!("verification_sql/insert_produced_conflict.sql");
const CONTAINS_PRODUCED_CONFLICT: &str =
    include_str!("verification_sql/contains_produced_conflict.sql");
const DELETE_PRODUCED_CONFLICTS: &str =
    include_str!("verification_sql/delete_produced_conflicts.sql");
const INSERT_TREE_CONTENT_COUNT: &str =
    include_str!("verification_sql/insert_tree_content_count.sql");
const CONTENT_COUNT: &str = include_str!("verification_sql/content_count.sql");
const UPDATE_TREE_CONTENT_COUNT: &str =
    include_str!("verification_sql/update_tree_content_count.sql");
const DELETE_TREE_CONTENT_COUNT: &str =
    include_str!("verification_sql/delete_tree_content_count.sql");
const COUNT_TREE_CONTENTS: &str = include_str!("verification_sql/count_tree_contents.sql");

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
    ) -> Result<bool, VerificationScratchError> {
        Ok(self
            .connection
            .prepare_cached(CONTAINS_TREE_SEMANTICS)?
            .query_row(
                params![tree.as_bytes().as_slice(), aggregate.as_bytes().as_slice()],
                |row| row.get(0),
            )?)
    }

    pub(crate) fn record_tree_semantics(
        &self,
        tree: TreeId,
        aggregate: ContentSetId,
    ) -> Result<(), VerificationScratchError> {
        self.connection
            .prepare_cached(INSERT_TREE_SEMANTICS)?
            .execute(params![
                tree.as_bytes().as_slice(),
                aggregate.as_bytes().as_slice()
            ])?;
        Ok(())
    }

    pub(crate) fn clear_tree_content_counts(&self) -> Result<(), VerificationScratchError> {
        self.connection
            .execute("DELETE FROM tree_content_counts", [])?;
        Ok(())
    }

    /// The number of distinct contents with a positive count.
    pub(crate) fn tree_content_entries(&self) -> Result<u64, VerificationScratchError> {
        Ok(self
            .connection
            .prepare_cached(COUNT_TREE_CONTENTS)?
            .query_row([], |row| row.get(0))?)
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
        prefix: &[u8],
        application_skip: usize,
        revision_skip: usize,
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
                    prefix,
                    application_skip,
                    revision_skip,
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
        prefix: &[u8],
        application_skip: usize,
        revision_skip: usize,
        retained: RetainedSetId,
        conflicts: ConflictSetId,
    ) -> Result<(), VerificationScratchError> {
        self.connection
            .prepare_cached(INSERT_MAP_PAIR)?
            .execute(params![
                application.as_bytes().as_slice(),
                revision.as_bytes().as_slice(),
                prefix,
                application_skip,
                revision_skip,
                retained.as_bytes().as_slice(),
                conflicts.as_bytes().as_slice()
            ])?;
        Ok(())
    }

    /// Start collecting conflict-production evidence for one view. Evidence must
    /// never flow between views that happen to share the same conflict-set root.
    pub(crate) fn clear_produced_conflicts(&self) -> Result<(), VerificationScratchError> {
        self.connection.execute(DELETE_PRODUCED_CONFLICTS, [])?;
        Ok(())
    }

    pub(crate) fn insert_produced_conflict(
        &self,
        root: ConflictSetId,
        conflict: ConflictId,
    ) -> Result<(), VerificationScratchError> {
        self.connection
            .prepare_cached(INSERT_PRODUCED_CONFLICT)?
            .execute(params![
                root.as_bytes().as_slice(),
                conflict.as_bytes().as_slice()
            ])?;
        Ok(())
    }

    pub(crate) fn contains_produced_conflict(
        &self,
        root: ConflictSetId,
        conflict: ConflictId,
    ) -> Result<bool, VerificationScratchError> {
        Ok(self
            .connection
            .prepare_cached(CONTAINS_PRODUCED_CONFLICT)?
            .query_row(
                params![root.as_bytes().as_slice(), conflict.as_bytes().as_slice()],
                |row| row.get(0),
            )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_content_entries_count_only_positive_counts() {
        let scratch = VerificationScratch::new().unwrap();
        let one = ContentId::from_digest([7; 32]);
        let two = ContentId::from_digest([8; 32]);
        scratch.adjust_tree_content_count(one, 2).unwrap();
        scratch.adjust_tree_content_count(two, 1).unwrap();
        assert_eq!(scratch.tree_content_entries().unwrap(), 2);
        scratch.adjust_tree_content_count(two, -1).unwrap();
        assert_eq!(scratch.tree_content_count(two).unwrap(), 0);
        assert_eq!(scratch.tree_content_count(one).unwrap(), 2);
        assert_eq!(scratch.tree_content_entries().unwrap(), 1);
        scratch.clear_tree_content_counts().unwrap();
        assert_eq!(scratch.tree_content_entries().unwrap(), 0);
    }
}
