//! Repository registry — maps repository names to stores.
//!
//! A registry either holds a fixed set of repositories, open for as long as it lives,
//! or opens them when asked through a [`RepositoryOpener`] the host supplies (a
//! directory of repositories, or the host's own lookup) and closes them once idle. A
//! session or publish holds its repository's [`RepositoryEntry`] while it runs, and
//! that is the lease: a repository is closed only when nothing holds it, it has not
//! been asked for in [`RegistryLimits::idle`], or more than
//! [`RegistryLimits::max_open`] are open and it is the least recently used. The
//! repositories of a fork network share its store ([`crate::network`]), which is
//! closed with the last of them, releasing the packed store's `catalog.lock` so
//! another process (maintenance, or a move to another host) can open it. Two sessions
//! asking for a closed repository at once open it once.
//!
//! A host that moves a repository's primary between machines fences the old one in
//! its [`HeadStore`]: a publish is acknowledged only once [`HeadStore::advance`]
//! moved the head, so an advance that refuses a stale primary (for example a
//! compare-and-swap on a database row that includes a fencing token) refuses the
//! publish. What a stale primary wrote to its store before that is content-addressed
//! and named by no head.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::OnceCell;
use tokio::time::Instant;
use tracing::{debug, info};
use uuid::Uuid;
use wheat_store::{
    NetworkStore, ObjectStoreError, PackedObjectStore, RepositoriesDirectory,
    RepositoriesDirectoryError, RepositoryDirectory, RepositoryStore,
};

use crate::admin::Operations;
use crate::commit::GroupCommit;
use crate::heads::HeadStore;
use crate::network::{Network, OpenNetworks};
use crate::quiesce::Quiesced;

/// A single repository: the stores of its fork network, and its head.
pub struct RepositoryEntry {
    pub network: Network,
    pub heads: Box<dyn HeadStore>,
    /// Publishes take turns joining onto the head, so none merges against a head
    /// another is about to replace, and are made durable in batches.
    pub commit: GroupCommit,
    /// Set when the repository is deleted: sessions using the entry end.
    removed: tokio::sync::watch::Sender<bool>,
}

impl RepositoryEntry {
    /// Opens the packed store in `data_dir/storage` and its publication head.
    pub fn open(data_dir: &Path) -> Result<Self, ObjectStoreError> {
        let directory = RepositoryDirectory::new(data_dir);
        Ok(Self::new(
            Box::new(directory.open_store()?),
            directory.publications(),
        ))
    }

    /// A repository alone in its network, over `store`.
    pub fn new(store: Box<dyn RepositoryStore>, heads: impl HeadStore + 'static) -> Self {
        Self::in_network(Network::new(store), heads)
    }

    /// A repository of `network`, whose head is `heads`.
    pub fn in_network(network: Network, heads: impl HeadStore + 'static) -> Self {
        Self {
            network,
            heads: Box::new(heads),
            commit: GroupCommit::default(),
            removed: tokio::sync::watch::Sender::new(false),
        }
    }

    /// Resolves once the repository has been deleted. A session using the entry ends
    /// when it does, so the store can close.
    pub async fn removed(&self) {
        let mut removed = self.removed.subscribe();
        // The sender is in the entry this borrows, so the wait cannot fail.
        let _ = removed.wait_for(|removed| *removed).await;
    }

    fn mark_removed(&self) {
        self.removed.send_replace(true);
    }
}

/// Opens repositories by name for a [`Registry`].
#[async_trait]
pub trait RepositoryOpener: Send + Sync {
    /// The repository `name`, or `None` when there is none. `name` is a valid
    /// repository path (`owner/name`, see [`wheat_protocol::repository_name`]).
    /// Opening can block on disk; the opener runs it where that is fine.
    async fn open(&self, name: &str) -> Result<Option<RepositoryEntry>, OpenError>;

    /// The repositories directory this opener serves, when it serves one: what the
    /// maintenance operations ([`crate::admin`]) work on.
    fn directory(&self) -> Option<DirectoryHandle> {
        None
    }

    /// What deletes this opener's repositories, when it can: what
    /// [`Registry::delete`] uses.
    fn deleter(&self) -> Option<&dyn RepositoryDeleter> {
        None
    }
}

