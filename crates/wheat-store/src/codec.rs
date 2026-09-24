//! Deterministic physical compression shared by packed and standalone records.

use std::io::{self, Read};

use thiserror::Error;
use wheat_core::ContentId;

pub(crate) const RAW_THRESHOLD: usize = 1024;
pub(crate) const ZSTD_LEVEL: i32 = 3;
pub(crate) const HARD_LOGICAL_RECORD_BYTES: usize = 1024 * 1024 * 1024;
/// Authenticated aggregate logical-size ceiling for one compressed block.
///
/// Current writers target 64 KiB blocks. Keeping the decode ceiling at 1 MiB
/// preserves those blocks while leaving room for a future 256 KiB policy.
pub(crate) const MAX_BLOCK_LOGICAL_BYTES: usize = 1024 * 1024;
const ENVELOPE_MAGIC: &[u8; 4] = b"WHCE";
const ENVELOPE_VERSION: u8 = 1;
pub(crate) const ENVELOPE_HEADER_LEN: usize = 4 + 1 + 1 + 8 + 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum Codec {
    Raw = 0,
    Zstd = 1,
    Delta = 2,
    DeltaZstd = 3,
    /// Multiple records grouped into one zstd frame. Each entry's
    /// `stored_length` is the compressed block size (shared by all entries in
    /// the block); `logical_length` is this record's decompressed size. The
    /// reader decompresses the block and extracts the record at its precomputed
    /// within-block byte offset.
    ZstdBlock = 4,
}

impl Codec {
    pub(crate) fn from_byte(value: u8) -> Result<Self, CodecError> {
        match value {
            0 => Ok(Self::Raw),
            1 => Ok(Self::Zstd),
            2 => Ok(Self::Delta),
            3 => Ok(Self::DeltaZstd),
            4 => Ok(Self::ZstdBlock),
            other => Err(CodecError::Unknown(other)),
        }
    }

    pub(crate) const fn to_byte(self) -> u8 {
        self as u8
    }

    /// Whether this codec stores content as a delta against a base.
    pub(crate) const fn is_delta(self) -> bool {
        matches!(self, Self::Delta | Self::DeltaZstd)
    }
}

#[derive(Debug, Error)]
pub(crate) enum CodecError {
    #[error("logical record exceeds the hard {HARD_LOGICAL_RECORD_BYTES} byte limit")]
    HardLimit,
    #[error("logical record requires {logical} bytes, exceeding the {max} byte caller limit")]
    ReadLimit { logical: usize, max: usize },
    #[error("invalid codec-tagged record: {0}")]
    Format(&'static str),
    #[error("unknown physical codec {0}")]
    Unknown(u8),
    #[error("zstd codec failed: {0}")]
    Zstd(#[source] io::Error),
    #[error("delta codec failed: {0}")]
    #[expect(
        dead_code,
        reason = "reserved for future delta error propagation paths"
    )]
    Delta(#[source] crate::delta::DeltaError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EnvelopeHeader {
    pub codec: Codec,
    pub stored_length: u64,
    pub logical_length: u64,
}

impl EnvelopeHeader {
    pub(crate) fn physical_length(self) -> Result<u64, CodecError> {
        (ENVELOPE_HEADER_LEN as u64)
            .checked_add(self.stored_length)
            .ok_or(CodecError::Format("envelope physical length overflows"))
    }

