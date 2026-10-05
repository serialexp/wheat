use wheat_core::{ContentId, ContentSetBlob, ContentSetId};

use crate::{CollectionError, ContentSetDeltaLimits, ImmutableObjectStore};

/// Maximum number of deltas between authenticated full snapshots.
pub const CONTENT_SET_CHAIN_DEPTH: usize = 16;
/// Maximum resolved membership accepted from an authenticated content-set chain.
pub const CONTENT_SET_MAX_MEMBERS: usize = 1_000_000;
/// Maximum canonical bytes accepted for one content-set object.
pub const CONTENT_SET_MAX_ENCODED_BYTES: usize = 33_000_042;

const CONTENT_ID_WIRE_BYTES: usize = 33;
const SNAPSHOT_FIXED_BYTES: usize = 6;
const DELTA_FIXED_BYTES: usize = 42;

/// One authenticated membership difference between two content manifests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentSetDelta {
    Added(ContentId),
    Removed(ContentId),
}

/// Flat sorted blob content set store.
pub struct ContentSetStore<'a, S> {
    objects: &'a S,
}

/// Iterator over the resolved members of a content set.
pub struct ContentSetCursor {
    members: Vec<ContentId>,
    index: usize,
}

#[derive(Debug)]
struct DeltaStep {
    added: Vec<ContentId>,
    removed: Vec<ContentId>,
}

impl ContentSetCursor {
    pub async fn next(&mut self) -> Result<Option<ContentId>, CollectionError> {
        let member = self.members.get(self.index).copied();
        self.index += usize::from(member.is_some());
        Ok(member)
    }
}

impl<'a, S: ImmutableObjectStore> ContentSetStore<'a, S> {
    #[must_use]
    pub fn new(objects: &'a S) -> Self {
        Self { objects }
    }

    /// Resolve a content set iteratively, rejecting chains beyond the write-policy bound.
    pub async fn resolve(&self, root: ContentSetId) -> Result<Vec<ContentId>, CollectionError> {
        Ok(self.resolve_with_depth(root).await?.0)
    }

    async fn resolve_with_depth(
        &self,
        root: ContentSetId,
    ) -> Result<(Vec<ContentId>, usize), CollectionError> {
        let mut current = root;
        let mut chain: Vec<DeltaStep> = Vec::with_capacity(CONTENT_SET_CHAIN_DEPTH);
        let mut depth = 0;

        loop {
            let stored = self.objects.get(current.into()).await?;
            if stored.canonical_bytes.len() > CONTENT_SET_MAX_ENCODED_BYTES {
                return Err(CollectionError::ContentSetByteLimitExceeded {
                    limit: CONTENT_SET_MAX_ENCODED_BYTES,
                });
            }
            match wheat_core::decode_content_set_blob(&stored.canonical_bytes)? {
                ContentSetBlob::Snapshot(mut members) => {
                    check_member_count(members.len())?;
                    let mut scratch = Vec::new();
                    for step in chain.iter().rev() {
                        apply_delta_into(&members, &step.added, &step.removed, &mut scratch)?;
                        std::mem::swap(&mut members, &mut scratch);
                    }
                    return Ok((members, depth));
                }
                ContentSetBlob::Delta {
                    base,
                    added,
                    removed,
                } => {
                    if depth == CONTENT_SET_CHAIN_DEPTH {
                        return Err(CollectionError::ContentSetChainTooDeep {
                            limit: CONTENT_SET_CHAIN_DEPTH,
                        });
                    }
                    if sorted_slices_intersect(&added, &removed) {
                        return Err(CollectionError::InvalidContentSetDelta);
                    }
                    check_member_count(added.len())?;
                    check_member_count(removed.len())?;
                    chain.push(DeltaStep { added, removed });
                    current = base;
                    depth += 1;
                }
            }
        }
    }

    pub async fn chain_depth(&self, root: ContentSetId) -> Result<usize, CollectionError> {
        Ok(self.resolve_with_depth(root).await?.1)
    }

    pub async fn empty_root(&self) -> Result<ContentSetId, CollectionError> {
        self.write_snapshot(&[]).await
    }

    pub async fn write_snapshot(
        &self,
        members: &[ContentId],
    ) -> Result<ContentSetId, CollectionError> {
        check_member_count(members.len())?;
        check_encoded_size(snapshot_encoded_size(members.len())?)?;
        let bytes = wheat_core::content_set_snapshot_bytes(members);
        let id = ContentSetId::of_canonical_bytes(&bytes);
        self.objects.put_if_absent(id.into(), &bytes).await?;
        Ok(id)
    }

