//! Errors as the text a client or a log line gets.

use std::error::Error;

/// `error` and its sources, as one line: a store's error names what failed, its
/// source why.
pub(crate) fn chain(error: &dyn Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}
