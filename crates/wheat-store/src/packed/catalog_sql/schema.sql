CREATE TABLE physical_source (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE
);
CREATE TABLE locator (
    namespace INTEGER NOT NULL,
    digest BLOB NOT NULL CHECK(length(digest) = 32),
    kind INTEGER NOT NULL,
    offset INTEGER NOT NULL CHECK(offset >= 0),
    codec INTEGER NOT NULL CHECK(codec IN (0, 1, 2, 3, 4)),
    stored_length INTEGER NOT NULL CHECK(stored_length >= 0),
    logical_length INTEGER NOT NULL CHECK(logical_length >= 0),
    source_id INTEGER NOT NULL REFERENCES physical_source(id),
    PRIMARY KEY (namespace, digest)
) WITHOUT ROWID;
CREATE TABLE checkpoint (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    catalog_generation INTEGER,
    catalog_digest BLOB CHECK(catalog_digest IS NULL OR length(catalog_digest) = 32),
    active_generation INTEGER,
    active_id BLOB CHECK(active_id IS NULL OR length(active_id) = 32),
    active_object_count INTEGER,
    active_payload_bytes INTEGER,
    active_total_length INTEGER,
    locator_count INTEGER NOT NULL DEFAULT 0 CHECK(locator_count >= 0),
    loose_recorded INTEGER NOT NULL DEFAULT 0 CHECK(loose_recorded IN (0, 1)),
    loose_object_bytes INTEGER NOT NULL DEFAULT 0 CHECK(loose_object_bytes >= 0),
    loose_content_bytes INTEGER NOT NULL DEFAULT 0 CHECK(loose_content_bytes >= 0),
    CHECK((catalog_generation IS NULL) = (catalog_digest IS NULL)),
    CHECK((active_generation IS NULL) = (active_id IS NULL)),
    CHECK((active_id IS NULL) = (active_object_count IS NULL)),
    CHECK((active_id IS NULL) = (active_payload_bytes IS NULL)),
    CHECK((active_id IS NULL) = (active_total_length IS NULL))
);
INSERT INTO checkpoint(singleton) VALUES(1);
-- Every file in loose-objects (kind 0) and loose-content (kind 1). A pending row's
-- file is being written or removed: open settles it by looking. The checkpoint's
-- loose byte totals count only settled rows.
CREATE TABLE loose_file (
    kind INTEGER NOT NULL CHECK(kind IN (0, 1)),
    digest BLOB NOT NULL CHECK(length(digest) = 32),
    length INTEGER NOT NULL CHECK(length >= 0),
    pending INTEGER NOT NULL CHECK(pending IN (0, 1)),
    PRIMARY KEY (kind, digest)
) WITHOUT ROWID;
CREATE INDEX loose_file_pending ON loose_file(pending) WHERE pending = 1;
PRAGMA user_version = 7;
