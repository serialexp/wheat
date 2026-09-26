use wheat_core::{
    CanonicalObjectError, CanonicalObjectKind, RepositoryEnvelopeRecord, RepositoryStateId, ViewId,
    ViewLabel, ViewLabelManifestRecord, decode_repository_envelope_record,
    decode_view_label_manifest_record, validate_canonical_object,
};

fn view(byte: u8) -> ViewId {
    ViewId::from_digest([byte; 32])
}

#[test]
fn manifest_is_order_independent_and_roundtrips() {
    let imported = ViewLabel {
        view: view(2),
        original_git_ref: Some("refs/heads/main".to_owned()),
        annotation: None,
    };
    let left =
        ViewLabelManifestRecord::new([("z", ViewLabel::from(view(1))), ("main", imported.clone())])
            .unwrap();
    let right = ViewLabelManifestRecord::new([("main", imported), ("z", ViewLabel::from(view(1)))])
        .unwrap();
    assert_eq!(left.canonical_bytes(), right.canonical_bytes());
    assert_eq!(
        decode_view_label_manifest_record(&left.canonical_bytes()),
        Ok(left.clone())
    );
    assert_eq!(
        left.labels["main"].original_git_ref.as_deref(),
        Some("refs/heads/main")
    );
    assert_eq!(
        validate_canonical_object(&left.canonical_bytes()),
        Ok(CanonicalObjectKind::ViewLabelManifest)
    );
}

#[test]
fn manifest_rejects_duplicate_and_invalid_names() {
    assert!(matches!(
        ViewLabelManifestRecord::new([("main", view(1)), ("main", view(2))]),
        Err(CanonicalObjectError::InvalidCollection(_))
    ));
    assert!(matches!(
        ViewLabelManifestRecord::new([(
            "main",
            ViewLabel {
                view: view(1),
                original_git_ref: Some("main".to_owned()),
                annotation: None,
            },
        )]),
        Err(CanonicalObjectError::InvalidViewLabel(_))
    ));
    for name in [
        "", "\0", "/main", "main/", "a//b", ".", "..", "a/./b", "a/../b",
    ] {
        assert!(matches!(
            ViewLabelManifestRecord::new([(name, view(1))]),
            Err(CanonicalObjectError::InvalidViewLabel(_))
        ));
    }
}

#[test]
fn envelope_roundtrips_and_has_a_distinct_tagged_identity() {
    let manifest = ViewLabelManifestRecord::new([("main", view(1))]).unwrap();
    let envelope = RepositoryEnvelopeRecord {
        state: RepositoryStateId::from_digest([3; 32]),
        labels: manifest.id(),
    };
    assert_eq!(
        decode_repository_envelope_record(&envelope.canonical_bytes()),
        Ok(envelope)
    );
    assert_eq!(
        validate_canonical_object(&envelope.canonical_bytes()),
        Ok(CanonicalObjectKind::RepositoryEnvelope)
    );
    assert_ne!(
        envelope.canonical_bytes()[..2],
        manifest.canonical_bytes()[..2]
    );
    let envelope_object: wheat_core::ObjectId = envelope.id().into();
    let manifest_object: wheat_core::ObjectId = manifest.id().into();
    assert_ne!(envelope_object, manifest_object);
}