/// Deletes the repositories a [`RepositoryOpener`] opens.
#[async_trait]
pub trait RepositoryDeleter: Send + Sync {
    /// Deletes the repository `name`: its head and what of its store no other
    /// repository uses. [`Registry::delete`] calls this once the sessions that used it
    /// ended and the registry dropped its entry, and [`Registry::get`] finds nothing
    /// under the name until this returns, so it is not opened again meanwhile. A store
    /// the entry shared with other open repositories (a fork network's) is still open.
    /// [`DeleteError::Missing`] when there is no such repository;
    /// [`DeleteError::Held`] when what it must remove is still in use at `deadline`.
    async fn delete(&self, name: &str, deadline: Instant) -> Result<(), DeleteError>;
}

/// A repositories directory and its content store, which the process serving it holds
/// open.
#[derive(Clone)]
pub struct DirectoryHandle {
    pub repositories: RepositoriesDirectory,
    pub content: PackedObjectStore,
}

/// A repository could not be opened (not: there is no such repository).
#[derive(Debug)]
pub struct OpenError(pub Box<dyn std::error::Error + Send + Sync>);

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for OpenError {}

/// When a [`Registry`] with an opener closes repositories nothing holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegistryLimits {
    /// A repository not asked for in this long is closed.
    pub idle: Duration,
    /// Past this many open repositories, opening another first closes the least
    /// recently used ones nothing holds. When every one is held, the registry opens
    /// more than this rather than refuse.
    pub max_open: usize,
}

impl Default for RegistryLimits {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(300),
            max_open: 256,
        }
    }
}

/// Repositories by name, open or opened when asked.
pub struct Registry {
    opener: Option<Arc<dyn RepositoryOpener>>,
    limits: RegistryLimits,
    slots: Mutex<HashMap<String, Slot>>,
    /// Names being deleted: [`Self::get`] finds nothing under them.
    deleting: Mutex<HashSet<String>>,
    operations: Operations,
}

/// How long [`Registry::delete`] waits for sessions using a repository to end.
const DELETE_WAIT: Duration = Duration::from_secs(30);

/// How often [`Registry::delete`] looks again whether they have.
const DELETE_POLL: Duration = Duration::from_millis(25);

