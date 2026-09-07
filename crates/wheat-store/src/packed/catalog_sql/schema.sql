PRAGMA journal_mode = OFF;
PRAGMA synchronous = OFF;
PRAGMA temp_store = MEMORY;
PRAGMA cache_size = -8192;
CREATE TABLE locator (
    namespace INTEGER NOT NULL,
    digest BLOB NOT NULL CHECK(length(digest) = 32),
    segment_file TEXT NOT NULL,
    PRIMARY KEY (namespace, digest)
) WITHOUT ROWID;
