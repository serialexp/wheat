//! Classification and canonical capture of file bytes.
//!
//! Text is UTF-8 without a NUL byte. Its line endings are canonicalized to LF
//! during capture; binary input is retained byte-for-byte.

use std::borrow::Cow;

/// The classification of bytes presented for capture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureKind {
    /// Valid UTF-8 containing no NUL bytes.
    Text,
    /// Invalid UTF-8 or bytes containing at least one NUL.
    Binary,
}

/// Canonically captured file content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapturedBytes<'a> {
    /// UTF-8 text, with every CRLF and standalone CR normalized to LF.
    Text(Cow<'a, str>),
    /// Binary content, preserved byte-for-byte.
    Binary(Cow<'a, [u8]>),
}

/// Classify file bytes without modifying them.
///
/// Valid UTF-8 is text unless it contains a NUL byte. All other input is
/// binary.
#[must_use]
pub fn classify(bytes: &[u8]) -> CaptureKind {
    if bytes.contains(&0) || std::str::from_utf8(bytes).is_err() {
        CaptureKind::Binary
    } else {
        CaptureKind::Text
    }
}

/// Capture bytes in their canonical representation.
///
/// Binary content is always borrowed and remains byte exact. Text without a CR
/// is also borrowed. Text containing CR line endings is normalized with one
/// output allocation; this does not add or remove a final newline except where
/// a final CR itself denotes that newline.
#[must_use]
pub fn capture(bytes: &[u8]) -> CapturedBytes<'_> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return CapturedBytes::Binary(Cow::Borrowed(bytes));
    };
    if bytes.contains(&0) {
        return CapturedBytes::Binary(Cow::Borrowed(bytes));
    }

    let Some(first_cr) = bytes.iter().position(|&byte| byte == b'\r') else {
        return CapturedBytes::Text(Cow::Borrowed(text));
    };

    // Copy contiguous byte ranges rather than growing once per character.
    let mut normalized = Vec::with_capacity(bytes.len());
    normalized.extend_from_slice(&bytes[..first_cr]);
    let mut cursor = first_cr;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\r' {
            normalized.push(b'\n');
            cursor += 1;
            if bytes.get(cursor) == Some(&b'\n') {
                cursor += 1;
            }

            let next_cr = bytes[cursor..]
                .iter()
                .position(|&byte| byte == b'\r')
                .map_or(bytes.len(), |offset| cursor + offset);
            normalized.extend_from_slice(&bytes[cursor..next_cr]);
            cursor = next_cr;
        } else {
            // `cursor` starts on a CR and advances either to another CR or EOF.
            unreachable!("normalization cursor must point to CR");
        }
    }

    // Replacing ASCII CR bytes with ASCII LF bytes preserves valid UTF-8.
    let normalized = String::from_utf8(normalized)
        .expect("normalizing CR line endings must preserve valid UTF-8");
    CapturedBytes::Text(Cow::Owned(normalized))
}

#[cfg(test)]
mod tests {
    use super::{CaptureKind, CapturedBytes, capture, classify};
    use std::borrow::Cow;

    #[test]
    fn normalizes_crlf() {
        assert_eq!(
            capture(b"first\r\nsecond\r\n"),
            CapturedBytes::Text(Cow::Owned(String::from("first\nsecond\n")))
        );
    }

    #[test]
    fn normalizes_standalone_cr() {
        assert_eq!(
            capture(b"first\rsecond\r"),
            CapturedBytes::Text(Cow::Owned(String::from("first\nsecond\n")))
        );
    }

    #[test]
    fn normalizes_mixed_line_endings() {
        assert_eq!(
            capture(b"one\r\ntwo\rthree\nfour"),
            CapturedBytes::Text(Cow::Owned(String::from("one\ntwo\nthree\nfour")))
        );
    }

    #[test]
    fn accepts_utf8_and_borrows_when_unchanged() {
        let bytes = "héllo 世界\n".as_bytes();
        assert_eq!(classify(bytes), CaptureKind::Text);
        assert!(
            matches!(capture(bytes), CapturedBytes::Text(Cow::Borrowed(text)) if text == "héllo 世界\n")
        );
    }

    #[test]
    fn nul_makes_input_binary_and_preserves_every_byte() {
        let bytes: &[u8] = b"left\0right\r\n";
        assert_eq!(classify(bytes), CaptureKind::Binary);
        assert!(
            matches!(capture(bytes), CapturedBytes::Binary(Cow::Borrowed(captured)) if captured == bytes)
        );
    }

    #[test]
    fn invalid_utf8_is_binary_and_preserves_every_byte() {
        let bytes: &[u8] = &[b'a', 0xff, b'\r', b'\n'];
        assert_eq!(classify(bytes), CaptureKind::Binary);
        assert!(
            matches!(capture(bytes), CapturedBytes::Binary(Cow::Borrowed(captured)) if captured == bytes)
        );
    }

    #[test]
    fn empty_input_is_borrowed_text() {
        assert_eq!(classify(b""), CaptureKind::Text);
        assert!(matches!(
            capture(b""),
            CapturedBytes::Text(Cow::Borrowed(""))
        ));
    }

    #[test]
    fn canonical_capture_is_idempotent_for_line_ending_combinations() {
        let endings = ["", "\n", "\r", "\r\n", "\r\r\n"];
        for first in endings {
            for second in endings {
                let input = format!("é{first}中{second}");
                let CapturedBytes::Text(canonical) = capture(input.as_bytes()) else {
                    panic!("UTF-8 fixture must be text");
                };
                assert!(!canonical.contains('\r'));
                assert!(matches!(
                    capture(canonical.as_bytes()),
                    CapturedBytes::Text(Cow::Borrowed(value)) if value == canonical
                ));
                assert_eq!(canonical.ends_with('\n'), !second.is_empty());
            }
        }
    }

    #[test]
    fn preserves_missing_final_newline() {
        let bytes: &[u8] = b"one\ntwo";
        assert!(matches!(
            capture(bytes),
            CapturedBytes::Text(Cow::Borrowed("one\ntwo"))
        ));

        assert_eq!(
            capture(b"one\r\ntwo"),
            CapturedBytes::Text(Cow::Owned(String::from("one\ntwo")))
        );
    }
}