    pub async fn write_delta(
        &self,
        base: ContentSetId,
        added: &[ContentId],
        removed: &[ContentId],
    ) -> Result<ContentSetId, CollectionError> {
        check_member_count(added.len())?;
        check_member_count(removed.len())?;
        check_encoded_size(delta_encoded_size(added.len(), removed.len())?)?;
        let bytes = wheat_core::content_set_delta_bytes(base, added, removed);
        let id = ContentSetId::of_canonical_bytes(&bytes);
        self.objects.put_if_absent(id.into(), &bytes).await?;
        Ok(id)
    }

    pub async fn contains(
        &self,
        root: ContentSetId,
        key: ContentId,
    ) -> Result<bool, CollectionError> {
        Ok(self.resolve(root).await?.binary_search(&key).is_ok())
    }

    pub async fn insert(
        &self,
        root: ContentSetId,
        key: ContentId,
    ) -> Result<ContentSetId, CollectionError> {
        self.update_batch(root, [(key, true)]).await
    }

    pub async fn remove(
        &self,
        root: ContentSetId,
        key: ContentId,
    ) -> Result<ContentSetId, CollectionError> {
        self.update_batch(root, [(key, false)]).await
    }

    /// Apply ordinary edits and write a canonical membership snapshot.
    ///
    /// The result root must stay derived from membership alone: repository state,
    /// view manifests, and every other identity-bearing structure compute their
    /// canonical IDs from the snapshot encoding (`content_manifest_bytes`), and
    /// `hydrate` rejects stores whose persisted roots diverge from that closure.
    /// Delta blobs encode update history, so they may never back these roots;
    /// use [`ContentSetStore::update_batch_compressing`] only for roots that no
    /// canonical identity is derived from.
    pub async fn update_batch(
        &self,
        root: ContentSetId,
        edits: impl IntoIterator<Item = (ContentId, bool)>,
    ) -> Result<ContentSetId, CollectionError> {
        self.update_batch_compressing_inner(root, edits, |_| false)
            .await
    }

    /// Like [`ContentSetStore::update_batch`], but selects the physically smaller
    /// bounded representation: a delta against the current root while the chain
    /// depth stays under [`CONTENT_SET_CHAIN_DEPTH`], else a full snapshot.
    ///
    /// The caller must guarantee the returned root never enters any structure
    /// whose canonical ID is recomputed from membership alone.
    pub async fn update_batch_compressing(
        &self,
        root: ContentSetId,
        edits: impl IntoIterator<Item = (ContentId, bool)>,
    ) -> Result<ContentSetId, CollectionError> {
        self.update_batch_compressing_inner(root, edits, |_| true)
            .await
    }

    #[tracing::instrument(name = "content_set_update", level = "debug", skip_all)]
    async fn update_batch_compressing_inner(
        &self,
        root: ContentSetId,
        edits: impl IntoIterator<Item = (ContentId, bool)>,
        allow_delta: impl Fn(usize) -> bool,
    ) -> Result<ContentSetId, CollectionError> {
        let (old, depth) = self.resolve_with_depth(root).await?;
        let edits = normalize_edits(edits);
        let (new, added, removed) = merge_edits(&old, &edits);
        if added.is_empty() && removed.is_empty() {
            return Ok(root);
        }

        check_member_count(new.len())?;
        let snapshot_size = snapshot_encoded_size(new.len())?;
        let delta_size = delta_encoded_size(added.len(), removed.len())?;
        if depth + 1 < CONTENT_SET_CHAIN_DEPTH && delta_size < snapshot_size && allow_delta(depth) {
            check_encoded_size(delta_size)?;
            return self.write_delta(root, &added, &removed).await;
        }

        check_encoded_size(snapshot_size)?;
        let snapshot_bytes = wheat_core::content_set_snapshot_bytes(&new);
        let id = ContentSetId::of_canonical_bytes(&snapshot_bytes);
        self.objects
            .put_if_absent(id.into(), &snapshot_bytes)
            .await?;
        Ok(id)
    }

    pub async fn cursor(&self, root: ContentSetId) -> Result<ContentSetCursor, CollectionError> {
        Ok(ContentSetCursor {
            members: self.resolve(root).await?,
            index: 0,
        })
    }

    pub async fn union(
        &self,
        root: ContentSetId,
        other: ContentSetId,
    ) -> Result<ContentSetId, CollectionError> {
        if root == other {
            return Ok(root);
        }
        let a = self.resolve(root).await?;
        let b = self.resolve(other).await?;
        self.write_snapshot(&sorted_union(&a, &b)).await
    }

