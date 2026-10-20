use wheat_core::*;

fn inventory(ids: &[u8]) -> Vec<InventoryRecord> {
    ids.iter()
        .map(|&id| InventoryRecord {
            id: [id; 32],
            bytes: u64::from(id) * 100,
        })
        .collect()
}

fn receipt(seed: u8) -> PublishReceiptRecord {
    PublishReceiptRecord {
        key: [seed; 16],
        request: [seed.wrapping_add(1); 16],
        receipt: [seed.wrapping_add(2); 16],
        generation: u64::from(seed) << 33,
        envelope: RepositoryEnvelopeId::from_digest([seed.wrapping_add(3); 32]),
        recorded_at: 1_800_000_000 + u64::from(seed),
    }
}

#[test]
fn repository_head_round_trips_at_its_fixed_size() {
    let head = RepositoryHeadRecord {
        generation: 1 << 40,
        envelope: RepositoryEnvelopeId::from_digest([6; 32]),
        receipts: Vec::new(),
    };
    let mut bytes = encode_repository_head(&head).unwrap();
    assert_eq!(bytes.len(), REPOSITORY_HEAD_ENCODED_SIZE);
    assert_eq!(decode_repository_head(&bytes).unwrap(), head);
    bytes.push(0);
    assert_eq!(
        decode_repository_head(&bytes),
        Err(StoreRecordError::NonCanonicalEncoding)
    );
    assert!(decode_repository_head(&[0; REPOSITORY_HEAD_ENCODED_SIZE]).is_err());
}

#[test]
fn repository_head_round_trips_with_receipts_in_order() {
    let head = RepositoryHeadRecord {
        generation: 7,
        envelope: RepositoryEnvelopeId::from_digest([6; 32]),
        receipts: vec![receipt(9), receipt(1), receipt(5)],
    };
    let mut bytes = encode_repository_head(&head).unwrap();
    assert_eq!(
        bytes.len(),
        REPOSITORY_HEAD_ENCODED_SIZE + 3 * PUBLISH_RECEIPT_ENCODED_SIZE
    );
    assert_eq!(decode_repository_head(&bytes).unwrap(), head);
    // A count that disagrees with the bytes after it does not decode.
    bytes.truncate(bytes.len() - PUBLISH_RECEIPT_ENCODED_SIZE);
    assert!(decode_repository_head(&bytes).is_err());
}

#[test]
fn every_field_of_a_publish_digest_input_changes_the_digest() {
    let key = |principal: &[u8], key| publish_key_digest(principal, key).unwrap();
    assert_ne!(key(b"alice", [1; 16]), key(b"bob", [1; 16]));
    assert_ne!(key(b"alice", [1; 16]), key(b"alice", [2; 16]));

    let envelope = |seed| RepositoryEnvelopeId::from_digest([seed; 32]);
    let request = publish_request_digest(envelope(1), 2, envelope(3), false);
    for other in [
        publish_request_digest(envelope(9), 2, envelope(3), false),
        publish_request_digest(envelope(1), 9, envelope(3), false),
        publish_request_digest(envelope(1), 2, envelope(9), false),
        publish_request_digest(envelope(1), 2, envelope(3), true),
    ] {
        assert_ne!(request, other);
    }

    let receipt = publish_receipt_id([1; 16], [2; 16], 3, envelope(4));
    for other in [
        publish_receipt_id([9; 16], [2; 16], 3, envelope(4)),
        publish_receipt_id([1; 16], [9; 16], 3, envelope(4)),
        publish_receipt_id([1; 16], [2; 16], 9, envelope(4)),
        publish_receipt_id([1; 16], [2; 16], 3, envelope(9)),
    ] {
        assert_ne!(receipt, other);
    }
    // The three are hashed under different tags.
    assert_ne!(key(b"", [1; 16]), receipt);
}

