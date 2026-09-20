//! Structural closure computation — collect all ObjectIds reachable from
//! a repository envelope.
//!
//! Walks the repository state using domain knowledge about every implicit
//! reference structure: retained-set trie keys, per-view lineage retained
//! sets, and the maps/trees/revisions/applications/edits referenced by
//! each retained view. This mirrors what `validate_complete` reads, but
//! without the validation logic or the SQLite scratch space (which is
//! `!Send` and incompatible with `tokio::spawn`).

use std::collections::HashMap;

use wheat_core::{ObjectId, RepositoryEnvelopeId, ViewId};

use crate::{
    ChangeMapStore, ContentStore, ImmutableObjectStore, RepositoryReader, RetainedSetStore,
    StoredObject, ViewLineageMapStore,
};

/// Collect all structural ObjectIds in the closure of an envelope,
/// returning each object's data alongside its ID.
///
/// Walks every retained view's revision map, application map, change map,
/// view lineage map, trees, revisions, applications, edits, and changes —
/// plus all retained-set trie nodes and the trie-key-encoded ObjectIds.
///
/// Requires all objects to be present in the store (server-side use).
/// Returns a map from ObjectId to StoredObject so callers (e.g.
/// handle_fetch) can serialize objects without re-reading them from
/// the store.
pub async fn collect_structural_closure<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    envelope_id: RepositoryEnvelopeId,
) -> HashMap<ObjectId, StoredObject> {
    let mut closure = HashMap::new();
    let envelope_oid: ObjectId = envelope_id.into();
    insert_oid(objects, envelope_oid, &mut closure).await;

    // --- Read envelope and state ---
    let reader = RepositoryReader::new(objects, content);
    let Ok(envelope) = reader.envelope(envelope_id).await else {
        return closure;
    };
    insert_oid(objects, envelope.state.into(), &mut closure).await;
    insert_oid(objects, envelope.labels.into(), &mut closure).await;

    let Ok(state) = reader.state(envelope.state).await else {
        return closure;
    };

    // State's explicit references.
    insert_oid(objects, state.base_view.into(), &mut closure).await;
    insert_oid(objects, state.changes.into(), &mut closure).await;
    insert_oid(objects, state.view_lineage.into(), &mut closure).await;
    insert_oid(objects, state.required_content.into(), &mut closure).await;
    insert_oid(objects, state.retained_views.into(), &mut closure).await;
    insert_oid(objects, state.retained_revisions.into(), &mut closure).await;
    insert_oid(objects, state.retained_applications.into(), &mut closure).await;
    insert_oid(objects, state.retained_changes.into(), &mut closure).await;

    // --- Walk retained sets (trie keys) ---
    let retained_store = RetainedSetStore::new(objects);
    let mut retained_view_ids = Vec::new();
    let mut retained_revision_ids = Vec::new();
    let mut retained_application_ids = Vec::new();
    let mut retained_change_ids = Vec::new();

    // Retained views.
    if let Ok(mut cursor) = retained_store.cursor(state.retained_views).await {
        while let Ok(Some(oid)) = cursor.next().await {
            retained_view_ids.push(ViewId::from_digest(*oid.as_bytes()));
            insert_oid(objects, oid, &mut closure).await;
        }
    }
    // Retained revisions.
    if let Ok(mut cursor) = retained_store.cursor(state.retained_revisions).await {
        while let Ok(Some(oid)) = cursor.next().await {
            retained_revision_ids.push(oid);
            insert_oid(objects, oid, &mut closure).await;
        }
    }
    // Retained applications.
    if let Ok(mut cursor) = retained_store.cursor(state.retained_applications).await {
        while let Ok(Some(oid)) = cursor.next().await {
            retained_application_ids.push(oid);
            insert_oid(objects, oid, &mut closure).await;
        }
    }
    // Retained changes.
    if let Ok(mut cursor) = retained_store.cursor(state.retained_changes).await {
        while let Ok(Some(oid)) = cursor.next().await {
            retained_change_ids.push(oid);
            insert_oid(objects, oid, &mut closure).await;
        }
    }

    // --- Per-view lineage retained sets ---
    let lineage = ViewLineageMapStore::new(objects);
    for view_id in &retained_view_ids {
        if let Ok(Some(children_root)) = lineage.get(state.view_lineage, *view_id).await {
            // The children_root itself is a RetainedSetId — an object.
            insert_oid(objects, children_root.into(), &mut closure).await;
            if let Ok(mut cursor) = retained_store.cursor(children_root).await {
                while let Ok(Some(oid)) = cursor.next().await {
                    insert_oid(objects, oid, &mut closure).await;
                }
            }
        }
    }

    // --- Walk all retained-set TRIE NODES (structural links) ---
    // The cursors above extracted trie-KEY ObjectIds. We also need the
    // trie NODES themselves (they are separate objects in the store).
    // BFS through all retained-set roots to get every trie node.
    bfs_trie_nodes(objects, state.retained_views.into(), &mut closure).await;
    bfs_trie_nodes(objects, state.retained_revisions.into(), &mut closure).await;
    bfs_trie_nodes(objects, state.retained_applications.into(), &mut closure).await;
    bfs_trie_nodes(objects, state.retained_changes.into(), &mut closure).await;

    // --- Walk view lineage map nodes ---
    bfs_trie_nodes(objects, state.view_lineage.into(), &mut closure).await;

    // Per-view lineage retained-set trie nodes.
    for view_id in &retained_view_ids {
        if let Ok(Some(children_root)) = lineage.get(state.view_lineage, *view_id).await {
            bfs_trie_nodes(objects, children_root.into(), &mut closure).await;
        }
    }

    // --- Walk change map ---
    bfs_trie_nodes(objects, state.changes.into(), &mut closure).await;

    // --- Walk each retained view's sub-structures ---
    for view_id in &retained_view_ids {
        let Ok(view) = reader.view(*view_id).await else {
            continue;
        };

        // Revision map — nodes + leaf RevisionIds (already in retained_revisions).
        bfs_trie_nodes(objects, view.revisions.into(), &mut closure).await;

        // Application map — nodes + leaf ApplicationIds.
        bfs_trie_nodes(objects, view.applications.into(), &mut closure).await;

        // Conflict set.
        bfs_trie_nodes(objects, view.unresolved_conflicts.into(), &mut closure).await;

        // Content manifest.
        insert_oid(objects, view.required_content.into(), &mut closure).await;
        if let Ok(manifest) = reader.manifest(view.required_content).await {
            bfs_trie_nodes(objects, manifest.content.into(), &mut closure).await;
        }

        // Tree.
        walk_tree(objects, view.tree.into(), &mut closure).await;
    }

    // --- Walk labels manifest ---
    if let Ok(labels) = reader.view_label_manifest(envelope.labels).await {
        for label in labels.labels.values() {
            insert_oid(objects, label.view.into(), &mut closure).await;
        }
    }

    // --- Walk each retained revision's sub-structures ---
    for oid in &retained_revision_ids {
        let revision_id = wheat_core::RevisionId::from_digest(*oid.as_bytes());
        if let Ok(rev) = reader.revision(revision_id).await {
            insert_oid(objects, rev.authored_against.into(), &mut closure).await;
            insert_oid(objects, rev.resulting_tree.into(), &mut closure).await;
            for edit_id in &rev.edits {
                insert_oid(objects, (*edit_id).into(), &mut closure).await;
                // Read the edit to include it.
                let _ = reader.edit(*edit_id).await;
            }
            // Walk the resulting tree.
            walk_tree(objects, rev.resulting_tree.into(), &mut closure).await;
        }
    }

    // --- Walk each retained application's sub-structures ---
    for oid in &retained_application_ids {
        let app_id = wheat_core::ApplicationId::from_digest(*oid.as_bytes());
        if let Ok(app) = reader.application(app_id).await {
            insert_oid(objects, app.revision_id.into(), &mut closure).await;
            insert_oid(objects, app.target_view.into(), &mut closure).await;
            match &app.result {
                wheat_core::ApplicationResultRecord::Clean { tree } => {
                    insert_oid(objects, (*tree).into(), &mut closure).await;
                    walk_tree(objects, (*tree).into(), &mut closure).await;
                }
                wheat_core::ApplicationResultRecord::Conflicted {
                    partial_tree,
                    conflict,
                } => {
                    insert_oid(objects, (*partial_tree).into(), &mut closure).await;
                    insert_oid(objects, (*conflict).into(), &mut closure).await;
                    walk_tree(objects, (*partial_tree).into(), &mut closure).await;
                }
            }
        }
    }

    // --- Walk each retained change ---
    let change_store = ChangeMapStore::new(objects);
    if let Ok(mut cursor) = change_store.cursor(state.changes).await {
        while let Ok(Some((_, change_oid))) = cursor.next().await {
            insert_oid(objects, change_oid.into(), &mut closure).await;
        }
    }

    // --- Final BFS pass to catch any remaining explicit references ---
    // This handles objects like edits, trees, content-set nodes, etc.
    // that may have been missed if the targeted walks above didn't
    // cover every code path that validate_complete touches.
    let snapshot: Vec<ObjectId> = closure.keys().copied().collect();
    let mut frontier = Vec::new();
    for oid in snapshot {
        if let Some(obj) = closure.get(&oid)
            && let Ok(children) = wheat_core::extract_object_references(&obj.canonical_bytes)
        {
            for child in children {
                if !closure.contains_key(&child) {
                    frontier.push(child);
                }
            }
        }
    }
    while let Some(oid) = frontier.pop() {
        if closure.contains_key(&oid) {
            continue;
        }
        let Ok(obj) = objects.get(oid).await else {
            continue;
        };
        if let Ok(children) = wheat_core::extract_object_references(&obj.canonical_bytes) {
            for child in children {
                if !closure.contains_key(&child) {
                    frontier.push(child);
                }
            }
        }
        closure.insert(oid, obj);
    }

    closure
}

