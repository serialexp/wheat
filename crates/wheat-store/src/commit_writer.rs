//! Direct, copy-on-write persistence of clean, first-parent commits.

use std::collections::BTreeMap;

use thiserror::Error;
use wheat_core::{
    ApplicationId, ApplicationRecord, ApplicationResultRecord, AppliedEditEvidence, ByteRange,
    CanonicalObjectError, Change, ChangeId, ChangeObjectId, ChangeProposal, ContentIo,
    ContentManifestRecord, EditId, RevisionId, RevisionRecord, Tree, ViewId, ViewRecord,
    decode_view_record,
    edit_application::{ApplicationError, ApplicationLimits, ApplicationOutcome, apply_edits},
};

use crate::{
    ApplicationMapError, ApplicationMapStore, CollectionError, ConflictSetStore, ContentSetStore,
    ContentStore, ContentStoreError, ImmutableObjectStore, MerkleTreeError, MerkleTreeStore,
    ObjectKind, ObjectStoreError, RevisionMapError, RevisionMapStore, TreeEdit,
    merkle_tree::TreePathShape,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PersistedCommit {
    pub change_id: ChangeId,
    pub revision_id: RevisionId,
    pub application_id: ApplicationId,
    pub resulting_view: ViewId,
}

#[derive(Debug, Error)]
pub enum CommitWriterError {
    #[error("immutable object storage failed")]
    Store(#[from] ObjectStoreError),
    #[error("content storage failed")]
    Content(#[from] ContentStoreError),
    #[error("tree operation failed")]
    Tree(#[from] MerkleTreeError),
    #[error("revision map operation failed")]
    Revisions(#[from] RevisionMapError),
    #[error("application map operation failed")]
    Applications(#[from] ApplicationMapError),
    #[error("set operation failed")]
    Sets(#[from] CollectionError),
    #[error("canonical object is invalid")]
    Canonical(#[from] CanonicalObjectError),
    #[error("proposal has more than one dependency")]
    MultipleDependencies,
    #[error("dependency {0} is not selected by the parent")]
    MissingDependency(ChangeId),
    #[error("change {0} is already selected by the parent")]
    DuplicateChange(ChangeId),
    #[error("parent view has unresolved conflicts")]
    ConflictedParent,
    #[error("edit application failed: {0:?}")]
    Application(ApplicationError),
    #[error("edit application did not produce a clean result")]
    FailedEdits(BTreeMap<EditId, ApplicationOutcome>),
    #[error("edit contains invalid path {0:?}")]
    InvalidPath(String),
}

type Result<T> = std::result::Result<T, CommitWriterError>;

pub struct PersistedCommitWriter<'a, O> {
    objects: &'a O,
    content: &'a ContentStore,
    limits: ApplicationLimits,
}

impl<'a, O: ImmutableObjectStore> PersistedCommitWriter<'a, O> {
    #[must_use]
    pub fn new(objects: &'a O, content: &'a ContentStore) -> Self {
        Self {
            objects,
            content,
            limits: ApplicationLimits::default(),
        }
    }

    #[must_use]
    pub fn with_limits(
        objects: &'a O,
        content: &'a ContentStore,
        limits: ApplicationLimits,
    ) -> Self {
        Self {
            objects,
            content,
            limits,
        }
    }

    pub async fn empty_view(&self) -> Result<ViewId> {
        let tree = MerkleTreeStore::new(self.objects).empty_root().await?;
        let revisions = RevisionMapStore::new(self.objects).empty_root().await?;
        let applications = ApplicationMapStore::new(self.objects).empty_root().await?;
        let unresolved_conflicts = ConflictSetStore::new(self.objects).empty_root().await?;
        let content = ContentSetStore::new(self.objects).empty_root().await?;
        let manifest = ContentManifestRecord { content };
        self.put(manifest.id().into(), &manifest.canonical_bytes())
            .await?;
        let view = ViewRecord {
            tree,
            revisions,
            applications,
            unresolved_conflicts,
            required_content: manifest.id(),
        };
        self.put(view.id().into(), &view.canonical_bytes()).await?;
        Ok(view.id())
    }

    pub async fn write(&self, parent: ViewId, proposal: ChangeProposal) -> Result<PersistedCommit> {
        if proposal.dependencies.len() > 1 {
            return Err(CommitWriterError::MultipleDependencies);
        }
        let parent_record = self.read_view(parent).await?;
        let revisions = RevisionMapStore::new(self.objects);
        if revisions
            .get(parent_record.revisions, proposal.id)
            .await?
            .is_some()
        {
            return Err(CommitWriterError::DuplicateChange(proposal.id));
        }
        if let Some(dependency) = proposal.dependencies.first()
            && revisions
                .get(parent_record.revisions, *dependency)
                .await?
                .is_none()
        {
            return Err(CommitWriterError::MissingDependency(*dependency));
        }
        let mut conflicts = ConflictSetStore::new(self.objects)
            .cursor(parent_record.unresolved_conflicts)
            .await?;
        if conflicts.next().await?.is_some() {
            return Err(CommitWriterError::ConflictedParent);
        }

        let trees = MerkleTreeStore::new(self.objects);
        let mut target = Tree::new();
        let mut paths = BTreeMap::<String, Vec<String>>::new();
        for edit in proposal.edits.edits.values() {
            if !paths.contains_key(&edit.path) {
                paths.insert(edit.path.clone(), parse_path(&edit.path)?);
            }
        }
        // One sorted traversal resolves every path; a file where a directory was expected
        // is an absent leaf here.
        let sorted = paths.keys().map(String::as_str).collect::<Vec<_>>();
        let shapes = trees.path_shapes(parent_record.tree, &sorted).await?;
        for (path, shape) in sorted.into_iter().zip(shapes) {
            if let TreePathShape::File(value) = shape {
                target.insert(path.to_owned(), value);
            }
        }
        let output = apply_edits(
            &target,
            proposal
                .edits
                .edits
                .iter()
                .map(|(id, edit)| (*id, edit.clone())),
            |id| self.content.resolve(id),
            self.limits,
        )
        .map_err(CommitWriterError::Application)?;
        let failed = output
            .outcomes
            .iter()
            .filter(|(_, outcome)| !matches!(outcome, ApplicationOutcome::Applied { .. }))
            .map(|(id, outcome)| (*id, outcome.clone()))
            .collect::<BTreeMap<_, _>>();
        if !failed.is_empty() {
            return Err(CommitWriterError::FailedEdits(failed));
        }

        // Build a map of new_content_id → old_content_id for delta compression.
        // Each edit knows the old and new file value for its path.
        let mut base_hints: BTreeMap<wheat_core::ContentId, wheat_core::ContentId> =
            BTreeMap::new();
        for (edit_id, outcome) in &output.outcomes {
            if !matches!(outcome, ApplicationOutcome::Applied { .. }) {
                continue;
            }
            let edit = &proposal.edits.edits[edit_id];
            let old_value = target.get(&edit.path);
            let new_value = output.tree.get(&edit.path);
            if let (Some(old), Some(new)) = (old_value, new_value)
                && old.content != new.content
            {
                base_hints.insert(new.content, old.content);
            }
        }

        // Generated blobs must be durable before any metadata can refer to them.
        // Use delta compression when we know the old version of the same file.
        for (id, bytes) in &output.generated_content {
            if let Some(&base_id) = base_hints.get(id) {
                self.content.put_with_base(*id, bytes, base_id)?;
            } else {
                self.content.store_generated(*id, bytes)?;
            }
        }

        let mut applied = BTreeMap::new();
        for (id, outcome) in &output.outcomes {
            let edit = &proposal.edits.edits[id];
            let ApplicationOutcome::Applied { read, write } = outcome else {
                unreachable!()
            };
            applied.insert(
                *id,
                AppliedEditEvidence {
                    target: target.get(&edit.path).copied(),
                    read: read.clone().map(byte_range),
                    write: write.clone().map(byte_range),
                },
            );
        }

        // Build and validate the final tree in one traversal. This preserves COW sharing while
        // avoiding persisted deletion-only roots during file/directory shape changes.
        let tree_edits = paths
            .iter()
            .map(|(path, parsed)| TreeEdit {
                path: parsed.clone(),
                value: output.tree.get(path).copied(),
            })
            .collect::<Vec<_>>();
        let resulting_tree = trees
            .apply_final_edits(parent_record.tree, &tree_edits)
            .await
            .map_err(|error| match error {
                MerkleTreeError::FileInPath(component) => CommitWriterError::InvalidPath(component),
                other => other.into(),
            })?;

        // Persist definitions before revision/application/view metadata.
        for (id, edit) in &proposal.edits.edits {
            self.put((*id).into(), &edit.canonical_bytes()).await?;
        }
        let change = Change {
            id: proposal.id,
            metadata: proposal.metadata,
        };
        self.put(
            ChangeObjectId::of_canonical_bytes(&change.canonical_bytes()).into(),
            &change.canonical_bytes(),
        )
        .await?;
        let revision = RevisionRecord {
            change_id: proposal.id,
            metadata: proposal.revision_metadata,
            predecessor: None,
            dependencies: proposal.dependencies,
            authored_against: parent,
            edits: proposal.edits.edits.keys().copied().collect(),
            transition: proposal.edits.transition,
            resulting_tree,
        };
        let revision_id = revision.id();
        self.put(revision_id.into(), &revision.canonical_bytes())
            .await?;
        let application = ApplicationRecord {
            revision_id,
            target_view: parent,
            result: ApplicationResultRecord::Clean {
                tree: resulting_tree,
            },
            applied: applied.clone(),
        };
        let application_id = application.id();
        self.put(application_id.into(), &application.canonical_bytes())
            .await?;
        let revision_root = revisions
            .insert(parent_record.revisions, proposal.id, revision_id)
            .await?;
        let application_root = ApplicationMapStore::new(self.objects)
            .insert(parent_record.applications, proposal.id, application_id)
            .await?;
        // A view manifest is the visible resulting tree only.  Edit definitions and evidence
        // retain their historical blobs through repository finalization, not through this root.
        let content_root = crate::commit_content::update_content(
            self.objects,
            parent_record.required_content,
            resulting_tree,
            paths
                .keys()
                .map(|path| (target.get(path).copied(), output.tree.get(path).copied())),
        )
        .await?;
        let manifest = ContentManifestRecord {
            content: content_root,
        };
        self.put(manifest.id().into(), &manifest.canonical_bytes())
            .await?;
        let view = ViewRecord {
            tree: resulting_tree,
            revisions: revision_root,
            applications: application_root,
            unresolved_conflicts: parent_record.unresolved_conflicts,
            required_content: manifest.id(),
        };
        let resulting_view = view.id();
        self.put(resulting_view.into(), &view.canonical_bytes())
            .await?;
        Ok(PersistedCommit {
            change_id: proposal.id,
            revision_id,
            application_id,
            resulting_view,
        })
    }

    async fn put(&self, id: wheat_core::ObjectId, bytes: &[u8]) -> Result<()> {
        self.objects.put_if_absent(id, bytes).await?;
        Ok(())
    }
    async fn read_view(&self, id: ViewId) -> Result<ViewRecord> {
        let object = self.objects.get(id.into()).await?;
        if object.kind != ObjectKind::View {
            return Err(ObjectStoreError::WrongKind {
                id: id.into(),
                expected: ObjectKind::View,
                actual: object.kind,
            }
            .into());
        }
        Ok(decode_view_record(&object.canonical_bytes)?)
    }
}

fn byte_range(range: std::ops::Range<usize>) -> ByteRange {
    ByteRange {
        start: range.start as u64,
        end: range.end as u64,
    }
}
fn parse_path(path: &str) -> Result<Vec<String>> {
    let components = path.split('/').map(str::to_owned).collect::<Vec<_>>();
    if components.is_empty()
        || components
            .iter()
            .any(|part| wheat_core::validate_directory_component(part).is_err())
    {
        return Err(CommitWriterError::InvalidPath(path.to_owned()));
    }
    Ok(components)
}