#[test]
fn repository_head_holds_at_most_max_publish_receipts() {
    let mut head = RepositoryHeadRecord {
        generation: 7,
        envelope: RepositoryEnvelopeId::from_digest([6; 32]),
        receipts: (0..MAX_PUBLISH_RECEIPTS)
            .map(|i| receipt(i as u8))
            .collect(),
    };
    let full = encode_repository_head(&head).unwrap();
    assert_eq!(decode_repository_head(&full).unwrap(), head);
    head.receipts.push(receipt(0));
    assert_eq!(
        encode_repository_head(&head),
        Err(StoreRecordError::InvalidCount)
    );
    // A forged count past the limit is refused before decoding.
    let mut forged = full;
    forged.extend_from_slice(&[0; PUBLISH_RECEIPT_ENCODED_SIZE]);
    assert_eq!(
        decode_repository_head(&forged),
        Err(StoreRecordError::InvalidCount)
    );
}

#[test]
fn content_envelope_header_round_trips_and_leaves_the_codec_to_the_caller() {
    let header = ContentEnvelopeHeaderRecord {
        codec: 9,
        stored_length: 12,
        logical_length: 1 << 33,
    };
    let bytes = encode_content_envelope_header(&header).unwrap();
    assert_eq!(bytes.len(), CONTENT_ENVELOPE_HEADER_ENCODED_SIZE);
    assert_eq!(&bytes[..4], b"WHCE");
    assert_eq!(decode_content_envelope_header(&bytes).unwrap(), header);
    assert!(decode_content_envelope_header(&bytes[..bytes.len() - 1]).is_err());
    let mut wrong_version = bytes.clone();
    wrong_version[4] = 2;
    assert!(decode_content_envelope_header(&wrong_version).is_err());
}

#[test]
fn snapshot_manifest_round_trips_and_bounds_its_counts_before_decoding() {
    let objects = inventory(&[1, 2, 3]);
    let contents = inventory(&[4, 5]);
    let encode = |objects: &[InventoryRecord], contents: &[InventoryRecord]| {
        encode_snapshot_manifest(
            7,
            [8; 32],
            [9; 32],
            objects.iter().copied(),
            contents.iter().copied(),
        )
        .unwrap()
    };
    let bytes = encode(&objects, &contents);
    assert_eq!(
        bytes.len(),
        SNAPSHOT_MANIFEST_FIXED_ENCODED_SIZE + 5 * INVENTORY_ENTRY_ENCODED_SIZE
    );
    assert_eq!(encode(&[], &[]).len(), SNAPSHOT_MANIFEST_FIXED_ENCODED_SIZE);
    let decoded = decode_snapshot_manifest(&bytes, 3, 2).unwrap();
    assert_eq!(
        decoded,
        SnapshotManifestRecord {
            head_generation: 7,
            head_envelope: [8; 32],
            envelope: [9; 32],
            objects,
            contents,
        }
    );
    // Either inventory over its limit is refused.
    assert_eq!(
        decode_snapshot_manifest(&bytes, 2, 2),
        Err(StoreRecordError::InvalidCount)
    );
    assert_eq!(
        decode_snapshot_manifest(&bytes, 3, 1),
        Err(StoreRecordError::InvalidCount)
    );
    // A forged count larger than the bytes can hold runs out of input.
    let mut forged = encode(&[], &[]);
    forged[78..82].copy_from_slice(&[0xff; 4]);
    assert!(matches!(
        decode_snapshot_manifest(&forged, usize::MAX, usize::MAX),
        Err(StoreRecordError::Decode(_))
    ));
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(decode_snapshot_manifest(&trailing, 3, 2).is_err());
    assert!(decode_snapshot_manifest(&bytes[..bytes.len() - 1], 3, 2).is_err());
}

#[test]
fn ownership_manifest_encoding_depends_on_every_field() {
    let base = || {
        encode_ownership_manifest(
            1,
            [2; 32],
            inventory(&[3]).into_iter(),
            inventory(&[4]).into_iter(),
        )
        .unwrap()
    };
    let encoded = base();
    assert_eq!(encoded, base());
    let variants = [
        encode_ownership_manifest(
            2,
            [2; 32],
            inventory(&[3]).into_iter(),
            inventory(&[4]).into_iter(),
        ),
        encode_ownership_manifest(
            1,
            [3; 32],
            inventory(&[3]).into_iter(),
            inventory(&[4]).into_iter(),
        ),
        // The same entry as an object or as content is a different manifest.
        encode_ownership_manifest(
            1,
            [2; 32],
            inventory(&[3, 4]).into_iter(),
            inventory(&[]).into_iter(),
        ),
        encode_ownership_manifest(
            1,
            [2; 32],
            inventory(&[3]).into_iter(),
            inventory(&[5]).into_iter(),
        ),
    ];
    for variant in variants {
        assert_ne!(variant.unwrap(), encoded);
    }
}

