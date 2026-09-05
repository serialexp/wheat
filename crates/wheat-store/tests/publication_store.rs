use std::{fs, sync::Arc, thread};

use wheat_core::ObjectId;
use wheat_store::{LocalPublicationStore, PublicationError, PublicationStore, RepositoryHead};

fn state(byte: u8) -> ObjectId {
    ObjectId::from_digest([byte; 32])
}

#[test]
fn initializes_idempotently_and_reopens() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("repository.head");
    let initial = RepositoryHead::initial(state(1));
    let store = LocalPublicationStore::new(&path);

    store.initialize(initial).unwrap();
    store.initialize(initial).unwrap();
    assert_eq!(store.read().unwrap(), initial);
    assert_eq!(LocalPublicationStore::new(path).read().unwrap(), initial);
}

#[test]
fn advances_only_from_the_exact_expected_head() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalPublicationStore::new(directory.path().join("repository.head"));
    let initial = RepositoryHead::initial(state(1));
    let next = initial.next(state(2)).unwrap();
    store.initialize(initial).unwrap();
    store.compare_and_advance(initial, next).unwrap();

    let stale_proposal = initial.next(state(3)).unwrap();
    assert!(matches!(
        store.compare_and_advance(initial, stale_proposal),
        Err(PublicationError::Stale { actual, .. }) if actual == next
    ));
    assert_eq!(store.read().unwrap(), next);
}

#[test]
fn rejects_generation_skips_without_changing_the_head() {
    let directory = tempfile::tempdir().unwrap();
    let store = LocalPublicationStore::new(directory.path().join("repository.head"));
    let initial = RepositoryHead::initial(state(1));
    store.initialize(initial).unwrap();
    let skipped = RepositoryHead {
        generation: 2,
        repository_state: state(2),
    };

    assert!(matches!(
        store.compare_and_advance(initial, skipped),
        Err(PublicationError::InvalidGeneration)
    ));
    assert_eq!(store.read().unwrap(), initial);
}

#[test]
fn concurrent_publishers_have_one_winner() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalPublicationStore::new(
        directory.path().join("repository.head"),
    ));
    let initial = RepositoryHead::initial(state(1));
    store.initialize(initial).unwrap();

    let publishers = [state(2), state(3)]
        .into_iter()
        .map(|repository_state| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                let proposed = initial.next(repository_state).unwrap();
                (proposed, store.compare_and_advance(initial, proposed))
            })
        })
        .collect::<Vec<_>>();
    let results = publishers
        .into_iter()
        .map(|publisher| publisher.join().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        results.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|(_, result)| matches!(result, Err(PublicationError::Stale { .. })))
            .count(),
        1
    );
    let winner = results
        .iter()
        .find_map(|(head, result)| result.is_ok().then_some(*head))
        .unwrap();
    assert_eq!(store.read().unwrap(), winner);
}

#[test]
fn reports_missing_and_corrupt_heads() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("repository.head");
    let store = LocalPublicationStore::new(&path);
    assert!(matches!(store.read(), Err(PublicationError::Missing)));

    fs::write(&path, b"not a head").unwrap();
    assert!(matches!(store.read(), Err(PublicationError::CorruptHead)));
}