/// A repository could not be deleted.
#[derive(Debug)]
pub enum DeleteError {
    /// The registry's opener cannot delete repositories.
    Unsupported,
    /// There is no such repository.
    Missing,
    /// Another delete of the name is running.
    Busy,
    /// Something still held the repository after the wait, so its store did not close.
    Held,
    Failed(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for DeleteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => f.write_str("this server cannot delete repositories"),
            Self::Missing => f.write_str("there is no such repository"),
            Self::Busy => f.write_str("the repository is already being deleted"),
            Self::Held => write!(
                f,
                "sessions still held the repository after {} seconds",
                DELETE_WAIT.as_secs()
            ),
            Self::Failed(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for DeleteError {}

/// One repository the registry holds or is opening. Sessions asking for it clone
/// `cell` under the registry's lock, so a slot whose cell and entry nobody else
/// holds is one nothing uses or is about to.
struct Slot {
    cell: Arc<OnceCell<Arc<RepositoryEntry>>>,
    /// Asked for last.
    used: Instant,
    /// Given at construction; never closed.
    pinned: bool,
}

impl Slot {
    fn pinned(entry: RepositoryEntry) -> Self {
        Self {
            cell: Arc::new(OnceCell::new_with(Some(Arc::new(entry)))),
            used: Instant::now(),
            pinned: true,
        }
    }

    /// Whether the slot may be closed: open, and nothing holds it.
    fn unused(&self) -> bool {
        !self.pinned && self.cell.initialized() && self.released()
    }

    /// Whether nothing but the slot holds its entry or is opening it.
    fn released(&self) -> bool {
        Arc::strong_count(&self.cell) == 1
            && self
                .cell
                .get()
                .is_none_or(|entry| Arc::strong_count(entry) == 1)
    }
}

impl Registry {
    fn fixed(repositories: impl IntoIterator<Item = (String, RepositoryEntry)>) -> Self {
        Self {
            opener: None,
            limits: RegistryLimits::default(),
            operations: Operations::default(),
            deleting: Mutex::default(),
            slots: Mutex::new(
                repositories
                    .into_iter()
                    .map(|(name, entry)| (name, Slot::pinned(entry)))
                    .collect(),
            ),
        }
    }

    /// A registry that opens repositories with `opener` when they are asked for, and
    /// closes them by `limits`.
    pub fn with_opener(opener: Arc<dyn RepositoryOpener>, limits: RegistryLimits) -> Self {
        Self {
            opener: Some(opener),
            limits,
            operations: Operations::default(),
            deleting: Mutex::default(),
            slots: Mutex::default(),
        }
    }

    /// Create a registry serving a single repository from a data directory.
    ///
    /// The directory must contain a packed `storage/` directory and `repository.head`.
    /// The repository name defaults to the directory's file name.
    pub fn single(data_dir: &Path) -> Result<Self, ObjectStoreError> {
        let name = data_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("default")
            .to_string();
        Self::single_named(name, data_dir)
    }

    /// Create a registry serving a single repository with an explicit name.
    pub fn single_named(name: String, data_dir: &Path) -> Result<Self, ObjectStoreError> {
        Ok(Self::fixed([(name, RepositoryEntry::open(data_dir)?)]))
    }

    /// Create a registry serving a single repository with pre-built stores.
    pub fn single_with_stores(
        name: String,
        store: Box<dyn RepositoryStore>,
        heads: impl HeadStore + 'static,
    ) -> Self {
        Self::single_entry(name, RepositoryEntry::new(store, heads))
    }

    /// A registry serving `entry` alone, as `name`.
    pub fn single_entry(name: String, entry: RepositoryEntry) -> Self {
        Self::fixed([(name, entry)])
    }

    /// A registry serving the repositories in `repos_dir` (a
    /// [`RepositoriesDirectory`]), each opened when first asked for (see
    /// [`DirectoryOpener`]). Fails when the directory's content store cannot be
    /// opened, as when another process serves it.
    pub fn from_repos_dir(
        repos_dir: &Path,
        limits: RegistryLimits,
    ) -> Result<Self, ObjectStoreError> {
        Ok(Self::with_opener(
            Arc::new(DirectoryOpener::new(repos_dir)?),
            limits,
        ))
    }

    /// The repository `name`, opening it if needed. `None` when there is none. Hold
    /// the entry only while using it: the registry cannot close a repository while
    /// something holds it.
    pub async fn get(&self, name: &str) -> Result<Option<Arc<RepositoryEntry>>, OpenError> {
        let cell = {
            let mut slots = self.lock();
            if self.is_deleting(name) {
                return Ok(None);
            }
            match slots.get_mut(name) {
                Some(slot) => {
                    slot.used = Instant::now();
                    Arc::clone(&slot.cell)
                }
                None => {
                    if self.opener.is_none() {
                        return Ok(None);
                    }
                    self.make_room(&mut slots);
                    let cell = Arc::new(OnceCell::new());
                    slots.insert(
                        name.to_owned(),
                        Slot {
                            cell: Arc::clone(&cell),
                            used: Instant::now(),
                            pinned: false,
                        },
                    );
                    cell
                }
            }
        };
        let opened = cell
            .get_or_try_init(|| async {
                let opener = self
                    .opener
                    .as_ref()
                    .expect("only an opener's slots are empty");
                match opener.open(name).await {
                    Ok(Some(entry)) => {
                        info!(repo = name, "repository opened");
                        Ok(Arc::new(entry))
                    }
                    Ok(None) => Err(None),
                    Err(error) => Err(Some(error)),
                }
            })
            .await;
        match opened {
            // Opened while a delete began: not handed out, so nothing holds it open.
            Ok(_) if self.is_deleting(name) => Ok(None),
            Ok(entry) => Ok(Some(Arc::clone(entry))),
            Err(failure) => {
                // Asked again, it is looked up again: it may exist by then.
                let mut slots = self.lock();
                if slots
                    .get(name)
                    .is_some_and(|slot| Arc::ptr_eq(&slot.cell, &cell) && !slot.cell.initialized())
                {
                    slots.remove(name);
                }
                match failure {
                    None => Ok(None),
                    Some(error) => Err(error),
                }
            }
        }
    }

    /// Holds the repository `name` still, with every repository of its fork network
    /// (they share a store), opening it if needed, so their files can be copied
    /// ([`wheat_store::NetworkDirectory::files`],
    /// [`wheat_store::RepositoryDirectory::files`]): refuses new publishes with a
    /// retryable error and waits for those writing to finish. A publish that is
    /// uploading is waited for to its end, so bound this with a timeout; dropped
    /// before it is ready, it lets publishes in again. `None` when there is no such
    /// repository. While held the network's store is not closed, and fetches go on.
    pub async fn quiesce(&self, name: &str) -> Result<Option<Quiesced>, OpenError> {
        let Some(entry) = self.get(name).await? else {
            return Ok(None);
        };
        Ok(Some(Quiesced::hold(entry).await))
    }

    /// Deletes the repository `name` while it is served: sessions using it end, the
    /// name stops finding it, and once nothing holds it open the opener's
    /// [`RepositoryDeleter`] deletes it (for a [`DirectoryOpener`], see
    /// [`RepositoriesDirectory::delete`]). Waits up to thirty seconds for the holders.
    pub async fn delete(&self, name: &str) -> Result<(), DeleteError> {
        let deleter = self
            .opener
            .as_deref()
            .and_then(RepositoryOpener::deleter)
            .ok_or(DeleteError::Unsupported)?;
        // Both under the slots' lock, which `get` holds when it checks the name and adds a
        // slot: it adds none for a name once this marks it.
        let (_deleting, slot) = {
            let mut slots = self.lock();
            (DeletingName::begin(self, name)?, slots.remove(name))
        };
        let deadline = Instant::now() + DELETE_WAIT;
        if let Some(slot) = slot {
            if let Some(entry) = slot.cell.get() {
                entry.mark_removed();
            }
            // The sessions are on their way out; a `get` opening it meanwhile finds the
            // name deleting and drops what it opened.
            while !slot.released() {
                if Instant::now() >= deadline {
                    return Err(DeleteError::Held);
                }
                tokio::time::sleep(DELETE_POLL).await;
            }
        }
        deleter.delete(name, deadline).await?;
        info!(repo = name, "repository deleted");
        Ok(())
    }

    fn is_deleting(&self, name: &str) -> bool {
        self.deleting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(name)
    }

    /// Closes the repositories nothing holds that were not asked for in
    /// [`RegistryLimits::idle`]. [`crate::server::run`] calls this periodically; an
    /// embedder serving through [`crate::connection`] calls it itself. Returns how
    /// many it closed.
    pub fn close_idle(&self) -> usize {
        let Some(cutoff) = Instant::now().checked_sub(self.limits.idle) else {
            return 0;
        };
        let mut slots = self.lock();
        let before = slots.len();
        // Dropped under the lock, so the store is closed before anyone can open it
        // again.
        slots.retain(|name, slot| {
            let close = slot.used <= cutoff && slot.unused();
            if close {
                debug!(repo = name.as_str(), "closing idle repository");
            }
            !close
        });
        before - slots.len()
    }

    /// The repositories directory served, when this registry serves one.
    pub fn directory(&self) -> Option<DirectoryHandle> {
        self.opener.as_ref()?.directory()
    }

    /// The maintenance operations started on this registry's directory.
    pub fn operations(&self) -> &Operations {
        &self.operations
    }

    /// The names of the repositories open now.
    pub fn open_names(&self) -> Vec<String> {
        self.lock()
            .iter()
            .filter(|(_, slot)| slot.cell.initialized())
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// How often [`crate::server::run`] calls [`Self::close_idle`]: often enough
    /// that a repository is closed within half its idle time again.
    pub(crate) fn close_interval(&self) -> Option<Duration> {
        self.opener
            .as_ref()
            .map(|_| (self.limits.idle / 2).max(Duration::from_millis(10)))
    }

    /// Before opening one more: closes the least recently used repositories nothing
    /// holds until fewer than [`RegistryLimits::max_open`] are open.
    fn make_room(&self, slots: &mut HashMap<String, Slot>) {
        while slots.len() >= self.limits.max_open {
            let Some(oldest) = slots
                .iter()
                .filter(|(_, slot)| slot.unused())
                .min_by_key(|(_, slot)| slot.used)
                .map(|(name, _)| name.clone())
            else {
                return;
            };
            debug!(
                repo = oldest.as_str(),
                "closing the least recently used repository"
            );
            slots.remove(&oldest);
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Slot>> {
        // A panic while holding the lock leaves the map as consistent as before it.
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A name in [`Registry::delete`], until it ends.
struct DeletingName<'a> {
    registry: &'a Registry,
    name: &'a str,
}

impl<'a> DeletingName<'a> {
    fn begin(registry: &'a Registry, name: &'a str) -> Result<Self, DeleteError> {
        let mut deleting = registry
            .deleting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !deleting.insert(name.to_owned()) {
            return Err(DeleteError::Busy);
        }
        Ok(Self { registry, name })
    }
}

impl Drop for DeletingName<'_> {
    fn drop(&mut self) {
        self.registry
            .deleting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(self.name);
    }
}

/// Opens `owner/repo` from a [`RepositoriesDirectory`], sharing one store among the
/// open repositories of each fork network, and the directory's content store among
/// every network. Names that are not `owner/repo`, hidden ones, and repositories
/// still being created or deleted are not found.
pub struct DirectoryOpener {
    repositories: RepositoriesDirectory,
    /// Open for as long as the opener is, and locked against other processes.
    content: PackedObjectStore,
    networks: OpenNetworks<Uuid>,
}

impl DirectoryOpener {
    /// Opens the content store of the repositories directory `root`, creating it if
    /// there is none.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, ObjectStoreError> {
        let repositories = RepositoriesDirectory::new(root);
        Ok(Self {
            content: repositories.open_content()?,
            repositories,
            networks: OpenNetworks::default(),
        })
    }
}

#[async_trait]
impl RepositoryOpener for DirectoryOpener {
    async fn open(&self, name: &str) -> Result<Option<RepositoryEntry>, OpenError> {
        let repositories = self.repositories.clone();
        let networks = self.networks.clone();
        let content = self.content.clone();
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || {
            let location = match repositories.locate(&name) {
                Ok(Some(location)) => location,
                Ok(None) | Err(RepositoriesDirectoryError::InvalidName(_)) => return Ok(None),
                Err(error) => return Err(OpenError(Box::new(error))),
            };
            let directory = repositories.network(location.network);
            let network = networks
                .open(&location.network, || {
                    directory.open_store().map(|pool| {
                        Box::new(NetworkStore::new(pool, content)) as Box<dyn RepositoryStore>
                    })
                })
                .map_err(|error| OpenError(Box::new(error)))?;
            Ok(Some(RepositoryEntry::in_network(
                network,
                directory.publications(location.repository),
            )))
        })
        .await
        .map_err(|error| OpenError(Box::new(error)))?
    }

    fn directory(&self) -> Option<DirectoryHandle> {
        Some(DirectoryHandle {
            repositories: self.repositories.clone(),
            content: self.content.clone(),
        })
    }

    fn deleter(&self) -> Option<&dyn RepositoryDeleter> {
        Some(self)
    }
}

#[async_trait]
impl RepositoryDeleter for DirectoryOpener {
    /// Removes the repository's head, and its network when it was the last one in it;
    /// see [`RepositoriesDirectory::delete`]. The network's store, open while another
    /// of its repositories is, is waited for until `deadline`.
    async fn delete(&self, name: &str, deadline: Instant) -> Result<(), DeleteError> {
        loop {
            let repositories = self.repositories.clone();
            let doomed = name.to_owned();
            let result = tokio::task::spawn_blocking(move || repositories.delete(&doomed))
                .await
                .map_err(|error| DeleteError::Failed(Box::new(error)))?;
            match result {
                Ok(()) => return Ok(()),
                Err(RepositoriesDirectoryError::Missing(_)) => return Err(DeleteError::Missing),
                Err(RepositoriesDirectoryError::Directory(
                    wheat_store::RepositoryDirectoryError::InUse(_),
                )) if Instant::now() < deadline => {
                    tokio::time::sleep(DELETE_POLL).await;
                }
                Err(RepositoriesDirectoryError::Directory(
                    wheat_store::RepositoryDirectoryError::InUse(_),
                )) => return Err(DeleteError::Held),
                Err(error) => return Err(DeleteError::Failed(Box::new(error))),
            }
        }
    }
}
