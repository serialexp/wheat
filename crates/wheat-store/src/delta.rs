//! Git pack delta encoding and decoding for content store delta compression.
//!
//! Encoding uses `gunnar-delta` (pure Rust). Decoding inlines the apply logic
//! from `gix-pack::data::delta` — it is `pub(crate)` in gix-pack and
//! completely self-contained (~85 lines, no dependencies beyond `std::io::Write`).

use thiserror::Error;

/// Base ContentId reference size in a delta envelope payload.
pub(crate) const BASE_REFERENCE_SIZE: usize = 32;

#[derive(Debug, Error)]
pub(crate) enum DeltaError {
    #[error("corrupt delta: {0}")]
    Corrupt(&'static str),
    #[error("delta command byte 0 is reserved")]
    ReservedCommand,
    #[error("delta encoding failed: {0}")]
    #[expect(
        dead_code,
        reason = "reserved for future delta encoding error propagation"
    )]
    Encode(#[source] gunnar_delta::Error),
}

/// Compute a git pack delta patch from `base` to `target`.
///
/// Returns `None` if the delta is not worthwhile (larger than `target` minus
/// the 32-byte base reference overhead, or encoding failed). The returned
/// bytes are a complete git delta payload (base_size varint + result_size
/// varint + copy/insert instructions).
pub(crate) fn compute_delta(base: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    // Don't bother with tiny files — the base reference alone is 32 bytes.
    if target.len() <= BASE_REFERENCE_SIZE + 8 {
        return None;
    }

    let mut out = Vec::new();
    let budget = target.len().saturating_sub(BASE_REFERENCE_SIZE + 1);
    let opts = gunnar_delta::Encode {
        max_size: Some(budget),
        ..gunnar_delta::Encode::default()
    };
    match gunnar_delta::delta_into(base, target, &mut out, &opts) {
        Ok(true) => Some(out),
        Ok(false) => None, // exceeded budget
        Err(_) => None,    // encoding failed (degenerate input)
    }
}

/// Apply a git pack delta patch to `base`, producing the target bytes.
///
/// The `patch` must be a complete delta payload: base_size varint +
/// result_size varint + copy/insert instructions. This is the format
/// produced by `compute_delta` / `gunnar_delta::delta`. The result-size header
/// must match the authenticated envelope's `expected_result_size` and fit
/// within `max_result_size`; both are checked before allocating the target.
///
/// Inlined from `gix_pack::data::delta::apply` (gix-pack 0.74.2,
/// `src/data/delta.rs`) which is `pub(crate)` and not part of the public API.
pub(crate) fn apply_delta(
    base: &[u8],
    patch: &[u8],
    expected_result_size: usize,
    max_result_size: usize,
) -> Result<Vec<u8>, DeltaError> {
    let (base_size, consumed) = decode_header_size(patch)?;
    let base_size = usize::try_from(base_size)
        .map_err(|_| DeltaError::Corrupt("delta base size does not fit in usize"))?;
    if base_size != base.len() {
        return Err(DeltaError::Corrupt(
            "delta base size header does not match actual base",
        ));
    }
    let patch = &patch[consumed..];
    let (result_size, consumed) = decode_header_size(patch)?;
    let result_size = usize::try_from(result_size)
        .map_err(|_| DeltaError::Corrupt("delta result size does not fit in usize"))?;
    if result_size != expected_result_size {
        return Err(DeltaError::Corrupt(
            "delta result size header does not match envelope logical length",
        ));
    }
    if result_size > max_result_size {
        return Err(DeltaError::Corrupt(
            "delta result size exceeds allowed limit",
        ));
    }
    let data = &patch[consumed..];

    let mut target = vec![0u8; result_size];
    apply_instructions(base, &mut target, data)?;
    Ok(target)
}

/// Decode a varint-encoded size from the front of a delta payload.
///
/// Returns `(decoded_size, bytes_consumed)`. Uses 7 bits per byte,
/// little-endian, high bit = continue.
fn decode_header_size(d: &[u8]) -> Result<(u64, usize), DeltaError> {
    let mut shift = 0u32;
    let mut size = 0u64;
    let mut consumed = 0;
    for cmd in d.iter() {
        if shift >= u64::BITS || (shift == 63 && cmd & 0x7e != 0) {
            return Err(DeltaError::Corrupt(
                "delta header size uses more bits than fit into u64",
            ));
        }
        consumed += 1;
        size |= (u64::from(*cmd) & 0x7f) << shift;
        if *cmd & 0x80 == 0 {
            return Ok((size, consumed));
        }
        shift += 7;
    }
    Err(DeltaError::Corrupt("delta header size is truncated"))
}