#[test]
fn materialization_progress_record_round_trips_and_its_checksum_trails_the_fields() {
    let fields = MaterializationProgressFields {
        opcode: 2,
        transaction: [7; 16],
        index: 40,
    };
    let record = encode_materialization_progress_record(&fields, 0xdead_beef).unwrap();
    assert_eq!(record.len(), MATERIALIZATION_PROGRESS_RECORD_ENCODED_SIZE);
    assert_eq!(
        decode_materialization_progress_record(&record).unwrap(),
        (fields, 0xdead_beef)
    );
    // The checksum covers exactly the encoded fields, which lead the record.
    let encoded_fields = encode_materialization_progress_fields(&fields).unwrap();
    assert!(record.starts_with(&encoded_fields));
    assert_eq!(record.len(), encoded_fields.len() + 4);
    assert!(
        decode_materialization_progress_record(&[0; MATERIALIZATION_PROGRESS_RECORD_ENCODED_SIZE])
            .is_err()
    );
    assert!(decode_materialization_progress_record(&record[..record.len() - 1]).is_err());
}

#[test]
fn git_import_source_frames_every_field() {
    let encode = |path: &[u8], head: &[u8], refs: &[(&[u8], &[u8])]| {
        encode_git_import_source(path, head, refs.iter().copied()).unwrap()
    };
    let base = encode(b"/repo", b"ref: main", &[(b"main", b"1234")]);
    assert_eq!(base, encode(b"/repo", b"ref: main", &[(b"main", b"1234")]));
    // Moving bytes between neighbouring fields changes the encoding.
    assert_ne!(base, encode(b"/repo", b"ref: mai", &[(b"nmain", b"1234")]));
    assert_ne!(base, encode(b"/rep", b"oref: main", &[(b"main", b"1234")]));
    assert_ne!(base, encode(b"/repo", b"ref: main", &[(b"mai", b"n1234")]));
    assert_ne!(
        base,
        encode(b"/repo", b"ref: main", &[(b"main", b"1234"), (b"", b"")])
    );
}

/// Every store record starts with its own magic, so none decodes as another.
#[test]
fn store_records_are_not_interchangeable() {
    let head = encode_repository_head(&RepositoryHeadRecord {
        generation: 1,
        envelope: RepositoryEnvelopeId::from_digest([1; 32]),
        receipts: Vec::new(),
    })
    .unwrap();
    let envelope = encode_content_envelope_header(&ContentEnvelopeHeaderRecord {
        codec: 0,
        stored_length: 0,
        logical_length: 0,
    })
    .unwrap();
    let progress = encode_materialization_progress_record(
        &MaterializationProgressFields {
            opcode: 1,
            transaction: [1; 16],
            index: 1,
        },
        0,
    )
    .unwrap();
    let snapshot =
        encode_snapshot_manifest(1, [1; 32], [1; 32], [].into_iter(), [].into_iter()).unwrap();
    let ownership = encode_ownership_manifest(1, [1; 32], [].into_iter(), [].into_iter()).unwrap();
    let source = encode_git_import_source(b"", b"", [].into_iter()).unwrap();
    let magics: Vec<&[u8]> = [&head, &envelope, &progress, &snapshot, &ownership, &source]
        .into_iter()
        .map(|record| &record[..4])
        .collect();
    for (index, magic) in magics.iter().enumerate() {
        assert!(!magics[index + 1..].contains(magic), "record {index}");
    }
    assert!(decode_repository_head(&envelope).is_err());
    assert!(decode_snapshot_manifest(&ownership, 0, 0).is_err());
}
