//! Exporting a fork network as a bundle: structure apart from content.

mod network_support;

use std::{collections::BTreeSet, fs, path::Path};

use network_support::{built, files_under, names};
use uuid::Uuid;
use wheat_core::ContentId;
use wheat_store::{
    BundleError, ContentStore, ExportLimits, NetworkDirectory, PackedObjectStore, PublicationStore,
    RepositoryDirectoryError, RepositoryReader, bundle_content_directory,
    bundle_structure_directory, read_bundle_content_chunk, read_bundle_manifest,
};

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
