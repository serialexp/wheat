//! The checks full and incremental validation share: one revision, application, view
//! or conflict at a time. Full validation proves membership against indexes of the
//! whole state ([`VerificationScratch`]); incremental validation against the new
//! state's collections. Both prove the same invariants with the same code.
use std::collections::{BTreeMap, BTreeSet};

use crate::{
    ApplicationMapDelta, ApplicationMapDeltaLimits, ApplicationMapStore, ConflictSetStore,
    ContentMapDelta, ContentMapError, ContentMapStore, ImmutableObjectStore, MerkleTreeStore,
    RepositoryPublicationError, RepositoryReader, RevisionMapDelta, RevisionMapDeltaLimits,
    RevisionMapStore, TreeDiffLimits, verification_scratch::VerificationScratch,
};
use wheat_core::*;

type Result<T> = std::result::Result<T, RepositoryPublicationError>;

pub(crate) fn invalid(message: &'static str) -> RepositoryPublicationError {
    RepositoryPublicationError::InvalidReferences(message)
}

fn tree_delta_limit(error: &crate::MerkleTreeError) -> bool {
    matches!(
        error,
        crate::MerkleTreeError::ChangeLimitExceeded { .. }
            | crate::MerkleTreeError::NodeLimitExceeded { .. }
            | crate::MerkleTreeError::EntryLimitExceeded { .. }
            | crate::MerkleTreeError::PathBytesExceeded { .. }
            | crate::MerkleTreeError::DepthExceeded { .. }
            | crate::MerkleTreeError::NodeBytesExceeded { .. }
            | crate::MerkleTreeError::BufferedEntriesExceeded { .. }
    )
}

fn content_delta_limit(error: &ContentMapError) -> bool {
    matches!(
        error,
        ContentMapError::DeltaChangeLimitExceeded { .. }
            | ContentMapError::DeltaNodeLimitExceeded { .. }
            | ContentMapError::DeltaPendingLimitExceeded { .. }
    )
}

/// Whether a touched leaf occurs strictly below `path`. The ordered range avoids
/// scanning every edit for each directory encountered during verification.
fn has_touched_descendant(touched: &BTreeSet<String>, path: &str) -> bool {
    let prefix = format!("{path}/");
    touched
        .range(prefix.clone()..)
        .next()
        .is_some_and(|candidate| candidate.starts_with(&prefix))
}

/// The state's four retained-object sets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Retained {
    Changes,
    Views,
    Revisions,
    Applications,
}

impl Retained {
    /// The family [`VerificationScratch`] indexes the set under.
    pub(crate) const fn family(self) -> u8 {
        match self {
            Self::Changes => 1,
            Self::Views => 2,
            Self::Revisions => 3,
            Self::Applications => 4,
        }
    }
}

/// Membership in the state being validated. Each check fails with the reason full
/// validation has always given.
pub(crate) trait Membership {
    /// `id` is in the state's retained set of `kind`.
    async fn retained(&self, kind: Retained, id: ObjectId) -> Result<()>;
    /// `change` is a logical change of the state.
    async fn change(&self, change: ChangeId) -> Result<()>;
    /// `content` is in the state's aggregate content set.
    async fn content(&self, content: ContentId) -> Result<()>;
}

impl Membership for VerificationScratch {
    async fn retained(&self, kind: Retained, id: ObjectId) -> Result<()> {
        if !self.contains_retained(kind.family(), id)? {
            return Err(invalid("reference not retained"));
        }
        Ok(())
    }

    async fn change(&self, change: ChangeId) -> Result<()> {
        if !self.contains_change(change)? {
            return Err(invalid("logical change not retained"));
        }
        Ok(())
    }

    async fn content(&self, content: ContentId) -> Result<()> {
        if !self.contains_content(content)? {
            return Err(invalid("content absent from aggregate manifest"));
        }
        Ok(())
    }
}

/// A revision's edits replayed against their target, with what the replay read so callers
/// need not read it again.
pub(crate) struct Replay {
    pub(crate) definitions: BTreeMap<EditId, EditRecord>,
    /// The file values the edits' paths hold in the target; a path crossing a file is
    /// absent, as the commit writer records it.
    pub(crate) target: Tree,
    pub(crate) result: wheat_core::edit_application::ApplicationResult,
}

/// A view other than the base, and what composes it: the application `application` of
/// its last change onto `parent`.
pub(crate) struct Composition {
    pub(crate) parent: ViewRecord,
    pub(crate) application_id: ApplicationId,
    pub(crate) application: ApplicationRecord,
}

/// What an application was checked against.
pub(crate) struct ApplicationContext {
    pub(crate) revision: RevisionRecord,
    pub(crate) target: ViewRecord,
}

pub(crate) struct Checks<'r, 'a, O, M> {
    pub(crate) reader: &'r RepositoryReader<'a, O>,
    pub(crate) membership: &'r M,
}

