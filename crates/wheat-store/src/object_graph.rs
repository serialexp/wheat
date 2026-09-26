//! What each stored object refers to: the one list every closure walk uses.
//!
//! A snapshot export follows every reference of every object to list a whole closure.
//! [`crate::StructuralDelta`] follows them too, pairing each with its counterpart on
//! the old side, to find what one envelope has that another does not. Both take their
//! references from here, so they cannot disagree about what a closure is.
//!
//! There is one function per kind of object. Each names the place a reference sits
//! with a site enum of its own. A new reference is a new site: the delta walk matches
//! sites without a catch-all, so it does not compile until it handles the new one, and
//! [`for_each_reference`] follows it for the snapshot walk.

use wheat_core::{
    ApplicationRecord, ApplicationResultRecord, CanonicalObjectError, Change, Conflict, ContentId,
    ContentManifestRecord, ContentSetBlob, DirectoryEntry, DirectoryNode, EditFailure,
    EditOperation, EditRecord, ObjectId, RepositoryEnvelopeRecord, RepositoryStateRecord,
    RevisionRecord, ViewLabelManifestRecord, ViewRecord,
};

use crate::{
    ObjectKind, StoredObject,
    application_map::ApplicationCollection,
    change_map::ChangeCollection,
    radix::{self, Collection, Key as _, Node},
    revision_map::RevisionCollection,
    sets::{ConflictCollection, RetainedCollection},
    view_lineage::ViewLineageCollection,
};

