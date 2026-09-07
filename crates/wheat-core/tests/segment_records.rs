use wheat_core::*;

#[test]
fn physical_records_round_trip_and_footer_is_fixed() {
    let page = SegmentIndexPage {
        entries: vec![SegmentIndexEntry {
            namespace: SEGMENT_NAMESPACE_CONTENT,
            kind: 0,
            digest: [3; 32],
            offset: 6,
            length: 9,
        }],
    };
    let bytes = encode_segment_index_page(&page).unwrap();
    assert_eq!(decode_segment_index_page(&bytes).unwrap(), page);
    let directory = SegmentDirectory {
        pages: vec![SegmentDirectoryEntry {
            first_key: SegmentIndexKey {
                namespace: 2,
                digest: [3; 32],
            },
            offset: 15,
            length: bytes.len() as u64,
            page_digest: [4; 32],
        }],
    };
    let bytes = encode_segment_directory(&directory).unwrap();
    assert_eq!(decode_segment_directory(&bytes).unwrap(), directory);
    let footer = SegmentFooter {
        total_length: 100,
        object_count: 1,
        index_start: 15,
        directory_offset: 70,
        directory_length: 85,
        body_digest: [8; 32],
    };
    let bytes = encode_segment_footer(&footer).unwrap();
    assert_eq!(bytes.len(), SEGMENT_FOOTER_ENCODED_SIZE);
    assert_eq!(decode_segment_footer(&bytes).unwrap(), footer);
    assert_eq!(footer.segment_id(), SegmentId([8; 32]));
    assert_eq!(
        encode_segment_prefix().unwrap().len(),
        SEGMENT_PREFIX_ENCODED_SIZE
    );
    let catalog = SegmentCatalogRecord {
        generation: 7,
        previous: Some([9; 32]),
        segment: [8; 32],
        stored_length: 100,
    };
    let bytes = encode_segment_catalog_record(&catalog).unwrap();
    assert_eq!(decode_segment_catalog_record(&bytes).unwrap(), catalog);
}

#[test]
fn edit_kind_index_entry_round_trips_and_unassigned_kinds_are_rejected() {
    let edit_entry = SegmentIndexEntry {
        namespace: SEGMENT_NAMESPACE_METADATA,
        kind: SegmentIndexEntry::canonical_kind_code(CanonicalObjectKind::Edit),
        digest: [16; 32],
        offset: SEGMENT_PREFIX_ENCODED_SIZE as u64,
        length: 42,
    };
    assert_eq!(edit_entry.kind, 16);
    let page = SegmentIndexPage {
        entries: vec![edit_entry],
    };
    let bytes = encode_segment_index_page(&page).unwrap();
    assert_eq!(decode_segment_index_page(&bytes).unwrap(), page);
    assert_eq!(
        SegmentIndexEntry::canonical_kind(16),
        Some(CanonicalObjectKind::Edit)
    );

    for kind in [0, 17, u16::MAX] {
        let invalid = SegmentIndexPage {
            entries: vec![SegmentIndexEntry {
                namespace: SEGMENT_NAMESPACE_METADATA,
                kind,
                digest: [0; 32],
                offset: 0,
                length: 0,
            }],
        };
        assert!(matches!(
            encode_segment_index_page(&invalid),
            Err(SegmentRecordError::InvalidNamespaceKind {
                namespace: SEGMENT_NAMESPACE_METADATA,
                kind: rejected,
            }) if rejected == kind
        ));
    }
}

#[test]
fn catalog_record_without_previous_round_trips_and_is_canonical() {
    let catalog = SegmentCatalogRecord {
        generation: 0,
        previous: None,
        segment: [5; 32],
        stored_length: 78,
    };
    let mut bytes = encode_segment_catalog_record(&catalog).unwrap();
    assert_eq!(decode_segment_catalog_record(&bytes).unwrap(), catalog);
    bytes.push(0);
    assert_eq!(
        decode_segment_catalog_record(&bytes),
        Err(SegmentRecordError::NonCanonicalEncoding)
    );
}

#[test]
fn rejects_trailing_bytes_and_malicious_counts() {
    let mut prefix = encode_segment_prefix().unwrap();
    prefix.push(0);
    assert!(decode_segment_prefix(&prefix).is_err());
    assert_eq!(
        decode_segment_index_page(&u32::MAX.to_be_bytes()),
        Err(SegmentRecordError::InvalidCount)
    );
    assert_eq!(
        decode_segment_directory(&u32::MAX.to_be_bytes()),
        Err(SegmentRecordError::InvalidCount)
    );
    let invalid = SegmentIndexPage {
        entries: vec![SegmentIndexEntry {
            namespace: 2,
            kind: 1,
            digest: [0; 32],
            offset: 0,
            length: 0,
        }],
    };
    assert!(matches!(
        encode_segment_index_page(&invalid),
        Err(SegmentRecordError::InvalidNamespaceKind { .. })
    ));
}
