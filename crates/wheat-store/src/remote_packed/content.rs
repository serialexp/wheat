//! Remote content backend — stores content on S3 as individual keys.

use std::sync::Arc;

use futures::TryStreamExt;
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

        // Validate all identity claims up front.
        for (id, bytes) in contents {
            let actual = ContentId::of(bytes);
            if actual != *id {
                return Err(ContentStoreError::Corrupt {
                    expected: *id,
                    actual,
                });
            }
        }

        // Upload concurrently — each PUT is independent.
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                let futs: Vec<_> = contents
                    .iter()
                    .map(|(id, bytes)| {
                        let path = self.path(*id);
                        let payload = object_store::PutPayload::from(bytes.clone());
                        let backend = &self.backend;
                        async move {
                            backend
                                .put(&path, payload)
                                .await
                                .map_err(|e| ContentStoreError::Io(std::io::Error::other(e)))
                        }
                    })
                    .collect();
                let results = futures::future::join_all(futs).await;
                for result in results {
                    result?;
                }
                Ok(())
            })
        })
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

    fn get_many_bounded(
        &self,
        ids: &[ContentId],
        max: usize,
    ) -> Option<Result<Vec<(ContentId, Vec<u8>)>, ContentStoreError>> {
        let rt = match tokio::runtime::Handle::try_current() {
            Ok(rt) => rt,
            Err(e) => return Some(Err(ContentStoreError::Io(std::io::Error::other(e)))),
        };
        Some(tokio::task::block_in_place(|| {
            rt.block_on(async {
                let futs: Vec<_> = ids
                    .iter()
                    .map(|&id| {
                        let path = self.path(id);
                        let backend = &self.backend;
                        async move {
                            let result = backend.get(&path).await.map_err(|e| match e {
                                object_store::Error::NotFound { .. } => {
                                    ContentStoreError::Missing(id)
                                }
                                e => ContentStoreError::Io(std::io::Error::other(e)),
                            })?;
                            let bytes = result
                                .bytes()
                                .await
                                .map_err(|e| ContentStoreError::Io(std::io::Error::other(e)))?;
                            let bytes = bytes.to_vec();

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

                            Ok((id, bytes))
                        }
                    })
                    .collect();
                let results = futures::future::join_all(futs).await;
                let mut out = Vec::with_capacity(results.len());
                for result in results {
                    out.push(result?);
                }
                Ok(out)
            })
        }))
    }

    fn list_ids(&self) -> Option<Result<Vec<ContentId>, ContentStoreError>> {
        let rt = match tokio::runtime::Handle::try_current() {
            Ok(rt) => rt,
            Err(e) => return Some(Err(ContentStoreError::Io(std::io::Error::other(e)))),
        };
        let prefix = Path::from(format!("{}/content", self.prefix));
        Some(tokio::task::block_in_place(|| {
            rt.block_on(async {
                let mut ids = Vec::new();
                let mut stream = self.backend.list(Some(&prefix));
                while let Some(meta) = stream
                    .try_next()
                    .await
                    .map_err(|e| ContentStoreError::Io(std::io::Error::other(e)))?
                {
                    // Path is {prefix}/content/{first2hex}/{resthex}
                    // Extract the two parts and reconstruct the full hex digest.
                    let path_str = meta.location.to_string();
                    let content_suffix = path_str
                        .strip_prefix(&format!("{}/content/", self.prefix));
                    if let Some(suffix) = content_suffix {
                        // suffix is "{first2hex}/{resthex}"
                        let hex: String = suffix.chars().filter(|c| *c != '/').collect();
                        if hex.len() == 64
                            && let Ok(bytes) = hex::decode(&hex)
                            && let Ok(digest) = <[u8; 32]>::try_from(bytes.as_slice())
                        {
                            ids.push(ContentId::from_digest(digest));
                        }
                    }
                }
                Ok(ids)
            })
        }))
    }
}
