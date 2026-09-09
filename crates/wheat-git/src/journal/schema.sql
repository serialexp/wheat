CREATE TABLE IF NOT EXISTS journal_identity (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    format_version INTEGER NOT NULL,
    source_snapshot BLOB NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS work (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    oid BLOB NOT NULL UNIQUE,
    status INTEGER NOT NULL CHECK (status IN (0, 1)),
    change_id BLOB,
    revision_id BLOB,
    view_id BLOB,
    application_id BLOB,
    CHECK (
        (status = 0 AND change_id IS NULL AND revision_id IS NULL
            AND view_id IS NULL AND application_id IS NULL)
        OR
        (status = 1 AND change_id IS NOT NULL AND revision_id IS NOT NULL
            AND view_id IS NOT NULL AND application_id IS NOT NULL)
    )
) STRICT;

CREATE TABLE IF NOT EXISTS work_parent (
    oid BLOB NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0),
    parent_oid BLOB NOT NULL,
    PRIMARY KEY (oid, position),
    FOREIGN KEY (oid) REFERENCES work(oid) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS work_parent_parent_oid ON work_parent(parent_oid);

-- Prepared mappings remain separate until backing-store proof and are gated by
-- journal format version 2; older prototype journals are deliberately restarted.
CREATE TABLE IF NOT EXISTS prepared_work (
    oid BLOB PRIMARY KEY,
    change_id BLOB NOT NULL,
    revision_id BLOB NOT NULL,
    view_id BLOB NOT NULL,
    application_id BLOB NOT NULL,
    FOREIGN KEY (oid) REFERENCES work(oid) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