impl<O, M> Checks<'_, '_, O, M>
where
    O: ImmutableObjectStore,
    M: Membership,
{
    /// The tree an application leaves: its clean tree, or its partial tree.
    pub(crate) fn result_tree(application: &ApplicationRecord) -> TreeId {
        match application.result {
            ApplicationResultRecord::Clean { tree } => tree,
            ApplicationResultRecord::Conflicted { partial_tree, .. } => partial_tree,
        }
    }

    pub(crate) async fn verify_untouched(
        &self,
        target_root: TreeId,
        result_root: TreeId,
        touched: &BTreeSet<String>,
    ) -> Result<()> {
        let trees = MerkleTreeStore::new(self.reader.objects);
        // `None` is an empty subtree. This matters when an exact edit creates the
        // first leaf below a directory, or removes the last one: Merkle trees do
        // not retain those otherwise-empty directories.
        let mut stack = vec![(String::new(), Some(target_root), Some(result_root), 0usize)];
        while let Some((prefix, target_id, result_id, depth)) = stack.pop() {
            if target_id == result_id {
                continue;
            }
            let target = match target_id {
                Some(id) => Some(trees.read_node(id).await?),
                None => None,
            };
            let result = match result_id {
                Some(id) => Some(trees.read_node(id).await?),
                None => None,
            };
            let names = target
                .as_ref()
                .into_iter()
                .flat_map(|node| node.entries.keys())
                .chain(
                    result
                        .as_ref()
                        .into_iter()
                        .flat_map(|node| node.entries.keys()),
                )
                .collect::<BTreeSet<_>>();
            for name in names {
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                let left = target.as_ref().and_then(|node| node.entries.get(name));
                let right = result.as_ref().and_then(|node| node.entries.get(name));
                let exact = touched.contains(&path);
                let below_touched = has_touched_descendant(touched, &path);
                match (left, right) {
                    (
                        Some(DirectoryEntry::Directory(left)),
                        Some(DirectoryEntry::Directory(right)),
                    ) => {
                        if left != right {
                            if !below_touched {
                                return Err(invalid("result changes an untouched path"));
                            }
                            if depth >= 1024 {
                                return Err(invalid("directory depth exceeds verification limit"));
                            }
                            stack.push((path, Some(*left), Some(*right), depth + 1));
                        }
                    }
                    // A missing directory is an empty subtree, rather than a
                    // structural mismatch. Walk its extant peer and ensure every
                    // changed leaf remains beneath an exact edit.
                    (Some(DirectoryEntry::Directory(left)), None) => {
                        if !below_touched {
                            return Err(invalid("result changes an untouched path"));
                        }
                        if depth >= 1024 {
                            return Err(invalid("directory depth exceeds verification limit"));
                        }
                        stack.push((path, Some(*left), None, depth + 1));
                    }
                    (None, Some(DirectoryEntry::Directory(right))) => {
                        if !below_touched {
                            return Err(invalid("result changes an untouched path"));
                        }
                        if depth >= 1024 {
                            return Err(invalid("directory depth exceeds verification limit"));
                        }
                        stack.push((path, None, Some(*right), depth + 1));
                    }
                    // The exact edit owns a file/directory boundary, but not the
                    // directory's contents. Compare that directory against an
                    // empty peer so a fabricated sibling still fails below.
                    (Some(DirectoryEntry::File(_)), Some(DirectoryEntry::Directory(right)))
                        if exact =>
                    {
                        if depth >= 1024 {
                            return Err(invalid("directory depth exceeds verification limit"));
                        }
                        stack.push((path, None, Some(*right), depth + 1));
                    }
                    (Some(DirectoryEntry::Directory(left)), Some(DirectoryEntry::File(_)))
                        if exact =>
                    {
                        if depth >= 1024 {
                            return Err(invalid("directory depth exceeds verification limit"));
                        }
                        stack.push((path, Some(*left), None, depth + 1));
                    }
                    // Descendant edits cannot replace an untouched file boundary.
                    (Some(DirectoryEntry::File(_)), Some(DirectoryEntry::Directory(_)))
                    | (Some(DirectoryEntry::Directory(_)), Some(DirectoryEntry::File(_))) => {
                        return Err(invalid("result changes untouched tree structure"));
                    }
                    _ if left != right && !exact => {
                        return Err(invalid("result changes an untouched path"));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Replays `edit_ids` on `target_root` and checks `result_root` is the outcome: equal
    /// outside the edits' paths, and holding the replayed values on them, each naming
    /// content in the aggregate. So `result_root` names only content `target_root` or the
    /// aggregate holds, whatever the replay's outcomes.
    pub(crate) async fn replay(
        &self,
        target_root: TreeId,
        result_root: TreeId,
        edit_ids: &BTreeSet<EditId>,
    ) -> Result<Replay> {
        let trees = MerkleTreeStore::new(self.reader.objects);
        let mut definitions = Vec::with_capacity(edit_ids.len());
        for id in edit_ids {
            definitions.push((*id, self.reader.edit(*id).await?));
        }
        let touched: BTreeSet<String> = definitions
            .iter()
            .map(|(_, edit)| edit.path.clone())
            .collect();
        // The commit writer treats a file encountered before a requested
        // descendant as an absent leaf. A simultaneous boundary deletion
        // can then replace that file with the requested directory.
        let paths = touched.iter().map(String::as_str).collect::<Vec<_>>();
        let shapes = trees.path_shapes(target_root, &paths).await?;
        let mut target = Tree::new();
        for (path, shape) in paths.into_iter().zip(shapes) {
            if let crate::merkle_tree::TreePathShape::File(value) = shape {
                target.insert(path.to_owned(), value);
            }
        }
        self.verify_untouched(target_root, result_root, &touched)
            .await?;
        let limits = wheat_core::edit_application::ApplicationLimits {
            allow_already_satisfied: true,
            ..Default::default()
        };
        let replay = wheat_core::edit_application::apply_edits(
            &target,
            definitions.iter().cloned(),
            |id| self.reader.content.get_verified(id),
            limits,
        )
        .map_err(|_| invalid("edit replay failed"))?;
        // `touched` is sorted, so one traversal checks every claimed value. A touched
        // path below a file in the result is absent: its directory became a file.
        let paths = touched.iter().map(String::as_str).collect::<Vec<_>>();
        let claimed = trees.path_shapes(result_root, &paths).await?;
        for (path, shape) in paths.into_iter().zip(claimed) {
            let claimed = match shape {
                crate::merkle_tree::TreePathShape::File(value) => Some(value),
                _ => None,
            };
            if claimed != replay.tree.get(path).copied() {
                return Err(invalid("claimed result differs from semantic replay"));
            }
            if let Some(value) = claimed {
                self.membership.content(value.content).await?;
            }
        }
        Ok(Replay {
            definitions: definitions.into_iter().collect(),
            target,
            result: replay,
        })
    }

    /// Checks I5 for one revision, apart from its resulting tree's content outside the
    /// paths it edits: its references are retained, its generation follows its parents,
    /// its transition partitions its edits and its predecessor's exactly, its
    /// subsumptions and resolutions name what they must, and its edits replay on the
    /// view it was authored against to its resulting tree.
    pub(crate) async fn revision(&self, revision: &RevisionRecord) -> Result<()> {
        self.membership
            .retained(Retained::Views, revision.authored_against.into())
            .await?;
        self.membership.change(revision.change_id).await?;
        for dependency in &revision.dependencies {
            self.membership.change(*dependency).await?;
        }
        let mut parent_generations = Vec::new();
        for id in revision.predecessor.iter().chain(&revision.combines) {
            self.membership
                .retained(Retained::Revisions, (*id).into())
                .await?;
            let parent = self.reader.revision(*id).await?;
            if parent.change_id != revision.change_id {
                return Err(invalid("predecessor belongs to another change"));
            }
            parent_generations.push(parent.generation);
        }
        if wheat_core::next_generation(parent_generations) != Some(revision.generation) {
            return Err(invalid("revision generation does not follow its parents"));
        }
        let transition = &revision.transition;
        if !transition.preserved.is_disjoint(&transition.added)
            || transition
                .preserved
                .union(&transition.added)
                .copied()
                .collect::<BTreeSet<_>>()
                != revision.edits
        {
            return Err(invalid("transition does not partition revision edits"));
        }
        let predecessor_edits = if let Some(predecessor) = revision.predecessor {
            self.reader.revision(predecessor).await?.edits
        } else {
            BTreeSet::new()
        };
        // Every predecessor edit is preserved or removed, none both (I5). Decoding
        // checked `added` disjoint from both, so it holds only new edits.
        if !transition.preserved.is_disjoint(&transition.removed)
            || transition
                .preserved
                .union(&transition.removed)
                .copied()
                .collect::<BTreeSet<_>>()
                != predecessor_edits
        {
            return Err(invalid("transition predecessor partition mismatch"));
        }
        for subsumption in &transition.subsumed {
            if !predecessor_edits.contains(&subsumption.source)
                || subsumption.replacements.is_empty()
                || !subsumption.replacements.is_subset(&revision.edits)
            {
                return Err(invalid("invalid edit subsumption"));
            }
        }
        for resolution in &transition.resolutions {
            self.membership
                .retained(Retained::Applications, resolution.application.into())
                .await?;
            if resolution.conflicted_edits.is_empty()
                || !resolution.replacement_edits.is_subset(&revision.edits)
            {
                return Err(invalid("invalid conflict resolution reference"));
            }
            let application = self.reader.application(resolution.application).await?;
            // A revision resolves a conflict of its predecessor's application, as
            // the mutator requires when it is authored.
            if Some(application.revision_id) != revision.predecessor {
                return Err(invalid("resolution application is not its predecessor's"));
            }
            let conflict = match application.result {
                ApplicationResultRecord::Conflicted { conflict, .. } => {
                    self.reader.conflict(conflict).await?
                }
                ApplicationResultRecord::Clean { .. } => {
                    return Err(invalid("resolution references clean application"));
                }
            };
            // It may resolve some of the conflict's failed edits, not only all.
            if !resolution
                .conflicted_edits
                .iter()
                .all(|edit| conflict.edits.contains_key(edit))
            {
                return Err(invalid("resolution conflict evidence mismatch"));
            }
        }
        for edit in &revision.edits {
            let definition = self.reader.edit(*edit).await?;
            match definition.operation {
                EditOperation::GuardedText {
                    before,
                    expected,
                    replacement,
                    after,
                    ..
                } => {
                    for id in [before, expected, replacement, after] {
                        self.membership.content(id).await?;
                    }
                }
                EditOperation::ExactFile {
                    expected,
                    replacement,
                } => {
                    for id in expected
                        .into_iter()
                        .chain(replacement)
                        .map(|value| value.content)
                    {
                        self.membership.content(id).await?;
                    }
                }
            }
        }
        let authored = self.reader.view(revision.authored_against).await?;
        let replay = self
            .replay(authored.tree, revision.resulting_tree, &revision.edits)
            .await?;
        // A combined revision follows a reapplied revision's rule: an edit already
        // satisfied where it landed is fine. Any other revision's edits all applied.
        let combined = !revision.combines.is_empty();
        if replay.result.outcomes.values().any(|outcome| {
            !matches!(
                outcome,
                wheat_core::edit_application::ApplicationOutcome::Applied { .. }
            ) && !(combined
                && matches!(
                    outcome,
                    wheat_core::edit_application::ApplicationOutcome::AlreadySatisfied { .. }
                ))
        }) {
            return Err(invalid("authored revision does not apply cleanly"));
        }
        Ok(())
    }

    /// Checks a conflict's evidence names retained views, aggregate content and edits
    /// that decode.
    pub(crate) async fn conflict(&self, id: ConflictId) -> Result<()> {
        let conflict = self.reader.conflict(id).await?;
        self.membership
            .retained(Retained::Views, conflict.base_view.into())
            .await?;
        for evidence in conflict.edits.values() {
            if let Some(value) = evidence.target {
                self.membership.content(value.content).await?;
            }
            if let EditFailure::Interference { with } = &evidence.reason {
                if with.is_empty() {
                    return Err(invalid("empty edit interference evidence"));
                }
                for edit in with {
                    self.reader.edit(*edit).await?;
                }
            }
        }
        Ok(())
    }

    /// Checks I6 for one application, apart from its result tree's content outside the
    /// paths its edits touch: its revision and target are retained, and its evidence,
    /// result tree and conflict are a replay of its revision's edits on its target.
    pub(crate) async fn application(&self, app: &ApplicationRecord) -> Result<ApplicationContext> {
        self.membership
            .retained(Retained::Revisions, app.revision_id.into())
            .await?;
        self.membership
            .retained(Retained::Views, app.target_view.into())
            .await?;
        let revision = self.reader.revision(app.revision_id).await?;
        let target = self.reader.view(app.target_view).await?;
        let claimed_conflict = match app.result {
            ApplicationResultRecord::Clean { .. } => None,
            ApplicationResultRecord::Conflicted { conflict, .. } => {
                let conflict_record = self.reader.conflict(conflict).await?;
                if conflict_record.base_view != app.target_view {
                    return Err(invalid("application conflict target mismatch"));
                }
                self.conflict(conflict).await?;
                Some(conflict_record)
            }
        };
        let replay = self
            .replay(target.tree, Self::result_tree(app), &revision.edits)
            .await?;
        let mut replay_applied = BTreeMap::new();
        let mut replay_failed = BTreeMap::new();
        for (edit, outcome) in replay.result.outcomes {
            let target_value = replay.target.get(&replay.definitions[&edit].path).copied();
            match outcome {
                wheat_core::edit_application::ApplicationOutcome::Applied { read, write } => {
                    replay_applied.insert(
                        edit,
                        AppliedEditEvidence {
                            target: target_value,
                            read: read.map(|r| ByteRange {
                                start: r.start as u64,
                                end: r.end as u64,
                            }),
                            write: write.map(|r| ByteRange {
                                start: r.start as u64,
                                end: r.end as u64,
                            }),
                        },
                    );
                }
                wheat_core::edit_application::ApplicationOutcome::AlreadySatisfied { read } => {
                    replay_applied.insert(
                        edit,
                        AppliedEditEvidence {
                            target: target_value,
                            read: read.map(|r| ByteRange {
                                start: r.start as u64,
                                end: r.end as u64,
                            }),
                            write: None,
                        },
                    );
                }
                outcome => {
                    let reason = match outcome {
                        wheat_core::edit_application::ApplicationOutcome::NoMatch => EditFailure::NoMatch,
                        wheat_core::edit_application::ApplicationOutcome::Ambiguous => EditFailure::Ambiguous,
                        wheat_core::edit_application::ApplicationOutcome::Interference { with } => EditFailure::Interference { with: with.into_iter().collect() },
                        wheat_core::edit_application::ApplicationOutcome::KindMismatch { .. } => EditFailure::KindMismatch,
                        wheat_core::edit_application::ApplicationOutcome::ExpectedFileMismatch { .. } => EditFailure::ExpectedFileMismatch,
                        wheat_core::edit_application::ApplicationOutcome::Applied { .. }
                        | wheat_core::edit_application::ApplicationOutcome::AlreadySatisfied { .. } => unreachable!(),
                    };
                    replay_failed.insert(
                        edit,
                        FailedEditEvidence {
                            target: target_value,
                            reason,
                        },
                    );
                }
            }
        }
        if replay_applied != app.applied {
            return Err(invalid("application evidence differs from semantic replay"));
        }
        match claimed_conflict {
            None if !replay_failed.is_empty() => {
                return Err(invalid("clean application has failed edits"));
            }
            Some(conflict) if conflict.edits != replay_failed => {
                return Err(invalid("conflict evidence differs from semantic replay"));
            }
            Some(_) if replay_failed.is_empty() => {
                return Err(invalid("conflicted application replayed cleanly"));
            }
            _ => {}
        }
        Ok(ApplicationContext { revision, target })
    }

    /// Checks where `application` (`id`) lands on its target `parent`: the target does
    /// not already select it, its order index follows the target's, and its change comes
    /// last in the canonical order of `parent`'s changes and it (I2): every change of
    /// `parent` after the last of its dependencies has a lower ID. O(log n) application
    /// reads, and one more per dependency.
    pub(crate) async fn application_placement(
        &self,
        id: ApplicationId,
        application: &ApplicationRecord,
        revision: &RevisionRecord,
        parent: &ViewRecord,
    ) -> Result<()> {
        let applications = ApplicationMapStore::new(self.reader.objects);
        if applications
            .get(parent.applications, revision.change_id)
            .await?
            == Some(id)
        {
            return Err(invalid("application target already selects application"));
        }
        let expected = self
            .reader
            .application_order(parent, revision.change_id)
            .await?;
        if application.order != expected {
            return Err(invalid(
                "application order index does not follow its target",
            ));
        }
        let mut after_dependencies = 0;
        for dependency in &revision.dependencies {
            let id = applications
                .get(parent.applications, *dependency)
                .await?
                .ok_or_else(|| invalid("application target lacks a dependency of its revision"))?;
            let position = self.reader.application(id).await?.order.position;
            after_dependencies = after_dependencies.max(position + 1);
        }
        if self
            .reader
            .highest_change_from(parent, after_dependencies)
            .await?
            .is_some_and(|highest| highest >= revision.change_id)
        {
            return Err(invalid("application is out of canonical order"));
        }
        Ok(())
    }

    /// Checks the base rule: the base view selects nothing.
    pub(crate) async fn base(&self, view: &ViewRecord) -> Result<()> {
        if view.order != ViewOrder::BASE {
            return Err(invalid("base view has changes in its order"));
        }
        let revisions = RevisionMapStore::new(self.reader.objects)
            .cursor(view.revisions)
            .await?
            .next()
            .await?;
        let applications = ApplicationMapStore::new(self.reader.objects)
            .cursor(view.applications)
            .await?
            .next()
            .await?;
        let conflicts = ConflictSetStore::new(self.reader.objects)
            .cursor(view.unresolved_conflicts)
            .await?
            .next()
            .await?;
        if revisions.is_some() || applications.is_some() || conflicts.is_some() {
            return Err(invalid("base view selects changes"));
        }
        Ok(())
    }

    /// Checks I1 for a view other than the base: it is its composition parent `T` (the
    /// target of its last change `k`'s application `A`) with `A` added. There is an edge
    /// `T → view` (`has_edge_from(T)`), its maps are `T`'s plus `k` and nothing else,
    /// `k`'s revision is `A`'s and of change `k`, its tree is `A`'s result, and its size,
    /// last change and weight follow `T`'s. The caller checks the conflicts and the
    /// content map.
    ///
    /// The map diffs stop at a second difference, so each reads the path to `k`.
    pub(crate) async fn composition(
        &self,
        view: &ViewRecord,
        has_edge_from: impl FnOnce(ViewId) -> bool,
    ) -> Result<Composition> {
        let change = view
            .order
            .last
            .ok_or_else(|| invalid("non-base view has no last change"))?;
        let revision_id = RevisionMapStore::new(self.reader.objects)
            .get(view.revisions, change)
            .await?
            .ok_or_else(|| invalid("view's last change is not in its revision map"))?;
        let application_id = ApplicationMapStore::new(self.reader.objects)
            .get(view.applications, change)
            .await?
            .ok_or_else(|| invalid("view's last change has no application"))?;
        let application = self.reader.application(application_id).await?;
        if application.revision_id != revision_id {
            return Err(invalid("view's last application is of another revision"));
        }
        let revision = self.reader.revision(revision_id).await?;
        if revision.change_id != change {
            return Err(invalid("selected revision change mismatch"));
        }
        let parent = self.reader.view(application.target_view).await?;
        if parent.order.then(change, revision.generation) != Some(view.order) {
            return Err(invalid("view order does not follow its composition parent"));
        }
        if !has_edge_from(application.target_view) {
            return Err(invalid(
                "view lacks a lineage edge from its composition parent",
            ));
        }
        if view.tree != Self::result_tree(&application) {
            return Err(invalid(
                "view tree is not the result of its last application",
            ));
        }
        let not_composed = || invalid("view maps are not its composition parent's plus one change");
        let revisions = match RevisionMapStore::new(self.reader.objects)
            .delta(
                parent.revisions,
                view.revisions,
                RevisionMapDeltaLimits {
                    max_selections: 1,
                    ..RevisionMapDeltaLimits::default()
                },
            )
            .await
        {
            Err(crate::RevisionMapError::DeltaSelectionLimitExceeded { .. }) => {
                return Err(not_composed());
            }
            delta => delta?,
        };
        if revisions
            != [RevisionMapDelta::Added {
                change,
                revision: revision_id,
            }]
        {
            return Err(not_composed());
        }
        let applications = match ApplicationMapStore::new(self.reader.objects)
            .delta(
                parent.applications,
                view.applications,
                ApplicationMapDeltaLimits {
                    max_changes: 1,
                    ..ApplicationMapDeltaLimits::default()
                },
            )
            .await
        {
            Err(crate::ApplicationMapError::DeltaChangeLimitExceeded { .. }) => {
                return Err(not_composed());
            }
            delta => delta?,
        };
        if applications
            != [ApplicationMapDelta::Added {
                change,
                application: application_id,
            }]
        {
            return Err(not_composed());
        }
        Ok(Composition {
            parent,
            application_id,
            application,
        })
    }

    /// The view's content map counts exactly the tree entries naming each blob, all in
    /// the aggregate (I7). Reads the whole tree and map: the fallback when an edge can't
    /// be checked by diff.
    pub(crate) async fn content_map_matches_tree(
        &self,
        scratch: &VerificationScratch,
        tree: TreeId,
        map: ContentMapId,
    ) -> Result<()> {
        scratch.clear_tree_content_counts()?;
        let trees = MerkleTreeStore::new(self.reader.objects);
        let mut contents = trees.content_cursor(tree, Default::default()).await?;
        while let Some(file) = contents.next().await? {
            scratch.adjust_tree_content_count(file.content, 1)?;
        }
        let mut entries = ContentMapStore::new(self.reader.objects)
            .cursor(map)
            .await?;
        let mut count = 0u64;
        while let Some((content, named)) = entries.next().await? {
            if scratch.tree_content_count(content)? != u64::from(named) {
                return Err(invalid("view content map differs from its tree"));
            }
            self.membership.content(content).await?;
            count += 1;
        }
        if count != scratch.tree_content_entries()? {
            return Err(invalid("view content map differs from its tree"));
        }
        Ok(())
    }

    /// Checks a lineage edge whose parent map is already proven equal to the parent's
    /// tree: the child's map must differ from the parent's by exactly the tree diff, so
    /// it equals the child's tree too, and every blob the child's tree gains is in the
    /// aggregate. Returns false when a diff exceeds its limits and the edge needs
    /// [`Self::content_map_matches_tree`] instead.
    pub(crate) async fn content_map_follows_tree_diff(
        &self,
        parent: &ViewRecord,
        child: &ViewRecord,
    ) -> Result<bool> {
        let mut net = BTreeMap::<ContentId, i64>::new();
        let mut diff = MerkleTreeStore::new(self.reader.objects).diff_cursor(
            parent.tree,
            child.tree,
            TreeDiffLimits::default(),
        );
        loop {
            let change = match diff.next().await {
                Ok(Some(change)) => change,
                Ok(None) => break,
                Err(error) if tree_delta_limit(&error) => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            if let Some(old) = change.old {
                *net.entry(old.content).or_default() -= 1;
            }
            if let Some(new) = change.new {
                self.membership.content(new.content).await?;
                *net.entry(new.content).or_default() += 1;
            }
        }
        net.retain(|_, change| *change != 0);
        let changes = match ContentMapStore::new(self.reader.objects)
            .delta(
                parent.required_content,
                child.required_content,
                Default::default(),
            )
            .await
        {
            Ok(changes) => changes,
            Err(error) if content_delta_limit(&error) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if changes.len() != net.len() {
            return Err(invalid("view content map differs from its tree"));
        }
        for change in changes {
            let (content, change) = match change {
                ContentMapDelta::Added { content, count } => (content, i64::from(count)),
                ContentMapDelta::Changed { content, old, new } => {
                    (content, i64::from(new) - i64::from(old))
                }
                ContentMapDelta::Removed { content, count } => (content, -i64::from(count)),
            };
            if net.get(&content) != Some(&change) {
                return Err(invalid("view content map differs from its tree"));
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod untouched_tests {
    use super::*;
    use crate::LooseObjectStore;

    fn file(byte: u8) -> FileValue {
        FileValue {
            content: ContentId::from_digest([byte; 32]),
            kind: FileKind::Regular,
        }
    }

    async fn node(
        trees: &MerkleTreeStore<'_, LooseObjectStore>,
        entries: impl IntoIterator<Item = (&str, DirectoryEntry)>,
    ) -> TreeId {
        trees
            .write_node(&DirectoryNode {
                entries: entries
                    .into_iter()
                    .map(|(name, entry)| (name.to_owned(), entry))
                    .collect(),
            })
            .await
            .unwrap()
    }

    async fn verify(
        objects: &LooseObjectStore,
        target: TreeId,
        result: TreeId,
        touched: &[&str],
    ) -> Result<()> {
        let directory = tempfile::tempdir().unwrap();
        let content = crate::ContentStore::new(directory.path());
        let reader = RepositoryReader::new(objects, &content);
        let scratch = VerificationScratch::new().unwrap();
        Checks {
            reader: &reader,
            membership: &scratch,
        }
        .verify_untouched(
            target,
            result,
            &touched.iter().map(|path| (*path).to_owned()).collect(),
        )
        .await
    }

    #[tokio::test]
    async fn untouched_allows_empty_nested_creation_and_removal_but_not_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::new(dir.path());
        let trees = MerkleTreeStore::new(&objects);
        let empty = node(&trees, []).await;
        let nested = node(&trees, [("leaf", DirectoryEntry::File(file(1)))]).await;
        let created = node(&trees, [("parent", DirectoryEntry::Directory(nested))]).await;
        verify(&objects, empty, created, &["parent/leaf"])
            .await
            .unwrap();
        verify(&objects, created, empty, &["parent/leaf"])
            .await
            .unwrap();
        let fabricated = node(
            &trees,
            [
                ("leaf", DirectoryEntry::File(file(1))),
                ("extra", DirectoryEntry::File(file(2))),
            ],
        )
        .await;
        let fabricated = node(&trees, [("parent", DirectoryEntry::Directory(fabricated))]).await;
        assert!(
            verify(&objects, empty, fabricated, &["parent/leaf"])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn untouched_requires_and_honors_explicit_file_directory_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::new(dir.path());
        let trees = MerkleTreeStore::new(&objects);
        let target = node(&trees, [("shape", DirectoryEntry::File(file(1)))]).await;
        let child = node(&trees, [("nested", DirectoryEntry::File(file(2)))]).await;
        let directory = node(&trees, [("shape", DirectoryEntry::Directory(child))]).await;
        assert!(
            verify(&objects, target, directory, &["shape/nested"])
                .await
                .is_err()
        );
        verify(&objects, target, directory, &["shape", "shape/nested"])
            .await
            .unwrap();
        let extra = node(
            &trees,
            [
                ("nested", DirectoryEntry::File(file(2))),
                ("extra", DirectoryEntry::File(file(3))),
            ],
        )
        .await;
        let extra = node(&trees, [("shape", DirectoryEntry::Directory(extra))]).await;
        assert!(
            verify(&objects, target, extra, &["shape", "shape/nested"])
                .await
                .is_err()
        );
        assert!(
            verify(&objects, directory, target, &["shape/nested"])
                .await
                .is_err()
        );
        verify(&objects, directory, target, &["shape", "shape/nested"])
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod content_map_tests {
    use super::*;
    use crate::LooseObjectStore;

    fn blob(byte: u8) -> ContentId {
        ContentId::from_digest([byte; 32])
    }

    async fn tree(objects: &LooseObjectStore, files: &[(&str, u8)]) -> TreeId {
        MerkleTreeStore::new(objects)
            .write_node(&DirectoryNode {
                entries: files
                    .iter()
                    .map(|(name, byte)| {
                        let file = FileValue {
                            content: blob(*byte),
                            kind: FileKind::Regular,
                        };
                        ((*name).to_owned(), DirectoryEntry::File(file))
                    })
                    .collect(),
            })
            .await
            .unwrap()
    }

    async fn map(objects: &LooseObjectStore, counts: &[(u8, i64)]) -> ContentMapId {
        let maps = ContentMapStore::new(objects);
        maps.adjust(
            maps.empty_root().await.unwrap(),
            counts.iter().map(|(byte, count)| (blob(*byte), *count)),
        )
        .await
        .unwrap()
    }

    fn view(tree: TreeId, required_content: ContentMapId) -> ViewRecord {
        ViewRecord {
            tree,
            revisions: RevisionMapId::from_digest([0; 32]),
            applications: ApplicationMapId::from_digest([0; 32]),
            unresolved_conflicts: ConflictSetId::from_digest([0; 32]),
            required_content,
            order: ViewOrder::BASE,
        }
    }

    /// Runs `check` with checks whose aggregate holds blobs 1 to 9, and a scratch for
    /// the full comparison.
    async fn with_checks<T>(
        objects: &LooseObjectStore,
        check: impl AsyncFnOnce(
            &Checks<'_, '_, LooseObjectStore, VerificationScratch>,
            &VerificationScratch,
        ) -> T,
    ) -> T {
        let directory = tempfile::tempdir().unwrap();
        let content = crate::ContentStore::new(directory.path());
        let reader = RepositoryReader::new(objects, &content);
        let aggregate = VerificationScratch::new().unwrap();
        for byte in 1..=9 {
            aggregate.insert_content(blob(byte)).unwrap();
        }
        let scratch = VerificationScratch::new().unwrap();
        check(
            &Checks {
                reader: &reader,
                membership: &aggregate,
            },
            &scratch,
        )
        .await
    }

    /// The full comparison, the fallback for an edge too large to diff: every count
    /// must match, and the map may name nothing the tree does not.
    #[tokio::test]
    async fn a_map_must_count_exactly_the_tree_entries() {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::new(dir.path());
        let files = tree(&objects, &[("a", 1), ("b", 1), ("c", 2)]).await;
        let exact = map(&objects, &[(1, 2), (2, 1)]).await;
        let undercounted = map(&objects, &[(1, 1), (2, 1)]).await;
        let extra = map(&objects, &[(1, 2), (2, 1), (3, 1)]).await;
        let missing = map(&objects, &[(1, 2)]).await;
        with_checks(&objects, async |checks, scratch| {
            checks
                .content_map_matches_tree(scratch, files, exact)
                .await
                .unwrap();
            for forged in [undercounted, extra, missing] {
                assert!(
                    checks
                        .content_map_matches_tree(scratch, files, forged)
                        .await
                        .is_err()
                );
            }
            // Earlier counts do not leak into the next comparison.
            checks
                .content_map_matches_tree(scratch, files, exact)
                .await
                .unwrap();
        })
        .await;
    }

    /// The full comparison also requires every blob to be in the aggregate.
    #[tokio::test]
    async fn a_fully_compared_map_must_name_aggregate_content() {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::new(dir.path());
        let files = tree(&objects, &[("a", 1), ("b", 42)]).await;
        let exact = map(&objects, &[(1, 1), (42, 1)]).await;
        with_checks(&objects, async |checks, scratch| {
            assert!(
                checks
                    .content_map_matches_tree(scratch, files, exact)
                    .await
                    .is_err()
            );
        })
        .await;
    }

    /// The incremental check: a child map must differ from its parent's by exactly
    /// the net count changes of the tree diff.
    #[tokio::test]
    async fn a_child_map_must_follow_the_tree_diff() {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::new(dir.path());
        let parent = view(
            tree(&objects, &[("a", 1), ("b", 1)]).await,
            map(&objects, &[(1, 2)]).await,
        );
        // `b` now names blob 2, and new file `c` names blob 1.
        let child_tree = tree(&objects, &[("a", 1), ("b", 2), ("c", 1)]).await;
        let exact = map(&objects, &[(1, 2), (2, 1)]).await;
        let forged = [
            map(&objects, &[(1, 1), (2, 1)]).await,
            map(&objects, &[(1, 2), (2, 2)]).await,
            map(&objects, &[(1, 2), (2, 1), (3, 1)]).await,
            map(&objects, &[(1, 2)]).await,
        ];
        with_checks(&objects, async |checks, _| {
            assert!(
                checks
                    .content_map_follows_tree_diff(&parent, &view(child_tree, exact))
                    .await
                    .unwrap()
            );
            for forged in forged {
                assert!(
                    checks
                        .content_map_follows_tree_diff(&parent, &view(child_tree, forged))
                        .await
                        .is_err()
                );
            }
        })
        .await;
    }

    /// A blob a child gains must be in the aggregate, even when its map is right.
    #[tokio::test]
    async fn a_gained_blob_must_be_in_the_aggregate() {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::new(dir.path());
        let parent = view(
            tree(&objects, &[("a", 1)]).await,
            map(&objects, &[(1, 1)]).await,
        );
        let child = view(
            tree(&objects, &[("a", 1), ("b", 42)]).await,
            map(&objects, &[(1, 1), (42, 1)]).await,
        );
        with_checks(&objects, async |checks, _| {
            assert!(
                checks
                    .content_map_follows_tree_diff(&parent, &child)
                    .await
                    .is_err()
            );
        })
        .await;
    }
}
