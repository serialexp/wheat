CREATE TABLE IF NOT EXISTS application (
    id INTEGER PRIMARY KEY,
    application BLOB UNIQUE NOT NULL,
    change BLOB NOT NULL,
    position INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS path (
    id INTEGER PRIMARY KEY,
    path TEXT UNIQUE NOT NULL
);
CREATE TABLE IF NOT EXISTS touch (
    path INTEGER NOT NULL,
    position INTEGER NOT NULL,
    application INTEGER NOT NULL,
    PRIMARY KEY (path, position, application)
) WITHOUT ROWID;
