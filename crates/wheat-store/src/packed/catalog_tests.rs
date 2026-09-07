use super::catalog::*;
use super::segment::{CONTENT_NAMESPACE, SegmentRecord};

#[test]
fn catalog_reopens_without_derived_index_and_rejects_concurrent_open() {
    let root = tempfile::tempdir().unwrap();
    let bytes = b"packed payload";
    let digest = *wheat_core::ContentId::of(bytes).as_bytes();
    {
        let mut store = PackedCatalog::open(root.path()).unwrap();
        assert!(matches!(
            PackedCatalog::open(root.path()),
            Err(CatalogError::Busy)
        ));
        store
            .put_records(&[SegmentRecord {
                namespace: CONTENT_NAMESPACE,
                kind: 0,
                digest,
                bytes,
            }])
            .unwrap();
        assert_eq!(
            store.get(CONTENT_NAMESPACE, digest).unwrap().unwrap().1,
            bytes
        );
    }
    let mut reopened = PackedCatalog::open(root.path()).unwrap();
    assert_eq!(
        reopened.get(CONTENT_NAMESPACE, digest).unwrap().unwrap().1,
        bytes
    );
}

#[test]
fn retry_after_segment_install_before_catalog_publication_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let bytes = b"retry payload";
    let digest = *wheat_core::ContentId::of(bytes).as_bytes();
    let record = SegmentRecord {
        namespace: CONTENT_NAMESPACE,
        kind: 0,
        digest,
        bytes,
    };
    {
        let mut store = PackedCatalog::open(root.path()).unwrap();
        store.put_records(&[record]).unwrap();
        let catalogs = root.path().join("catalog");
        let published = std::fs::read_dir(&catalogs)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::remove_file(published).unwrap();
        // Simulate a crash after the immutable segment rename but before catalog publication.
        drop(store);
    }
    let mut reopened = PackedCatalog::open(root.path()).unwrap();
    assert!(reopened.get(CONTENT_NAMESPACE, digest).unwrap().is_none());
    reopened.put_records(&[record]).unwrap();
    assert_eq!(
        reopened.get(CONTENT_NAMESPACE, digest).unwrap().unwrap().1,
        bytes
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("segments"))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("catalog"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn catalog_rejects_corruption_and_ignores_unpublished_files() {
    let root = tempfile::tempdir().unwrap();
    let bytes = b"payload";
    let digest = *wheat_core::ContentId::of(bytes).as_bytes();
    {
        let mut store = PackedCatalog::open(root.path()).unwrap();
        store
            .put_records(&[SegmentRecord {
                namespace: CONTENT_NAMESPACE,
                kind: 0,
                digest,
                bytes,
            }])
            .unwrap();
    }
    std::fs::write(root.path().join("segments/orphan.segment"), b"incomplete").unwrap();
    assert!(PackedCatalog::open(root.path()).is_ok());
    let catalog = std::fs::read_dir(root.path().join("catalog"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(catalog, b"corrupt").unwrap();
    assert!(PackedCatalog::open(root.path()).is_err());
}