/// A reference from one object: to another object, at the place `S` names, or to
/// content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Ref<S> {
    Object(S, ObjectId),
    Content(ContentId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EnvelopeSite {
    State,
    Labels,
}

pub(crate) fn envelope<E>(
    record: &RepositoryEnvelopeRecord,
    mut f: impl FnMut(Ref<EnvelopeSite>) -> Result<(), E>,
) -> Result<(), E> {
    f(Ref::Object(EnvelopeSite::State, record.state.into()))?;
    f(Ref::Object(EnvelopeSite::Labels, record.labels.into()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LabelSite {
    View,
}

pub(crate) fn labels<E>(
    record: &ViewLabelManifestRecord,
    mut f: impl FnMut(Ref<LabelSite>) -> Result<(), E>,
) -> Result<(), E> {
    for label in record.labels.values() {
        f(Ref::Object(LabelSite::View, label.view.into()))?;
    }
    Ok(())
}

/// The fields of a repository state. All but the base view and the required content
/// are roots of radix collections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StateSite {
    BaseView,
    Changes,
    ViewLineage,
    RetainedChanges,
    RetainedViews,
    RetainedRevisions,
    RetainedApplications,
    RequiredContent,
}

pub(crate) fn state<E>(
    record: &RepositoryStateRecord,
    mut f: impl FnMut(Ref<StateSite>) -> Result<(), E>,
) -> Result<(), E> {
    for (site, id) in [
        (StateSite::BaseView, record.base_view.into()),
        (StateSite::Changes, record.changes.into()),
        (StateSite::ViewLineage, record.view_lineage.into()),
        (StateSite::RetainedChanges, record.retained_changes.into()),
        (StateSite::RetainedViews, record.retained_views.into()),
        (
            StateSite::RetainedRevisions,
            record.retained_revisions.into(),
        ),
        (
            StateSite::RetainedApplications,
            record.retained_applications.into(),
        ),
        (StateSite::RequiredContent, record.required_content.into()),
    ] {
        f(Ref::Object(site, id))?;
    }
    Ok(())
}

/// The fields of a view. The revisions, applications and unresolved conflicts are
/// roots of radix collections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ViewSite {
    Tree,
    Revisions,
    Applications,
    UnresolvedConflicts,
    RequiredContent,
}

pub(crate) fn view<E>(
    record: &ViewRecord,
    mut f: impl FnMut(Ref<ViewSite>) -> Result<(), E>,
) -> Result<(), E> {
    for (site, id) in [
        (ViewSite::Tree, record.tree.into()),
        (ViewSite::Revisions, record.revisions.into()),
        (ViewSite::Applications, record.applications.into()),
        (
            ViewSite::UnresolvedConflicts,
            record.unresolved_conflicts.into(),
        ),
        (ViewSite::RequiredContent, record.required_content.into()),
    ] {
        f(Ref::Object(site, id))?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManifestSite {
    ContentSet,
}

pub(crate) fn manifest<E>(
    record: &ContentManifestRecord,
    mut f: impl FnMut(Ref<ManifestSite>) -> Result<(), E>,
) -> Result<(), E> {
    f(Ref::Object(ManifestSite::ContentSet, record.content.into()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContentSetSite {
    /// The set a delta applies to.
    Base,
}

/// Decodes a content set if it can refer to anything, which only a delta does. A
/// snapshot is not decoded: at 100,000 files it is about 3.3 MB.
pub(crate) fn decode_content_set(
    stored: &StoredObject,
) -> Result<Option<ContentSetBlob>, CanonicalObjectError> {
    if stored.kind == ObjectKind::ContentSetDelta {
        wheat_core::decode_content_set_blob(&stored.canonical_bytes).map(Some)
    } else {
        Ok(None)
    }
}

/// A content set's members are not listed: walks gather content from the trees and
/// edits that use it.
pub(crate) fn content_set<E>(
    blob: &ContentSetBlob,
    mut f: impl FnMut(Ref<ContentSetSite>) -> Result<(), E>,
) -> Result<(), E> {
    match blob {
        ContentSetBlob::Snapshot(_) => Ok(()),
        ContentSetBlob::Delta { base, .. } => f(Ref::Object(ContentSetSite::Base, (*base).into())),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TreeSite<'a> {
    /// The subdirectory with this name.
    Directory(&'a str),
}

pub(crate) fn tree<'a, E>(
    node: &'a DirectoryNode,
    mut f: impl FnMut(Ref<TreeSite<'a>>) -> Result<(), E>,
) -> Result<(), E> {
    for (name, entry) in &node.entries {
        match entry {
            DirectoryEntry::Directory(id) => {
                f(Ref::Object(TreeSite::Directory(name), (*id).into()))?;
            }
            DirectoryEntry::File(value) => f(Ref::Content(value.content))?,
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RevisionSite {
    Predecessor,
    AuthoredAgainst,
    Tree,
    /// An edit of the revision, or one its transition names.
    Edit,
    /// The conflicted application a resolution resolves.
    ResolutionApplication,
}

/// The transition's preserved, added and removed edits are not listed again:
/// verification requires each of them to be one of `edits` or an edit of the
/// predecessor.
pub(crate) fn revision<E>(
    record: &RevisionRecord,
    mut f: impl FnMut(Ref<RevisionSite>) -> Result<(), E>,
) -> Result<(), E> {
    if let Some(id) = record.predecessor {
        f(Ref::Object(RevisionSite::Predecessor, id.into()))?;
    }
    f(Ref::Object(
        RevisionSite::AuthoredAgainst,
        record.authored_against.into(),
    ))?;
    f(Ref::Object(
        RevisionSite::Tree,
        record.resulting_tree.into(),
    ))?;
    for id in &record.edits {
        f(Ref::Object(RevisionSite::Edit, (*id).into()))?;
    }
    for resolution in &record.transition.resolutions {
        f(Ref::Object(
            RevisionSite::ResolutionApplication,
            resolution.application.into(),
        ))?;
        for id in resolution
            .conflicted_edits
            .iter()
            .chain(&resolution.replacement_edits)
        {
            f(Ref::Object(RevisionSite::Edit, (*id).into()))?;
        }
    }
    for subsumption in &record.transition.subsumed {
        for id in std::iter::once(&subsumption.source).chain(&subsumption.replacements) {
            f(Ref::Object(RevisionSite::Edit, (*id).into()))?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApplicationSite {
    Revision,
    TargetView,
    /// The resulting tree, or the partial tree of a conflicted application.
    Tree,
    Conflict,
    /// An edit that applied.
    Edit,
}

pub(crate) fn application<E>(
    record: &ApplicationRecord,
    mut f: impl FnMut(Ref<ApplicationSite>) -> Result<(), E>,
) -> Result<(), E> {
    f(Ref::Object(
        ApplicationSite::Revision,
        record.revision_id.into(),
    ))?;
    f(Ref::Object(
        ApplicationSite::TargetView,
        record.target_view.into(),
    ))?;
    match record.result {
        ApplicationResultRecord::Clean { tree } => {
            f(Ref::Object(ApplicationSite::Tree, tree.into()))?;
        }
        ApplicationResultRecord::Conflicted {
            partial_tree,
            conflict,
        } => {
            f(Ref::Object(ApplicationSite::Tree, partial_tree.into()))?;
            f(Ref::Object(ApplicationSite::Conflict, conflict.into()))?;
        }
    }
    for (edit, evidence) in &record.applied {
        f(Ref::Object(ApplicationSite::Edit, (*edit).into()))?;
        if let Some(value) = evidence.target {
            f(Ref::Content(value.content))?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConflictSite {
    BaseView,
    /// An edit that failed, or one it interfered with.
    Edit,
}

pub(crate) fn conflict<E>(
    record: &Conflict,
    mut f: impl FnMut(Ref<ConflictSite>) -> Result<(), E>,
) -> Result<(), E> {
    f(Ref::Object(ConflictSite::BaseView, record.base_view.into()))?;
    for (edit, evidence) in &record.edits {
        f(Ref::Object(ConflictSite::Edit, (*edit).into()))?;
        if let Some(value) = evidence.target {
            f(Ref::Content(value.content))?;
        }
        if let EditFailure::Interference { with } = &evidence.reason {
            for id in with {
                f(Ref::Object(ConflictSite::Edit, (*id).into()))?;
            }
        }
    }
    Ok(())
}

/// An edit refers to content only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EditSite {}

pub(crate) fn edit<E>(
    record: &EditRecord,
    mut f: impl FnMut(Ref<EditSite>) -> Result<(), E>,
) -> Result<(), E> {
    match &record.operation {
        EditOperation::GuardedText {
            before,
            expected,
            replacement,
            after,
            ..
        } => {
            for id in [before, expected, replacement, after] {
                f(Ref::Content(*id))?;
            }
        }
        EditOperation::ExactFile {
            expected,
            replacement,
        } => {
            for value in expected.iter().chain(replacement) {
                f(Ref::Content(value.content))?;
            }
        }
    }
    Ok(())
}

/// A change refers to nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChangeSite {}

pub(crate) fn change<E>(
    _record: &Change,
    _f: impl FnMut(Ref<ChangeSite>) -> Result<(), E>,
) -> Result<(), E> {
    Ok(())
}

/// A radix node of any collection: values are object IDs, or nothing for sets.
pub(crate) type RadixNode = Node<ObjectId, Option<ObjectId>>;

/// The kinds of radix node, and what their keys and values are.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum RadixKind {
    /// A set of objects: each key is a member.
    RetainedSet,
    /// A set of conflicts: each key is a member.
    ConflictSet,
    /// Change → its current revision.
    RevisionMap,
    /// Revision → its application.
    ApplicationMap,
    /// Change ID → change object.
    ChangeMap,
    /// Parent view → the root of the set of its children.
    ViewLineage,
}

impl RadixKind {
    pub(crate) fn of(kind: ObjectKind) -> Option<Self> {
        Some(match kind {
            ObjectKind::RetainedSetNode => Self::RetainedSet,
            ObjectKind::ConflictSetNode => Self::ConflictSet,
            ObjectKind::RevisionMapNode => Self::RevisionMap,
            ObjectKind::ApplicationMapNode => Self::ApplicationMap,
            ObjectKind::ChangeMapNode => Self::ChangeMap,
            ObjectKind::ViewLineageMapNode => Self::ViewLineage,
            _ => return None,
        })
    }

    pub(crate) fn object_kind(self) -> ObjectKind {
        match self {
            Self::RetainedSet => ObjectKind::RetainedSetNode,
            Self::ConflictSet => ObjectKind::ConflictSetNode,
            Self::RevisionMap => ObjectKind::RevisionMapNode,
            Self::ApplicationMap => ObjectKind::ApplicationMapNode,
            Self::ChangeMap => ObjectKind::ChangeMapNode,
            Self::ViewLineage => ObjectKind::ViewLineageMapNode,
        }
    }

    pub(crate) fn key_len(self) -> usize {
        match self {
            Self::RetainedSet => <RetainedCollection as Collection>::Key::LEN,
            Self::ConflictSet => <ConflictCollection as Collection>::Key::LEN,
            Self::RevisionMap => <RevisionCollection as Collection>::Key::LEN,
            Self::ApplicationMap => <ApplicationCollection as Collection>::Key::LEN,
            Self::ChangeMap => <ChangeCollection as Collection>::Key::LEN,
            Self::ViewLineage => <ViewLineageCollection as Collection>::Key::LEN,
        }
    }

    pub(crate) fn decode(self, bytes: &[u8]) -> Result<RadixNode, CanonicalObjectError> {
        match self {
            Self::RetainedSet => uniform::<RetainedCollection>(bytes),
            Self::ConflictSet => uniform::<ConflictCollection>(bytes),
            Self::RevisionMap => uniform::<RevisionCollection>(bytes),
            Self::ApplicationMap => uniform::<ApplicationCollection>(bytes),
            Self::ChangeMap => uniform::<ChangeCollection>(bytes),
            Self::ViewLineage => uniform::<ViewLineageCollection>(bytes),
        }
    }

    /// Whether a key is itself the object it refers to, as in a set. Otherwise the
    /// key's value is.
    fn members_are_keys(self) -> bool {
        matches!(self, Self::RetainedSet | Self::ConflictSet)
    }
}

/// A radix value as an object reference, if it is one.
trait RadixValue {
    fn object(self) -> Option<ObjectId>;
}
impl RadixValue for () {
    fn object(self) -> Option<ObjectId> {
        None
    }
}
macro_rules! radix_value {
    ($($id:ty),+) => {$(
        impl RadixValue for $id {
            fn object(self) -> Option<ObjectId> {
                Some(self.into())
            }
        }
    )+};
}
radix_value!(
    wheat_core::RevisionId,
    wheat_core::ApplicationId,
    wheat_core::ChangeObjectId,
    wheat_core::RetainedSetId
);

fn uniform<C: Collection>(bytes: &[u8]) -> Result<RadixNode, CanonicalObjectError>
where
    C::Value: RadixValue,
{
    let node = C::unpack(C::decode(bytes)?);
    Ok(Node {
        prefix: node.prefix,
        value: node.value.map(RadixValue::object),
        children: node
            .children
            .into_iter()
            .map(|(edge, child)| (edge, child.into()))
            .collect(),
        entries: node
            .entries
            .into_iter()
            .map(|(suffix, value)| (suffix, value.object()))
            .collect(),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RadixSite {
    /// A node one key byte further down.
    Child { edge: u8 },
    /// What a key refers to: the key itself in a set, its value in a map. `entry` is
    /// the index of an inline entry; `None` is the key that ends at the node's prefix.
    Key { entry: Option<usize> },
}

/// A radix node whose shape does not fit the key position it was reached at.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MalformedRadix {
    pub id: ObjectId,
    pub position: usize,
}

/// The references of radix node `id`, which sits below the key bytes `above`. Keys
/// come before children.
pub(crate) fn radix<E: From<MalformedRadix>>(
    kind: RadixKind,
    id: ObjectId,
    node: &RadixNode,
    above: &[u8],
    mut f: impl FnMut(Ref<RadixSite>) -> Result<(), E>,
) -> Result<(), E> {
    let malformed = || MalformedRadix {
        id,
        position: above.len(),
    };
    radix::validate_node_at(node, above.len(), kind.key_len()).map_err(|_| malformed())?;
    let keys = node
        .value
        .iter()
        .map(|value| (None, [].as_slice(), *value))
        .chain(
            node.entries
                .iter()
                .enumerate()
                .map(|(index, (suffix, value))| (Some(index), suffix.as_slice(), *value)),
        );
    for (entry, suffix, value) in keys {
        let target = if kind.members_are_keys() {
            ObjectId::from_digest(full_key([above, &node.prefix, suffix]).ok_or_else(malformed)?)
        } else {
            value.ok_or_else(malformed)?
        };
        f(Ref::Object(RadixSite::Key { entry }, target))?;
    }
    for &(edge, child) in &node.children {
        f(Ref::Object(RadixSite::Child { edge }, child))?;
    }
    Ok(())
}

/// `parts` joined, if they make exactly one 32-byte key.
fn full_key(parts: [&[u8]; 3]) -> Option<[u8; 32]> {
    let mut key = [0; 32];
    let mut at = 0;
    for part in parts {
        key.get_mut(at..at + part.len())?.copy_from_slice(part);
        at += part.len();
    }
    (at == key.len()).then_some(key)
}

/// Where a reference leads, for a walk that follows everything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Reference<'a> {
    Object(ObjectId),
    /// The root node of a radix collection.
    RadixRoot(ObjectId),
    /// A radix node below another, with the key bytes above it.
    RadixChild {
        id: ObjectId,
        above: &'a [u8],
    },
    Content(ContentId),
}

fn plain<S>(reference: Ref<S>) -> Reference<'static> {
    match reference {
        Ref::Object(_, id) => Reference::Object(id),
        Ref::Content(id) => Reference::Content(id),
    }
}

/// Every reference of `stored`. What a radix node refers to depends on where it
/// sits in its collection: `radix_path` is the key bytes above it, and without it a
/// radix node's references are not listed.
pub(crate) fn for_each_reference<E>(
    stored: &StoredObject,
    radix_path: Option<&[u8]>,
    mut f: impl FnMut(Reference<'_>) -> Result<(), E>,
) -> Result<(), E>
where
    E: From<CanonicalObjectError> + From<MalformedRadix>,
{
    let bytes = &stored.canonical_bytes;
    match stored.kind {
        ObjectKind::RepositoryEnvelope => envelope(
            &wheat_core::decode_repository_envelope_record(bytes)?,
            |reference| f(plain(reference)),
        ),
        ObjectKind::ViewLabelManifest => labels(
            &wheat_core::decode_view_label_manifest_record(bytes)?,
            |reference| f(plain(reference)),
        ),
        ObjectKind::RepositoryState => state(
            &wheat_core::decode_repository_state_record(bytes)?,
            |reference| {
                f(match reference {
                    Ref::Object(site, id) => match site {
                        StateSite::BaseView | StateSite::RequiredContent => Reference::Object(id),
                        StateSite::Changes
                        | StateSite::ViewLineage
                        | StateSite::RetainedChanges
                        | StateSite::RetainedViews
                        | StateSite::RetainedRevisions
                        | StateSite::RetainedApplications => Reference::RadixRoot(id),
                    },
                    Ref::Content(id) => Reference::Content(id),
                })
            },
        ),
        ObjectKind::View => view(&wheat_core::decode_view_record(bytes)?, |reference| {
            f(match reference {
                Ref::Object(site, id) => match site {
                    ViewSite::Tree | ViewSite::RequiredContent => Reference::Object(id),
                    ViewSite::Revisions
                    | ViewSite::Applications
                    | ViewSite::UnresolvedConflicts => Reference::RadixRoot(id),
                },
                Ref::Content(id) => Reference::Content(id),
            })
        }),
        ObjectKind::ContentManifest => manifest(
            &wheat_core::decode_content_manifest_record(bytes)?,
            |reference| f(plain(reference)),
        ),
        ObjectKind::ContentSetSnapshot | ObjectKind::ContentSetDelta => {
            match decode_content_set(stored)? {
                Some(blob) => content_set(&blob, |reference| f(plain(reference))),
                None => Ok(()),
            }
        }
        ObjectKind::Tree => tree(&wheat_core::decode_directory_node(bytes)?, |reference| {
            f(plain(reference))
        }),
        ObjectKind::Revision => {
            revision(&wheat_core::decode_revision_record(bytes)?, |reference| {
                f(plain(reference))
            })
        }
        ObjectKind::Application => application(
            &wheat_core::decode_application_record(bytes)?,
            |reference| f(plain(reference)),
        ),
        ObjectKind::Conflict => conflict(&wheat_core::decode_conflict(bytes)?, |reference| {
            f(plain(reference))
        }),
        ObjectKind::Edit => edit(&wheat_core::decode_edit_record(bytes)?, |reference| {
            f(plain(reference))
        }),
        ObjectKind::Change => change(&wheat_core::decode_change(bytes)?, |reference| {
            f(plain(reference))
        }),
        ObjectKind::RetainedSetNode => follow_radix(RadixKind::RetainedSet, stored, radix_path, f),
        ObjectKind::ConflictSetNode => follow_radix(RadixKind::ConflictSet, stored, radix_path, f),
        ObjectKind::RevisionMapNode => follow_radix(RadixKind::RevisionMap, stored, radix_path, f),
        ObjectKind::ApplicationMapNode => {
            follow_radix(RadixKind::ApplicationMap, stored, radix_path, f)
        }
        ObjectKind::ChangeMapNode => follow_radix(RadixKind::ChangeMap, stored, radix_path, f),
        ObjectKind::ViewLineageMapNode => {
            follow_radix(RadixKind::ViewLineage, stored, radix_path, f)
        }
    }
}

fn follow_radix<E>(
    kind: RadixKind,
    stored: &StoredObject,
    radix_path: Option<&[u8]>,
    mut f: impl FnMut(Reference<'_>) -> Result<(), E>,
) -> Result<(), E>
where
    E: From<CanonicalObjectError> + From<MalformedRadix>,
{
    let Some(above) = radix_path else {
        return Ok(());
    };
    let node = kind.decode(&stored.canonical_bytes)?;
    // Key bytes above each child: `above`, the node's prefix, the edge.
    let mut path = [0; 32];
    let base = above.len() + node.prefix.len();
    radix(kind, stored.id, &node, above, |reference| match reference {
        Ref::Object(RadixSite::Key { .. }, id) => f(if kind == RadixKind::ViewLineage {
            Reference::RadixRoot(id)
        } else {
            Reference::Object(id)
        }),
        Ref::Object(RadixSite::Child { edge }, id) => {
            // A node with children ends before the full key, so this fits.
            let Some(slot) = path.get_mut(base) else {
                return Err(MalformedRadix {
                    id: stored.id,
                    position: above.len(),
                }
                .into());
            };
            *slot = edge;
            path[..above.len()].copy_from_slice(above);
            path[above.len()..base].copy_from_slice(&node.prefix);
            f(Reference::RadixChild {
                id,
                above: &path[..=base],
            })
        }
        Ref::Content(id) => f(Reference::Content(id)),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::{Durability, ImmutableObjectStore, LooseObjectStore, RetainedSetStore};

    /// Every member of the set under `root`, found by following its references.
    async fn members(objects: &LooseObjectStore, root: ObjectId) -> HashSet<ObjectId> {
        let mut members = HashSet::new();
        let mut pending = vec![(root, Vec::new())];
        while let Some((id, above)) = pending.pop() {
            let stored = objects.get(id).await.unwrap();
            let mut children = Vec::new();
            for_each_reference(&stored, Some(&above), |reference| {
                match reference {
                    Reference::Object(member) => assert!(members.insert(member)),
                    Reference::RadixChild { id, above } => children.push((id, above.to_vec())),
                    other => panic!("a set node refers to {other:?}"),
                }
                Ok::<_, crate::SnapshotError>(())
            })
            .unwrap();
            pending.extend(children);
        }
        members
    }

    #[tokio::test]
    async fn every_set_member_is_listed_wherever_the_node_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::with_durability(dir.path(), Durability::Deferred);
        let sets = RetainedSetStore::new(&objects);
        let empty = sets.empty_root().await.unwrap();
        for count in [1u32, 2, 7, 600] {
            let keys: HashSet<ObjectId> = (0..count)
                .map(|index| ObjectId::from_digest(*blake3::hash(&index.to_le_bytes()).as_bytes()))
                .collect();
            let root = sets
                .update_batch(empty, keys.iter().map(|key| (*key, true)))
                .await
                .unwrap();
            let node = RadixKind::RetainedSet
                .decode(&objects.get(root.into()).await.unwrap().canonical_bytes)
                .unwrap();
            if count > 1 {
                // Most members sit inline in their parent rather than in a node of
                // their own, and those are the ones a walk can miss.
                assert!(!node.entries.is_empty(), "{count} keys");
            }
            assert_eq!(members(&objects, root.into()).await, keys, "{count} keys");
        }
    }

    #[tokio::test]
    async fn a_set_node_reached_at_the_wrong_depth_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::with_durability(dir.path(), Durability::Deferred);
        let sets = RetainedSetStore::new(&objects);
        let empty = sets.empty_root().await.unwrap();
        let keys = (0..7u32)
            .map(|index| ObjectId::from_digest(*blake3::hash(&index.to_le_bytes()).as_bytes()));
        let root = sets
            .update_batch(empty, keys.map(|key| (key, true)))
            .await
            .unwrap();
        let stored = objects.get(root.into()).await.unwrap();
        let error = for_each_reference(&stored, Some(&[1, 2, 3]), |_| {
            Ok::<_, crate::SnapshotError>(())
        })
        .unwrap_err();
        assert!(
            matches!(error, crate::SnapshotError::InvalidManifest(_)),
            "{error}"
        );
    }
}
