//! Classification and canonical capture of file bytes.
//!
//! Text is UTF-8 without a NUL byte. Its line endings are canonicalized to LF
//! during capture; binary input is retained byte-for-byte.

use std::borrow::Cow;

/// Incremental canonical capture result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamingCaptureResult {
    pub kind: CaptureKind,
    pub digest: [u8; 32],
    pub bytes: usize,
    pub retained: Option<Vec<u8>>,
}

/// Streaming counterpart to [`capture`].
///
/// It hashes raw and LF-normalized bytes in parallel because a NUL or invalid
/// UTF-8 sequence near EOF retroactively makes the entire input binary. Only
/// canonical text up to `retain_limit` is retained; larger and binary inputs
/// have flat memory use regardless of input size.
pub struct StreamingCapture {
    raw: blake3::Hasher,
    canonical: blake3::Hasher,
    raw_bytes: usize,
    canonical_bytes: usize,
    retained: Option<Vec<u8>>,
    retain_limit: usize,
    pending_cr: bool,
    utf8_tail: [u8; 4],
    utf8_tail_len: usize,
    binary: bool,
}

impl StreamingCapture {
    #[must_use]
    pub fn new(retain_limit: usize) -> Self {
        Self {
            raw: blake3::Hasher::new(),
            canonical: blake3::Hasher::new(),
            raw_bytes: 0,
            canonical_bytes: 0,
            // Reserve once: growth during `update` would otherwise reallocate on
            // successive chunks. Callers already bound this inline-retention cap.
            retained: Some(Vec::with_capacity(retain_limit)),
            retain_limit,
            pending_cr: false,
            utf8_tail: [0; 4],
            utf8_tail_len: 0,
            binary: false,
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.raw.update(bytes);
        self.raw_bytes = self
            .raw_bytes
            .checked_add(bytes.len())
            .expect("captured byte length exceeds address space");
        self.validate_utf8(bytes);
        if bytes.contains(&0) {
            self.binary = true;
        }
        // Once classification is irreversibly binary, canonical hashing and
        // retention are wasted work. Raw hashing above must continue for every
        // later chunk because binary identity is byte exact.
        if self.binary {
            self.retained = None;
            return;
        }
        // An empty update is not an EOF marker. In particular, it must not
        // resolve a trailing CR before a later chunk starts with LF.
        if bytes.is_empty() {
            return;
        }

        let mut start = 0;
        if self.pending_cr {
            self.emit(b"\n");
            self.pending_cr = false;
            if bytes.first() == Some(&b'\n') {
                start = 1;
            }
        }
        while start < bytes.len() {
            let Some(offset) = bytes[start..].iter().position(|byte| *byte == b'\r') else {
                self.emit(&bytes[start..]);
                return;
            };
            let cr = start + offset;
            self.emit(&bytes[start..cr]);
            if cr + 1 == bytes.len() {
                self.pending_cr = true;
                return;
            }
            self.emit(b"\n");
            start = cr + 1 + usize::from(bytes[cr + 1] == b'\n');
        }
    }

    #[must_use]
    pub fn finish(mut self) -> StreamingCaptureResult {
        if self.pending_cr && !self.binary {
            self.emit(b"\n");
        }
        if self.utf8_tail_len != 0 {
            self.binary = true;
        }
        if self.binary {
            StreamingCaptureResult {
                kind: CaptureKind::Binary,
                digest: *self.raw.finalize().as_bytes(),
                bytes: self.raw_bytes,
                retained: None,
            }
        } else {
            StreamingCaptureResult {
                kind: CaptureKind::Text,
                digest: *self.canonical.finalize().as_bytes(),
                bytes: self.canonical_bytes,
                retained: self.retained,
            }
        }
    }

    fn emit(&mut self, bytes: &[u8]) {
        self.canonical.update(bytes);
        self.canonical_bytes = self
            .canonical_bytes
            .checked_add(bytes.len())
            .expect("canonical byte length exceeds address space");
        if let Some(retained) = &mut self.retained {
            if retained.len().saturating_add(bytes.len()) <= self.retain_limit {
                retained.extend_from_slice(bytes);
            } else {
                self.retained = None;
            }
        }
    }

    fn validate_utf8(&mut self, mut bytes: &[u8]) {
        if self.binary {
            return;
        }
        while self.utf8_tail_len != 0 && !bytes.is_empty() {
            self.utf8_tail[self.utf8_tail_len] = bytes[0];
            self.utf8_tail_len += 1;
            bytes = &bytes[1..];
            match std::str::from_utf8(&self.utf8_tail[..self.utf8_tail_len]) {
                Ok(_) => self.utf8_tail_len = 0,
                Err(error) if error.error_len().is_some() || self.utf8_tail_len == 4 => {
                    self.binary = true;
                    return;
                }
                Err(_) => {}
            }
        }
        if self.utf8_tail_len != 0 {
            return;
        }
        match std::str::from_utf8(bytes) {
            Ok(_) => {}
            Err(error) if error.error_len().is_some() => self.binary = true,
            Err(error) => {
                let valid = error.valid_up_to();
                let tail = &bytes[valid..];
                self.utf8_tail[..tail.len()].copy_from_slice(tail);
                self.utf8_tail_len = tail.len();
            }
        }
    }
}

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
    use super::{CaptureKind, CapturedBytes, StreamingCapture, capture, classify};
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
    fn streaming_matches_capture_at_every_split_and_retention_bound() {
        let fixtures: &[&[u8]] = &[
            b"",
            b"one\r\ntwo\rthree",
            "héllo 世界\r\n".as_bytes(),
            b"prefix\r\nsuffix\xff",
            b"prefix\r\nsuffix\0",
            b"last\r",
            &[0xc2],
            &[0xe2, 0x82],
            &[0xf0, 0x9f, 0x92],
            &[0xf0, 0x9f, 0x92, 0xa9],
            &[0xe2, 0x28, 0xa1],
            &[0xf4, 0x90, 0x80, 0x80],
        ];
        for input in fixtures {
            for split in 0..=input.len() {
                for limit in [0, 3, input.len(), input.len() + 1] {
                    let mut stream = StreamingCapture::new(limit);
                    stream.update(&input[..split]);
                    stream.update(&[]);
                    stream.update(&input[split..]);
                    let result = stream.finish();
                    match capture(input) {
                        CapturedBytes::Text(expected) => {
                            assert_eq!(result.kind, CaptureKind::Text);
                            assert_eq!(
                                result.digest,
                                *blake3::hash(expected.as_bytes()).as_bytes()
                            );
                            assert_eq!(result.bytes, expected.len());
                            assert_eq!(
                                result.retained.as_deref(),
                                (expected.len() <= limit).then_some(expected.as_bytes())
                            );
                        }
                        CapturedBytes::Binary(expected) => {
                            assert_eq!(result.kind, CaptureKind::Binary);
                            assert_eq!(result.digest, *blake3::hash(&expected).as_bytes());
                            assert_eq!(result.bytes, expected.len());
                            assert_eq!(result.retained, None);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn binary_capture_drops_retention_immediately_and_keeps_raw_identity() {
        let mut stream = StreamingCapture::new(1024 * 1024);
        stream.update(b"text that had been retained\r\n");
        stream.update(b"late nul\0");
        stream.update(b"and all bytes after classification\r\n");
        let raw =
            b"text that had been retained\r\nlate nul\0and all bytes after classification\r\n";
        let result = stream.finish();
        assert_eq!(result.kind, CaptureKind::Binary);
        assert_eq!(result.digest, *blake3::hash(raw).as_bytes());
        assert_eq!(result.bytes, raw.len());
        assert_eq!(result.retained, None);
    }

    #[test]
    fn streaming_many_small_chunks_matches_capture() {
        let input = "é\r\n世界\rstandalone\r\nend".as_bytes();
        let mut stream = StreamingCapture::new(input.len());
        for byte in input {
            stream.update(std::slice::from_ref(byte));
        }
        let result = stream.finish();
        let CapturedBytes::Text(expected) = capture(input) else {
            panic!("fixture must be text")
        };
        assert_eq!(result.kind, CaptureKind::Text);
        assert_eq!(result.digest, *blake3::hash(expected.as_bytes()).as_bytes());
        assert_eq!(result.bytes, expected.len());
        assert_eq!(result.retained.as_deref(), Some(expected.as_bytes()));
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