    pub(crate) fn validate(self, physical_length: u64, max: usize) -> Result<(), CodecError> {
        if self.physical_length()? != physical_length {
            return Err(CodecError::Format("stored length differs"));
        }
        // For delta codecs, the logical_length limit is checked later after
        // reconstruction, not here — the stored payload is a patch, not the
        // final content. We still validate the hard limit.
        if self.codec.is_delta() {
            let logical =
                usize::try_from(self.logical_length).map_err(|_| CodecError::HardLimit)?;
            if logical > HARD_LOGICAL_RECORD_BYTES {
                return Err(CodecError::HardLimit);
            }
        } else {
            validate_logical_length(self.logical_length, max)?;
        }
        match self.codec {
            Codec::Raw if self.stored_length != self.logical_length => {
                Err(CodecError::Format("raw lengths differ"))
            }
            Codec::Zstd if self.stored_length > self.logical_length => Err(CodecError::Format(
                "compressed length exceeds logical length",
            )),
            Codec::Delta | Codec::DeltaZstd => {
                // Delta payloads must have at least the 32-byte base reference.
                if self.stored_length < crate::delta::BASE_REFERENCE_SIZE as u64 {
                    return Err(CodecError::Format(
                        "delta payload too short for base reference",
                    ));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum EnvelopeFileError {
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error("physical envelope changed while it was being read")]
    Changed,
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Clone, Debug)]
pub(crate) struct PhysicalPayload {
    pub codec: Codec,
    pub logical_length: u64,
    pub bytes: Vec<u8>,
}

pub(crate) fn prepare(logical: &[u8]) -> Result<PhysicalPayload, CodecError> {
    if logical.len() > HARD_LOGICAL_RECORD_BYTES {
        return Err(CodecError::HardLimit);
    }
    let logical_length = logical.len() as u64;
    if logical.len() < RAW_THRESHOLD {
        return Ok(PhysicalPayload {
            codec: Codec::Raw,
            logical_length,
            bytes: logical.to_vec(),
        });
    }
    let compressed = zstd::bulk::compress(logical, ZSTD_LEVEL).map_err(CodecError::Zstd)?;
    let required_saving = 64usize.max(logical.len() / 16);
    if compressed.len().saturating_add(required_saving) <= logical.len() {
        Ok(PhysicalPayload {
            codec: Codec::Zstd,
            logical_length,
            bytes: compressed,
        })
    } else {
        Ok(PhysicalPayload {
            codec: Codec::Raw,
            logical_length,
            bytes: logical.to_vec(),
        })
    }
}

fn validate_logical_length(logical_length: u64, max: usize) -> Result<usize, CodecError> {
    let logical = usize::try_from(logical_length).map_err(|_| CodecError::HardLimit)?;
    if logical > HARD_LOGICAL_RECORD_BYTES {
        return Err(CodecError::HardLimit);
    }
    if logical > max {
        return Err(CodecError::ReadLimit { logical, max });
    }
    Ok(logical)
}

/// Result of decoding an envelope payload. Either final content bytes or a
/// delta reference requiring further resolution.
pub(crate) enum DecodedPayload {
    /// Fully decoded content bytes (Raw or Zstd codec).
    Content(Vec<u8>),
    /// Delta reference: the content is stored as a patch against a base.
    /// The content store must resolve this by fetching the base and applying
    /// the patch.
    Delta { base_id: ContentId, patch: Vec<u8> },
}

pub(crate) fn decode(
    codec: Codec,
    stored: Vec<u8>,
    logical_length: u64,
    max: usize,
) -> Result<Vec<u8>, CodecError> {
    if codec == Codec::ZstdBlock {
        return Err(CodecError::Format(
            "ZstdBlock codec requires block-level decoding, not per-record decode",
        ));
    }
    match decode_payload(codec, stored, logical_length, max)? {
        DecodedPayload::Content(bytes) => Ok(bytes),
        DecodedPayload::Delta { .. } => Err(CodecError::Format(
            "delta codec in a context that does not support delta resolution",
        )),
    }
}

/// Decompresses exactly one zstd block payload (codec 4) whose logical size was
/// derived from the authenticated index entries belonging to that block.
pub(crate) fn decode_block(
    compressed: &[u8],
    expected_logical_length: usize,
) -> Result<Vec<u8>, CodecError> {
    if expected_logical_length > MAX_BLOCK_LOGICAL_BYTES {
        return Err(CodecError::ReadLimit {
            logical: expected_logical_length,
            max: MAX_BLOCK_LOGICAL_BYTES,
        });
    }
    let frame_size = zstd::zstd_safe::find_frame_compressed_size(compressed)
        .map_err(|_| CodecError::Format("malformed or truncated zstd block frame"))?;
    if frame_size != compressed.len() {
        return Err(CodecError::Format("trailing zstd block frame bytes"));
    }
    let frame_logical = zstd::zstd_safe::get_frame_content_size(compressed)
        .map_err(|_| CodecError::Format("malformed zstd block frame"))?
        .ok_or(CodecError::Format("zstd block frame omits content size"))?;
    if frame_logical != expected_logical_length as u64 {
        return Err(CodecError::Format(
            "zstd block frame logical length differs",
        ));
    }
    let output =
        zstd::bulk::decompress(compressed, expected_logical_length).map_err(CodecError::Zstd)?;
    if output.len() != expected_logical_length {
        return Err(CodecError::Format("zstd block output length differs"));
    }
    Ok(output)
}

/// Decode a stored payload into either final content or a delta reference.
///
/// For non-delta codecs, validates lengths and returns fully decoded bytes.
/// For delta codecs, extracts the base ContentId and decompresses the patch
/// (if DeltaZstd), but does **not** resolve the delta — that requires access
/// to the content store, which the codec layer does not have.
pub(crate) fn decode_payload(
    codec: Codec,
    stored: Vec<u8>,
    logical_length: u64,
    max: usize,
) -> Result<DecodedPayload, CodecError> {
    match codec {
        Codec::Raw => {
            let logical = validate_logical_length(logical_length, max)?;
            if stored.len() != logical {
                return Err(CodecError::Format("raw lengths differ"));
            }
            Ok(DecodedPayload::Content(stored))
        }
        Codec::Zstd => {
            let logical = validate_logical_length(logical_length, max)?;
            let frame_size = zstd::zstd_safe::find_frame_compressed_size(&stored)
                .map_err(|_| CodecError::Format("malformed or truncated zstd frame"))?;
            if frame_size != stored.len() {
                return Err(CodecError::Format("trailing zstd frame bytes"));
            }
            let frame_logical = zstd::zstd_safe::get_frame_content_size(&stored)
                .map_err(|_| CodecError::Format("malformed zstd frame"))?
                .ok_or(CodecError::Format("zstd frame omits content size"))?;
            if frame_logical != logical_length {
                return Err(CodecError::Format("zstd frame logical length differs"));
            }
            let output = zstd::bulk::decompress(&stored, logical).map_err(CodecError::Zstd)?;
            if output.len() != logical {
                return Err(CodecError::Format("zstd output length differs"));
            }
            Ok(DecodedPayload::Content(output))
        }
        Codec::Delta => {
            let base_ref_size = crate::delta::BASE_REFERENCE_SIZE;
            if stored.len() < base_ref_size {
                return Err(CodecError::Format(
                    "delta payload too short for base reference",
                ));
            }
            let base_digest: [u8; 32] = stored[..base_ref_size].try_into().unwrap();
            let base_id = ContentId::from_digest(base_digest);
            let patch = stored[base_ref_size..].to_vec();
            Ok(DecodedPayload::Delta { base_id, patch })
        }
        Codec::DeltaZstd => {
            let base_ref_size = crate::delta::BASE_REFERENCE_SIZE;
            if stored.len() < base_ref_size {
                return Err(CodecError::Format(
                    "delta payload too short for base reference",
                ));
            }
            let base_digest: [u8; 32] = stored[..base_ref_size].try_into().unwrap();
            let base_id = ContentId::from_digest(base_digest);
            let compressed_patch = &stored[base_ref_size..];
            // Decompress the patch. We don't know the exact decompressed size,
            // so use the streaming decoder.
            let patch = zstd::bulk::decompress(compressed_patch, HARD_LOGICAL_RECORD_BYTES)
                .map_err(CodecError::Zstd)?;
            Ok(DecodedPayload::Delta { base_id, patch })
        }
        Codec::ZstdBlock => Err(CodecError::Format(
            "ZstdBlock codec requires block-level decoding, not per-record decode",
        )),
    }
}

/// Encode content as a delta envelope against a known base.
///
/// The `target_logical_length` is the size of the fully reconstructed content.
/// `base_id` identifies the base content (32-byte BLAKE3 digest).
/// `patch` is the git pack delta payload from `delta::compute_delta`.
///
/// Chooses between Delta (codec 2) and DeltaZstd (codec 3) based on whether
/// zstd compression of the patch saves enough bytes.
pub(crate) fn encode_delta_envelope(
    target_logical_length: usize,
    base_id: ContentId,
    patch: &[u8],
) -> Result<Vec<u8>, CodecError> {
    if target_logical_length > HARD_LOGICAL_RECORD_BYTES {
        return Err(CodecError::HardLimit);
    }
    let base_ref_size = crate::delta::BASE_REFERENCE_SIZE;
    let logical_length = target_logical_length as u64;

    // Try zstd-compressing the patch if it's large enough.
    let (codec, payload_after_base) = if patch.len() >= RAW_THRESHOLD {
        let compressed = zstd::bulk::compress(patch, ZSTD_LEVEL).map_err(CodecError::Zstd)?;
        let required_saving = 64usize.max(patch.len() / 16);
        if compressed.len().saturating_add(required_saving) <= patch.len() {
            (Codec::DeltaZstd, compressed)
        } else {
            (Codec::Delta, patch.to_vec())
        }
    } else {
        (Codec::Delta, patch.to_vec())
    };

    let stored_length = (base_ref_size + payload_after_base.len()) as u64;
    let mut encoded =
        Vec::with_capacity(ENVELOPE_HEADER_LEN + base_ref_size + payload_after_base.len());
    encoded.extend_from_slice(ENVELOPE_MAGIC);
    encoded.push(ENVELOPE_VERSION);
    encoded.push(codec.to_byte());
    encoded.extend_from_slice(&stored_length.to_be_bytes());
    encoded.extend_from_slice(&logical_length.to_be_bytes());
    encoded.extend_from_slice(base_id.as_bytes());
    encoded.extend_from_slice(&payload_after_base);
    Ok(encoded)
}

/// Physical payload of an in-segment metadata delta: the bare git delta patch
/// (`Delta`) or one zstd frame of it (`DeltaZstd`), under the same savings
/// rule as whole records. The base is named by the segment index entry, so,
/// unlike a content envelope, the payload carries no base reference.
pub(crate) fn prepare_segment_delta(
    target_logical_length: usize,
    patch: &[u8],
) -> Result<PhysicalPayload, CodecError> {
    if target_logical_length > HARD_LOGICAL_RECORD_BYTES {
        return Err(CodecError::HardLimit);
    }
    if patch.len() > target_logical_length {
        return Err(CodecError::Format("delta patch exceeds its target length"));
    }
    let logical_length = target_logical_length as u64;
    if patch.len() >= RAW_THRESHOLD {
        let compressed = zstd::bulk::compress(patch, ZSTD_LEVEL).map_err(CodecError::Zstd)?;
        let required_saving = 64usize.max(patch.len() / 16);
        if compressed.len().saturating_add(required_saving) <= patch.len() {
            return Ok(PhysicalPayload {
                codec: Codec::DeltaZstd,
                logical_length,
                bytes: compressed,
            });
        }
    }
    Ok(PhysicalPayload {
        codec: Codec::Delta,
        logical_length,
        bytes: patch.to_vec(),
    })
}

/// Decodes an in-segment delta payload into its git delta patch. Writers only
/// store patches no longer than the target, so the target's authenticated
/// logical length bounds the patch before any allocation.
pub(crate) fn decode_segment_delta_patch(
    codec: Codec,
    stored: Vec<u8>,
    logical_length: u64,
) -> Result<Vec<u8>, CodecError> {
    let bound = usize::try_from(logical_length).map_err(|_| CodecError::HardLimit)?;
    if bound > HARD_LOGICAL_RECORD_BYTES {
        return Err(CodecError::HardLimit);
    }
    match codec {
        Codec::Delta => {
            if stored.len() > bound {
                return Err(CodecError::Format("delta patch exceeds its target length"));
            }
            Ok(stored)
        }
        Codec::DeltaZstd => {
            let frame_size = zstd::zstd_safe::find_frame_compressed_size(&stored)
                .map_err(|_| CodecError::Format("malformed or truncated zstd delta frame"))?;
            if frame_size != stored.len() {
                return Err(CodecError::Format("trailing zstd delta frame bytes"));
            }
            let patch_length = zstd::zstd_safe::get_frame_content_size(&stored)
                .map_err(|_| CodecError::Format("malformed zstd delta frame"))?
                .ok_or(CodecError::Format("zstd delta frame omits content size"))?;
            if patch_length > logical_length {
                return Err(CodecError::Format("delta patch exceeds its target length"));
            }
            let patch =
                zstd::bulk::decompress(&stored, patch_length as usize).map_err(CodecError::Zstd)?;
            if patch.len() as u64 != patch_length {
                return Err(CodecError::Format("zstd delta output length differs"));
            }
            Ok(patch)
        }
        _ => Err(CodecError::Format("not a delta codec")),
    }
}

pub(crate) fn encode_envelope(logical: &[u8]) -> Result<Vec<u8>, CodecError> {
    let payload = prepare(logical)?;
    let stored_length = payload.bytes.len() as u64;
    let mut encoded = Vec::with_capacity(ENVELOPE_HEADER_LEN + payload.bytes.len());
    encoded.extend_from_slice(ENVELOPE_MAGIC);
    encoded.push(ENVELOPE_VERSION);
    encoded.push(payload.codec.to_byte());
    encoded.extend_from_slice(&stored_length.to_be_bytes());
    encoded.extend_from_slice(&payload.logical_length.to_be_bytes());
    encoded.extend_from_slice(&payload.bytes);
    Ok(encoded)
}

pub(crate) fn parse_envelope_header(encoded: &[u8]) -> Result<EnvelopeHeader, CodecError> {
    if encoded.len() != ENVELOPE_HEADER_LEN || &encoded[..4] != ENVELOPE_MAGIC {
        return Err(CodecError::Format("bad envelope prefix"));
    }
    if encoded[4] != ENVELOPE_VERSION {
        return Err(CodecError::Format("unsupported envelope version"));
    }
    Ok(EnvelopeHeader {
        codec: Codec::from_byte(encoded[5])?,
        stored_length: u64::from_be_bytes(encoded[6..14].try_into().unwrap()),
        logical_length: u64::from_be_bytes(encoded[14..22].try_into().unwrap()),
    })
}

pub(crate) fn decode_envelope_payload(
    header: EnvelopeHeader,
    stored: Vec<u8>,
    max: usize,
) -> Result<Vec<u8>, CodecError> {
    header.validate(
        (ENVELOPE_HEADER_LEN as u64).saturating_add(stored.len() as u64),
        max,
    )?;
    decode(header.codec, stored, header.logical_length, max)
}

#[cfg(test)]
pub(crate) fn decode_envelope(encoded: &[u8], max: usize) -> Result<Vec<u8>, CodecError> {
    if encoded.len() < ENVELOPE_HEADER_LEN {
        return Err(CodecError::Format("bad envelope prefix"));
    }
    let header = parse_envelope_header(&encoded[..ENVELOPE_HEADER_LEN])?;
    header.validate(encoded.len() as u64, max)?;
    decode_envelope_payload(header, encoded[ENVELOPE_HEADER_LEN..].to_vec(), max)
}

/// Read only an envelope file's fixed header, after checking the file's exact
/// physical length against it. The stored payload is not read.
pub(crate) fn read_envelope_file_header(
    reader: &mut impl Read,
    physical_length: u64,
) -> Result<EnvelopeHeader, EnvelopeFileError> {
    if physical_length < ENVELOPE_HEADER_LEN as u64 {
        return Err(CodecError::Format("bad envelope prefix").into());
    }
    let mut header_bytes = [0_u8; ENVELOPE_HEADER_LEN];
    reader.read_exact(&mut header_bytes).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            EnvelopeFileError::Changed
        } else {
            EnvelopeFileError::Io(error)
        }
    })?;
    let header = parse_envelope_header(&header_bytes)?;
    if header.physical_length()? != physical_length {
        return Err(CodecError::Format("stored length differs").into());
    }
    Ok(header)
}

/// Read an envelope file and return the header + raw stored payload without
/// decoding. Used by the delta-aware content store to inspect the codec before
/// deciding whether to decode directly or follow a delta chain.
pub(crate) fn read_envelope_file_raw(
    reader: &mut impl Read,
    physical_length: u64,
) -> Result<(EnvelopeHeader, Vec<u8>), EnvelopeFileError> {
    // Validates the physical length but not logical limits; the caller handles
    // those after delta resolution.
    let header = read_envelope_file_header(reader, physical_length)?;
    let logical = usize::try_from(header.logical_length)
        .map_err(|_| CodecError::Format("logical length does not fit this platform"))?;
    if logical > HARD_LOGICAL_RECORD_BYTES {
        return Err(CodecError::HardLimit.into());
    }
    let stored_length = usize::try_from(header.stored_length)
        .map_err(|_| CodecError::Format("stored length does not fit this platform"))?;
    let mut stored = vec![0_u8; stored_length];
    reader.read_exact(&mut stored).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            EnvelopeFileError::Changed
        } else {
            EnvelopeFileError::Io(error)
        }
    })?;
    let mut extra = [0_u8; 1];
    if reader.read(&mut extra)? != 0 {
        return Err(EnvelopeFileError::Changed);
    }
    Ok((header, stored))
}

