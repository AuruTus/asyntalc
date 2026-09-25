ALTER TABLE questions ADD COLUMN model_turn INTEGER NOT NULL DEFAULT 0;
UPDATE questions SET model_turn=ordinal;

CREATE TABLE tool_exchanges (
    run_id TEXT NOT NULL REFERENCES runs(id),
    model_turn INTEGER NOT NULL,
    call_id TEXT NOT NULL,
    assistant_json TEXT NOT NULL,
    result_json TEXT NOT NULL,
    PRIMARY KEY(run_id, model_turn),
    UNIQUE(run_id, call_id)
);

PRAGMA user_version=5;
