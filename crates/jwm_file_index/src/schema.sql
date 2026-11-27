PRAGMA trusted_schema=OFF;
PRAGMA temp_store=MEMORY;
PRAGMA foreign_keys=ON;
CREATE TABLE roots(
    id INTEGER PRIMARY KEY,
    incarnation BLOB NOT NULL UNIQUE CHECK (typeof(incarnation) = 'blob' AND length(incarnation)=16),
    raw_root BLOB NOT NULL UNIQUE CHECK (typeof(raw_root) = 'blob' AND length(raw_root) BETWEEN 1 AND 4096),
    root_identity BLOB NOT NULL CHECK (typeof(root_identity) = 'blob' AND length(root_identity)=16),
    enabled INTEGER NOT NULL CHECK (enabled in (0, 1)),
    generation INTEGER NOT NULL CHECK (generation >= 0)
);
CREATE TABLE entries(
    id INTEGER PRIMARY KEY,
    root_id INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
    raw_key BLOB NOT NULL CHECK (typeof(raw_key) = 'blob'),
    folded TEXT NOT NULL,
    UNIQUE (root_id, raw_key)
);
CREATE VIRTUAL TABLE names USING fts5(
    folded, content='entries', content_rowid='id', tokenize='trigram case_sensitive 1'
);
CREATE TRIGGER entries_insert AFTER INSERT ON entries BEGIN
    INSERT INTO names(rowid, folded) VALUES (new.id, new.folded);
END;
CREATE TRIGGER entries_delete AFTER DELETE ON entries BEGIN
    INSERT INTO names(names, rowid, folded) VALUES ('delete', old.id, old.folded);
END;
