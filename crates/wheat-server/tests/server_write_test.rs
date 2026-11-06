//! Server writes: label changes an embedder makes through the group commit, checked
//! per label against the head they join onto, idempotent by key.

use crate::support;

use std::sync::Arc;

use wheat_core::{ApplicationContext, Repository, Tree, ViewId, ViewLabel};
use wheat_server::access::Principal;
use wheat_server::commit::CommitFailure;
use wheat_server::quiesce::Quiesced;
use wheat_server::registry::RepositoryEntry;
use wheat_server::write::{LabelChange, ServerWrite, WriteFailure, write};
use wheat_store::{
    LocalPublicationStore, PackedObjectStore, PublicationStore, RepositoryHead,
    RepositoryPublisher, RepositoryReader,
};

use support::edit;

struct Server {
    _dir: tempfile::TempDir,
    objects: PackedObjectStore,
    entry: Arc<RepositoryEntry>,
    initial: RepositoryHead,
    /// The base view, the view with one change, and one with a second change.
    base: ViewId,
    first: ViewId,
    second: ViewId,
}

/// A repository with two changes, one after the other, and no labels.
async fn server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let objects = PackedObjectStore::open(dir.path().join("storage")).unwrap();
    let content = objects.content_store();
    let heads = LocalPublicationStore::new(dir.path().join("repository.head"));
    content.put(b"one").unwrap();
    content.put(b"two").unwrap();
    let mut repo = Repository::new(Tree::new());
    let base = repo.base_view().id;
    let (_, first) = repo
        .create_change(
            base,
            base,
            [],
            edit("one.txt", b"one"),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let first = first.resulting_view;
    let (_, second) = repo
        .create_change(
            base,
            first,
            [],
            edit("two.txt", b"two"),
            ApplicationContext::new(&content),
        )
        .unwrap();
    RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repo)
        .await
        .unwrap();
    let initial = heads.read().unwrap();
    Server {
        _dir: dir,
        entry: Arc::new(RepositoryEntry::new(Box::new(objects.clone()), heads)),
        objects,
        initial,
        base,
        first,
        second: second.resulting_view,
    }
}

fn change(name: &str, expected: Option<ViewId>, new: Option<ViewId>) -> LabelChange {
    LabelChange {
        name: name.to_owned(),
        expected,
        new: new.map(ViewLabel::from),
    }
}

fn server_write(key: u8, labels: Vec<LabelChange>) -> ServerWrite {
    ServerWrite {
        principal: Principal {
            name: "gothab".into(),
        },
        key: [key; 16],
        labels,
        force_labels: false,
    }
}

