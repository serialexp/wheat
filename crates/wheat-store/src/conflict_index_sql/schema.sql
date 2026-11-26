CREATE TABLE IF NOT EXISTS application (
    application BLOB PRIMARY KEY,
    tail INTEGER
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS entry (
    id INTEGER PRIMARY KEY,
    application BLOB UNIQUE NOT NULL,
    previous INTEGER,
    jump INTEGER,
    jump_ordinal INTEGER NOT NULL,
    jump_position INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    position INTEGER NOT NULL,
    conflict BLOB NOT NULL,
    change BLOB NOT NULL
);
