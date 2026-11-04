//! Storage-neutral content access used by executable repository operations.

use crate::{ContentId, VerifiedContent, edit_application::ApplicationLimits};

/// Resolves authenticated content and durably stores generated canonical bytes.
///
/// Implementations may treat storing an already-present identical object as success.
/// Resolved content comes back verified by whoever read it, and generated content arrives
/// verified, hashed once where it was generated.
pub trait ContentIo {
    type Error: std::fmt::Display;

    fn resolve(&self, id: ContentId) -> Result<VerifiedContent, Self::Error>;
    /// Confirms `id` is present and intact without returning its bytes. By default it
    /// resolves them; a store that can confirm a large blob without reading it whole
    /// should.
    fn confirm(&self, id: ContentId) -> Result<(), Self::Error> {
        self.resolve(id).map(drop)
    }
    /// The length of `id` in bytes. By default it resolves the content; a store that
    /// records lengths reads them instead, through [`ContentIo::known_length`].
    fn logical_length(&self, id: ContentId) -> Result<u64, Self::Error> {
        match self.known_length(id)? {
            Some(length) => Ok(length),
            None => self.resolve(id).map(|content| content.bytes().len() as u64),
        }
    }
    /// The length of `id` when the store records it, without reading the content. By
    /// default it is unknown.
    fn known_length(&self, _id: ContentId) -> Result<Option<u64>, Self::Error> {
        Ok(None)
    }
    fn store_generated(&self, content: VerifiedContent<&[u8]>) -> Result<(), Self::Error>;
}

/// A [`ContentIo`] as the [`ContentSource`] of an application.
///
/// [`ContentSource`]: crate::edit_application::ContentSource
pub struct FromContentIo<'a, C: ContentIo + ?Sized>(pub &'a C);

impl<C: ContentIo + ?Sized> crate::edit_application::ContentSource for FromContentIo<'_, C> {
    type Error = C::Error;

    fn resolve(&mut self, id: ContentId) -> Result<VerifiedContent, Self::Error> {
        self.0.resolve(id)
    }

    fn confirm(&mut self, id: ContentId) -> Result<Option<VerifiedContent>, Self::Error> {
        self.0.confirm(id).map(|()| None)
    }

    fn logical_length(&mut self, id: ContentId) -> Result<Option<u64>, Self::Error> {
        self.0.known_length(id)
    }
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
