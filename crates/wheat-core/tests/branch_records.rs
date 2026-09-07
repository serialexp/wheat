use wheat_core::{
    BranchManifestRecord, CanonicalObjectError, CanonicalObjectKind, RepositoryEnvelopeRecord,
    RepositoryStateId, ViewId, decode_branch_manifest_record, decode_repository_envelope_record,
    validate_canonical_object,
};

fn view(byte: u8) -> ViewId {
    ViewId::from_digest([byte; 32])
}

#[test]
fn manifest_is_order_independent_and_roundtrips() {
    let left = BranchManifestRecord::new([("z", view(1)), ("refs/heads/main", view(2))]).unwrap();
    let right = BranchManifestRecord::new([("refs/heads/main", view(2)), ("z", view(1))]).unwrap();
    assert_eq!(left.canonical_bytes(), right.canonical_bytes());
    assert_eq!(
        decode_branch_manifest_record(&left.canonical_bytes()),
        Ok(left.clone())
    );
    assert_eq!(
        validate_canonical_object(&left.canonical_bytes()),
        Ok(CanonicalObjectKind::BranchManifest)
    );
}

#[test]
fn manifest_rejects_duplicate_and_invalid_names() {
    assert!(matches!(
        BranchManifestRecord::new([("main", view(1)), ("main", view(2))]),
        Err(CanonicalObjectError::InvalidCollection(_))
    ));
    for name in [
        "", "\0", "/main", "main/", "a//b", ".", "..", "a/./b", "a/../b",
    ] {
        assert!(matches!(
            BranchManifestRecord::new([(name, view(1))]),
            Err(CanonicalObjectError::InvalidBranchName(_))
        ));
    }
}

#[test]
fn envelope_roundtrips_and_has_a_distinct_tagged_identity() {
    let manifest = BranchManifestRecord::new([("main", view(1))]).unwrap();
    let envelope = RepositoryEnvelopeRecord {
        state: RepositoryStateId::from_digest([3; 32]),
        branches: manifest.id(),
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
