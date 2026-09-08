use rusqlite::{Connection, params};
use tempfile::NamedTempFile;
use thiserror::Error;
use wheat_core::{
    ApplicationMapId, ChangeId, ChangeObjectId, ConflictSetId, ContentId, ContentSetId, ObjectId,
    RetainedSetId, RevisionMapId, TreeId,
};

const SCHEMA: &str = include_str!("verification_sql/schema.sql");
const INSERT_CHANGE: &str = include_str!("verification_sql/insert_change.sql");
const CONTAINS_CHANGE: &str = include_str!("verification_sql/contains_change.sql");
const INSERT_CONTENT: &str = include_str!("verification_sql/insert_content.sql");
const CONTAINS_CONTENT: &str = include_str!("verification_sql/contains_content.sql");
const INSERT_RETAINED: &str = include_str!("verification_sql/insert_retained.sql");
const CONTAINS_RETAINED: &str = include_str!("verification_sql/contains_retained.sql");
const INSERT_TREE: &str = include_str!("verification_sql/insert_tree.sql");
const CONTAINS_TREE: &str = include_str!("verification_sql/contains_tree.sql");
const INSERT_VISIBLE_SET: &str = include_str!("verification_sql/insert_visible_set.sql");
const CONTAINS_VISIBLE_SET: &str = include_str!("verification_sql/contains_visible_set.sql");
const INSERT_REVISION_MAP: &str = include_str!("verification_sql/insert_revision_map.sql");
const CONTAINS_REVISION_MAP: &str = include_str!("verification_sql/contains_revision_map.sql");
const INSERT_MAP_PAIR: &str = include_str!("verification_sql/insert_map_pair.sql");
const CONTAINS_MAP_PAIR: &str = include_str!("verification_sql/contains_map_pair.sql");

#[derive(Debug, Error)]
pub enum VerificationScratchError {
    #[error("verification scratch file failed")]
    File(#[from] std::io::Error),
    #[error("verification scratch database failed")]
    Database(#[from] rusqlite::Error),
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

    pub(crate) fn tree_verified(
        &self,
        tree: TreeId,
        aggregate: ContentSetId,
        visible: Option<ContentSetId>,
    ) -> Result<bool, VerificationScratchError> {
        Ok(self.connection.prepare_cached(CONTAINS_TREE)?.query_row(
            params![
                tree.as_bytes().as_slice(),
                aggregate.as_bytes().as_slice(),
                Self::visible_bytes(visible)
            ],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn record_tree(
        &self,
        tree: TreeId,
        aggregate: ContentSetId,
        visible: Option<ContentSetId>,
    ) -> Result<(), VerificationScratchError> {
        self.connection
            .prepare_cached(INSERT_TREE)?
            .execute(params![
                tree.as_bytes().as_slice(),
                aggregate.as_bytes().as_slice(),
                Self::visible_bytes(visible)
            ])?;
        Ok(())
    }

    pub(crate) fn visible_set_verified(
        &self,
        node: ContentSetId,
        aggregate: ContentSetId,
    ) -> Result<bool, VerificationScratchError> {
        Ok(self
            .connection
            .prepare_cached(CONTAINS_VISIBLE_SET)?
            .query_row(
                params![node.as_bytes().as_slice(), aggregate.as_bytes().as_slice()],
                |row| row.get(0),
            )?)
    }

    pub(crate) fn record_visible_set(
        &self,
        node: ContentSetId,
        aggregate: ContentSetId,
    ) -> Result<(), VerificationScratchError> {
        self.connection
            .prepare_cached(INSERT_VISIBLE_SET)?
            .execute(params![
                node.as_bytes().as_slice(),
                aggregate.as_bytes().as_slice()
            ])?;
        Ok(())
    }

    pub(crate) fn revision_map_verified(
        &self,
        node: RevisionMapId,
        retained: RetainedSetId,
    ) -> Result<bool, VerificationScratchError> {
        Ok(self
            .connection
            .prepare_cached(CONTAINS_REVISION_MAP)?
            .query_row(
                params![node.as_bytes().as_slice(), retained.as_bytes().as_slice()],
                |row| row.get(0),
            )?)
    }

    pub(crate) fn record_revision_map(
        &self,
        node: RevisionMapId,
        retained: RetainedSetId,
    ) -> Result<(), VerificationScratchError> {
        self.connection
            .prepare_cached(INSERT_REVISION_MAP)?
            .execute(params![
                node.as_bytes().as_slice(),
                retained.as_bytes().as_slice()
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
