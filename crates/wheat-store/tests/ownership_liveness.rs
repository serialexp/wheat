use std::cell::RefCell;

use wheat_core::{ContentId, ObjectId};
use wheat_store::{
    InventoryEntry, Owner, OwnershipError, OwnershipLimits, OwnershipManifest, SnapshotId,
    prove_union_liveness, prove_union_liveness_bounded, prove_union_liveness_with,
};

fn manifest(
    owner: Owner,
    objects: impl IntoIterator<Item = ObjectId>,
    contents: impl IntoIterator<Item = ContentId>,
) -> OwnershipManifest {
    OwnershipManifest {
        owner,
        objects: objects
            .into_iter()
            .map(|id| InventoryEntry { id, bytes: 10 })
            .collect(),
        contents: contents
            .into_iter()
            .map(|id| InventoryEntry { id, bytes: 20 })
            .collect(),
    }
}

#[test]
fn union_retains_shared_bytes_for_independent_repository_and_snapshot_owners() {
    let object_a = ObjectId::from_digest([1; 32]);
    let object_shared = ObjectId::from_digest([2; 32]);
    let object_b = ObjectId::from_digest([3; 32]);
    let content_a = ContentId::from_digest([1; 32]);
    let content_shared = ContentId::from_digest([2; 32]);
    let content_b = ContentId::from_digest([3; 32]);
    let repository = manifest(
        Owner::repository([3; 32]),
        [object_a, object_shared],
        [content_a, content_shared],
    );
    let snapshot = manifest(
        Owner::snapshot(SnapshotId([4; 32])),
        [object_shared, object_b],
        [content_shared, content_b],
    );
    let proof = prove_union_liveness([
        (&repository, repository.authenticated_id()),
        (&snapshot, snapshot.authenticated_id()),
    ])
    .unwrap();
    assert_eq!(
        proof.objects.into_iter().collect::<Vec<_>>(),
        [object_a, object_shared, object_b]
    );
    assert_eq!(
        proof.contents.into_iter().collect::<Vec<_>>(),
        [content_a, content_shared, content_b]
    );
}

#[test]
fn callback_proof_is_bounded_and_does_not_emit_before_complete_authority_validation() {
    let first = manifest(
        Owner::repository([5; 32]),
        [
            ObjectId::from_digest([1; 32]),
            ObjectId::from_digest([2; 32]),
        ],
        [ContentId::from_digest([1; 32])],
    );
    let corrupt = manifest(
        Owner::snapshot(SnapshotId([6; 32])),
        [ObjectId::from_digest([3; 32])],
        [ContentId::from_digest([2; 32])],
    );
    let runs = RefCell::new(Vec::new());
    let result = prove_union_liveness_with(
        [
            (&first, first.authenticated_id()),
            (&corrupt, wheat_store::OwnershipManifestId([0; 32])),
        ],
        OwnershipLimits::default(),
        |entries| {
            runs.borrow_mut().push(entries.len());
            Ok(())
        },
        |_| Ok(()),
    );
    assert_eq!(result, Err(OwnershipError::Authentication));
    assert!(runs.borrow().is_empty());

    let object_runs = RefCell::new(Vec::new());
    prove_union_liveness_with(
        [
            (&first, first.authenticated_id()),
            (&first, first.authenticated_id()),
        ],
        OwnershipLimits::default(),
        |entries| {
            object_runs
                .borrow_mut()
                .push(entries.iter().map(|entry| entry.id).collect::<Vec<_>>());
            Ok(())
        },
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(
        object_runs.borrow().len(),
        1,
        "duplicate authority is emitted once"
    );
}

#[test]
fn callback_proof_bounds_aggregate_work_including_cross_owner_duplicates() {
    let shared_object = ObjectId::from_digest([11; 32]);
    let shared_content = ContentId::from_digest([12; 32]);
    let first = manifest(
        Owner::repository([13; 32]),
        [shared_object],
        [shared_content],
    );
    let second = manifest(
        Owner::snapshot(SnapshotId([14; 32])),
        [shared_object],
        [shared_content],
    );
    let limits = OwnershipLimits {
        max_live_objects: 1,
        max_live_contents: 1,
        ..OwnershipLimits::default()
    };
    let calls = RefCell::new(0usize);
    let result = prove_union_liveness_with(
        [
            (&first, first.authenticated_id()),
            (&second, second.authenticated_id()),
        ],
        limits,
        |_| {
            *calls.borrow_mut() += 1;
            Ok(())
        },
        |_| Ok(()),
    );
    assert_eq!(result, Err(OwnershipError::LimitExceeded("live objects")));
    assert_eq!(*calls.borrow(), 0, "validation is atomic before callbacks");
}

#[test]
fn every_limit_and_ambiguity_fails_closed() {
    let first = manifest(
        Owner::repository([7; 32]),
        [
            ObjectId::from_digest([1; 32]),
            ObjectId::from_digest([2; 32]),
        ],
        [
            ContentId::from_digest([1; 32]),
            ContentId::from_digest([2; 32]),
        ],
    );
    let second = manifest(
        Owner::snapshot(SnapshotId([8; 32])),
        [
            ObjectId::from_digest([2; 32]),
            ObjectId::from_digest([3; 32]),
        ],
        [
            ContentId::from_digest([2; 32]),
            ContentId::from_digest([3; 32]),
        ],
    );
    let claims = [
        (&first, first.authenticated_id()),
        (&second, second.authenticated_id()),
    ];
    for (limits, expected) in [
        (
            OwnershipLimits {
                max_owners: 1,
                ..OwnershipLimits::default()
            },
            "owners",
        ),
        (
            OwnershipLimits {
                max_objects_per_owner: 1,
                ..OwnershipLimits::default()
            },
            "owner inventory",
        ),
        (
            OwnershipLimits {
                max_contents_per_owner: 1,
                ..OwnershipLimits::default()
            },
            "owner inventory",
        ),
        (
            OwnershipLimits {
                max_live_objects: 2,
                ..OwnershipLimits::default()
            },
            "live objects",
        ),
        (
            OwnershipLimits {
                max_live_contents: 2,
                ..OwnershipLimits::default()
            },
            "live contents",
        ),
    ] {
        assert_eq!(
            prove_union_liveness_bounded(claims, limits),
            Err(OwnershipError::LimitExceeded(expected))
        );
    }

    let conflicting = manifest(
        first.owner,
        [ObjectId::from_digest([9; 32])],
        [ContentId::from_digest([9; 32])],
    );
    assert_eq!(
        prove_union_liveness([
            (&first, first.authenticated_id()),
            (&conflicting, conflicting.authenticated_id()),
        ]),
        Err(OwnershipError::AmbiguousOwner)
    );

    let unordered = OwnershipManifest {
        owner: Owner::repository([10; 32]),
        objects: vec![
            InventoryEntry {
                id: ObjectId::from_digest([2; 32]),
                bytes: 1,
            },
            InventoryEntry {
                id: ObjectId::from_digest([1; 32]),
                bytes: 1,
            },
        ],
        contents: vec![],
    };
    assert_eq!(
        prove_union_liveness([(&unordered, unordered.authenticated_id())]),
        Err(OwnershipError::InvalidInventory)
    );
}
