//! Storage-neutral content access used by executable repository operations.

use crate::{ContentId, edit_application::ApplicationLimits};

/// Resolves authenticated content and durably stores generated canonical bytes.
///
/// Implementations may treat storing an already-present identical object as success. The
/// repository verifies resolved and generated identities independently.
pub trait ContentIo {
    type Error: std::fmt::Display;

    fn resolve(&self, id: ContentId) -> Result<Vec<u8>, Self::Error>;
    fn store_generated(&self, id: ContentId, bytes: &[u8]) -> Result<(), Self::Error>;
}

pub struct ApplicationContext<'a, C: ContentIo + ?Sized> {
    pub content: &'a C,
    pub limits: ApplicationLimits,
}

impl<C: ContentIo + ?Sized> Copy for ApplicationContext<'_, C> {}
impl<C: ContentIo + ?Sized> Clone for ApplicationContext<'_, C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, C: ContentIo + ?Sized> ApplicationContext<'a, C> {
    #[must_use]
    pub fn new(content: &'a C) -> Self {
        Self {
            content,
            limits: ApplicationLimits::default(),
        }
    }

    #[must_use]
    pub fn with_limits(content: &'a C, limits: ApplicationLimits) -> Self {
        Self { content, limits }
    }
}
