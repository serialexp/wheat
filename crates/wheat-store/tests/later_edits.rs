//! The edits later changes made to a path, read for placing an edit into an earlier change.

use std::{
    collections::BTreeSet,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use wheat_core::{
    edit_capture::{FileSnapshot, capture_edit},
    edit_placement::{Origin, attribute},
    guarded::Limits,
    *,
};
use wheat_store::*;

/// Counts reads of a chosen set of objects.
struct CountingStore {
    inner: LooseObjectStore,
    watched: Mutex<BTreeSet<ObjectId>>,
    watched_reads: AtomicUsize,
    reads: AtomicUsize,
}

impl CountingStore {
    fn watch(&self, ids: BTreeSet<ObjectId>) {
        *self.watched.lock().unwrap() = ids;
        self.watched_reads.store(0, Ordering::Relaxed);
        self.reads.store(0, Ordering::Relaxed);
    }
}

#[async_trait]
impl ImmutableObjectStore for CountingStore {
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, bytes).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if self.watched.lock().unwrap().contains(&id) {
            self.watched_reads.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    objects: CountingStore,
    content: ContentStore,
    heads: LocalPublicationStore,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    Fixture {
        objects: CountingStore {
            inner: LooseObjectStore::new(dir.path().join("objects")),
            watched: Mutex::new(BTreeSet::new()),
            watched_reads: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
        },
        content: ContentStore::new(dir.path().join("content")),
        heads: LocalPublicationStore::new(dir.path().join("head")),
        _dir: dir,
    }
}

fn snapshot(text: &str) -> FileSnapshot<'_> {
    FileSnapshot {
        kind: FileKind::Regular,
        bytes: text.as_bytes(),
    }
}

/// The edits taking `path` from `before` to `after`, with their content stored.
fn edit(content: &ContentStore, path: &str, before: Option<&str>, after: &str) -> RevisionEdits {
    let captured = capture_edit(
        path,
        before.map(snapshot),
        Some(snapshot(after)),
        None,
        Limits::default(),
    )
    .unwrap();
    for captured in captured.content.values() {
        content.put_verified(captured.borrowed()).unwrap();
    }
    RevisionEdits {
        transition: EditTransition {
            added: captured.edits.keys().copied().collect(),
            ..EditTransition::default()
        },
        edits: captured.edits,
    }
}

fn proposal(id: ChangeId, edits: RevisionEdits) -> ChangeProposal {
    ChangeProposal {
        id,
        metadata: ChangeMetadata::default(),
        dependencies: BTreeSet::new(),
        revision_metadata: RevisionMetadata::default(),
        edits,
    }
}

const BASE: &str = "a\nb\nc\nd\ne\n";

