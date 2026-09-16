PRAGMA journal_mode = OFF;
PRAGMA synchronous = OFF;
PRAGMA cache_size = -8192;
PRAGMA temp_store = FILE;

CREATE TABLE changes (
    change_id BLOB PRIMARY KEY NOT NULL,
    object_id BLOB NOT NULL
) WITHOUT ROWID;
CREATE TABLE aggregate_content (
    content_id BLOB PRIMARY KEY NOT NULL
) WITHOUT ROWID;
CREATE TABLE retained_objects (
    family INTEGER NOT NULL,
    object_id BLOB NOT NULL,
    PRIMARY KEY (family, object_id)
) WITHOUT ROWID;
CREATE TABLE revision_records (
    object_id BLOB PRIMARY KEY NOT NULL,
    change_id BLOB NOT NULL
) WITHOUT ROWID;
CREATE TABLE application_records (
    object_id BLOB PRIMARY KEY NOT NULL,
    revision_id BLOB NOT NULL,
    conflict_id BLOB
) WITHOUT ROWID;
-- Structural tree receipts are context-free but retain subtree height so a
-- node authenticated shallowly cannot evade the directory-depth limit later.
CREATE TABLE verified_tree_auth (
    tree_id BLOB PRIMARY KEY NOT NULL,
    height INTEGER NOT NULL
) WITHOUT ROWID;

-- Semantic receipts remain context-specific: aggregate membership and a view's
-- visible manifest are independent of structural tree authentication.
CREATE TABLE verified_tree_semantics (
    tree_id BLOB NOT NULL,
    aggregate_id BLOB NOT NULL,
    visible_id BLOB NOT NULL,
    PRIMARY KEY (tree_id, aggregate_id, visible_id)
) WITHOUT ROWID;

-- Current DFS path's file-content multiplicities. This keeps branch state
-- disk-backed and bounded without cloning whole trees for sibling views.
CREATE TABLE tree_content_counts (
    content_id BLOB PRIMARY KEY NOT NULL,
    count INTEGER NOT NULL CHECK (count > 0)
) WITHOUT ROWID;

-- Before-images for one unresolved branch. Unlike SQLite savepoints, this
-- journal restores only count state and never rolls back authenticated receipts.
CREATE TABLE tree_content_undo (
    mark_id INTEGER NOT NULL,
    content_id BLOB NOT NULL,
    prior_count INTEGER NOT NULL CHECK (prior_count >= 0),
    PRIMARY KEY (mark_id, content_id)
) WITHOUT ROWID;
-- A radix node can be reused under different incoming prefixes. Receipts must
-- name that reconstructed prefix as well as the external validation context.
CREATE TABLE verified_radix_nodes (
    kind INTEGER NOT NULL,
    context_id BLOB NOT NULL,
    node_id BLOB NOT NULL,
    prefix BLOB NOT NULL,
    PRIMARY KEY (kind, context_id, node_id, prefix)
) WITHOUT ROWID;
-- Pair receipts are contextual: compressed nodes can be reached at multiple prefixes.
-- A receipt is written only after the whole subtree has been checked and contributes
-- no conflict evidence, so pruning it cannot hide reverse-conflict validation.
CREATE TABLE verified_map_pairs (
    application_node BLOB NOT NULL,
    revision_node BLOB NOT NULL,
    prefix BLOB NOT NULL,
    application_skip INTEGER NOT NULL,
    revision_skip INTEGER NOT NULL,
    retained_root BLOB NOT NULL,
    conflict_root BLOB NOT NULL,
    PRIMARY KEY (
        application_node, revision_node, prefix, application_skip, revision_skip,
        retained_root, conflict_root
    )
) WITHOUT ROWID;

-- Conflict evidence is stream-produced during paired map verification. Keeping it
-- disk-backed avoids an unbounded verifier HashSet and makes reverse checks exact.
CREATE TABLE produced_conflicts (
    conflict_root BLOB NOT NULL,
    conflict_id BLOB NOT NULL,
    PRIMARY KEY (conflict_root, conflict_id)
) WITHOUT ROWID;

-- Verification scratch has no durability value and is deleted with the run. Keep
-- its millions of membership and receipt operations in one transaction instead
-- of paying SQLite's per-statement autocommit machinery.
BEGIN IMMEDIATE;
