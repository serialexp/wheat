use wheat_core::*;

#[test]
fn physical_catalog_round_trips_and_rejects_noncanonical_order() {
    let snapshot = PhysicalCatalogSnapshot {
        generation: 7,
        segments: vec![
            PhysicalSegmentDescriptor {
                generation: 6,
                sequence: 0,
                segment: [1; 32],
                stored_length: 100,
            },
            PhysicalSegmentDescriptor {
                generation: 7,
                sequence: 0,
                segment: [2; 32],
                stored_length: 200,
            },
        ],
        content: PhysicalContentGeneration {
            generation: 0,
            manifest: [0; 32],
        },
    };
    let encoded = encode_physical_catalog_snapshot(&snapshot).unwrap();
    // Pins the exact byte layout: magic, version, generation, segments, content.
    let expected = [
        0x57, 0x48, 0x43, 0x54, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, 0x00,
        0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x64, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02,
        0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02,
        0x02, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc8, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    assert_eq!(encoded, expected);
    assert_eq!(
        decode_physical_catalog_snapshot(&encoded).unwrap(),
        snapshot
    );
    let mut malicious_count = encoded.clone();
    malicious_count[14..18].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(
        decode_physical_catalog_snapshot(&malicious_count),
        Err(SegmentRecordError::InvalidCount)
    );

    let mut invalid = snapshot;
    invalid.segments.swap(0, 1);
    assert_eq!(
        encode_physical_catalog_snapshot(&invalid),
        Err(SegmentRecordError::NonCanonicalEncoding)
    );
}

#[test]
fn physical_records_round_trip_and_footer_is_fixed() {
    let page = SegmentIndexPage {
        entries: vec![SegmentIndexEntry {
            namespace: SEGMENT_NAMESPACE_CONTENT,
            kind: 0,
            digest: [3; 32],
            offset: 6,
            codec: 1,
            stored_length: 9,
            logical_length: 90,
            delta_base: None,
            delta_depth: 0,
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
}

#[test]
fn edit_kind_index_entry_round_trips_and_unassigned_kinds_are_rejected() {
    let edit_entry = SegmentIndexEntry {
        namespace: SEGMENT_NAMESPACE_METADATA,
        kind: SegmentIndexEntry::canonical_kind_code(CanonicalObjectKind::Edit),
        digest: [16; 32],
        offset: SEGMENT_PREFIX_ENCODED_SIZE as u64,
        codec: 0,
        stored_length: 42,
        logical_length: 42,
        delta_base: None,
        delta_depth: 0,
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

    assert_eq!(
        SegmentIndexEntry::canonical_kind(17),
        Some(CanonicalObjectKind::ChangeMapNode)
    );
    assert_eq!(
        SegmentIndexEntry::canonical_kind(18),
        Some(CanonicalObjectKind::ViewLineageMapNode)
    );

    assert_eq!(
        SegmentIndexEntry::canonical_kind(7),
        Some(CanonicalObjectKind::ContentMapNode)
    );
    assert_eq!(
        SegmentIndexEntry::canonical_kind(12),
        Some(CanonicalObjectKind::ContentSetNode)
    );

    // 19 and 20 were the removed content-set snapshot and delta kinds.
    for kind in [0, 19, 20, 21, u16::MAX] {
        let invalid = SegmentIndexPage {
            entries: vec![SegmentIndexEntry {
                namespace: SEGMENT_NAMESPACE_METADATA,
                kind,
                digest: [0; 32],
                offset: 0,
                codec: 0,
                stored_length: 0,
                logical_length: 0,
                delta_base: None,
                delta_depth: 0,
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
fn codec_identity_and_raw_lengths_are_validated() {
    let entry = |codec, stored_length, logical_length| SegmentIndexPage {
        entries: vec![SegmentIndexEntry {
            namespace: SEGMENT_NAMESPACE_CONTENT,
            kind: 0,
            digest: [1; 32],
            offset: SEGMENT_PREFIX_ENCODED_SIZE as u64,
            codec,
            stored_length,
            logical_length,
            delta_base: None,
            delta_depth: 0,
        }],
    };
    assert_eq!(
        encode_segment_index_page(&entry(5, 1, 1)),
        Err(SegmentRecordError::InvalidCodec(5))
    );
    // Content records are never physical deltas.
    assert_eq!(
        encode_segment_index_page(&entry(2, 1, 1)),
        Err(SegmentRecordError::InvalidDelta)
    );
    assert_eq!(
        encode_segment_index_page(&entry(0, 1, 2)),
        Err(SegmentRecordError::InvalidRawLengths)
    );
    let zstd = entry(1, 10, 10_000);
    assert_eq!(
        decode_segment_index_page(&encode_segment_index_page(&zstd).unwrap()).unwrap(),
        zstd
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
            codec: 0,
            stored_length: 0,
            logical_length: 0,
            delta_base: None,
            delta_depth: 0,
        }],
    };
    assert!(matches!(
        encode_segment_index_page(&invalid),
        Err(SegmentRecordError::InvalidNamespaceKind { .. })
    ));
}

#[test]
fn delta_entries_round_trip_and_their_fields_are_validated() {
    let tree = SegmentIndexEntry::canonical_kind_code(CanonicalObjectKind::Tree);
    let entry = |codec, delta_base, delta_depth| SegmentIndexEntry {
        namespace: SEGMENT_NAMESPACE_METADATA,
        kind: tree,
        digest: [1; 32],
        offset: SEGMENT_PREFIX_ENCODED_SIZE as u64,
        codec,
        stored_length: 12,
        logical_length: 4_000,
        delta_base,
        delta_depth,
    };
    let page = |entry| SegmentIndexPage {
        entries: vec![entry],
    };

    let whole = page(entry(1, None, 0));
    let whole_bytes = encode_segment_index_page(&whole).unwrap();
    assert_eq!(whole_bytes.len(), 4 + SEGMENT_INDEX_ENTRY_ENCODED_SIZE);
    assert_eq!(decode_segment_index_page(&whole_bytes).unwrap(), whole);
    for codec in [2, 3] {
        for depth in [1, SEGMENT_MAX_DELTA_DEPTH] {
            let delta = page(entry(codec, Some([2; 32]), depth));
            let bytes = encode_segment_index_page(&delta).unwrap();
            assert_eq!(bytes.len(), 4 + SEGMENT_DELTA_INDEX_ENTRY_ENCODED_SIZE);
            assert_eq!(decode_segment_index_page(&bytes).unwrap(), delta);
        }
    }

    for invalid in [
        entry(2, None, 1),
        entry(2, Some([2; 32]), 0),
        entry(3, Some([2; 32]), SEGMENT_MAX_DELTA_DEPTH + 1),
        entry(2, Some([1; 32]), 1),
        entry(1, Some([2; 32]), 0),
        entry(4, None, 1),
    ] {
        assert_eq!(
            encode_segment_index_page(&page(invalid)),
            Err(SegmentRecordError::InvalidDelta)
        );
    }

    // A decoded page must be rejected the same way: flip a valid whole
    // entry's depth byte (the final byte) to one.
    let mut forged = whole_bytes;
    *forged.last_mut().unwrap() = 1;
    assert_eq!(
        decode_segment_index_page(&forged),
        Err(SegmentRecordError::InvalidDelta)
    );
}

#[test]
fn catalog_record_round_trips_and_is_canonical() {
    for previous in [None, Some([9; 32])] {
        let catalog = SegmentCatalogRecord {
            generation: 7,
            previous,
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
}

/// The remote chain record and the local snapshot share a directory layout
/// (`catalog/{generation}-{digest}.catalog`), so neither may decode as the other.
#[test]
fn catalog_record_and_physical_snapshot_are_not_interchangeable() {
    let record = encode_segment_catalog_record(&SegmentCatalogRecord {
        generation: 1,
        previous: None,
        segment: [1; 32],
        stored_length: 10,
    })
    .unwrap();
    assert!(decode_physical_catalog_snapshot(&record).is_err());
    let snapshot = encode_physical_catalog_snapshot(&PhysicalCatalogSnapshot {
        generation: 1,
        segments: Vec::new(),
        content: PhysicalContentGeneration {
            generation: 0,
            manifest: [0; 32],
        },
    })
    .unwrap();
    assert!(decode_segment_catalog_record(&snapshot).is_err());
}
