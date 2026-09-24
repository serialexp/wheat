//! Pin down which canonical validation rejects the delta blob.

use wheat_core::{ContentId, ContentSetId};

#[test]
fn content_set_delta_blob_validates() {
    let id = |b: u8| {
        let mut bytes = [0u8; 32];
        bytes[0] = b;
        ContentId::from_digest(bytes)
    };
    let base = ContentSetId::from_digest([7u8; 32]);
    let bytes = wheat_core::content_set_delta_bytes(base, &[id(4), id(5)], &[id(2)]);
    match wheat_core::validate_canonical_object(&bytes) {
        Ok(kind) => println!("delta validates as {kind:?}"),
        Err(error) => panic!("delta rejected: {error:?}"),
    }
    let snapshot = wheat_core::content_set_snapshot_bytes(&[id(1), id(2), id(3)]);
    match wheat_core::validate_canonical_object(&snapshot) {
        Ok(kind) => println!("snapshot validates as {kind:?}"),
        Err(error) => panic!("snapshot rejected: {error:?}"),
    }
}
