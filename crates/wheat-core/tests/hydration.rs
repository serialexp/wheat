use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
};
use wheat_core::*;
#[derive(Default)]
struct Memory(RefCell<BTreeMap<ContentId, Vec<u8>>>);
impl ContentIo for Memory {
    type Error = &'static str;
    fn resolve(&self, id: ContentId) -> Result<VerifiedContent, Self::Error> {
        let bytes = self.0.borrow().get(&id).cloned().ok_or("missing")?;
        VerifiedContent::check(id, bytes).map_err(|_| "corrupt")
    }
    fn store_generated(&self, content: VerifiedContent<&[u8]>) -> Result<(), Self::Error> {
        self.0
            .borrow_mut()
            .insert(content.id(), content.bytes().to_vec());
        Ok(())
    }
}
#[test]
fn hydration_requires_authenticated_selected_edit_definitions() {
    let io = Memory::default();
    let bytes = b"value";
    let value = FileValue {
        content: ContentId::of(bytes),
        kind: FileKind::Regular,
    };
    io.0.borrow_mut().insert(value.content, bytes.to_vec());
    let edit = EditRecord::new(
        "file",
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(value),
        },
    )
    .unwrap();
    let id = edit.id();
    let edits = RevisionEdits {
        edits: BTreeMap::from([(id, edit.clone())]),
        transition: EditTransition {
            added: BTreeSet::from([id]),
            ..Default::default()
        },
    };
    let mut repo = Repository::new(Tree::new());
    let base = repo.base_view().id;
    let (_, out) = repo
        .create_change(base, base, [], edits, ApplicationContext::new(&io))
        .unwrap();
    let revision = repo.revision(out.revision_id).unwrap();
    let record = decode_revision_record(&revision.canonical_bytes()).unwrap();
    assert!(
        hydrate_revision(
            record.clone(),
            revision.resulting_tree().clone(),
            BTreeMap::new()
        )
        .is_err()
    );
    let hydrated = hydrate_revision(
        record,
        revision.resulting_tree().clone(),
        BTreeMap::from([(id, edit)]),
    )
    .unwrap();
    assert_eq!(hydrated.canonical_bytes(), revision.canonical_bytes());
    let app = repo
        .applications_for_revision(out.revision_id)
        .next()
        .unwrap();
    let record = decode_application_record(&app.canonical_bytes()).unwrap();
    let hydrated =
        hydrate_application(record, app.result.materialized_tree().clone(), None).unwrap();
    assert_eq!(&hydrated, app);
}

#[test]
fn reconstruction_rejects_duplicate_and_missing_view_roots() {
    let repo = Repository::new(Tree::new());
    let state =
        decode_repository_state_record(&repo.canonical_closure().state.canonical_bytes()).unwrap();
    let parts = |views| RepositoryParts {
        state,
        changes: vec![],
        views,
        revisions: vec![],
        applications: vec![],
        view_lineage: BTreeSet::new(),
    };
    assert!(Repository::from_parts(parts(vec![])).is_err());
    assert!(
        Repository::from_parts(parts(vec![
            repo.base_view().clone(),
            repo.base_view().clone(),
        ]))
        .is_err()
    );
    let restored = Repository::from_parts(parts(vec![repo.base_view().clone()])).unwrap();
    assert_eq!(restored.canonical_closure(), repo.canonical_closure());
    assert_eq!(restored.base_view().unresolved_conflicts, BTreeSet::new());
}
