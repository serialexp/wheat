//! A stored object comes from disk or a peer, so its length and count fields are
//! untrusted. A forged count must fail to decode, not reserve memory for the
//! items it claims: `u32::MAX` directory entries is hundreds of GiB, and a
//! failed allocation aborts the whole process instead of returning an error.

use wheat_core::{CanonicalObjectError, validate_canonical_object};

#[test]
fn a_forged_tree_entry_count_is_a_decode_error() {
    // TreeV1's wire tag (257), then an entry count of u32::MAX and no entries.
    let bytes = [0x01, 0x01, 0xff, 0xff, 0xff, 0xff];
    let result = validate_canonical_object(&bytes);
    assert!(
        matches!(result, Err(CanonicalObjectError::Decode(_))),
        "{result:?}"
    );
}
