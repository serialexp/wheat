CREATE TABLE IF NOT EXISTS stamp (
    id INTEGER PRIMARY KEY CHECK (id = 0),
    lineage BLOB NOT NULL,
    generation INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS anchor (
    view BLOB PRIMARY KEY NOT NULL,
    descendants INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS descendant (
    anchor BLOB NOT NULL,
    view BLOB NOT NULL,
    tip INTEGER NOT NULL,
    PRIMARY KEY (anchor, view)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS descendant_by_view ON descendant (view);
CREATE INDEX IF NOT EXISTS descendant_tips ON descendant (anchor, view) WHERE tip = 1;
