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
    let request = publish_request_digest(envelope(1), 2, envelope(3), false, false);
    for other in [
        publish_request_digest(envelope(9), 2, envelope(3), false, false),
        publish_request_digest(envelope(1), 9, envelope(3), false, false),
        publish_request_digest(envelope(1), 2, envelope(9), false, false),
        publish_request_digest(envelope(1), 2, envelope(3), true, false),
        publish_request_digest(envelope(1), 2, envelope(3), false, true),
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

fn id(seed: u8) -> uuid::Uuid {
    uuid::Uuid::from_bytes([seed; 16])
}

fn root(seed: u8) -> RepositoryRootRecord {
    RepositoryRootRecord {
        generation: u64::from(seed) << 20,
        envelope: RepositoryEnvelopeId::from_digest([seed; 32]),
    }
}

fn materialized(seed: u8) -> MaterializedRootsRecord {
    MaterializedRootsRecord {
        repository_head: root(seed),
        view: ViewId::from_digest([seed.wrapping_add(1); 32]),
        tree: TreeId::from_digest([seed.wrapping_add(2); 32]),
    }
}

fn cas(seed: u8) -> PendingRepositoryCasRecord {
    PendingRepositoryCasRecord {
        expected_head: root(seed),
        proposed_head: root(seed.wrapping_add(1)),
        resulting_frontier: ViewId::from_digest([seed.wrapping_add(2); 32]),
    }
}

fn workspace(full: bool) -> WorkspaceMetadataRecord {
    WorkspaceMetadataRecord {
        workspace_id: id(1),
        registration_token: id(2),
        store_path: "/stores/ünïcode store".into(),
        store_id: id(3),
        active_label: "main".into(),
        basis: ViewId::from_digest([4; 32]),
        observed_label_binding: ViewId::from_digest([5; 32]),
        selected_frontier: full.then(|| ViewId::from_digest([6; 32])),
        materialized: full.then(|| materialized(7)),
        selected_change: full.then(|| ChangeId::from_bytes([8; 16])),
        pending_repository_operation: full.then(|| PendingRepositoryOperationRecord {
            cas: cas(9),
            selected_change: Some(ChangeId::from_bytes([10; 16])),
            rebinds: true,
        }),
    }
}

fn pin(full: bool) -> WorkspacePinRecord {
    WorkspacePinRecord {
        store_id: id(1),
        workspace_id: id(2),
        registration_token: id(3),
        workspace_path: "/work/space".into(),
        basis: ViewId::from_digest([4; 32]),
        observed_label_binding: ViewId::from_digest([5; 32]),
        selected_frontier: full.then(|| ViewId::from_digest([6; 32])),
        selected_change: full.then(|| ChangeId::from_bytes([7; 16])),
        materialized: full.then(|| materialized(8)),
        pending_materialization: full.then(|| materialized(9)),
        pending_repository_cas: full.then(|| cas(10)),
    }
}

/// `bytes` with one more byte, and with the version after the four-byte magic
/// raised to 2.
fn tampered(bytes: &[u8]) -> [Vec<u8>; 2] {
    let mut trailing = bytes.to_vec();
    trailing.push(0);
    let mut version = bytes.to_vec();
    version[5] = 2;
    [trailing, version]
}

#[test]
fn repository_location_round_trips_and_refuses_nil_identities() {
    let record = RepositoryLocationRecord {
        network: id(1),
        repository: id(2),
    };
    let bytes = encode_repository_location(record).unwrap();
    assert_eq!(bytes.len(), 4 + 2 + 16 + 16);
    assert_eq!(decode_repository_location(&bytes).unwrap(), record);
    let [trailing, version] = tampered(&bytes);
    assert_eq!(
        decode_repository_location(&trailing),
        Err(StoreRecordError::NonCanonicalEncoding)
    );
    assert!(matches!(
        decode_repository_location(&version),
        Err(StoreRecordError::Decode(_))
    ));
    for (nil, field) in [
        (
            RepositoryLocationRecord {
                network: uuid::Uuid::nil(),
                ..record
            },
            "network ID",
        ),
        (
            RepositoryLocationRecord {
                repository: uuid::Uuid::nil(),
                ..record
            },
            "repository ID",
        ),
    ] {
        let bytes = encode_repository_location(nil).unwrap();
        assert_eq!(
            decode_repository_location(&bytes),
            Err(StoreRecordError::NilIdentity(field))
        );
    }
}

#[test]
fn store_metadata_round_trips_and_refuses_a_nil_store() {
    let record = StoreMetadataRecord { store_id: id(1) };
    let bytes = encode_store_metadata(record).unwrap();
    assert_eq!(decode_store_metadata(&bytes).unwrap(), record);
    let [trailing, version] = tampered(&bytes);
    assert_eq!(
        decode_store_metadata(&trailing),
        Err(StoreRecordError::NonCanonicalEncoding)
    );
    assert!(matches!(
        decode_store_metadata(&version),
        Err(StoreRecordError::Decode(_))
    ));
    let nil = encode_store_metadata(StoreMetadataRecord {
        store_id: uuid::Uuid::nil(),
    })
    .unwrap();
    assert_eq!(
        decode_store_metadata(&nil),
        Err(StoreRecordError::NilIdentity("store ID"))
    );
}

#[test]
fn workspace_metadata_round_trips_with_and_without_optional_roots() {
    for full in [false, true] {
        let record = workspace(full);
        let bytes = encode_workspace_metadata(&record).unwrap();
        assert_eq!(decode_workspace_metadata(&bytes).unwrap(), record);
        let [trailing, version] = tampered(&bytes);
        assert_eq!(
            decode_workspace_metadata(&trailing),
            Err(StoreRecordError::NonCanonicalEncoding)
        );
        assert!(decode_workspace_metadata(&version).is_err());
    }
    // A pending operation that leaves no change selected is its own state.
    let mut record = workspace(true);
    record.pending_repository_operation = Some(PendingRepositoryOperationRecord {
        cas: cas(1),
        selected_change: None,
        rebinds: false,
    });
    let bytes = encode_workspace_metadata(&record).unwrap();
    assert_eq!(decode_workspace_metadata(&bytes).unwrap(), record);
}

#[test]
fn workspace_metadata_refuses_nil_identities() {
    for (field, name) in [
        (0, "workspace ID"),
        (1, "registration token"),
        (2, "store ID"),
    ] {
        let mut record = workspace(false);
        match field {
            0 => record.workspace_id = uuid::Uuid::nil(),
            1 => record.registration_token = uuid::Uuid::nil(),
            _ => record.store_id = uuid::Uuid::nil(),
        }
        let bytes = encode_workspace_metadata(&record).unwrap();
        assert_eq!(
            decode_workspace_metadata(&bytes),
            Err(StoreRecordError::NilIdentity(name))
        );
    }
}

#[test]
fn workspace_pin_round_trips_and_refuses_nil_identities() {
    for full in [false, true] {
        let record = pin(full);
        let bytes = encode_workspace_pin(&record).unwrap();
        assert_eq!(decode_workspace_pin(&bytes).unwrap(), record);
        let [trailing, version] = tampered(&bytes);
        assert_eq!(
            decode_workspace_pin(&trailing),
            Err(StoreRecordError::NonCanonicalEncoding)
        );
        assert!(decode_workspace_pin(&version).is_err());
    }
    for (field, name) in [
        (0, "store ID"),
        (1, "workspace ID"),
        (2, "registration token"),
    ] {
        let mut record = pin(false);
        match field {
            0 => record.store_id = uuid::Uuid::nil(),
            1 => record.workspace_id = uuid::Uuid::nil(),
            _ => record.registration_token = uuid::Uuid::nil(),
        }
        let bytes = encode_workspace_pin(&record).unwrap();
        assert_eq!(
            decode_workspace_pin(&bytes),
            Err(StoreRecordError::NilIdentity(name))
        );
    }
}

#[test]
fn remote_config_round_trips_and_refuses_tampering() {
    let record = RemoteConfigRecord {
        display_url: "wheat://example.com/team/repo".into(),
        canonical_identity: "wheat://example.com:443/team/repo".into(),
        head: root(3),
    };
    let bytes = encode_remote_config(&record).unwrap();
    assert_eq!(decode_remote_config(&bytes).unwrap(), record);
    let [trailing, version] = tampered(&bytes);
    assert_eq!(
        decode_remote_config(&trailing),
        Err(StoreRecordError::NonCanonicalEncoding)
    );
    assert!(decode_remote_config(&version).is_err());
    // A string is UTF-8.
    let mut invalid = bytes.clone();
    let start = invalid
        .windows(5)
        .position(|window| window == b"wheat")
        .unwrap();
    invalid[start] = 0xff;
    assert!(decode_remote_config(&invalid).is_err());
}

#[test]
fn client_control_records_have_distinct_magics() {
    let records = [
        encode_store_metadata(StoreMetadataRecord { store_id: id(1) }).unwrap(),
        encode_workspace_metadata(&workspace(false)).unwrap(),
        encode_workspace_pin(&pin(false)).unwrap(),
        encode_remote_config(&RemoteConfigRecord {
            display_url: String::new(),
            canonical_identity: String::new(),
            head: root(1),
        })
        .unwrap(),
        encode_repository_head(&RepositoryHeadRecord {
            generation: 1,
            envelope: RepositoryEnvelopeId::from_digest([1; 32]),
            receipts: Vec::new(),
        })
        .unwrap(),
    ];
    for (index, record) in records.iter().enumerate() {
        for other in &records[index + 1..] {
            assert_ne!(record[..4], other[..4], "record {index}");
        }
    }
    assert!(decode_workspace_pin(&records[1]).is_err());
    assert!(decode_workspace_metadata(&records[2]).is_err());
}

#[test]
fn bundle_records_round_trip_and_refuse_noncanonical_bytes() {
    let manifest = BundleManifestRecord {
        network: id(1),
        repositories: vec![id(2), id(3)],
        files: vec![
            BundleFileRecord {
                path: "storage/packed/catalog.head".into(),
                length: 7,
                digest: [4; 32],
            },
            BundleFileRecord {
                path: "heads/ab/repository.head".into(),
                length: 48,
                digest: [5; 32],
            },
        ],
        content_count: 3,
        content_bytes: 600,
        content_chunks: vec![[6; 32], [7; 32]],
    };
    let bytes = encode_bundle_manifest(&manifest).unwrap();
    assert_eq!(decode_bundle_manifest(&bytes).unwrap(), manifest);
    let [trailing, version] = tampered(&bytes);
    assert_eq!(
        decode_bundle_manifest(&trailing),
        Err(StoreRecordError::NonCanonicalEncoding)
    );
    assert!(matches!(
        decode_bundle_manifest(&version),
        Err(StoreRecordError::Decode(_))
    ));
    let nil = BundleManifestRecord {
        repositories: vec![uuid::Uuid::nil()],
        ..manifest
    };
    assert_eq!(
        decode_bundle_manifest(&encode_bundle_manifest(&nil).unwrap()),
        Err(StoreRecordError::NilIdentity("repository ID"))
    );

    let contents = inventory(&[1, 2, 3]);
    let bytes = encode_bundle_content_chunk(contents.iter().copied()).unwrap();
    assert_eq!(decode_bundle_content_chunk(&bytes).unwrap(), contents);
    let [trailing, version] = tampered(&bytes);
    assert_eq!(
        decode_bundle_content_chunk(&trailing),
        Err(StoreRecordError::NonCanonicalEncoding)
    );
    assert!(matches!(
        decode_bundle_content_chunk(&version),
        Err(StoreRecordError::Decode(_))
    ));
}

const MIB: u32 = 1 << 20;

fn chunk_list(lengths: &[u32]) -> Vec<ContentChunkRecord> {
    lengths
        .iter()
        .enumerate()
        .map(|(index, &length)| ContentChunkRecord {
            id: ContentId::from_digest([index as u8; 32]),
            length,
        })
        .collect()
}

fn chunked(lengths: &[u32]) -> ContentChunkManifestRecord {
    ContentChunkManifestRecord {
        logical_length: lengths.iter().map(|&length| u64::from(length)).sum(),
        entries: ContentManifestEntries::Chunks(chunk_list(lengths)),
    }
}

fn parted(lengths: &[u64]) -> ContentChunkManifestRecord {
    ContentChunkManifestRecord {
        logical_length: lengths.iter().sum(),
        entries: ContentManifestEntries::Parts(
            lengths
                .iter()
                .enumerate()
                .map(|(index, &length)| ContentPartRecord {
                    id: ContentId::from_digest([index as u8; 32]),
                    length,
                })
                .collect(),
        ),
    }
}

/// The fewest bytes a part before a blob's last holds: the fewest chunks, each of
/// the shortest length.
const SHORTEST_PART: u64 = MIN_CONTENT_PART_CHUNKS as u64 * MIB as u64;

fn manifest_error(manifest: &ContentChunkManifestRecord) -> StoreRecordError {
    manifest.validate().unwrap_err()
}

fn invalid(manifest: ContentChunkManifestRecord) {
    let error = manifest_error(&manifest);
    assert!(
        matches!(error, StoreRecordError::InvalidChunkManifest(_)),
        "{error:?}"
    );
    assert_eq!(encode_content_chunk_manifest(&manifest), Err(error));
}

#[test]
fn chunk_manifest_round_trips_and_refuses_trailing_bytes_and_versions() {
    // Just over the threshold: four longest chunks and one byte.
    let manifest = chunked(&[4 * MIB, 4 * MIB, 4 * MIB, 4 * MIB, 1]);
    assert_eq!(manifest.logical_length, CHUNKED_CONTENT_THRESHOLD + 1);
    let bytes = encode_content_chunk_manifest(&manifest).unwrap();
    assert_eq!(
        bytes.len(),
        CONTENT_CHUNK_MANIFEST_FIXED_ENCODED_SIZE + 5 * CONTENT_CHUNK_ENCODED_SIZE
    );
    assert_eq!(decode_content_chunk_manifest(&bytes).unwrap(), manifest);

    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_eq!(
        decode_content_chunk_manifest(&trailing),
        Err(StoreRecordError::NonCanonicalEncoding)
    );
    let mut version = bytes.clone();
    version[4] = 2;
    assert!(matches!(
        decode_content_chunk_manifest(&version),
        Err(StoreRecordError::Decode(_))
    ));
}

#[test]
fn chunk_manifest_refuses_what_the_chunker_would_not_produce() {
    // Exactly the threshold is stored whole.
    invalid(chunked(&[4 * MIB, 4 * MIB, 4 * MIB, 4 * MIB]));
    // A short chunk before the last, and a long one.
    invalid(chunked(&[4 * MIB, 4 * MIB, 4 * MIB, MIB - 1, 4 * MIB]));
    invalid(chunked(&[4 * MIB, 4 * MIB, 4 * MIB, 4 * MIB + 1, 4 * MIB]));
    // An empty last chunk, and a long one.
    invalid(chunked(&[4 * MIB, 4 * MIB, 4 * MIB, 4 * MIB, MIB, 0]));
    invalid(chunked(&[4 * MIB, 4 * MIB, 4 * MIB, 4 * MIB, 4 * MIB + 1]));
    // Lengths that do not sum to the blob's.
    let mut short = chunked(&[4 * MIB, 4 * MIB, 4 * MIB, 4 * MIB, 1]);
    short.logical_length += 1;
    invalid(short);
    // No chunks, and no parts.
    invalid(ContentChunkManifestRecord {
        logical_length: CHUNKED_CONTENT_THRESHOLD + 1,
        entries: ContentManifestEntries::Chunks(Vec::new()),
    });
    invalid(ContentChunkManifestRecord {
        logical_length: CHUNKED_CONTENT_THRESHOLD + 1,
        entries: ContentManifestEntries::Parts(Vec::new()),
    });
    // Longer than the format takes.
    invalid(ContentChunkManifestRecord {
        logical_length: MAX_CHUNKED_CONTENT_BYTES + 1,
        entries: ContentManifestEntries::Chunks(Vec::new()),
    });
}

#[test]
fn chunk_manifest_decode_validates_what_it_reads() {
    let bytes =
        encode_content_chunk_manifest(&chunked(&[4 * MIB, 4 * MIB, 4 * MIB, 4 * MIB, 1])).unwrap();
    // The last chunk's length, just before the empty parts' four-byte count, set
    // to zero.
    let mut empty_last = bytes.clone();
    let end = empty_last.len() - 4;
    empty_last[end - 4..end].fill(0);
    assert!(matches!(
        decode_content_chunk_manifest(&empty_last),
        Err(StoreRecordError::InvalidChunkManifest(_))
    ));
    // The logical length, after the magic and version, set to zero.
    let mut empty = bytes;
    empty[5..13].fill(0);
    assert!(matches!(
        decode_content_chunk_manifest(&empty),
        Err(StoreRecordError::InvalidChunkManifest(_))
    ));
}

#[test]
fn chunk_manifest_lists_at_most_the_direct_limit_of_chunks() {
    let mut lengths = vec![MIB; MAX_DIRECT_CONTENT_CHUNKS];
    let manifest = chunked(&lengths);
    let bytes = encode_content_chunk_manifest(&manifest).unwrap();
    assert_eq!(
        bytes.len(),
        CONTENT_CHUNK_MANIFEST_FIXED_ENCODED_SIZE
            + MAX_DIRECT_CONTENT_CHUNKS * CONTENT_CHUNK_ENCODED_SIZE
    );
    assert!(bytes.len() <= MAX_CONTENT_CHUNK_MANIFEST_BYTES);
    assert_eq!(decode_content_chunk_manifest(&bytes).unwrap(), manifest);

    lengths.push(1);
    assert_eq!(
        manifest_error(&chunked(&lengths)),
        StoreRecordError::InvalidCount
    );
}

#[test]
fn chunk_manifest_takes_the_longest_blob_in_the_shortest_parts() {
    let manifest = parted(&vec![SHORTEST_PART; MAX_CONTENT_PARTS]);
    assert_eq!(manifest.logical_length, MAX_CHUNKED_CONTENT_BYTES);
    let bytes = encode_content_chunk_manifest(&manifest).unwrap();
    assert_eq!(
        bytes.len(),
        CONTENT_CHUNK_MANIFEST_FIXED_ENCODED_SIZE + MAX_CONTENT_PARTS * CONTENT_PART_ENCODED_SIZE
    );
    assert!(bytes.len() <= MAX_CONTENT_CHUNK_MANIFEST_BYTES);
    assert_eq!(decode_content_chunk_manifest(&bytes).unwrap(), manifest);

    // One part more is one byte more than the format takes.
    let mut longer = vec![SHORTEST_PART; MAX_CONTENT_PARTS];
    longer.push(1);
    invalid(parted(&longer));
}

#[test]
fn chunk_manifest_refuses_parts_a_long_blob_would_not_have() {
    // Every part but the last holds at least the fewest chunks of the shortest
    // length, and none more than the most chunks of the longest.
    parted(&[SHORTEST_PART, 1]).validate().unwrap();
    invalid(parted(&[SHORTEST_PART - 1, SHORTEST_PART]));
    invalid(parted(&[MAX_CONTENT_PART_BYTES + 1, 1]));
    invalid(parted(&[SHORTEST_PART, 0]));
    invalid(parted(&[SHORTEST_PART, MAX_CONTENT_PART_BYTES + 1]));
    let mut short = parted(&[SHORTEST_PART, 1]);
    short.logical_length += 1;
    invalid(short);
}

#[test]
fn chunk_manifest_refuses_both_chunks_and_parts() {
    let length = SHORTEST_PART;
    let direct = chunked(&vec![MIB; MIN_CONTENT_PART_CHUNKS]);
    assert_eq!(direct.logical_length, length);
    let direct = encode_content_chunk_manifest(&direct).unwrap();
    let parts = encode_content_chunk_manifest(&parted(&[length])).unwrap();
    // The direct manifest up to its empty parts' count, then the parted one's
    // parts: the same blob, listed both ways.
    let fixed = CONTENT_CHUNK_MANIFEST_FIXED_ENCODED_SIZE;
    let mut both = direct[..direct.len() - 4].to_vec();
    both.extend_from_slice(&parts[fixed - 4..]);
    assert_eq!(
        decode_content_chunk_manifest(&both),
        Err(StoreRecordError::InvalidChunkManifest(
            "it lists both chunks and parts"
        ))
    );
}

#[test]
fn chunk_part_round_trips_and_is_checked_against_its_manifest() {
    let part = ContentChunkPartRecord {
        chunks: chunk_list(&vec![MIB; MIN_CONTENT_PART_CHUNKS]),
    };
    let bytes = encode_content_chunk_part(&part).unwrap();
    assert_eq!(
        bytes.len(),
        CONTENT_CHUNK_PART_FIXED_ENCODED_SIZE
            + MIN_CONTENT_PART_CHUNKS * CONTENT_CHUNK_ENCODED_SIZE
    );
    assert_eq!(decode_content_chunk_part(&bytes).unwrap(), part);
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_eq!(
        decode_content_chunk_part(&trailing),
        Err(StoreRecordError::NonCanonicalEncoding)
    );

    part.validate(SHORTEST_PART, false).unwrap();
    part.validate(SHORTEST_PART, true).unwrap();
    // A part's chunks hold exactly the length its manifest names.
    assert!(part.validate(SHORTEST_PART + 1, false).is_err());

    // Too few chunks for any part but the last; the last may hold one.
    let one = ContentChunkPartRecord {
        chunks: chunk_list(&[1]),
    };
    assert!(one.validate(1, false).is_err());
    one.validate(1, true).unwrap();
    // Only the blob's last chunk is shorter than the shortest chunk.
    let mut short_end = vec![MIB; MIN_CONTENT_PART_CHUNKS];
    short_end[MIN_CONTENT_PART_CHUNKS - 1] = 1;
    let short_end = ContentChunkPartRecord {
        chunks: chunk_list(&short_end),
    };
    let length = SHORTEST_PART - u64::from(MIB) + 1;
    assert!(short_end.validate(length, false).is_err());
    short_end.validate(length, true).unwrap();
    // No part holds more than the most chunks.
    let many = ContentChunkPartRecord {
        chunks: chunk_list(&vec![MIB; MAX_CONTENT_PART_CHUNKS + 1]),
    };
    assert!(
        many.validate(u64::from(MIB) * many.chunks.len() as u64, true)
            .is_err()
    );
    assert_eq!(
        encode_content_chunk_part(&many),
        Err(StoreRecordError::InvalidCount)
    );
}