    pub async fn difference(
        &self,
        root: ContentSetId,
        other: ContentSetId,
    ) -> Result<ContentSetId, CollectionError> {
        if root == other {
            return self.empty_root().await;
        }
        let a = self.resolve(root).await?;
        let b = self.resolve(other).await?;
        self.write_snapshot(&sorted_difference(&a, &b)).await
    }

    pub async fn delta(
        &self,
        old: ContentSetId,
        new: ContentSetId,
        limits: ContentSetDeltaLimits,
    ) -> Result<Vec<ContentSetDelta>, CollectionError> {
        if old == new {
            return Ok(Vec::new());
        }
        let old = self.resolve(old).await?;
        let new = self.resolve(new).await?;
        let mut result = Vec::new();
        walk_difference(&old, &new, |id, added| {
            if result.len() == limits.max_changes {
                return Err(CollectionError::DeltaChangeLimitExceeded {
                    limit: limits.max_changes,
                });
            }
            result.push(if added {
                ContentSetDelta::Added(id)
            } else {
                ContentSetDelta::Removed(id)
            });
            Ok(())
        })?;
        Ok(result)
    }

    /// The members of `new` that `old` lacks, in order. Unlike [`Self::delta`] there
    /// is no change limit: both sets are already bounded by
    /// [`CONTENT_SET_MAX_MEMBERS`].
    pub async fn added(
        &self,
        old: ContentSetId,
        new: ContentSetId,
    ) -> Result<Vec<ContentId>, CollectionError> {
        if old == new {
            return Ok(Vec::new());
        }
        let old = self.resolve(old).await?;
        let new = self.resolve(new).await?;
        let mut added = Vec::new();
        let Ok(()) = walk_difference::<std::convert::Infallible>(&old, &new, |id, is_added| {
            if is_added {
                added.push(id);
            }
            Ok(())
        });
        Ok(added)
    }
}

fn normalize_edits(edits: impl IntoIterator<Item = (ContentId, bool)>) -> Vec<(ContentId, bool)> {
    let mut indexed: Vec<_> = edits
        .into_iter()
        .enumerate()
        .map(|(index, (id, present))| (id, index, present))
        .collect();
    indexed.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    let mut normalized = Vec::with_capacity(indexed.len());
    for (id, _, present) in indexed {
        if normalized.last().is_some_and(|(last, _)| *last == id) {
            normalized.last_mut().expect("checked above").1 = present;
        } else {
            normalized.push((id, present));
        }
    }
    normalized
}

/// Merge sorted edits into sorted membership in one pass while collecting the exact delta.
fn merge_edits(
    old: &[ContentId],
    edits: &[(ContentId, bool)],
) -> (Vec<ContentId>, Vec<ContentId>, Vec<ContentId>) {
    let mut new = Vec::with_capacity(old.len().saturating_add(edits.len()));
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let (mut oi, mut ei) = (0, 0);
    while oi < old.len() || ei < edits.len() {
        match (old.get(oi), edits.get(ei)) {
            (Some(old_id), Some((edit_id, present))) => match old_id.cmp(edit_id) {
                std::cmp::Ordering::Less => {
                    new.push(*old_id);
                    oi += 1;
                }
                std::cmp::Ordering::Greater => {
                    if *present {
                        new.push(*edit_id);
                        added.push(*edit_id);
                    }
                    ei += 1;
                }
                std::cmp::Ordering::Equal => {
                    if *present {
                        new.push(*old_id);
                    } else {
                        removed.push(*old_id);
                    }
                    oi += 1;
                    ei += 1;
                }
            },
            (Some(old_id), None) => {
                new.extend_from_slice(&old[oi..]);
                debug_assert_eq!(*old_id, old[oi]);
                break;
            }
            (None, Some((edit_id, present))) => {
                if *present {
                    new.push(*edit_id);
                    added.push(*edit_id);
                }
                ei += 1;
            }
            (None, None) => break,
        }
    }
    (new, added, removed)
}

fn check_member_count(count: usize) -> Result<(), CollectionError> {
    if count > CONTENT_SET_MAX_MEMBERS {
        return Err(CollectionError::ContentSetMemberLimitExceeded {
            limit: CONTENT_SET_MAX_MEMBERS,
        });
    }
    Ok(())
}

