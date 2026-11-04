//! Importing a network from a bundle, and adopting a standalone repository.

use crate::network_support;

use std::{fs, path::PathBuf};

use network_support::{Built, Fixture, built, names as listing, publish_change};
use uuid::Uuid;
use wheat_core::{ContentId, Repository, RepositoryLocationRecord, Tree, encode_bundle_manifest};
use wheat_store::{
    BundleError, ImportError, ImportLimits, PublicationStore, RepositoriesDirectoryError,
    RepositoryDirectory, RepositoryReader, bundle_structure_directory, read_bundle_manifest,
};

/// `built()`'s network exported to a bundle in its fixture, with the names an import
/// gives its repositories.
async fn exported() -> (Built, PathBuf, Vec<(Uuid, String)>) {
    let built = built().await;
    let bundle = built.fixture.root().join("bundle");
    built
        .fixture
        .repositories
        .export_network(built.network_id, &bundle)
        .unwrap()
        .finish(&built.fixture.content.content_store())
        .await
        .unwrap();
    let names = built
        .repositories
        .iter()
        .zip(["carol/one", "dave/two"])
        .map(|(repository, name)| (*repository, name.to_owned()))
        .collect();
    (built, bundle, names)
}

/// Whether `target` holds an imported `built`: its heads, names and content, and the
/// network validates against `target`'s content.
async fn assert_holds(target: &Fixture, built: &Built, names: &[(Uuid, String)]) {
    let network = target.repositories.network(built.network_id);
    assert_eq!(network.repositories().unwrap(), built.repositories);
    for ((repository, name), head) in names.iter().zip(&built.heads) {
        assert_eq!(
            target.repositories.locate(name).unwrap(),
            Some(RepositoryLocationRecord {
                network: built.network_id,
                repository: *repository,
            })
        );
        assert_eq!(network.publications(*repository).read().unwrap(), *head);
    }
    let content = target.content.content_store();
    for id in &built.contents {
        assert_eq!(
            content.get(*id).unwrap(),
            built.fixture.content.content_store().get(*id).unwrap()
        );
    }
    let pool = network.open_store().unwrap();
    let reader = RepositoryReader::new(&pool, &content);
    for head in &built.heads {
        let state = reader.envelope(head.envelope).await.unwrap().state;
        reader.validate_complete(state).await.unwrap();
    }
    pool.close().unwrap();
}

#[tokio::test]
async fn an_imported_network_keeps_its_ids_heads_and_content() {
    let (built, bundle, names) = exported().await;
    let target = Fixture::new();

    let imported = target
        .repositories
        .import_network(&bundle, &names, &target.content.content_store())
        .await
        .unwrap();

    assert_eq!(imported.network, built.network_id);
    assert!(!imported.resumed);
    assert_eq!(
        imported
            .repositories
            .iter()
            .map(|(name, placed)| (name.as_str(), placed.head))
            .collect::<Vec<_>>(),
        ["carol/one", "dave/two"]
            .into_iter()
            .zip(built.heads.iter().copied())
            .collect::<Vec<_>>()
    );
    assert_holds(&target, &built, &names).await;
    // The bundle is read, not consumed, and the target holds nothing else.
    assert!(bundle.join("bundle.manifest").is_file());
    assert_eq!(names_of(&target), ["carol", "dave"]);
    assert_eq!(
        fs::read_dir(target.repositories.root().join("networks"))
            .unwrap()
            .count(),
        1
    );
}

/// A blob over the chunking threshold is copied into the bundle and out of it a chunk
/// at a time, and validated where it lands.
#[tokio::test(flavor = "multi_thread")]
async fn a_long_blob_moves_through_a_bundle_as_chunks() {
    let mut source = Fixture::new();
    let placed = source
        .repositories
        .create_empty("alice/long", "main")
        .await
        .unwrap();
    let network = source.repositories.network(placed.location.network);
    let long = (0..wheat_core::CHUNKED_CONTENT_THRESHOLD as usize + (3 << 20))
        .map(|index| (index % 251) as u8 ^ (index >> 20) as u8)
        .collect::<Vec<_>>();
    let id = ContentId::of(&long);
    let base = source.base;
    source
        .add(
            &network,
            placed.location.repository,
            base,
            "long.bin",
            &long,
        )
        .await;
    let bundle = source.root().join("bundle");
    let manifest = source
        .repositories
        .export_network(placed.location.network, &bundle)
        .unwrap()
        .finish(&source.content.content_store())
        .await
        .unwrap();
    assert_eq!(manifest.content_count, 1);
    assert_eq!(manifest.content_bytes, long.len() as u64);
    let held = wheat_store::PackedObjectStore::open(wheat_store::bundle_content_directory(&bundle))
        .unwrap();
    assert!(held.content_store().chunk_manifest(id).unwrap().is_some());
    held.close().unwrap();

    let target = Fixture::new();
    let names = [(placed.location.repository, "carol/long".to_owned())];
    target
        .repositories
        .import_network(&bundle, &names, &target.content.content_store())
        .await
        .unwrap();
    let content = target.content.content_store();
    assert!(content.chunk_manifest(id).unwrap().is_some());
    assert!(content.get(id).unwrap() == long);
}

