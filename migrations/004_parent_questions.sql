-- Executed in a transaction with foreign_keys disabled during the table rebuild.
CREATE TABLE runs_new (
    queue_position INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    input TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued','running','completed','failed','cancelled','timed_out','waiting_for_parent')),
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
INSERT INTO runs_new SELECT * FROM runs;
DROP TABLE runs;
ALTER TABLE runs_new RENAME TO runs;
CREATE INDEX runs_pending ON runs(status, queue_position);
CREATE INDEX runs_session_order ON runs(session_id, queue_position);
CREATE UNIQUE INDEX runs_session_active ON runs(session_id) WHERE status='running';
CREATE TABLE messages_new (
    id INTEGER PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    role TEXT NOT NULL CHECK (role IN ('user','assistant','tool')),
    content TEXT NOT NULL,
    tool_calls TEXT,
    tool_call_id TEXT
);
INSERT INTO messages_new(id,run_id,role,content) SELECT id,run_id,role,content FROM messages;
DROP TABLE messages;
ALTER TABLE messages_new RENAME TO messages;
CREATE TABLE questions (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    ordinal INTEGER NOT NULL,
    call_id TEXT NOT NULL,
    question_json TEXT NOT NULL,
    assistant_json TEXT NOT NULL,
    answer TEXT,
    receipt_json TEXT,
    UNIQUE(run_id,ordinal),
    UNIQUE(run_id,call_id)
);
CREATE UNIQUE INDEX questions_pending ON questions(run_id) WHERE answer IS NULL;
PRAGMA user_version = 4;
