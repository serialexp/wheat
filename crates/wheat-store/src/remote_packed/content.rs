//! Content on S3, as one key per ContentId.

use std::{io, ops::Range, sync::Arc};

use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use object_store::{ObjectStore as BackendObjectStore, ObjectStoreExt, path::Path};
use wheat_core::ContentId;

use crate::{BlobPage, BlobRead, ContentBlobStore};

/// Content records in an object store.
///
/// Layout: `{prefix}/content/{first2hex}/{resthex}`
pub(super) struct ObjectStoreBlobs {
    backend: Arc<dyn BackendObjectStore>,
    prefix: String,
}

impl std::fmt::Debug for ObjectStoreBlobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectStoreBlobs")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl ObjectStoreBlobs {
    pub(super) fn new(backend: Arc<dyn BackendObjectStore>, prefix: String) -> Self {
        Self { backend, prefix }
    }

    fn directory(&self) -> String {
        format!("{}/content", self.prefix)
    }

    fn path(&self, id: ContentId) -> Path {
        let hex = hex::encode(id.as_bytes());
        Path::from(format!("{}/{}/{}", self.directory(), &hex[..2], &hex[2..]))
    }

    /// The ID a listed key names, if it names one.
    fn id_at(&self, location: &Path) -> Option<ContentId> {
        let suffix = location
            .as_ref()
            .strip_prefix(&self.directory())?
            .strip_prefix('/')?;
        let (first, rest) = suffix.split_once('/')?;
        let mut digest = [0_u8; 32];
        hex::decode_to_slice(format!("{first}{rest}"), &mut digest).ok()?;
        Some(ContentId::from_digest(digest))
    }
}

/// `None` for a key that isn't there, the error otherwise.
fn found<T>(result: object_store::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(error) => Err(io::Error::other(error)),
    }
}

#[async_trait]
impl ContentBlobStore for ObjectStoreBlobs {
    async fn put(&self, id: ContentId, record: Vec<u8>) -> io::Result<()> {
        self.backend
            .put(&self.path(id), object_store::PutPayload::from(record))
            .await
            .map(|_| ())
            .map_err(io::Error::other)
    }

    async fn get(&self, id: ContentId) -> io::Result<Option<Vec<u8>>> {
        let Some(result) = found(self.backend.get(&self.path(id)).await)? else {
            return Ok(None);
        };
        let bytes = result.bytes().await.map_err(io::Error::other)?;
        Ok(Some(Vec::from(bytes)))
    }

    async fn get_range(&self, id: ContentId, range: Range<u64>) -> io::Result<Option<BlobRead>> {
        let options = object_store::GetOptions::new().with_range(Some(range));
        let Some(result) = found(self.backend.get_opts(&self.path(id), options).await)? else {
            return Ok(None);
        };
        let size = result.meta.size;
        let bytes = result.bytes().await.map_err(io::Error::other)?;
        Ok(Some(BlobRead {
            bytes: Vec::from(bytes),
            size,
        }))
    }

    async fn list_page(
        &self,
        cursor: Option<String>,
        limit: usize,
    ) -> io::Result<Option<BlobPage>> {
        let directory = Path::from(self.directory());
        let listing = match &cursor {
            Some(after) => self
                .backend
                .list_with_offset(Some(&directory), &Path::from(after.as_str())),
            None => self.backend.list(Some(&directory)),
        };
        let listed = listing
            .take(limit)
            .try_collect::<Vec<_>>()
            .await
            .map_err(io::Error::other)?;
        let next = (listed.len() == limit)
            .then(|| listed.last().map(|meta| meta.location.to_string()))
            .flatten();
        Ok(Some(BlobPage {
            ids: listed
                .iter()
                .filter_map(|meta| self.id_at(&meta.location))
                .collect(),
            next,
        }))
    }
}

#[cfg(test)]
mod tests {
    use object_store::{PutPayload, memory::InMemory};

    use super::*;
    use crate::{ContentStore, ContentStoreError, codec};

    struct Fixture {
        backend: Arc<InMemory>,
        blobs: Arc<ObjectStoreBlobs>,
        store: ContentStore,
    }

