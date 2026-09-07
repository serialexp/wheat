//! Pure, bounded authoring and application of exact guarded byte replacements.
//!
//! Text is canonical UTF-8 with LF line endings and no NUL bytes. Matching is
//! deliberately exact: there is no whitespace, Unicode, or fuzzy fallback.

mod apply;
pub mod authoring;
mod capture;
pub mod line_diff;
mod matcher;
mod types;

pub use apply::apply;
pub use capture::capture;
pub use matcher::match_guard;
pub use types::*;

#[cfg(test)]
mod tests;