/// Insert an ObjectId into the closure map, fetching its StoredObject from
/// the store if not already present. Skips if already in the map.
async fn insert_oid<O: ImmutableObjectStore>(
    objects: &O,
    oid: ObjectId,
    closure: &mut HashMap<ObjectId, StoredObject>,
) {
    if closure.contains_key(&oid) {
        return;
    }
    if let Ok(obj) = objects.get(oid).await {
        closure.insert(oid, obj);
    }
}

/// BFS through a trie's structural nodes. Adds every node ObjectId and its
/// StoredObject to the closure. Does NOT walk trie keys — use
/// `RetainedSetStore::cursor` for that.
async fn bfs_trie_nodes<O: ImmutableObjectStore>(
    objects: &O,
    root: ObjectId,
    closure: &mut HashMap<ObjectId, StoredObject>,
) {
    let mut frontier = vec![root];
    while let Some(oid) = frontier.pop() {
        if closure.contains_key(&oid) {
            continue;
        }
        let Ok(obj) = objects.get(oid).await else {
            continue;
        };
        if let Ok(children) = wheat_core::extract_object_references(&obj.canonical_bytes) {
            for child in children {
                if !closure.contains_key(&child) {
                    frontier.push(child);
                }
            }
        }
        closure.insert(oid, obj);
    }
}

/// Walk a merkle tree, adding every tree node ObjectId to the closure.
async fn walk_tree<O: ImmutableObjectStore>(
    objects: &O,
    root: ObjectId,
    closure: &mut HashMap<ObjectId, StoredObject>,
) {
    // Trees are just another node type — BFS through child references works.
    bfs_trie_nodes(objects, root, closure).await;
}