fn check_encoded_size(size: usize) -> Result<(), CollectionError> {
    if size > CONTENT_SET_MAX_ENCODED_BYTES {
        return Err(CollectionError::ContentSetByteLimitExceeded {
            limit: CONTENT_SET_MAX_ENCODED_BYTES,
        });
    }
    Ok(())
}

fn snapshot_encoded_size(members: usize) -> Result<usize, CollectionError> {
    members
        .checked_mul(CONTENT_ID_WIRE_BYTES)
        .and_then(|bytes| bytes.checked_add(SNAPSHOT_FIXED_BYTES))
        .ok_or(CollectionError::ContentSetByteLimitExceeded {
            limit: CONTENT_SET_MAX_ENCODED_BYTES,
        })
}

fn delta_encoded_size(added: usize, removed: usize) -> Result<usize, CollectionError> {
    added
        .checked_add(removed)
        .and_then(|members| members.checked_mul(CONTENT_ID_WIRE_BYTES))
        .and_then(|bytes| bytes.checked_add(DELTA_FIXED_BYTES))
        .ok_or(CollectionError::ContentSetByteLimitExceeded {
            limit: CONTENT_SET_MAX_ENCODED_BYTES,
        })
}

fn sorted_slices_intersect(a: &[ContentId], b: &[ContentId]) -> bool {
    let (mut ai, mut bi) = (0, 0);
    while ai < a.len() && bi < b.len() {
        match a[ai].cmp(&b[bi]) {
            std::cmp::Ordering::Less => ai += 1,
            std::cmp::Ordering::Greater => bi += 1,
            std::cmp::Ordering::Equal => return true,
        }
    }
    false
}

fn apply_delta_into(
    old: &[ContentId],
    added: &[ContentId],
    removed: &[ContentId],
    output: &mut Vec<ContentId>,
) -> Result<(), CollectionError> {
    debug_assert!(!sorted_slices_intersect(added, removed));
    let maximum = old.len().checked_add(added.len()).ok_or(
        CollectionError::ContentSetMemberLimitExceeded {
            limit: CONTENT_SET_MAX_MEMBERS,
        },
    )?;
    output.clear();
    if output.capacity() < maximum.min(CONTENT_SET_MAX_MEMBERS) {
        output.reserve(maximum.min(CONTENT_SET_MAX_MEMBERS));
    }
    let (mut oi, mut ai, mut ri) = (0, 0, 0);
    while oi < old.len() || ai < added.len() {
        let add = added.get(ai).copied();
        let existing = old.get(oi).copied();
        match (existing, add) {
            (Some(old_id), Some(add_id)) if add_id < old_id => {
                output.push(add_id);
                ai += 1;
            }
            (Some(old_id), _) => {
                while ri < removed.len() && removed[ri] < old_id {
                    ri += 1;
                }
                if removed.get(ri).copied() != Some(old_id) {
                    output.push(old_id);
                }
                oi += 1;
                if add == Some(old_id) {
                    ai += 1;
                }
            }
            (None, Some(add_id)) => {
                check_member_count(output.len().saturating_add(added.len() - ai))?;
                output.extend_from_slice(&added[ai..]);
                debug_assert_eq!(add_id, added[ai]);
                break;
            }
            (None, None) => break,
        }
        check_member_count(output.len())?;
    }
    Ok(())
}

fn walk_difference<E>(
    old: &[ContentId],
    new: &[ContentId],
    mut emit: impl FnMut(ContentId, bool) -> Result<(), E>,
) -> Result<(), E> {
    let (mut oi, mut ni) = (0, 0);
    while oi < old.len() && ni < new.len() {
        match old[oi].cmp(&new[ni]) {
            std::cmp::Ordering::Less => {
                emit(old[oi], false)?;
                oi += 1;
            }
            std::cmp::Ordering::Greater => {
                emit(new[ni], true)?;
                ni += 1;
            }
            std::cmp::Ordering::Equal => {
                oi += 1;
                ni += 1;
            }
        }
    }
    for &id in &old[oi..] {
        emit(id, false)?;
    }
    for &id in &new[ni..] {
        emit(id, true)?;
    }
    Ok(())
}

fn sorted_union(a: &[ContentId], b: &[ContentId]) -> Vec<ContentId> {
    let edits = b.iter().copied().map(|id| (id, true)).collect::<Vec<_>>();
    merge_edits(a, &edits).0
}

fn sorted_difference(a: &[ContentId], b: &[ContentId]) -> Vec<ContentId> {
    let edits = b.iter().copied().map(|id| (id, false)).collect::<Vec<_>>();
    merge_edits(a, &edits).0
}