    fn fixture() -> Fixture {
        let backend = Arc::new(InMemory::new());
        let blobs = Arc::new(ObjectStoreBlobs::new(
            backend.clone(),
            "test/remote-packed".to_owned(),
        ));
        Fixture {
            backend,
            store: ContentStore::over_blobs(blobs.clone()),
            blobs,
        }
    }

    impl Fixture {
        async fn store_record(&self, id: ContentId, record: Vec<u8>) {
            self.backend
                .put(&self.blobs.path(id), PutPayload::from(record))
                .await
                .unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reads_check_the_bound_and_the_exact_record_size() {
        let fixture = fixture();
        let logical = vec![b'x'; 4096];
        let id = fixture.store.put(&logical).unwrap();
        assert!(matches!(
            fixture.store.get_bounded(id, logical.len() - 1),
            Err(ContentStoreError::TooLarge { .. })
        ));
        assert_eq!(
            fixture.store.get_bounded(id, logical.len()).unwrap(),
            logical
        );

        let encoded = codec::encode_envelope(&logical).unwrap();
        let mut trailing = encoded.clone();
        trailing.push(0);
        fixture.store_record(id, trailing).await;
        assert!(matches!(
            fixture.store.get(id),
            Err(ContentStoreError::InvalidFormat { .. })
        ));

        fixture
            .store_record(id, encoded[..encoded.len() - 1].to_vec())
            .await;
        assert!(matches!(
            fixture.store.get(id),
            Err(ContentStoreError::InvalidFormat { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_length_comes_from_the_header_and_checks_the_record_size() {
        let fixture = fixture();
        let logical = vec![b'y'; 4096];
        let id = ContentId::of(&logical);
        assert!(matches!(
            fixture.store.logical_length(id),
            Err(ContentStoreError::Missing(actual)) if actual == id
        ));

        let encoded = codec::encode_envelope(&logical).unwrap();
        fixture.store_record(id, encoded.clone()).await;
        assert_eq!(fixture.store.logical_length(id).unwrap(), 4096);
        let header = fixture
            .blobs
            .get_range(id, 0..codec::ENVELOPE_HEADER_LEN as u64)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(header.bytes.len(), codec::ENVELOPE_HEADER_LEN);
        assert_eq!(header.size, encoded.len() as u64);

        let mut trailing = encoded.clone();
        trailing.push(0);
        fixture.store_record(id, trailing).await;
        assert!(matches!(
            fixture.store.logical_length(id),
            Err(ContentStoreError::InvalidFormat { .. })
        ));

        fixture.store_record(id, encoded[..10].to_vec()).await;
        assert!(matches!(
            fixture.store.logical_length(id),
            Err(ContentStoreError::InvalidFormat { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_forged_huge_logical_length_is_refused() {
        let fixture = fixture();
        let id = ContentId::of(b"x");
        let mut encoded = codec::encode_envelope(b"x").unwrap();
        let header_length = codec::ENVELOPE_HEADER_LEN;
        let mut header = codec::parse_envelope_header(&encoded[..header_length]).unwrap();
        header.logical_length = u64::MAX;
        encoded[..header_length].copy_from_slice(&codec::encode_envelope_header(header).unwrap());
        fixture.store_record(id, encoded).await;
        assert!(matches!(
            fixture.store.get(id),
            Err(ContentStoreError::TooLarge { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn listing_pages_through_every_stored_id() {
        let fixture = fixture();
        let ids = (0..5_u8)
            .map(|byte| fixture.store.put(&[byte]).unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        // A key outside the content layout is skipped.
        fixture
            .backend
            .put(
                &Path::from("test/remote-packed/content/zz/not-an-id"),
                PutPayload::from(vec![0]),
            )
            .await
            .unwrap();

        let mut listed = std::collections::BTreeSet::new();
        let mut cursor = None;
        loop {
            let page = fixture.blobs.list_page(cursor, 2).await.unwrap().unwrap();
            assert!(page.ids.len() <= 2);
            listed.extend(page.ids);
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(listed, ids);
        assert_eq!(
            fixture
                .store
                .existing_ids(&ids.iter().copied().collect::<Vec<_>>())
                .unwrap(),
            ids.into_iter().collect()
        );
    }
}
