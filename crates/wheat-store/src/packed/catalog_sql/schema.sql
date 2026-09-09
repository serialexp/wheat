CREATE TABLE IF NOT EXISTS locator (
    namespace INTEGER NOT NULL,
    digest BLOB NOT NULL CHECK(length(digest) = 32),
    segment_file TEXT NOT NULL,
    PRIMARY KEY (namespace, digest)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS checkpoint (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    generation INTEGER,
    head_digest BLOB CHECK(head_digest IS NULL OR length(head_digest) = 32),
    CHECK((generation IS NULL) = (head_digest IS NULL))
);
INSERT OR IGNORE INTO checkpoint(singleton, generation, head_digest) VALUES(1, NULL, NULL);