/// Reads an envelope without allocating for its payload until the fixed header,
/// caller limit, and stat-derived exact physical length have all been validated.
pub(crate) fn read_envelope_file(
    reader: &mut impl Read,
    physical_length: u64,
    max: usize,
) -> Result<Vec<u8>, EnvelopeFileError> {
    if physical_length < ENVELOPE_HEADER_LEN as u64 {
        return Err(CodecError::Format("bad envelope prefix").into());
    }
    let mut header_bytes = [0_u8; ENVELOPE_HEADER_LEN];
    reader.read_exact(&mut header_bytes).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            EnvelopeFileError::Changed
        } else {
            EnvelopeFileError::Io(error)
        }
    })?;
    let header = parse_envelope_header(&header_bytes)?;
    header.validate(physical_length, max)?;
    let stored_length = usize::try_from(header.stored_length)
        .map_err(|_| CodecError::Format("stored length does not fit this platform"))?;
    let mut stored = vec![0_u8; stored_length];
    reader.read_exact(&mut stored).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            EnvelopeFileError::Changed
        } else {
            EnvelopeFileError::Io(error)
        }
    })?;
    let mut extra = [0_u8; 1];
    if reader.read(&mut extra)? != 0 {
        return Err(EnvelopeFileError::Changed);
    }
    Ok(decode_envelope_payload(header, stored, max)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_threshold_and_savings_policy() {
        assert_eq!(prepare(&vec![0; 1023]).unwrap().codec, Codec::Raw);
        assert_eq!(prepare(&vec![0; 1024]).unwrap().codec, Codec::Zstd);
        let mut state = 0x1234_5678_u32;
        let noisy: Vec<_> = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        let first = prepare(&noisy).unwrap();
        let second = prepare(&noisy).unwrap();
        assert_eq!(first.codec, Codec::Raw);
        assert_eq!(first.bytes, second.bytes);
    }

    #[test]
    fn block_decoder_requires_exact_single_frame_and_logical_size() {
        let logical = vec![b'x'; 64 * 1024];
        let compressed = zstd::bulk::compress(&logical, ZSTD_LEVEL).unwrap();
        assert_eq!(decode_block(&compressed, logical.len()).unwrap(), logical);
        assert!(matches!(
            decode_block(&compressed, logical.len() - 1),
            Err(CodecError::Format(
                "zstd block frame logical length differs"
            ))
        ));

        let mut trailing = compressed.clone();
        trailing.push(0);
        assert!(matches!(
            decode_block(&trailing, logical.len()),
            Err(CodecError::Format("trailing zstd block frame bytes"))
        ));

        assert!(matches!(
            decode_block(&compressed, MAX_BLOCK_LOGICAL_BYTES + 1),
            Err(CodecError::ReadLimit { .. })
        ));
    }

    #[test]
    fn envelope_rejects_bounds_truncation_and_trailing_frames() {
        let logical = vec![b'x'; 4096];
        let encoded = encode_envelope(&logical).unwrap();
        assert_eq!(decode_envelope(&encoded, logical.len()).unwrap(), logical);
        assert!(matches!(
            decode_envelope(&encoded, logical.len() - 1),
            Err(CodecError::ReadLimit { .. })
        ));
        assert!(decode_envelope(&encoded[..encoded.len() - 1], usize::MAX).is_err());
        let mut trailing = encoded;
        trailing.push(0);
        // Repair the envelope stored length so frame-consumption validation sees it.
        let length = (trailing.len() - ENVELOPE_HEADER_LEN) as u64;
        trailing[6..14].copy_from_slice(&length.to_be_bytes());
        assert!(matches!(
            decode_envelope(&trailing, usize::MAX),
            Err(CodecError::Format("trailing zstd frame bytes"))
        ));
    }

    #[test]
    fn file_reader_rejects_forged_logical_length_before_payload_read() {
        let mut header = encode_envelope(b"x").unwrap();
        header.truncate(ENVELOPE_HEADER_LEN);
        header[14..22].copy_from_slice(&u64::MAX.to_be_bytes());
        let mut reader = &header[..];
        assert!(matches!(
            read_envelope_file(&mut reader, (ENVELOPE_HEADER_LEN + 1) as u64, usize::MAX),
            Err(EnvelopeFileError::Codec(CodecError::HardLimit))
        ));
    }

    #[test]
    fn file_reader_requires_exact_stat_size_and_detects_payload_changes() {
        let encoded = encode_envelope(b"payload").unwrap();
        let mut exact = &encoded[..];
        assert_eq!(
            read_envelope_file(&mut exact, encoded.len() as u64, 7).unwrap(),
            b"payload"
        );

        let mut wrong_stat = &encoded[..];
        assert!(matches!(
            read_envelope_file(&mut wrong_stat, encoded.len() as u64 + 1, usize::MAX),
            Err(EnvelopeFileError::Codec(CodecError::Format(
                "stored length differs"
            )))
        ));

        let mut truncated = &encoded[..encoded.len() - 1];
        assert!(matches!(
            read_envelope_file(&mut truncated, encoded.len() as u64, usize::MAX),
            Err(EnvelopeFileError::Changed)
        ));

        let mut trailing = encoded;
        trailing.push(0);
        let physical_length = (trailing.len() - 1) as u64;
        let mut grown = &trailing[..];
        assert!(matches!(
            read_envelope_file(&mut grown, physical_length, usize::MAX),
            Err(EnvelopeFileError::Changed)
        ));
    }
}
