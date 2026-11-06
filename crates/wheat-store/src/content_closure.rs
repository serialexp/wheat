//! What reading stored content needs: the records a record refers to, followed until
//! none refers further. An embedder that collects its blob store by age keeps a
//! record whose reader is kept only if it is told about both (see
//! `wheat_server::naming`).

use std::collections::HashSet;

use wheat_core::{ContentId, decode_content_chunk_part};

use crate::{ContentStore, ContentStoreError, RecordReferences};

/// The longest part record read while following a long blob's manifest: a part
/// lists at most 16,384 chunks of 36 bytes.
pub(crate) const MAX_PART_BYTES: usize = 1 << 20;

impl ContentStore {
    /// `ids` and every record reading them needs, each once: a delta's base, its
    /// base's, and so on; a chunked blob's chunks and parts; and each part's chunks.
    /// With them, each of `bases` that a delta may have been stored against, and what
    /// reading it needs; a chunked blob is no delta's base, so one of `bases` that is
    /// chunked is left out, unless it is in `ids` too.
    ///
    /// Chunks and parts are stored whole, so they are not read; each other record's
    /// references are read as [`Self::record_references`] does, and each part once.
    /// Fails, with what is missing, unless every record is stored.
    pub fn needed_to_read(
        &self,
        ids: impl IntoIterator<Item = ContentId>,
        bases: impl IntoIterator<Item = ContentId>,
    ) -> Result<Vec<ContentId>, ContentStoreError> {
        let mut seen = HashSet::new();
        let mut needed = Vec::new();
        let mut frontier: Vec<ContentId> = ids.into_iter().filter(|id| seen.insert(*id)).collect();
        let mut only_bases: HashSet<ContentId> = HashSet::new();
        for base in bases {
            if seen.insert(base) {
                only_bases.insert(base);
                frontier.push(base);
            }
        }
        let mut next = Vec::new();
        while !frontier.is_empty() {
            let references = self.record_references(&frontier)?;
            for (&id, references) in frontier.iter().zip(references) {
                let chunked = matches!(
                    references,
                    RecordReferences::Chunks(_) | RecordReferences::Parts(_)
                );
                if chunked && only_bases.contains(&id) {
                    continue;
                }
                needed.push(id);
                match references {
                    RecordReferences::None => {}
                    RecordReferences::Delta { base } => {
                        if seen.insert(base) {
                            next.push(base);
                        }
                    }
                    RecordReferences::Chunks(chunks) => {
                        needed.extend(chunks.into_iter().filter(|chunk| seen.insert(*chunk)));
                    }
                    RecordReferences::Parts(parts) => {
                        for part in parts {
                            if !seen.insert(part) {
                                continue;
                            }
                            needed.push(part);
                            let bytes = self.get_bounded(part, MAX_PART_BYTES)?;
                            let record = decode_content_chunk_part(&bytes).map_err(|_| {
                                crate::content_store::codec_error(
                                    part,
                                    MAX_PART_BYTES,
                                    crate::codec::CodecError::Format("invalid chunk part"),
                                )
                            })?;
                            needed.extend(
                                record
                                    .chunks
                                    .iter()
                                    .map(|chunk| chunk.id)
                                    .filter(|chunk| seen.insert(*chunk)),
                            );
                        }
                    }
                }
            }
            std::mem::swap(&mut frontier, &mut next);
            next.clear();
        }
        Ok(needed)
    }
}
