-- Executed in a transaction with foreign_keys disabled during the table rebuild.
CREATE TABLE runs_new (
    queue_position INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    input TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued','running','completed','failed','cancelled','timed_out')),
    revision INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    finished_at_ms INTEGER,
    result_text TEXT,
    error_code TEXT,
    finish_reason TEXT,
    error_message TEXT,
    model_requests INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER,
    output_tokens INTEGER,
    partial_text TEXT,
    deadline_at_ms INTEGER,
    stop_reason TEXT CHECK (stop_reason IN ('cancelled','timed_out'))
);
INSERT INTO runs_new SELECT *, CASE WHEN status IN ('queued','running')
    THEN unixepoch()*1000 + 600000 ELSE NULL END, NULL FROM runs;
DROP TABLE runs;
ALTER TABLE runs_new RENAME TO runs;
CREATE INDEX runs_pending ON runs(status, queue_position);
CREATE INDEX runs_session_order ON runs(session_id, queue_position);
CREATE UNIQUE INDEX runs_session_active ON runs(session_id) WHERE status='running';
ALTER TABLE sessions ADD COLUMN scheduler_order INTEGER NOT NULL DEFAULT 0;
CREATE TABLE submissions (
    key TEXT PRIMARY KEY,
    request_json TEXT NOT NULL,
    receipt_json TEXT NOT NULL,
    run_id TEXT NOT NULL UNIQUE REFERENCES runs(id)
);
PRAGMA user_version = 3;
