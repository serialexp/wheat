//! A served repository to write to and read back, shared by the tests that move a
//! repository's head (mirror, restore).

use std::collections::BTreeSet;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use wheat_core::{
    ChangeId, ChangeMetadata, ChangeProposal, ContentId, Repository, RevisionEdits,
    RevisionMetadata, Tree, VerifiedContent, ViewId, ViewLabel,
};
use wheat_server::access::Principal;
use wheat_server::naming::ContentNaming;
use wheat_server::network::Network;
use wheat_server::registry::RepositoryEntry;
use wheat_server::write::{ExpectedLabel, LabelChange, ServerWrite, open_stage};
use wheat_store::{
    LocalPublicationStore, MerkleTreeStore, PackedObjectStore, RepositoryHead, RepositoryPublisher,
    RepositoryReader,
};

use super::edit;

/// Records each naming.
#[derive(Default)]
pub struct Recording {
    pub calls: Mutex<Vec<Vec<ContentId>>>,
}

#[async_trait]
impl ContentNaming for Recording {
    fn renew_after(&self) -> Duration {
        Duration::from_secs(3600)
    }

    async fn named(&self, ids: &[ContentId]) -> io::Result<()> {
        self.calls.lock().unwrap().push(ids.to_vec());
        Ok(())
    }
}

/// A repository on a packed store, holding only its base view.
pub struct Repo {
    dir: tempfile::TempDir,
    pub objects: PackedObjectStore,
    pub base: ViewId,
}

impl Repo {
    /// The repository's head file, opened again: the entry holds the first.
    pub fn heads(&self) -> LocalPublicationStore {
        LocalPublicationStore::new(self.dir.path().join("repository.head"))
    }
}

pub async fn repository() -> (Repo, LocalPublicationStore) {
    let dir = tempfile::tempdir().unwrap();
    let objects = PackedObjectStore::open(dir.path().join("storage")).unwrap();
    let content = objects.content_store();
    let heads = LocalPublicationStore::new(dir.path().join("repository.head"));
    let repo = Repository::new(Tree::new());
    let base = repo.base_view().id;
    RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repo)
        .await
        .unwrap();
    (Repo { dir, objects, base }, heads)
}

/// A repository whose network tells `naming`.
pub struct Named {
    pub repo: Repo,
    pub entry: Arc<RepositoryEntry>,
    pub naming: Arc<Recording>,
}

pub async fn named_repository() -> Named {
    let (repo, heads) = repository().await;
    let naming = Arc::new(Recording::default());
    let network = Network::with_naming(Box::new(repo.objects.clone()), naming.clone());
    Named {
        entry: Arc::new(RepositoryEntry::in_network(network, heads)),
        repo,
        naming,
    }
}

pub fn principal() -> Principal {
    Principal {
        name: "gothab".into(),
    }
}

pub fn id(bytes: &[u8]) -> ContentId {
    VerifiedContent::new(bytes).id()
}

pub fn change(name: &str, expected: ExpectedLabel, new: ViewId) -> LabelChange {
    LabelChange {
        name: name.to_owned(),
        expected,
        new: Some(ViewLabel::from(new)),
    }
}

pub fn server_write(key: u8, labels: Vec<LabelChange>) -> ServerWrite {
    ServerWrite {
        principal: principal(),
        key: [key; 16],
        labels,
        force_labels: false,
    }
}

pub fn proposal(edits: RevisionEdits) -> ChangeProposal {
    ChangeProposal {
        id: ChangeId::new(),
        metadata: ChangeMetadata::default(),
        dependencies: BTreeSet::new(),
        revision_metadata: RevisionMetadata::default(),
        edits,
    }
}

/// Publishes to `entry` a change writing `path` on `view`, moving `main` from
/// `expected` to it; answers its view.
pub async fn commit(
    entry: &Arc<RepositoryEntry>,
    key: u8,
    view: ViewId,
    expected: ExpectedLabel,
    path: &'static str,
    value: &'static [u8],
) -> ViewId {
    let stage = open_stage(entry).await.unwrap();
    let edited = stage
        .build(move |builder| async move {
            builder.content().put(value).unwrap();
            let mutator = builder.mutator();
            let (prepared, outcome) = mutator
                .create_on_frontier(builder.head(), view, view, proposal(edit(path, value)))
                .await
                .unwrap();
            mutator.publish(prepared).await.unwrap();
            outcome.resulting_view
        })
        .await
        .unwrap();
    stage
        .submit(
            server_write(key, vec![change("main", expected, edited)]),
            [key; 32],
        )
        .await
        .unwrap();
    edited
}

pub async fn head(entry: &RepositoryEntry) -> RepositoryHead {
    entry.heads.head().await.unwrap()
}

/// The labels `head` holds in `repo`, by name.
pub async fn labels(repo: &Repo, head: RepositoryHead) -> Vec<(String, ViewId)> {
    let content = repo.objects.content_store();
    let reader = RepositoryReader::new(&repo.objects, &content);
    let envelope = reader.envelope(head.envelope).await.unwrap();
    reader
        .view_label_manifest(envelope.labels)
        .await
        .unwrap()
        .labels
        .into_iter()
        .map(|(name, label)| (name, label.view))
        .collect()
}

/// What `view` holds at `path` in `repo`, read back whole.
pub async fn file(repo: &Repo, view: ViewId, path: &str) -> Option<Vec<u8>> {
    let content = repo.objects.content_store();
    let tree = RepositoryReader::new(&repo.objects, &content)
        .view(view)
        .await
        .unwrap()
        .tree;
    let value = MerkleTreeStore::new(&repo.objects)
        .lookup_many(tree, &[path])
        .await
        .unwrap()
        .pop()
        .unwrap()?;
    Some(content.get(value.content).unwrap())
}
