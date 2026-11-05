//! A local cache of which changes touch each path, so a path's history costs what
//! its hits number instead of a walk over the whole history.
//!
//! For every application it indexes, the index holds the files its change changed
//! and their ancestor directories (not the root, whose history is the plain
//! history), at the application's canonical position. No writer knows about the
//! index: a query first brings it up to the view it asks about, walking back from the
//! view's producing application to the first one the index holds and adding what each
//! application passed changed, oldest first. An indexed application's predecessors
//! are therefore always indexed, and two processes catching up at once only write the
//! same rows twice.
//!
//! A store can hold several views' histories (a fork network, a rewritten label), so
//! a path's rows include applications other views hold. A query keeps the rows whose
//! application is the one the view's application map holds for its change.
//!
//! The views are the authority; the index can be deleted at any time.

use std::{
    num::NonZeroUsize,
    path::Path,
    sync::{Mutex, MutexGuard, PoisonError},
};

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;
use wheat_core::{
    ApplicationId, ApplicationResultRecord, ChangeId, RevisionId, TreeId, ViewId, ViewRecord,
};

use crate::{
    ApplicationMapStore, ImmutableObjectStore, MerkleTreeError, MerkleTreeStore,
    RepositoryPublicationError, RepositoryReader, TreeDiffLimits,
};

const SCHEMA_VERSION: i64 = 1;
const SCHEMA: &str = include_str!("path_history_index_sql/schema.sql");
const DROP: &str = include_str!("path_history_index_sql/drop.sql");
const HAS_APPLICATION: &str = include_str!("path_history_index_sql/has_application.sql");
const INSERT_APPLICATION: &str = include_str!("path_history_index_sql/insert_application.sql");
const APPLICATION_ID: &str = include_str!("path_history_index_sql/application_id.sql");
const INSERT_PATH: &str = include_str!("path_history_index_sql/insert_path.sql");
const PATH_ID: &str = include_str!("path_history_index_sql/path_id.sql");
const INSERT_TOUCH: &str = include_str!("path_history_index_sql/insert_touch.sql");
const TOUCHES: &str = include_str!("path_history_index_sql/touches.sql");

/// How long a write waits for another process's write to finish.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How many applications catching up writes in one transaction.
const CHUNK_APPLICATIONS: usize = 1024;

/// How many candidate rows a query reads from the index at a time.
const CANDIDATE_BATCH: i64 = 256;