fn names_of(target: &Fixture) -> Vec<String> {
    listing(&target.repositories.root().join("names"))
}

#[tokio::test]
async fn small_batches_and_content_already_there_import_the_same() {
    let (built, bundle, names) = exported().await;
    let target = Fixture::new();
    // One blob is already in the target, as an earlier network could have put it.
    target.content.content_store().put(b"shared a").unwrap();

    target
        .repositories
        .import_network_with(
            &bundle,
            &names,
            &target.content.content_store(),
            ImportLimits {
                batch_bytes: 1,
                batch_blobs: 2,
            },
        )
        .await
        .unwrap();

    assert_holds(&target, &built, &names).await;
}

#[tokio::test]
async fn an_import_that_stopped_before_the_names_is_finished_by_the_next() {
    let (built, bundle, names) = exported().await;
    let target = Fixture::new();
    let content = target.content.content_store();
    target
        .repositories
        .import_network(&bundle, &names, &content)
        .await
        .unwrap();
    // A crash after the network was installed leaves it without names.
    fs::remove_dir_all(target.repositories.root().join("names")).unwrap();
    assert_eq!(target.repositories.locate("carol/one").unwrap(), None);

    let again = target
        .repositories
        .import_network(&bundle, &names, &content)
        .await
        .unwrap();

    assert!(again.resumed);
    assert_holds(&target, &built, &names).await;
    // And once it is all there, importing it again changes nothing.
    assert!(
        target
            .repositories
            .import_network(&bundle, &names, &content)
            .await
            .unwrap()
            .resumed
    );
}

#[tokio::test]
async fn a_network_named_otherwise_is_not_named_twice() {
    let (_built, bundle, names) = exported().await;
    let target = Fixture::new();
    let content = target.content.content_store();
    target
        .repositories
        .import_network(&bundle, &names, &content)
        .await
        .unwrap();
    let renamed = names
        .iter()
        .map(|(repository, name)| (*repository, format!("{name}-again")))
        .collect::<Vec<_>>();

    let error = target
        .repositories
        .import_network(&bundle, &renamed, &content)
        .await
        .unwrap_err();

    assert!(matches!(error, ImportError::NamedElsewhere(_)), "{error:?}");
    assert_eq!(target.repositories.locate("carol/one-again").unwrap(), None);
}

