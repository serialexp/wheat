//! Remote content backend — stores content on S3 as individual keys.

use std::sync::Arc;

use object_store::{ObjectStore as BackendObjectStore, ObjectStoreExt, path::Path};
use wheat_core::ContentId;

use crate::content_store::ContentBackend;
use crate::ContentStoreError;

/// S3-backed content storage with one key per ContentId.
///
/// Layout: `{prefix}/content/{first2hex}/{resthex}`
pub(super) struct RemoteContentBackend {
    backend: Arc<dyn BackendObjectStore>,
    prefix: String,
}

impl std::fmt::Debug for RemoteContentBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteContentBackend")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl RemoteContentBackend {
    pub(super) fn new(backend: Arc<dyn BackendObjectStore>, prefix: String) -> Self {
        Self { backend, prefix }
    }

    fn path(&self, id: ContentId) -> Path {
        let hex = hex::encode(id.as_bytes());
        Path::from(format!("{}/content/{}/{}", self.prefix, &hex[..2], &hex[2..]))
    }
}

impl ContentBackend for RemoteContentBackend {
    fn put_many(&self, contents: &[(ContentId, Vec<u8>)]) -> Result<(), ContentStoreError> {
        // ContentBackend is synchronous, but the S3 backend is async.
        // Use a blocking runtime handle to bridge.
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|e| ContentStoreError::Io(std::io::Error::other(e)))?;
        for (id, bytes) in contents {
            let actual = ContentId::of(bytes);
            if actual != *id {
                return Err(ContentStoreError::Corrupt {
                    expected: *id,
                    actual,
                });
            }
            let path = self.path(*id);
            let payload = object_store::PutPayload::from(bytes.clone());
            // Use block_in_place to avoid nested runtime issues.
            let result = tokio::task::block_in_place(|| rt.block_on(self.backend.put(&path, payload)));
            match result {
                Ok(_) => {}
                Err(e) => return Err(ContentStoreError::Io(std::io::Error::other(e))),
            }
        }
        Ok(())
    }

    fn get_bounded(&self, id: ContentId, max: usize) -> Result<Vec<u8>, ContentStoreError> {
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|e| ContentStoreError::Io(std::io::Error::other(e)))?;
        let path = self.path(id);
        let bytes = tokio::task::block_in_place(|| {
            rt.block_on(async {
                let result = self.backend.get(&path).await.map_err(|e| match e {
                    object_store::Error::NotFound { .. } => ContentStoreError::Missing(id),
                    e => ContentStoreError::Io(std::io::Error::other(e)),
                })?;
                let bytes = result
                    .bytes()
                    .await
                    .map_err(|e| ContentStoreError::Io(std::io::Error::other(e)))?;
                Ok::<_, ContentStoreError>(bytes.to_vec())
            })
        })?;

        if bytes.len() > max {
            return Err(ContentStoreError::TooLarge {
                id,
                max,
                minimum: bytes.len(),
            });
        }

        let actual = ContentId::of(&bytes);
        if actual != id {
            return Err(ContentStoreError::Corrupt {
                expected: id,
                actual,
            });
        }

        Ok(bytes)
    }
}
