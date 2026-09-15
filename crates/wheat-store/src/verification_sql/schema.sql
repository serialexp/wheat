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
CREATE TABLE verified_trees (
    tree_id BLOB NOT NULL,
    aggregate_id BLOB NOT NULL,
    visible_id BLOB NOT NULL,
    PRIMARY KEY (tree_id, aggregate_id, visible_id)
) WITHOUT ROWID;
CREATE TABLE verified_visible_sets (
    node_id BLOB NOT NULL,
    aggregate_id BLOB NOT NULL,
    PRIMARY KEY (node_id, aggregate_id)
) WITHOUT ROWID;
CREATE TABLE verified_revision_maps (
    node_id BLOB NOT NULL,
    retained_root BLOB NOT NULL,
    PRIMARY KEY (node_id, retained_root)
) WITHOUT ROWID;
-- A pair receipt is deliberately written only for subtrees containing no conflicts.
-- Thus pruning one can never hide a conflict needed by reverse conflict validation.
CREATE TABLE verified_map_pairs (
    application_node BLOB NOT NULL,
    revision_node BLOB NOT NULL,
    retained_root BLOB NOT NULL,
    conflict_root BLOB NOT NULL,
    PRIMARY KEY (application_node, revision_node, retained_root, conflict_root)
) WITHOUT ROWID;

-- Verification scratch has no durability value and is deleted with the run. Keep
-- its millions of membership and receipt operations in one transaction instead
-- of paying SQLite's per-statement autocommit machinery.
BEGIN IMMEDIATE;