#[tokio::test]
async fn later_edits_follow_every_change_above_and_skip_the_rest() {
    let fixture = fixture();
    let heads = &fixture.heads;
    let objects = &fixture.objects;
    let mut head = RepositoryPublisher::new(objects, &fixture.content, heads)
        .initialize(&Repository::new(Tree::new()))
        .await
        .unwrap();
    let base = Repository::new(Tree::new()).base_view().id;
    let mutator = PersistedRepositoryMutator::new(objects, &fixture.content, heads);
    let mut effective = base;
    let mut create = async |id: ChangeId, edits: RevisionEdits| {
        let (prepared, outcome) = mutator
            .create(head, base, effective, proposal(id, edits))
            .await
            .unwrap();
        head = mutator.publish(prepared).await.unwrap();
        effective = outcome.resulting_view;
        outcome
    };

    // A creates the file, B and D edit it, C only touches another file.
    let a = ChangeId::new();
    create(a, edit(&fixture.content, "f", None, BASE)).await;
    let b = ChangeId::new();
    let after_b = "a\nb\nC1\nC2\nd\ne\n";
    let b_edits = edit(&fixture.content, "f", Some(BASE), after_b);
    create(b, b_edits.clone()).await;
    let c = ChangeId::new();
    let c_edits = edit(&fixture.content, "other", None, "unrelated\n");
    create(c, c_edits.clone()).await;
    let d = ChangeId::new();
    let after_d = "a\nb\nC1\nC2\nd\nE\n";
    let d_edits = edit(&fixture.content, "f", Some(after_b), after_d);
    let d1 = create(d, d_edits.clone()).await;

    let paths = BTreeSet::from(["f".to_owned()]);
    let session = mutator.open_authoring(head, base, effective).await.unwrap();
    objects.watch(c_edits.edits.keys().map(|id| (*id).into()).collect());
    let later = mutator.later_edits(&session, a, &paths).await.unwrap();
    assert_eq!(objects.watched_reads.load(Ordering::Relaxed), 0);
    let history = &later.paths["f"];
    assert_eq!(
        history.below.unwrap().content,
        ContentId::of(BASE.as_bytes())
    );
    assert_eq!(
        history
            .steps
            .iter()
            .map(|step| step.change)
            .collect::<Vec<_>>(),
        vec![b, d]
    );
    assert_eq!(history.steps[0].edits, b_edits.edits);
    assert_eq!(history.steps[1].edits, d_edits.edits);
    assert_eq!(history.steps[1].revision, d1.revision_id);

    // A workspace that shows the file as B left it saw B's edit but not D's.
    let value = |text: &str| {
        Some(FileValue {
            content: ContentId::of(text.as_bytes()),
            kind: FileKind::Regular,
        })
    };
    let steps = |shown| {
        history.up_to(shown).map(|seen| {
            seen.steps
                .iter()
                .map(|step| step.change)
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(history.steps[1].after, value(after_d));
    assert_eq!(steps(value(after_d)), Some(vec![b, d]));
    assert_eq!(steps(value(after_b)), Some(vec![b]));
    assert_eq!(steps(value(BASE)), Some(vec![]));
    assert_eq!(steps(value("never\n")), None);
    assert_eq!(steps(None), Some(vec![]));

    // The writes attribute every byte of the file the author sees.
    let provenance = attribute(
        Some(BASE.len()),
        &history.placement_steps(&fixture.content).unwrap(),
    )
    .unwrap();
    let mut rebuilt = String::new();
    for segment in provenance.segments.unwrap() {
        match segment.origin {
            Origin::Below => rebuilt.push_str(&BASE[segment.below]),
            Origin::Above(edit) => {
                assert!(edit.change == b || edit.change == d);
                rebuilt.push_str(&after_d[segment.range]);
            }
        }
    }
    assert_eq!(rebuilt, after_d);

    // With the change on top there is nothing above it, and no edit is read.
    let all_edits = [&b_edits, &c_edits, &d_edits]
        .iter()
        .flat_map(|edits| edits.edits.keys().map(|id| (*id).into()))
        .collect();
    objects.watch(all_edits);
    let later = mutator.later_edits(&session, d, &paths).await.unwrap();
    assert!(later.paths["f"].steps.is_empty());
    assert_eq!(objects.watched_reads.load(Ordering::Relaxed), 0);
    assert!(objects.reads.load(Ordering::Relaxed) <= 8);

    // The same session then publishes a revision.
    let reader = RepositoryReader::new(objects, &fixture.content);
    let current = RevisionMapStore::new(objects)
        .get(reader.view(effective).await.unwrap().revisions, c)
        .await
        .unwrap()
        .unwrap();
    let mut revised = c_edits.clone();
    revised.transition = EditTransition {
        preserved: c_edits.edits.keys().copied().collect(),
        ..EditTransition::default()
    };
    let mut extra = edit(&fixture.content, "extra", None, "x\n");
    revised.edits.append(&mut extra.edits);
    revised.transition.added = extra.transition.added;
    let (prepared, _) = mutator
        .revise_changes_in(
            session,
            [(
                c,
                RevisionRequest {
                    predecessor: current,
                    metadata: RevisionMetadata::default(),
                    edits: revised,
                },
            )]
            .into(),
        )
        .await
        .unwrap();
    mutator.publish(prepared).await.unwrap();
}