/// The labels `head` holds, by name.
async fn labels(server: &Server, head: RepositoryHead) -> Vec<(String, ViewId)> {
    let content = server.objects.content_store();
    let reader = RepositoryReader::new(&server.objects, &content);
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

#[tokio::test]
async fn a_write_creates_moves_and_deletes_labels() {
    let server = server().await;
    let created = write(
        &server.entry,
        server_write(
            1,
            vec![
                change("main", None, Some(server.first)),
                change(".pull/1/head", None, Some(server.second)),
            ],
        ),
    )
    .await
    .unwrap();
    assert_eq!(created.generation, server.initial.generation + 1);
    assert_eq!(
        labels(&server, created).await,
        [
            (".pull/1/head".to_owned(), server.second),
            ("main".to_owned(), server.first)
        ]
    );
    let moved = write(
        &server.entry,
        server_write(
            2,
            vec![
                change("main", Some(server.first), Some(server.second)),
                change(".pull/1/head", Some(server.second), None),
            ],
        ),
    )
    .await
    .unwrap();
    assert_eq!(moved.generation, created.generation + 1);
    assert_eq!(
        labels(&server, moved).await,
        [("main".to_owned(), server.second)]
    );
    assert_eq!(server.entry.heads.head().await.unwrap(), moved);
}

#[tokio::test]
async fn a_label_not_where_the_write_expects_it_is_reported_with_its_view() {
    let server = server().await;
    let head = write(
        &server.entry,
        server_write(1, vec![change("main", None, Some(server.first))]),
    )
    .await
    .unwrap();
    let refused = write(
        &server.entry,
        server_write(
            2,
            vec![
                change("main", Some(server.base), Some(server.second)),
                change("dev", Some(server.first), None),
                change("other", None, Some(server.first)),
            ],
        ),
    )
    .await;
    let Err(WriteFailure::LabelMoved { head: at, labels }) = refused else {
        panic!("expected the labels to have moved: {refused:?}");
    };
    assert_eq!(at, head);
    assert_eq!(
        labels,
        [
            ("main".to_owned(), Some(server.first)),
            ("dev".to_owned(), None)
        ]
    );
    // Creating a label that exists is refused the same way.
    let refused = write(
        &server.entry,
        server_write(3, vec![change("main", None, Some(server.second))]),
    )
    .await;
    assert!(matches!(
        refused,
        Err(WriteFailure::LabelMoved { labels, .. }) if labels == [("main".to_owned(), Some(server.first))]
    ));
    assert_eq!(server.entry.heads.head().await.unwrap(), head);
}

#[tokio::test]
async fn a_write_sent_again_gets_its_first_answer() {
    let server = server().await;
    let labels = vec![change("main", None, Some(server.first))];
    let first = write(&server.entry, server_write(7, labels.clone()))
        .await
        .unwrap();
    // The label now exists, so only the receipt can answer this.
    let again = write(&server.entry, server_write(7, labels)).await.unwrap();
    assert_eq!(again, first);
    assert_eq!(server.entry.heads.head().await.unwrap(), first);
    // The same key for something else is refused.
    let reused = write(
        &server.entry,
        server_write(7, vec![change("dev", None, Some(server.first))]),
    )
    .await;
    assert!(
        matches!(
            reused,
            Err(WriteFailure::Commit(CommitFailure::Rejected { .. }))
        ),
        "{reused:?}"
    );
}

#[tokio::test]
async fn labels_move_forward_unless_forced_and_only_to_retained_views() {
    let server = server().await;
    write(
        &server.entry,
        server_write(1, vec![change("main", None, Some(server.second))]),
    )
    .await
    .unwrap();
    let backward = vec![change("main", Some(server.second), Some(server.base))];
    let refused = write(&server.entry, server_write(2, backward.clone())).await;
    assert!(
        matches!(
            refused,
            Err(WriteFailure::Commit(CommitFailure::Rejected { .. }))
        ),
        "{refused:?}"
    );
    let forced = write(
        &server.entry,
        ServerWrite {
            force_labels: true,
            ..server_write(3, backward)
        },
    )
    .await
    .unwrap();
    assert_eq!(
        labels(&server, forced).await,
        [("main".to_owned(), server.base)]
    );
    let unknown = ViewId::from_digest([9; 32]);
    let refused = write(
        &server.entry,
        server_write(4, vec![change("other", None, Some(unknown))]),
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(WriteFailure::Commit(CommitFailure::Rejected { .. }))
        ),
        "{refused:?}"
    );
}

#[tokio::test]
async fn concurrent_writes_join_and_are_checked_against_the_head_they_join() {
    let server = server().await;
    // Two writes of different labels both land; two that both create one label do
    // not, whichever comes first.
    let (a, b, c) = tokio::join!(
        write(
            &server.entry,
            server_write(1, vec![change("a", None, Some(server.first))])
        ),
        write(
            &server.entry,
            server_write(2, vec![change("b", None, Some(server.first))])
        ),
        write(
            &server.entry,
            server_write(3, vec![change("a", None, Some(server.second))])
        ),
    );
    let landed = [&a, &c].iter().filter(|outcome| outcome.is_ok()).count();
    assert_eq!(landed, 1, "{a:?} {c:?}");
    assert!(b.is_ok(), "{b:?}");
    let head = server.entry.heads.head().await.unwrap();
    let labels = labels(&server, head).await;
    let a_view = if a.is_ok() {
        server.first
    } else {
        server.second
    };
    assert_eq!(
        labels,
        [("a".to_owned(), a_view), ("b".to_owned(), server.first)]
    );
}

#[tokio::test]
async fn invalid_writes_and_quiesced_networks_are_refused() {
    let server = server().await;
    for labels in [
        vec![],
        vec![
            change("main", None, Some(server.first)),
            change("main", None, Some(server.second)),
        ],
        vec![change("main", None, None)],
        vec![change("", None, Some(server.first))],
    ] {
        let refused = write(&server.entry, server_write(1, labels)).await;
        assert!(
            matches!(refused, Err(WriteFailure::Invalid(_))),
            "{refused:?}"
        );
    }
    let held = Quiesced::hold(Arc::clone(&server.entry)).await;
    let refused = write(
        &server.entry,
        server_write(2, vec![change("main", None, Some(server.first))]),
    )
    .await;
    assert!(
        matches!(refused, Err(WriteFailure::Quiesced { .. })),
        "{refused:?}"
    );
    drop(held);
    write(
        &server.entry,
        server_write(2, vec![change("main", None, Some(server.first))]),
    )
    .await
    .unwrap();
}
