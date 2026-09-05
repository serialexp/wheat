use std::sync::Arc;

use async_trait::async_trait;
use object_store::{
    ObjectStore as BackendObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload,
    aws::{AmazonS3Builder, S3ConditionalPut},
    path::Path,
};
use thiserror::Error;
use wheat_core::{ObjectId, validate_canonical_object};

use crate::{ImmutableObjectStore, ObjectStoreError, StoredObject};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct S3ObjectStoreConfig {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub prefix: String,
    pub allow_http: bool,
}

impl S3ObjectStoreConfig {
    #[must_use]
    pub fn for_endpoint(
        endpoint: impl Into<String>,
        bucket: impl Into<String>,
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
    ) -> Self {
        let endpoint = endpoint.into();
        Self {
            allow_http: endpoint.starts_with("http://"),
            endpoint,
            bucket: bucket.into(),
            region: "us-east-1".into(),
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            prefix: "wheat/objects".into(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ObjectStoreAdapterBuildError {
    #[error("failed to construct S3-compatible object store: {0}")]
    Backend(#[from] object_store::Error),
}

#[derive(Clone)]
pub struct ObjectStoreAdapter {
    backend: Arc<dyn BackendObjectStore>,
    prefix: String,
}

impl std::fmt::Debug for ObjectStoreAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObjectStoreAdapter")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl ObjectStoreAdapter {
    #[must_use]
    pub fn new(backend: Arc<dyn BackendObjectStore>, prefix: impl Into<String>) -> Self {
        Self {
            backend,
            prefix: prefix.into().trim_matches('/').to_owned(),
        }
    }

    pub fn s3(config: &S3ObjectStoreConfig) -> Result<Self, ObjectStoreAdapterBuildError> {
        let backend = AmazonS3Builder::new()
            .with_endpoint(&config.endpoint)
            .with_bucket_name(&config.bucket)
            .with_region(&config.region)
            .with_access_key_id(&config.access_key_id)
            .with_secret_access_key(&config.secret_access_key)
            .with_allow_http(config.allow_http)
            .with_virtual_hosted_style_request(false)
            .with_conditional_put(S3ConditionalPut::ETagMatch)
            .build()?;
        Ok(Self::new(Arc::new(backend), &config.prefix))
    }

    fn path(&self, id: ObjectId) -> Path {
        let encoded = id.to_string();
        let suffix = format!("{}/{}/{}", self.prefix, &encoded[..2], &encoded[2..]);
        Path::from(suffix)
    }

    async fn read_and_verify(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        let path = self.path(id);
        let result = self
            .backend
            .get(&path)
            .await
            .map_err(|error| map_backend_error(error, id))?;
        let bytes = result
            .bytes()
            .await
            .map_err(|error| map_backend_error(error, id))?
            .to_vec();
        let actual = ObjectId::of_canonical_bytes(&bytes);
        if actual != id {
            return Err(ObjectStoreError::Corrupt {
                expected: id,
                actual,
            });
        }
        let kind = validate_canonical_object(&bytes)?.into();
        Ok(StoredObject {
            id,
            kind,
            canonical_bytes: bytes,
        })
    }
}

#[async_trait]
impl ImmutableObjectStore for ObjectStoreAdapter {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        validate_canonical_object(canonical_bytes)?;
        let actual = ObjectId::of_canonical_bytes(canonical_bytes);
        if actual != id {
            return Err(ObjectStoreError::Corrupt {
                expected: id,
                actual,
            });
        }
        let options = PutOptions {
            mode: PutMode::Create,
            ..Default::default()
        };
        match self
            .backend
            .put_opts(
                &self.path(id),
                PutPayload::from(canonical_bytes.to_vec()),
                options,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(object_store::Error::AlreadyExists { .. })
            | Err(object_store::Error::Precondition { .. }) => {
                let stored = self.read_and_verify(id).await?;
                if stored.canonical_bytes == canonical_bytes {
                    Ok(())
                } else {
                    Err(ObjectStoreError::Corrupt {
                        expected: id,
                        actual: ObjectId::of_canonical_bytes(&stored.canonical_bytes),
                    })
                }
            }
            Err(error) => Err(map_backend_error(error, id)),
        }
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.read_and_verify(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        let mut results = Vec::with_capacity(ids.len());
        for id in ids {
            results.push(match self.backend.head(&self.path(*id)).await {
                Ok(_) => true,
                Err(object_store::Error::NotFound { .. }) => false,
                Err(error) => return Err(map_backend_error(error, *id)),
            });
        }
        Ok(results)
    }
}

fn map_backend_error(error: object_store::Error, id: ObjectId) -> ObjectStoreError {
    match error {
        object_store::Error::NotFound { .. } => ObjectStoreError::Missing(id),
        error => ObjectStoreError::Backend(error.to_string()),
    }
}
