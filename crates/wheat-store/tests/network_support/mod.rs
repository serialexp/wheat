//! A fork network to export or import in the tests, built the way a server does.
#![allow(dead_code)]

use std::{collections::BTreeSet, fs, path::Path};

use uuid::Uuid;
use wheat_core::{
    ChangeId, ChangeMetadata, ChangeProposal, ContentId, EditOperation, EditRecord, EditTransition,
    FileKind, FileValue, Repository, RevisionEdits, RevisionMetadata, Tree, ViewId,
};
use wheat_store::{
    ContentStore, LocalPublicationStore, NetworkDirectory, PackedObjectStore,
    PersistedRepositoryMutator, PublicationStore, RepositoriesDirectory, RepositoryHead,
};

/// A repositories directory with a content store open beside it, as a server has.
pub struct Fixture {
    _root: tempfile::TempDir,
    pub repositories: RepositoriesDirectory,
    pub content: PackedObjectStore,
    pub base: ViewId,
    changes: u128,
}

impl Fixture {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let repositories = RepositoriesDirectory::new(root.path().join("repos"));
        fs::create_dir(repositories.root()).unwrap();
        Self {
            content: repositories.open_content().unwrap(),
            repositories,
            _root: root,
            base: Repository::new(Tree::new()).base_view().id,
            changes: 0,
        }
    }

    pub fn root(&self) -> &Path {
        self._root.path()
    }

    /// Adds a change to the repository `repository` of `network` that writes `bytes`
    /// to `path`, on top of `effective`; returns the view it makes. The content goes
    /// to the directory's content store.
    pub async fn add(
        &mut self,
        network: &NetworkDirectory,
        repository: Uuid,
        effective: ViewId,
        path: &str,
        bytes: &[u8],
    ) -> ViewId {
        let pool = network.open_store().unwrap();
        let content = self.content.content_store();
        self.changes += 1;
        let view = publish_change(
            &pool,
            &content,
            &network.publications(repository),
            self.base,
            effective,
            self.changes,
            path,
            bytes,
        )
        .await;
        drop(content);
        pool.close().unwrap();
        view
    }
}

/// Publishes a change number `serial` writing `bytes` to `path` on top of `effective`
/// to the repository whose objects are in `pool`, whose content goes to `content` and
/// whose head is `heads`; returns the view it makes.
#[allow(clippy::too_many_arguments)]
pub async fn publish_change(
    pool: &PackedObjectStore,
    content: &ContentStore,
    heads: &LocalPublicationStore,
    base: ViewId,
    effective: ViewId,
    serial: u128,
    path: &str,
    bytes: &[u8],
) -> ViewId {
    content.put(bytes).unwrap();
    let record = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: ContentId::of(bytes),
                kind: FileKind::Regular,
            }),
        },
    )
    .unwrap();
    let id = record.id();
    let mutator = PersistedRepositoryMutator::new(pool, content, heads);
    let (prepared, outcome) = mutator
        .create_on_frontier(
            heads.read().unwrap(),
            base,
            effective,
            ChangeProposal {
                id: ChangeId::from_bytes(((serial << 80) | 7).to_be_bytes()),
                metadata: ChangeMetadata::default(),
                dependencies: Default::default(),
                revision_metadata: RevisionMetadata::default(),
                edits: RevisionEdits {
                    edits: [(id, record)].into(),
                    transition: EditTransition {
                        added: [id].into(),
                        ..EditTransition::default()
                    },
                },
            },
        )
        .await
        .unwrap();
    mutator.publish(prepared).await.unwrap();
    outcome.resulting_view
}

/// The names in `directory`, sorted, without name locks.
pub fn names(directory: &Path) -> Vec<String> {
    let mut names = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| !name.ends_with(".wheat.lock"))
        .collect::<Vec<_>>();
    names.sort();
    names
}

pub fn files_under(directory: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(next).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                files.push(entry.file_name().into_string().unwrap());
            }
        }
    }
    files
}

/// A network of a parent and a fork, each with files of its own and two in common.
pub struct Built {
    pub fixture: Fixture,
    pub network_id: Uuid,
    pub repositories: Vec<Uuid>,
    pub contents: BTreeSet<ContentId>,
    pub heads: Vec<RepositoryHead>,
}

pub async fn built() -> Built {
    let mut fixture = Fixture::new();
    let placed = fixture
        .repositories
        .create_empty("alice/repo", "main")
        .await
        .unwrap();
    let network = fixture.repositories.network(placed.location.network);
    let parent = placed.location.repository;
    let base = fixture.base;
    let view = fixture.add(&network, parent, base, "a", b"shared a").await;
    let view = fixture.add(&network, parent, view, "b", b"shared b").await;
    let fork = fixture.repositories.fork("alice/repo", "bob/repo").unwrap();
    let forked = fork.location.repository;
    fixture
        .add(&network, forked, view, "c", b"only in the fork")
        .await;
    fixture
        .add(&network, parent, view, "d", b"only in the parent")
        .await;
    let mut repositories = vec![parent, forked];
    repositories.sort();
    let heads = repositories
        .iter()
        .map(|repository| network.publications(*repository).read().unwrap())
        .collect();
    Built {
        contents: [
            &b"shared a"[..],
            b"shared b",
            b"only in the fork",
            b"only in the parent",
        ]
        .iter()
        .map(|bytes| ContentId::of(bytes))
        .collect(),
        network_id: placed.location.network,
        repositories,
        heads,
        fixture,
    }
}
