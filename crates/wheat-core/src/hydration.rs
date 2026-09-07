//! Validated reconstruction of executable model objects from persisted records.

use std::collections::BTreeMap;

use crate::{
    Application, ApplicationRecord, ApplicationResult, ApplicationResultRecord, ChangeRevision,
    Conflict, EditId, EditRecord, RepositoryError, RevisionEdits, RevisionRecord, Tree,
};

/// Materialize a revision record with the tree object it references.
///
/// The supplied tree is rejected unless its canonical root is exactly the root recorded by the
/// revision. The returned revision retains the record's content-addressed identity.
pub fn hydrate_revision(
    record: RevisionRecord,
    resulting_tree: Tree,
    edit_definitions: BTreeMap<EditId, EditRecord>,
) -> Result<ChangeRevision, RepositoryError> {
    if crate::wire::tree_id(&resulting_tree) != record.resulting_tree {
        return Err(RepositoryError::InvalidRepositoryState(
            "revision resulting tree reference",
        ));
    }

    if edit_definitions
        .keys()
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        != record.edits
        || edit_definitions.iter().any(|(id, edit)| edit.id() != *id)
    {
        return Err(RepositoryError::InvalidRepositoryState(
            "revision edit definitions",
        ));
    }
    Ok(ChangeRevision {
        id: record.id(),
        change_id: record.change_id,
        metadata: record.metadata,
        predecessor: record.predecessor,
        dependencies: record.dependencies,
        authored_against: record.authored_against,
        edits: RevisionEdits {
            edits: edit_definitions,
            transition: record.transition,
        },
        resulting_tree,
    })
}

/// Materialize an application record with the tree and optional conflict it references.
///
/// This validates the tree root, the presence (or absence) and identity of the conflict, and the
/// conflict's target-view reference before constructing the executable application.
pub fn hydrate_application(
    record: ApplicationRecord,
    materialized_tree: Tree,
    conflict: Option<Conflict>,
) -> Result<Application, RepositoryError> {
    let result = match record.result {
        ApplicationResultRecord::Clean { tree } => {
            if crate::wire::tree_id(&materialized_tree) != tree {
                return Err(RepositoryError::InvalidRepositoryState(
                    "application resulting tree reference",
                ));
            }
            if conflict.is_some() {
                return Err(RepositoryError::InvalidRepositoryState(
                    "clean application conflict reference",
                ));
            }
            ApplicationResult::Clean {
                tree: materialized_tree,
            }
        }
        ApplicationResultRecord::Conflicted {
            partial_tree,
            conflict: conflict_id,
        } => {
            if crate::wire::tree_id(&materialized_tree) != partial_tree {
                return Err(RepositoryError::InvalidRepositoryState(
                    "application partial tree reference",
                ));
            }
            let conflict = conflict.ok_or(RepositoryError::InvalidRepositoryState(
                "missing application conflict",
            ))?;
            if conflict.id != conflict_id
                || conflict.id != crate::ConflictId::of_canonical_bytes(&conflict.canonical_bytes())
            {
                return Err(RepositoryError::InvalidRepositoryState(
                    "application conflict reference",
                ));
            }
            if conflict.base_view != record.target_view {
                return Err(RepositoryError::InvalidRepositoryState(
                    "application conflict target view",
                ));
            }
            ApplicationResult::Conflicted {
                partial_tree: materialized_tree,
                conflict,
            }
        }
    };

    let id = record.id();
    let application = Application {
        id,
        revision_id: record.revision_id,
        target_view: record.target_view,
        result,
        applied: record.applied,
    };
    if application.id != crate::ApplicationId::of_canonical_bytes(&application.canonical_bytes()) {
        return Err(RepositoryError::InvalidRepositoryState(
            "application hydrated identity",
        ));
    }
    Ok(application)
}
