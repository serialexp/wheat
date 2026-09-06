//! Direct, copy-on-write persistence of clean, first-parent commits.

use thiserror::Error;
use wheat_core::{
    ApplicationId, ApplicationRecord, ApplicationResultRecord, CanonicalObjectError, Change,
    ChangeId, ChangeObjectId, ChangeProposal, ContentId, ContentManifestRecord, DirectoryEntry,
    RevisionId, RevisionRecord, TreeId, ViewId, ViewRecord, decode_view_record,
};

use crate::{
    ApplicationMapError, ApplicationMapStore, CollectionError, ConflictSetStore, ContentSetStore,
    ContentStore, ContentStoreError, ImmutableObjectStore, MerkleTreeError, MerkleTreeStore,
    ObjectKind, ObjectStoreError, RevisionMapError, RevisionMapStore, TreeEdit,
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
    #[error("transformation contains duplicate path {0:?}")]
    DuplicatePath(String),
    #[error("transformation contains invalid path {0:?}")]
    InvalidPath(String),
    #[error(
        "transformation expectation failed at {path:?}: expected {expected:?}, found {actual:?}"
    )]
    ExpectedValue {
        path: String,
        expected: Option<ContentId>,
        actual: Option<ContentId>,
    },
}

type Result<T> = std::result::Result<T, CommitWriterError>;

pub struct PersistedCommitWriter<'a, O> {
    objects: &'a O,
    content: &'a ContentStore,
}

impl<'a, O: ImmutableObjectStore> PersistedCommitWriter<'a, O> {
    #[must_use]
    pub fn new(objects: &'a O, content: &'a ContentStore) -> Self {
        Self { objects, content }
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

    pub async fn write(
        &self,
        parent: ViewId,
        mut proposal: ChangeProposal,
    ) -> Result<PersistedCommit> {
        if proposal.dependencies.len() > 1 {
            return Err(CommitWriterError::MultipleDependencies);
        }
        proposal
            .transformation
            .edits
            .sort_by(|a, b| a.path.cmp(&b.path));
        for pair in proposal.transformation.edits.windows(2) {
            if pair[0].path == pair[1].path {
                return Err(CommitWriterError::DuplicatePath(pair[0].path.clone()));
            }
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

        let tree_store = MerkleTreeStore::new(self.objects);
        let mut parsed = Vec::with_capacity(proposal.transformation.edits.len());
        for edit in &proposal.transformation.edits {
            let path = parse_path(&edit.path)?;
            let actual = match tree_store.lookup(parent_record.tree, &path).await {
                Ok(value) => value,
                // A file at an ancestor means this exact file path is absent. This is
                // important for a same-commit file-to-directory replacement.
                Err(MerkleTreeError::FileInPath(_)) => None,
                Err(error) => return Err(error.into()),
            };
            if actual != edit.expected {
                return Err(CommitWriterError::ExpectedValue {
                    path: edit.path.clone(),
                    expected: edit.expected,
                    actual,
                });
            }
            if let Some(id) = edit.expected {
                self.content.get(id)?;
            }
            if let Some(id) = edit.replacement {
                self.content.get(id)?;
            }
            parsed.push((path, edit.expected, edit.replacement));
        }

        // Remove existing leaves first so file/directory shape changes are possible.
        let deletions = parsed
            .iter()
            .filter(|(_, old, _)| old.is_some())
            .map(|(path, _, _)| TreeEdit {
                path: path.clone(),
                content: None,
            })
            .collect::<Vec<_>>();
        let mut resulting_tree = parent_record.tree;
        if !deletions.is_empty() {
            resulting_tree = tree_store.apply_edits(resulting_tree, &deletions).await?;
        }
        // A file insertion must not erase untouched descendants of a directory.
        for (path, _, replacement) in &parsed {
            if replacement.is_none() {
                continue;
            }
            let mut node = tree_store.read_node(resulting_tree).await?;
            for (index, component) in path.iter().enumerate() {
                match node.entries.get(component) {
                    Some(DirectoryEntry::Directory(_)) if index + 1 == path.len() => {
                        return Err(CommitWriterError::InvalidPath(path.join("/")));
                    }
                    Some(DirectoryEntry::Directory(child)) => {
                        node = tree_store.read_node(*child).await?
                    }
                    Some(DirectoryEntry::File(_)) if index + 1 != path.len() => {
                        return Err(CommitWriterError::InvalidPath(path.join("/")));
                    }
                    _ => break,
                }
            }
        }
        let insertions = parsed
            .iter()
            .filter_map(|(path, _, new)| {
                new.map(|content| TreeEdit {
                    path: path.clone(),
                    content: Some(content),
                })
            })
            .collect::<Vec<_>>();
        if !insertions.is_empty() {
            resulting_tree = tree_store.apply_edits(resulting_tree, &insertions).await?;
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
            transformation: proposal.transformation,
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
        let content_root = self.content_set(resulting_tree).await?;
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

    // Temporary scan-per-commit implementation; traversal and set writes remain bounded.
    async fn content_set(&self, tree: TreeId) -> Result<wheat_core::ContentSetId> {
        let sets = ContentSetStore::new(self.objects);
        let mut root = sets.empty_root().await?;
        let mut stack = vec![tree];
        let trees = MerkleTreeStore::new(self.objects);
        let mut batch = Vec::with_capacity(1024);
        while let Some(id) = stack.pop() {
            for entry in trees.read_node(id).await?.entries.into_values() {
                match entry {
                    DirectoryEntry::Directory(child) => stack.push(child),
                    DirectoryEntry::File(content) => {
                        self.content.get(content)?;
                        batch.push((content, true));
                        if batch.len() == 1024 {
                            root = sets.update_batch(root, batch.drain(..)).await?;
                        }
                    }
                }
            }
        }
        if !batch.is_empty() {
            root = sets.update_batch(root, batch).await?;
        }
        Ok(root)
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
