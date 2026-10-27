//! Exporting a fork network as a bundle: structure apart from content.

use std::{collections::BTreeSet, fs, path::Path};

use uuid::Uuid;
use wheat_core::{
    ChangeId, ChangeMetadata, ChangeProposal, ContentId, EditOperation, EditRecord, EditTransition,
    FileKind, FileValue, Repository, RevisionEdits, RevisionMetadata, Tree, ViewId,
};
use wheat_store::{
    BundleError, ContentStore, ExportLimits, NetworkDirectory, PackedObjectStore,
    PersistedRepositoryMutator, PublicationStore, RepositoriesDirectory, RepositoryDirectoryError,
    RepositoryHead, RepositoryReader, bundle_content_directory, bundle_structure_directory,
    read_bundle_content_chunk, read_bundle_manifest,
};

/// A repositories directory with a content store open beside it, as a server has.
struct Fixture {
    _root: tempfile::TempDir,
    repositories: RepositoriesDirectory,
    content: PackedObjectStore,
    base: ViewId,
    changes: u128,
}

impl Fixture {
    fn new() -> Self {
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

    fn root(&self) -> &Path {
        self._root.path()
    }

    /// Adds a change to the repository `repository` of `network` that writes `bytes`
    /// to `path`, on top of `effective`; returns the view it makes.
    async fn add(
        &mut self,
        network: &NetworkDirectory,
        repository: Uuid,
        effective: ViewId,
        path: &str,
        bytes: &[u8],
    ) -> ViewId {
        let pool = network.open_store().unwrap();
        let content = self.content.content_store();
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
        self.changes += 1;
        let heads = network.publications(repository);
        let mutator = PersistedRepositoryMutator::new(&pool, &content, &heads);
        let (prepared, outcome) = mutator
            .create_on_frontier(
                heads.read().unwrap(),
                self.base,
                effective,
                ChangeProposal {
                    id: ChangeId::from_bytes(((self.changes << 80) | 7).to_be_bytes()),
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
        drop(content);
        pool.close().unwrap();
        outcome.resulting_view
    }
}

/// The names in `directory`, sorted, without name locks.
fn names(directory: &Path) -> Vec<String> {
    let mut names = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| !name.ends_with(".wheat.lock"))
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn files_under(directory: &Path) -> Vec<String> {
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
struct Built {
    fixture: Fixture,
    network_id: Uuid,
    repositories: Vec<Uuid>,
    contents: BTreeSet<ContentId>,
    heads: Vec<RepositoryHead>,
}

async fn built() -> Built {
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

#[tokio::test]
async fn an_exported_network_has_its_structure_every_head_and_its_content_once() {
    let built = built().await;
    let fixture = &built.fixture;
    let bundle = fixture.root().join("bundle");
    let source = fixture.content.content_store();

    // The network's store is not open here, but the content store is, as in a server.
    let export = fixture
        .repositories
        .export_network(built.network_id, &bundle)
        .unwrap();
    let copied = export
        .files()
        .iter()
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    assert!(
        copied.contains(&"storage/packed/catalog.head"),
        "{copied:?}"
    );
    assert_eq!(
        copied
            .iter()
            .filter(|path| path.ends_with("repository.head"))
            .count(),
        2
    );
    assert!(!bundle.exists(), "nothing is in place before finish");
    let manifest = export.finish(&source).await.unwrap();

    // Nothing a running store keeps is left in it, and nothing else is beside it.
    for name in files_under(&bundle) {
        assert!(
            name != "catalog.lock" && !name.ends_with(".sqlite3"),
            "{name} is in the bundle"
        );
    }
    assert_eq!(names(fixture.root()), ["bundle", "repos"]);

    assert_eq!(read_bundle_manifest(&bundle).unwrap(), manifest);
    assert_eq!(manifest.network, built.network_id);
    assert_eq!(manifest.repositories, built.repositories);
    let mut listed = Vec::new();
    let mut bytes = 0;
    for index in 0..manifest.content_chunks.len() {
        for entry in read_bundle_content_chunk(&bundle, &manifest, index).unwrap() {
            listed.push(ContentId::from_digest(entry.id));
            bytes += entry.bytes;
        }
    }
    let mut sorted = listed.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(listed, sorted, "listed in order, each once");
    assert_eq!(listed.into_iter().collect::<BTreeSet<_>>(), built.contents);
    assert_eq!(manifest.content_count, 4);
    assert_eq!(manifest.content_bytes, bytes);

    // The content is in the bundle's own store.
    let content = PackedObjectStore::open(bundle_content_directory(&bundle)).unwrap();
    let held = content.content_store();
    for id in &built.contents {
        assert_eq!(held.get(*id).unwrap(), source.get(*id).unwrap());
    }
    drop(held);
    content.close().unwrap();

    // The structure is the network: it opens, with the heads the source has.
    let copy = NetworkDirectory::new(bundle_structure_directory(&bundle));
    assert_eq!(copy.repositories().unwrap(), built.repositories);
    for (repository, head) in built.repositories.iter().zip(&built.heads) {
        assert_eq!(copy.publications(*repository).read().unwrap(), *head);
    }
    let pool = copy.open_store().unwrap();
    let pool_content = pool.content_store();
    let reader = RepositoryReader::new(&pool, &pool_content);
    for head in &built.heads {
        reader.envelope(head.envelope).await.unwrap();
    }
    drop(pool_content);
    pool.close().unwrap();
}

#[tokio::test]
async fn a_small_chunk_and_batch_give_the_same_list_in_more_pieces() {
    let built = built().await;
    let fixture = &built.fixture;
    let source = fixture.content.content_store();
    let whole = fixture.root().join("whole");
    let pieces = fixture.root().join("pieces");

    let one = fixture
        .repositories
        .export_network(built.network_id, &whole)
        .unwrap()
        .finish(&source)
        .await
        .unwrap();
    let many = fixture
        .repositories
        .export_network(built.network_id, &pieces)
        .unwrap()
        .finish_with(
            &source,
            ExportLimits {
                chunk_entries: 3,
                batch_bytes: 1,
                batch_blobs: 2,
            },
        )
        .await
        .unwrap();

    assert_eq!(one.content_chunks.len(), 1);
    assert_eq!(many.content_chunks.len(), 2);
    assert_eq!(many.content_count, one.content_count);
    let read = |bundle: &Path, manifest| {
        (0..many.content_chunks.len().max(one.content_chunks.len()))
            .filter_map(|index| read_bundle_content_chunk(bundle, manifest, index).ok())
            .flatten()
            .collect::<Vec<_>>()
    };
    assert_eq!(read(&whole, &one), read(&pieces, &many));
    assert_eq!(read(&pieces, &many).len(), 4);
}

#[tokio::test]
async fn a_tampered_chunk_is_not_read() {
    let built = built().await;
    let fixture = &built.fixture;
    let bundle = fixture.root().join("bundle");
    let manifest = fixture
        .repositories
        .export_network(built.network_id, &bundle)
        .unwrap()
        .finish(&fixture.content.content_store())
        .await
        .unwrap();
    let chunk = bundle.join("content.list/000000");
    let mut bytes = fs::read(&chunk).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(&chunk, bytes).unwrap();

    assert!(matches!(
        read_bundle_content_chunk(&bundle, &manifest, 0),
        Err(BundleError::Mismatch(_))
    ));
}

#[tokio::test]
async fn refusals_and_failures_leave_nothing_behind() {
    let built = built().await;
    let fixture = &built.fixture;
    let source = fixture.content.content_store();
    let bundle = fixture.root().join("bundle");

    // A network that is not there.
    assert!(matches!(
        fixture
            .repositories
            .export_network(Uuid::from_u128(9), &bundle),
        Err(BundleError::Missing(_))
    ));
    // A destination that is.
    fs::create_dir(&bundle).unwrap();
    assert!(matches!(
        fixture
            .repositories
            .export_network(built.network_id, &bundle),
        Err(BundleError::Directory(RepositoryDirectoryError::Exists(_)))
    ));
    fs::remove_dir(&bundle).unwrap();

    // An export dropped before it finishes.
    drop(
        fixture
            .repositories
            .export_network(built.network_id, &bundle)
            .unwrap(),
    );
    assert!(!bundle.exists());

    // Content the source lacks: the head reaches it, so the export fails closed.
    let empty = tempfile::tempdir().unwrap();
    let lacking = ContentStore::new(empty.path());
    let error = fixture
        .repositories
        .export_network(built.network_id, &bundle)
        .unwrap()
        .finish(&lacking)
        .await
        .unwrap_err();
    assert!(matches!(error, BundleError::MissingContent(id) if built.contents.contains(&id)));
    assert!(!bundle.exists());
    assert_eq!(names(fixture.root()), ["repos"]);

    // And the source still exports afterwards.
    fixture
        .repositories
        .export_network(built.network_id, &bundle)
        .unwrap()
        .finish(&source)
        .await
        .unwrap();
}
