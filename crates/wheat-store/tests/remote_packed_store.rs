//! Integration tests for `RemotePackedStore` against an S3-compatible backend.
//!
//! Run with:
//!   scripts/wheat-seaweedfs-test   (bare SeaweedFS, no latency)
//! Or set WHEAT_TEST_S3_* env vars and:
//!   cargo test -p wheat-store --test remote_packed_store -- --ignored --nocapture

use wheat_core::{DirectoryEntry, DirectoryNode, ObjectId, TreeId};
use wheat_store::{
    ImmutableObjectStore, RemotePackedStore, RemotePackedStoreConfig, S3ObjectStoreConfig,
};

fn s3_config() -> Option<RemotePackedStoreConfig> {
    let endpoint = std::env::var("WHEAT_TEST_S3_ENDPOINT").ok()?;
    let bucket = std::env::var("WHEAT_TEST_S3_BUCKET").ok()?;
    let region = std::env::var("WHEAT_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".into());
    let access_key_id = std::env::var("WHEAT_TEST_S3_ACCESS_KEY_ID").ok()?;
    let secret_access_key = std::env::var("WHEAT_TEST_S3_SECRET_ACCESS_KEY").ok()?;
    Some(RemotePackedStoreConfig {
        s3: S3ObjectStoreConfig {
            endpoint: endpoint.clone(),
            bucket,
            region,
            access_key_id,
            secret_access_key,
            prefix: format!("wheat-test-{}", std::process::id()),
            allow_http: endpoint.starts_with("http://"),
        },
        segment_cache_bytes: Some(64 * 1024 * 1024),
    })
}

fn make_objects() -> Vec<(ObjectId, Vec<u8>)> {
    let mut objects = Vec::new();
    // A DirectoryNode is a valid canonical object.
    for i in 0u8..20 {
        let mut node = DirectoryNode::default();
        // Vary the content to get distinct ObjectIds.
        node.entries.insert(
            format!("child-{i}"),
            DirectoryEntry::Directory(TreeId::from_digest([i; 32])),
        );
        let bytes = node.canonical_bytes();
        let id = ObjectId::of_canonical_bytes(&bytes);
        objects.push((id, bytes));
    }
    objects
}

#[tokio::test]
#[ignore = "requires WHEAT_TEST_S3_* env vars (e.g. via scripts/wheat-seaweedfs-test)"]
async fn put_seal_get_round_trip() {
    let Some(config) = s3_config() else {
        eprintln!("skipping: WHEAT_TEST_S3_* not configured");
        return;
    };

    let store = RemotePackedStore::open(&config).await.unwrap();
    let objects = make_objects();

    // Put all objects.
    for (id, bytes) in &objects {
        store.put_if_absent(*id, bytes).await.unwrap();
    }

    // Before seal: objects should be in the pending buffer.
    for (id, _) in &objects {
        let exists = store.exists_many(&[*id]).await.unwrap();
        assert!(exists[0], "object should exist in pending buffer");
    }

    // Seal — builds segment, uploads to S3, updates catalog.
    store.seal().await.unwrap();

    // After seal: objects should be retrievable from the sealed segment.
    for (id, expected_bytes) in &objects {
        let stored = store.get(*id).await.unwrap();
        assert_eq!(stored.id, *id);
        assert_eq!(&*stored.canonical_bytes, expected_bytes);
    }

    eprintln!("put_seal_get_round_trip: {} objects stored and retrieved", objects.len());
}

#[tokio::test]
#[ignore = "requires WHEAT_TEST_S3_* env vars (e.g. via scripts/wheat-seaweedfs-test)"]
async fn catalog_chain_survives_reopen() {
    let Some(config) = s3_config() else {
        eprintln!("skipping: WHEAT_TEST_S3_* not configured");
        return;
    };

    let objects = make_objects();

    // Open, put, seal, close.
    {
        let store = RemotePackedStore::open(&config).await.unwrap();
        for (id, bytes) in &objects {
            store.put_if_absent(*id, bytes).await.unwrap();
        }
        store.seal().await.unwrap();
    }

    // Re-open from the same S3 prefix.
    {
        let store2 = RemotePackedStore::open(&config).await.unwrap();
        for (id, expected_bytes) in &objects {
            let stored = store2.get(*id).await.unwrap();
            assert_eq!(stored.id, *id);
            assert_eq!(&*stored.canonical_bytes, expected_bytes);
        }
    }

    eprintln!("catalog_chain_survives_reopen: {} objects survived re-open", objects.len());
}

#[tokio::test]
#[ignore = "requires WHEAT_TEST_S3_* env vars (e.g. via scripts/wheat-seaweedfs-test)"]
async fn content_round_trip() {
    let Some(config) = s3_config() else {
        eprintln!("skipping: WHEAT_TEST_S3_* not configured");
        return;
    };

    let store = RemotePackedStore::open(&config).await.unwrap();
    let content_store = store.content_store();

    let data = b"hello wheat packed segments";
    let id = content_store.put(data).unwrap();
    let retrieved = content_store.get(id).unwrap();
    assert_eq!(retrieved, data);

    eprintln!("content_round_trip: content stored and retrieved via S3");
}