#[tokio::test]
async fn names_that_do_not_fit_the_bundle_are_refused_before_anything_is_written() {
    let (built, bundle, names) = exported().await;
    let target = Fixture::new();
    let content = target.content.content_store();
    let import = |names: Vec<(Uuid, String)>| {
        let target = &target;
        let bundle = &bundle;
        let content = &content;
        async move {
            target
                .repositories
                .import_network(bundle, &names, content)
                .await
                .unwrap_err()
        }
    };

    assert!(matches!(
        import(vec![names[0].clone()]).await,
        ImportError::Unnamed(repository) if repository == names[1].0
    ));
    assert!(matches!(
        import(vec![
            names[0].clone(),
            names[1].clone(),
            (Uuid::from_u128(1), "x/y".into())
        ])
        .await,
        ImportError::UnknownRepository(_)
    ));
    assert!(matches!(
        import(vec![names[0].clone(), (names[1].0, names[0].1.clone())]).await,
        ImportError::DuplicateName(_)
    ));
    assert!(matches!(
        import(vec![names[0].clone(), (names[1].0, "no-slash".into())]).await,
        ImportError::Repositories(RepositoriesDirectoryError::InvalidName(_))
    ));

    // A name that is taken.
    target
        .repositories
        .create_empty("dave/two", "main")
        .await
        .unwrap();
    assert!(matches!(
        import(names.clone()).await,
        ImportError::Repositories(RepositoriesDirectoryError::Exists(name)) if name == "dave/two"
    ));

    assert!(
        !target
            .repositories
            .network(built.network_id)
            .root()
            .exists()
    );
    assert_eq!(target.repositories.locate("carol/one").unwrap(), None);
    // Nothing is left beside the names either.
    assert_eq!(
        listing(&target.repositories.root().join("names/carol")),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn a_damaged_bundle_is_refused_and_leaves_nothing() {
    let (built, bundle, names) = exported().await;
    let target = Fixture::new();
    let content = target.content.content_store();
    let placed = || {
        target
            .repositories
            .network(built.network_id)
            .root()
            .exists()
            || target.repositories.locate("carol/one").unwrap().is_some()
    };

    // A structure file changed.
    let manifest = read_bundle_manifest(&bundle).unwrap();
    let damaged = bundle_structure_directory(&bundle).join(&manifest.files[0].path);
    let original = fs::read(&damaged).unwrap();
    let mut bytes = original.clone();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(&damaged, bytes).unwrap();
    let error = target
        .repositories
        .import_network(&bundle, &names, &content)
        .await
        .unwrap_err();
    assert!(
        matches!(error, ImportError::Bundle(BundleError::Mismatch(_))),
        "{error:?}"
    );
    assert!(!placed());
    fs::write(&damaged, original).unwrap();

    // A path that leaves the network.
    for path in [
        "../escape",
        "storage/../../escape",
        "/etc/passwd",
        "heads/x/repository.head",
        "other",
    ] {
        let mut forged = manifest.clone();
        forged.files[0].path = path.to_owned();
        fs::write(
            bundle.join("bundle.manifest"),
            encode_bundle_manifest(&forged).unwrap(),
        )
        .unwrap();
        let error = target
            .repositories
            .import_network(&bundle, &names, &content)
            .await
            .unwrap_err();
        assert!(
            matches!(error, ImportError::InvalidPath(_)),
            "{path}: {error:?}"
        );
    }
    fs::write(
        bundle.join("bundle.manifest"),
        encode_bundle_manifest(&manifest).unwrap(),
    )
    .unwrap();
    assert!(!placed());

    // Content the bundle lacks.
    fs::remove_dir_all(bundle.join("content")).unwrap();
    let error = target
        .repositories
        .import_network(&bundle, &names, &content)
        .await
        .unwrap_err();
    assert!(
        matches!(error, ImportError::Bundle(BundleError::MissingContent(id)) if built.contents.contains(&id)),
        "{error:?}"
    );
    assert!(!placed());
    assert_eq!(
        listing(&target.repositories.root().join("names/carol")),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn an_adopted_repository_gets_a_network_of_its_own() {
    let source = tempfile::tempdir().unwrap();
    let directory = RepositoryDirectory::new(source.path().join("repo"));
    let head = directory.create_empty("main").await.unwrap();
    // Content goes to the repository's own store, as `wheat import` leaves it.
    let pool = directory.open_store().unwrap();
    let own = pool.content_store();
    let base = Repository::new(Tree::new()).base_view().id;
    publish_change(
        &pool,
        &own,
        &directory.publications(),
        base,
        base,
        1,
        "a",
        b"adopted a",
    )
    .await;
    publish_change(
        &pool,
        &own,
        &directory.publications(),
        base,
        base,
        2,
        "b",
        b"adopted b",
    )
    .await;
    drop(own);
    pool.close().unwrap();
    let head_before = directory.publications().read().unwrap();
    assert_ne!(head_before, head);

    let target = Fixture::new();
    let content = target.content.content_store();
    let placed = target
        .repositories
        .adopt_repository(&directory, "erin/adopted", &content)
        .await
        .unwrap();

    assert_eq!(placed.head, head_before);
    assert_eq!(
        target.repositories.locate("erin/adopted").unwrap(),
        Some(placed.location)
    );
    let network = target.repositories.network(placed.location.network);
    assert_eq!(
        network.repositories().unwrap(),
        [placed.location.repository]
    );
    assert_eq!(
        network
            .publications(placed.location.repository)
            .read()
            .unwrap(),
        head_before
    );
    assert_eq!(
        content.get(ContentId::of(b"adopted a")).unwrap(),
        b"adopted a"
    );
    assert_eq!(
        content.get(ContentId::of(b"adopted b")).unwrap(),
        b"adopted b"
    );
    let pool = network.open_store().unwrap();
    let reader = RepositoryReader::new(&pool, &content);
    let state = reader.envelope(head_before.envelope).await.unwrap().state;
    reader.validate_complete(state).await.unwrap();
    pool.close().unwrap();

    // The same directory adopted again is another network.
    let second = target
        .repositories
        .adopt_repository(&directory, "erin/again", &content)
        .await
        .unwrap();
    assert_ne!(second.location.network, placed.location.network);
    // A name that is taken refuses it.
    assert!(matches!(
        target
            .repositories
            .adopt_repository(&directory, "erin/again", &content)
            .await,
        Err(ImportError::Repositories(
            RepositoriesDirectoryError::Exists(_)
        ))
    ));
    // And something that is not a repository is not adopted.
    assert!(matches!(
        target
            .repositories
            .adopt_repository(
                &RepositoryDirectory::new(source.path().join("none")),
                "erin/none",
                &content
            )
            .await,
        Err(ImportError::Directory(_))
    ));
}
