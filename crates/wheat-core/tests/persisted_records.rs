use std::collections::BTreeMap;
use wheat_core::*;

#[test]
fn identity_and_emitted_view_roots_use_the_same_builder() {
    let mut repository = Repository::new(Tree::new());
    repository
        .create_change(
            [],
            Transformation {
                edits: vec![FileEdit {
                    path: "dir/file".into(),
                    expected: None,
                    replacement: Some(ContentId::of(b"value")),
                }],
            },
        )
        .unwrap();
    let view = repository.working_view();
    let mut objects = BTreeMap::new();
    let record = build_view_record(view, &mut |object: CanonicalObject| {
        objects.insert(object.id, object.bytes);
    });
    assert_eq!(record.id(), view.id);
    assert!(
        objects.contains_key(&ObjectId::from(record.required_content)),
        "view builder emits its referenced manifest"
    );
    assert_eq!(record.canonical_bytes(), view.canonical_bytes());
    assert_eq!(
        decode_view_record(&record.canonical_bytes()).unwrap(),
        record
    );
    let closure = repository.canonical_closure();
    for (id, bytes) in objects {
        assert!(
            closure
                .objects
                .iter()
                .any(|object| object.id == id && object.bytes == bytes)
        );
    }
    for id in [
        ObjectId::from(record.revisions),
        ObjectId::from(record.applications),
        ObjectId::from(record.unresolved_conflicts),
        ObjectId::from(record.required_content),
    ] {
        assert!(closure.objects.iter().any(|object| object.id == id));
    }
}

#[test]
fn root_only_view_encoding_stays_constant_size() {
    let mut tree = Tree::new();
    let mut frontier = BTreeMap::new();
    let empty = RepositoryView::new(tree.clone(), frontier.clone());
    for n in 0..32u8 {
        tree.insert(format!("file{n}"), ContentId::of(&[n]));
        frontier.insert(
            ChangeId::from_bytes([n; 16]),
            RevisionId::from_digest([n; 32]),
        );
    }
    let populated = RepositoryView::new(tree, frontier);
    assert_eq!(
        empty.canonical_bytes().len(),
        populated.canonical_bytes().len()
    );
    assert_ne!(empty.id, populated.id);
}

#[test]
fn every_semantic_record_roundtrips_canonical_bytes() {
    let mut repository = Repository::new(Tree::new());
    repository
        .create_change(
            [],
            Transformation {
                edits: vec![FileEdit {
                    path: "file".into(),
                    expected: None,
                    replacement: Some(ContentId::of(b"value")),
                }],
            },
        )
        .unwrap();
    for object in repository.canonical_closure().objects {
        let encoded = match validate_canonical_object(&object.bytes).unwrap() {
            CanonicalObjectKind::Revision => decode_revision_record(&object.bytes)
                .unwrap()
                .canonical_bytes(),
            CanonicalObjectKind::Application => decode_application_record(&object.bytes)
                .unwrap()
                .canonical_bytes(),
            CanonicalObjectKind::Change => decode_change(&object.bytes).unwrap().canonical_bytes(),
            CanonicalObjectKind::View => {
                decode_view_record(&object.bytes).unwrap().canonical_bytes()
            }
            CanonicalObjectKind::ContentManifest => decode_content_manifest_record(&object.bytes)
                .unwrap()
                .canonical_bytes(),
            CanonicalObjectKind::RepositoryState => decode_repository_state_record(&object.bytes)
                .unwrap()
                .canonical_bytes(),
            _ => continue,
        };
        assert_eq!(encoded, object.bytes);
    }
}
