CREATE TABLE physical_source (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE
);
CREATE TABLE locator (
    namespace INTEGER NOT NULL,
    digest BLOB NOT NULL CHECK(length(digest) = 32),
    kind INTEGER NOT NULL,
    offset INTEGER NOT NULL CHECK(offset >= 0),
    length INTEGER NOT NULL CHECK(length >= 0),
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
    active_index_start INTEGER,
    active_directory_offset INTEGER,
    active_directory_length INTEGER,
    locator_count INTEGER NOT NULL DEFAULT 0 CHECK(locator_count >= 0),
    CHECK((catalog_generation IS NULL) = (catalog_digest IS NULL)),
    CHECK((active_generation IS NULL) = (active_id IS NULL)),
    CHECK((active_id IS NULL) = (active_object_count IS NULL)),
    CHECK((active_id IS NULL) = (active_payload_bytes IS NULL)),
    CHECK((active_id IS NULL) = (active_total_length IS NULL)),
    CHECK((active_id IS NULL) = (active_index_start IS NULL)),
    CHECK((active_id IS NULL) = (active_directory_offset IS NULL)),
    CHECK((active_id IS NULL) = (active_directory_length IS NULL))
);
INSERT INTO checkpoint(singleton) VALUES(1);
PRAGMA user_version = 2;
