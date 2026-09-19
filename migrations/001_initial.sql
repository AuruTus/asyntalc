BEGIN IMMEDIATE;
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    created_at_ms INTEGER NOT NULL
);
CREATE TABLE runs (
    queue_position INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    input TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'completed', 'failed')),
    revision INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    finished_at_ms INTEGER,
    result_text TEXT,
    error_code TEXT
);
CREATE INDEX runs_pending ON runs(status, queue_position);
CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
    content TEXT NOT NULL
);
CREATE TABLE events (
    run_id TEXT NOT NULL REFERENCES runs(id),
    sequence INTEGER NOT NULL,
    kind TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (run_id, sequence)
);
PRAGMA user_version = 1;
COMMIT;