/// Apply delta copy/insert instructions. `target` must be pre-sized to the
/// exact result size.
fn apply_instructions(base: &[u8], target: &mut [u8], data: &[u8]) -> Result<(), DeltaError> {
    fn next_byte(data: &[u8], i: &mut usize) -> Result<u8, DeltaError> {
        let byte = *data
            .get(*i)
            .ok_or(DeltaError::Corrupt("delta instruction is truncated"))?;
        *i += 1;
        Ok(byte)
    }

    let mut i = 0;
    let mut written: usize = 0;
    while let Some(cmd) = data.get(i) {
        i += 1;
        match cmd {
            cmd if cmd & 0b1000_0000 != 0 => {
                // Copy from base.
                let (mut ofs, mut size): (u32, u32) = (0, 0);
                if cmd & 0b0000_0001 != 0 {
                    ofs = u32::from(next_byte(data, &mut i)?);
                }
                if cmd & 0b0000_0010 != 0 {
                    ofs |= u32::from(next_byte(data, &mut i)?) << 8;
                }
                if cmd & 0b0000_0100 != 0 {
                    ofs |= u32::from(next_byte(data, &mut i)?) << 16;
                }
                if cmd & 0b0000_1000 != 0 {
                    ofs |= u32::from(next_byte(data, &mut i)?) << 24;
                }
                if cmd & 0b0001_0000 != 0 {
                    size = u32::from(next_byte(data, &mut i)?);
                }
                if cmd & 0b0010_0000 != 0 {
                    size |= u32::from(next_byte(data, &mut i)?) << 8;
                }
                if cmd & 0b0100_0000 != 0 {
                    size |= u32::from(next_byte(data, &mut i)?) << 16;
                }
                if size == 0 {
                    size = 0x10000; // 65536
                }
                let ofs = ofs as usize;
                let size = size as usize;
                let end = ofs
                    .checked_add(size)
                    .ok_or(DeltaError::Corrupt("delta copy range overflows"))?;
                let src = base
                    .get(ofs..end)
                    .ok_or(DeltaError::Corrupt("delta copy range exceeds base size"))?;
                let dst = written
                    .checked_add(size)
                    .and_then(|stop| target.get_mut(written..stop))
                    .ok_or(DeltaError::Corrupt("delta copy exceeds result size"))?;
                dst.copy_from_slice(src);
                written += size;
            }
            0 => {
                return Err(DeltaError::ReservedCommand);
            }
            &size => {
                // Insert literal bytes.
                let size = size as usize;
                let end = i
                    .checked_add(size)
                    .ok_or(DeltaError::Corrupt("delta insert range overflows"))?;
                let src = data
                    .get(i..end)
                    .ok_or(DeltaError::Corrupt("delta insert data is truncated"))?;
                let dst = written
                    .checked_add(size)
                    .and_then(|stop| target.get_mut(written..stop))
                    .ok_or(DeltaError::Corrupt("delta insert exceeds result size"))?;
                dst.copy_from_slice(src);
                i = end;
                written += size;
            }
        }
    }
    if written != target.len() {
        return Err(DeltaError::Corrupt(
            "delta instructions produced fewer bytes than promised",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_delta() {
        let base = b"the quick brown fox jumps over the lazy dog".repeat(4);
        let mut target = base.clone();
        target.extend_from_slice(b" and then stops");

        let patch = compute_delta(&base, &target).expect("delta should be worthwhile");
        assert!(
            patch.len() < target.len(),
            "delta should be smaller than target"
        );

        let reconstructed = apply_delta(&base, &patch, target.len(), target.len()).unwrap();
        assert_eq!(reconstructed, target);
    }

    #[test]
    fn delta_not_worthwhile_for_completely_different_data() {
        let base = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let target = b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        // These are completely different, so the delta is the entire target
        // plus overhead — should not be worthwhile.
        let result = compute_delta(base, target);
        // Either None (budget exceeded) or the patch is basically the whole thing.
        // The budget check means this should be None.
        assert!(result.is_none());
    }

    #[test]
    fn delta_not_worthwhile_for_tiny_files() {
        let base = b"hello";
        let target = b"hello world";
        // Too small — the 32-byte base reference alone is bigger than the target.
        assert!(compute_delta(base, target).is_none());
    }

    #[test]
    fn apply_detects_base_size_mismatch() {
        let base = b"hello world";
        let target = b"hello cruel world";
        let patch = compute_delta(base, target);
        // If we got a patch, try applying it with wrong base
        if let Some(patch) = patch {
            let wrong_base = b"wrong";
            assert!(apply_delta(wrong_base, &patch, target.len(), target.len()).is_err());
        }
    }

    #[test]
    fn instructions_cannot_write_past_the_result() {
        let base = b"abcdef";
        let mut target = [0_u8; 4];
        // Insert 3 literal bytes, then 2 more: the second insert passes the end.
        let inserts = [3, b'x', b'y', b'z', 2, b'u', b'v'];
        assert!(matches!(
            apply_instructions(base, &mut target, &inserts),
            Err(DeltaError::Corrupt("delta insert exceeds result size"))
        ));
        // Copy 3 bytes from offset 0, then 3 more: the second copy passes the end.
        let copies = [0b1001_0001, 0, 3, 0b1001_0001, 3, 3];
        assert!(matches!(
            apply_instructions(base, &mut target, &copies),
            Err(DeltaError::Corrupt("delta copy exceeds result size"))
        ));
        // Exactly filling the result is fine.
        let exact = [0b1001_0001, 0, 3, 1, b'!'];
        apply_instructions(base, &mut target, &exact).unwrap();
        assert_eq!(&target, b"abc!");
    }

    #[test]
    fn apply_detects_truncated_patch() {
        assert!(apply_delta(b"base", &[], 0, 0).is_err());
        assert!(apply_delta(b"base", &[0x80], 0, 0).is_err()); // truncated base varint
        assert!(apply_delta(b"base", &[4, 0x80], 0, 0).is_err()); // truncated result varint
    }

    #[test]
    fn apply_rejects_varint_overflow_and_oversized_results_before_allocating() {
        let overflow = [0x84, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02];
        assert!(apply_delta(b"base", &overflow, 0, 0).is_err());
        let continued = [
            0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0,
        ];
        assert!(apply_delta(b"base", &continued, 0, 0).is_err());

        // base size 4, result size u64::MAX, no instructions. The authenticated
        // envelope size and caller limit reject this before any result allocation.
        let mut huge = vec![4];
        huge.extend([0xff; 9]);
        huge.push(0x01);
        assert!(apply_delta(b"base", &huge, 1, 1).is_err());
    }

    #[test]
    fn apply_rejects_envelope_mismatch_and_request_limit() {
        // Copy all four base bytes: base-size 4, result-size 4, copy command.
        let patch = [4, 4, 0x90, 4];
        assert!(apply_delta(b"base", &patch, 3, 4).is_err());
        assert!(apply_delta(b"base", &patch, 4, 3).is_err());
        assert_eq!(apply_delta(b"base", &patch, 4, 4).unwrap(), b"base");
    }

    #[test]
    fn round_trip_realistic_source() {
        // Simulate a typical source file edit: one line changed in a 5 KB file.
        let mut base = String::new();
        for i in 0..100 {
            base.push_str(&format!("fn function_{i}() -> i32 {{ {i} }}\n"));
        }
        let mut target = base.clone();
        target = target.replace(
            "fn function_50() -> i32 { 50 }",
            "fn function_50() -> i32 { 42 }",
        );

        let base_bytes = base.as_bytes();
        let target_bytes = target.as_bytes();

        let patch = compute_delta(base_bytes, target_bytes).expect("should be worthwhile");
        assert!(
            patch.len() < target_bytes.len() / 2,
            "delta should be much smaller than target for a one-line change"
        );

        let reconstructed =
            apply_delta(base_bytes, &patch, target_bytes.len(), target_bytes.len()).unwrap();
        assert_eq!(reconstructed, target_bytes);
    }
}