#[derive(Debug, Error)]
pub enum PathHistoryIndexError {
    #[error("path history index file failed")]
    File(#[from] std::io::Error),
    #[error("path history index database failed")]
    Database(#[from] rusqlite::Error),
    #[error("path history index holds a malformed row")]
    Malformed,
    #[error("application order does not line up with the view")]
    OrderMismatch,
    #[error(transparent)]
    Read(#[from] RepositoryPublicationError),
    #[error(transparent)]
    Tree(#[from] MerkleTreeError),
}

/// One change that touches a path in a view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PathTouch {
    /// The change's index in the view's canonical order.
    pub position: u64,
    pub application: ApplicationId,
    pub change: ChangeId,
}

/// A page of [`PathHistoryIndex::touching`], newest first.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PathTouches {
    pub touches: Vec<PathTouch>,
    /// The `before` for the next page; none when no older change touches the path.
    pub next: Option<u64>,
}

/// What catching up read for one application, before its paths are known.
struct Pending {
    id: ApplicationId,
    position: u64,
    revision: RevisionId,
    tree: TreeId,
    target: ViewId,
}

/// Indexed applications not written yet: each one's ID, change, position and the end
/// of its paths in `paths`.
#[derive(Default)]
struct Chunk {
    applications: Vec<(ApplicationId, ChangeId, i64, usize)>,
    paths: Vec<String>,
}

/// The changes that touch each path, kept in a SQLite file next to the store. See the
/// module documentation.
///
/// The connection sits behind a mutex so a query future holding `&self` across an
/// await is `Send`; it is never held across one.
pub struct PathHistoryIndex {
    connection: Mutex<Connection>,
}

impl PathHistoryIndex {
    /// Opens the index at `path`, creating it when missing. A file this version cannot
    /// read is emptied: the index holds nothing the views do not.
    pub fn open(path: &Path) -> Result<Self, PathHistoryIndexError> {
        let connection = match Self::connect(path) {
            Ok(connection) => connection,
            Err(PathHistoryIndexError::Database(_)) => {
                for suffix in ["", "-wal", "-shm"] {
                    let mut file = path.as_os_str().to_owned();
                    file.push(suffix);
                    match std::fs::remove_file(&file) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                Self::connect(path)?
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn connect(path: &Path) -> Result<Connection, PathHistoryIndexError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            connection.execute_batch(DROP)?;
            connection.execute_batch(SCHEMA)?;
            connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(connection)
    }

    /// The connection. A panic mid-statement leaves an open transaction that rolls
    /// back, so a poisoned lock is taken as is.
    fn connection(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Indexes the applications in `view`'s history the index does not hold yet and
    /// returns how many. Costs an application and a revision read for each, a tree
    /// diff for each, and one more application read to find where to stop.
    #[tracing::instrument(name = "path_history_catch_up", level = "debug", skip_all)]
    pub async fn catch_up<O: ImmutableObjectStore>(
        &self,
        reader: &RepositoryReader<'_, O>,
        view: &ViewRecord,
    ) -> Result<usize, PathHistoryIndexError> {
        let mut pending = Vec::new();
        let mut next = reader.producing_application(view).await?;
        while let Some(id) = next {
            if self.has_application(id)? {
                break;
            }
            let record = reader.application(id).await?;
            if record.id() != id {
                return Err(RepositoryPublicationError::InvalidReferences(
                    "application does not match its identity",
                )
                .into());
            }
            next = record.order.previous;
            pending.push(Pending {
                id,
                position: record.order.position,
                revision: record.revision_id,
                tree: result_tree(&record.result),
                target: record.target_view,
            });
        }
        let Some(oldest) = pending.last() else {
            return Ok(0);
        };
        // A view's tree is its producing application's result, so after the oldest
        // application each one's parent tree is the one before it's result.
        let mut parent = reader.view(oldest.target).await?.tree;
        let trees = MerkleTreeStore::new(reader.objects);
        let limits = TreeDiffLimits {
            max_changes: usize::MAX,
            max_nodes: usize::MAX,
            max_entries: usize::MAX,
            max_buffered_entries: usize::MAX,
            ..TreeDiffLimits::default()
        };
        let mut chunk = Chunk::default();
        for application in pending.iter().rev() {
            let change = reader.revision(application.revision).await?.change_id;
            let position = i64::try_from(application.position)
                .map_err(|_| PathHistoryIndexError::Malformed)?;
            let start = chunk.paths.len();
            let mut diff = trees.diff_cursor(parent, application.tree, limits);
            while let Some(changed) = diff.next().await? {
                push_touched(&mut chunk.paths, start, changed.path);
            }
            chunk
                .applications
                .push((application.id, change, position, chunk.paths.len()));
            if chunk.applications.len() == CHUNK_APPLICATIONS {
                self.write(&chunk)?;
                chunk.applications.clear();
                chunk.paths.clear();
            }
            parent = application.tree;
        }
        self.write(&chunk)?;
        Ok(pending.len())
    }

    fn has_application(&self, id: ApplicationId) -> Result<bool, PathHistoryIndexError> {
        Ok(self
            .connection()
            .prepare_cached(HAS_APPLICATION)?
            .exists([id.as_bytes()])?)
    }

    /// Writes `chunk` in one transaction.
    fn write(&self, chunk: &Chunk) -> Result<(), PathHistoryIndexError> {
        if chunk.applications.is_empty() {
            return Ok(());
        }
        let mut connection = self.connection();
        let transaction = connection.transaction()?;
        {
            let mut insert_application = transaction.prepare_cached(INSERT_APPLICATION)?;
            let mut application_id = transaction.prepare_cached(APPLICATION_ID)?;
            let mut insert_path = transaction.prepare_cached(INSERT_PATH)?;
            let mut path_id = transaction.prepare_cached(PATH_ID)?;
            let mut insert_touch = transaction.prepare_cached(INSERT_TOUCH)?;
            let mut start = 0;
            for (application, change, position, end) in &chunk.applications {
                insert_application.execute(params![
                    application.as_bytes(),
                    change.as_bytes(),
                    position
                ])?;
                let row: i64 =
                    application_id.query_row([application.as_bytes()], |row| row.get(0))?;
                for path in &chunk.paths[start..*end] {
                    insert_path.execute([path])?;
                    let path: i64 = path_id.query_row([path], |row| row.get(0))?;
                    insert_touch.execute(params![path, position, row])?;
                }
                start = *end;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Up to `limit` of the changes in `view` before canonical position `before` (all
    /// of them when `None`) that touch `path`, newest first. A file's changes are the
    /// ones that changed it; a directory's, the ones that changed anything below it.
    /// Catches the index up to `view` first. Each candidate row costs an O(log n)
    /// application map lookup, and a row only counts when it is in `view`; a page
    /// also looks at the next hit to tell whether there is another page.
    #[tracing::instrument(name = "path_history_touching", level = "debug", skip_all)]
    pub async fn touching<O: ImmutableObjectStore>(
        &self,
        reader: &RepositoryReader<'_, O>,
        view: &ViewRecord,
        path: &str,
        before: Option<u64>,
        limit: NonZeroUsize,
    ) -> Result<PathTouches, PathHistoryIndexError> {
        self.catch_up(reader, view).await?;
        let size = view.order.size;
        let before = before.unwrap_or(size).min(size);
        let Some(path) = self.path_id(path)? else {
            return Ok(PathTouches::default());
        };
        let applications = ApplicationMapStore::new(reader.objects);
        let mut cursor = (
            i64::try_from(before).map_err(|_| PathHistoryIndexError::Malformed)?,
            i64::MIN,
        );
        let mut touches = Vec::with_capacity(limit.get().min(1024));
        loop {
            let candidates = self.candidates(path, cursor)?;
            let Some(last) = candidates.last() else {
                return Ok(PathTouches {
                    touches,
                    next: None,
                });
            };
            cursor = (last.position, last.row);
            for candidate in candidates {
                let held = applications
                    .get(view.applications, candidate.touch.change)
                    .await
                    .map_err(RepositoryPublicationError::from)?;
                if held != Some(candidate.touch.application) {
                    continue;
                }
                if touches.len() == limit.get() {
                    let next = touches.last().map(|touch: &PathTouch| touch.position);
                    return Ok(PathTouches { touches, next });
                }
                touches.push(candidate.touch);
            }
        }
    }

    fn path_id(&self, path: &str) -> Result<Option<i64>, PathHistoryIndexError> {
        Ok(self
            .connection()
            .prepare_cached(PATH_ID)?
            .query_row([path], |row| row.get(0))
            .optional()?)
    }

    /// The next batch of `path`'s rows below `cursor` (a position and an application
    /// row), newest first.
    fn candidates(
        &self,
        path: i64,
        cursor: (i64, i64),
    ) -> Result<Vec<Candidate>, PathHistoryIndexError> {
        let connection = self.connection();
        let mut statement = connection.prepare_cached(TOUCHES)?;
        let mut rows = statement.query(params![path, cursor.0, cursor.1, CANDIDATE_BATCH])?;
        let mut candidates = Vec::new();
        while let Some(row) = rows.next()? {
            let application: [u8; 32] = row
                .get::<_, Vec<u8>>(0)?
                .try_into()
                .map_err(|_| PathHistoryIndexError::Malformed)?;
            let change: [u8; 16] = row
                .get::<_, Vec<u8>>(1)?
                .try_into()
                .map_err(|_| PathHistoryIndexError::Malformed)?;
            let position: i64 = row.get(2)?;
            candidates.push(Candidate {
                touch: PathTouch {
                    position: u64::try_from(position)
                        .map_err(|_| PathHistoryIndexError::Malformed)?,
                    application: ApplicationId::from_digest(application),
                    change: ChangeId::from_bytes(change),
                },
                position,
                row: row.get(3)?,
            });
        }
        Ok(candidates)
    }
}

/// A row of the index for a path, with where it sits in the scan.
struct Candidate {
    touch: PathTouch,
    position: i64,
    row: i64,
}

/// The tree an application leaves: its clean tree, or its partial tree.
fn result_tree(result: &ApplicationResultRecord) -> TreeId {
    match result {
        ApplicationResultRecord::Clean { tree } => *tree,
        ApplicationResultRecord::Conflicted { partial_tree, .. } => *partial_tree,
    }
}

/// Adds those ancestor directories of the changed file `file` this change has not
/// added yet to `paths`, then `file`; the entries from `start` on are this change's. A
/// diff reports a directory's files one after another, so a directory is new unless
/// it holds the file added just before, which is the last entry.
fn push_touched(paths: &mut Vec<String>, start: usize, file: String) {
    let previous = paths[start..].last().map_or("", String::as_str);
    let mut end = file.len();
    let mut new = 0;
    while let Some(slash) = file[..end].rfind('/') {
        if previous.starts_with(&file[..=slash]) {
            break;
        }
        new += 1;
        end = slash;
    }
    let mut end = file.len();
    for _ in 0..new {
        end = file[..end].rfind('/').unwrap_or(0);
        paths.push(file[..end].to_owned());
    }
    paths.push(file);
}
#[cfg(test)]
mod tests {
    use super::push_touched;

    /// The paths one change touches, given the files its diff reports in order.
    fn touched(files: &[&str]) -> Vec<String> {
        let mut paths = vec!["earlier/change".to_owned()];
        for file in files {
            push_touched(&mut paths, 1, (*file).to_owned());
        }
        paths.split_off(1)
    }

    #[test]
    fn each_ancestor_directory_is_added_once_and_the_root_never() {
        assert_eq!(
            touched(&["a/b/x", "a/b/y", "a/c", "a-b/z", "b"]),
            ["a/b", "a", "a/b/x", "a/b/y", "a/c", "a-b", "a-b/z", "b"]
        );
        // A name that only shares a prefix with the directory before is not in it.
        assert_eq!(touched(&["ab/x", "a/y"]), ["ab", "ab/x", "a", "a/y"]);
        // Another change's paths do not count as this one's.
        assert_eq!(touched(&["earlier/x"]), ["earlier", "earlier/x"]);
    }
}
