use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

const MAX_PENDING: i64 = 128;
type Job = Box<dyn FnOnce(&mut Connection) + Send>;

#[derive(Clone)]
pub struct Store {
    sender: mpsc::Sender<Job>,
}

#[derive(Debug)]
pub struct StoreError(pub &'static str);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for StoreError {}

#[derive(Serialize)]
pub struct Snapshot {
    pub run_id: String,
    pub session_id: String,
    pub status: String,
    pub revision: i64,
    pub phase: &'static str,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub result: Option<Value>,
    pub run_error: Option<Value>,
}

pub struct Work {
    pub run_id: String,
    pub input: String,
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        anyhow::ensure!(
            version <= 1,
            "database schema is newer than this executable"
        );
        if version == 0 {
            conn.execute_batch(include_str!("../migrations/001_initial.sql"))?;
        }
        recover(&mut conn)?;
        let (sender, mut receiver) = mpsc::channel::<Job>(64);
        std::thread::Builder::new()
            .name("asyntalc-store".into())
            .spawn(move || {
                while let Some(job) = receiver.blocking_recv() {
                    job(&mut conn);
                }
            })?;
        Ok(Self { sender })
    }

    async fn call<T, F>(&self, job: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> anyhow::Result<T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(Box::new(move |conn| {
                let _ = tx.send(job(conn));
            }))
            .await
            .map_err(|_| anyhow::anyhow!("database worker stopped"))?;
        rx.await.context("database worker stopped")?
    }

    pub async fn barrier(&self) -> anyhow::Result<()> {
        self.call(|_| Ok(())).await
    }

    pub async fn submit(&self, session_id: Option<String>, input: String) -> anyhow::Result<Value> {
        self.call(move |conn| {
            let tx = conn.transaction()?;
            let pending: i64 = tx.query_row("SELECT count(*) FROM runs WHERE status IN ('queued','running')", [], |r| r.get(0))?;
            if pending >= MAX_PENDING { return Err(StoreError("capacity_exceeded").into()); }
            let session_id = session_id.unwrap_or_else(|| format!("session_{}", uuid::Uuid::new_v4()));
            let run_id = format!("run_{}", uuid::Uuid::new_v4());
            let now = now_ms();
            tx.execute("INSERT OR IGNORE INTO sessions (id, created_at_ms) VALUES (?1, ?2)", params![session_id, now])?;
            tx.execute("INSERT INTO runs (id, session_id, input, status, revision, created_at_ms) VALUES (?1, ?2, ?3, 'queued', 1, ?4)", params![run_id, session_id, input, now])?;
            event(&tx, &run_id, 1, "run.submitted")?;
            tx.commit()?;
            Ok(json!({"run_id": run_id, "session_id": session_id, "status": "queued", "revision": 1}))
        }).await
    }

    pub async fn snapshot(&self, run_id: String, include_text: bool) -> anyhow::Result<Snapshot> {
        self.call(move |conn| {
            let snapshot = conn.query_row(
                "SELECT id, session_id, status, revision, created_at_ms, started_at_ms, finished_at_ms, result_text, error_code FROM runs WHERE id = ?1",
                [&run_id], |row| {
                    let status: String = row.get(2)?;
                    let text: Option<String> = row.get(7)?;
                    let error: Option<String> = row.get(8)?;
                    let result = text.map(|text| {
                        let mut end = text.len().min(16 * 1024);
                        while !text.is_char_boundary(end) { end -= 1; }
                        json!({"format": "text", "text": if include_text { Some(&text[..end]) } else { None }, "truncated": include_text && end < text.len(), "full_result_available": true, "finish_reason": "stop"})
                    });
                    Ok(Snapshot {
                        run_id: row.get(0)?, session_id: row.get(1)?,
                        phase: match status.as_str() { "queued" => "queued", "running" => "fake_execution", _ => "finished" },
                        status, revision: row.get(3)?, created_at_ms: row.get(4)?, started_at_ms: row.get(5)?, finished_at_ms: row.get(6)?, result,
                        run_error: error.map(|code| json!({"code": code, "message": "Daemon stopped before the run completed"})),
                    })
                }).optional()?.ok_or(StoreError("run_not_found"))?;
            Ok(snapshot)
        }).await
    }

    pub async fn result(&self, run_id: String) -> anyhow::Result<Value> {
        self.call(move |conn| {
            let row: Option<(String, Option<String>)> = conn.query_row(
                "SELECT session_id, result_text FROM runs WHERE id = ?1", [&run_id],
                |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
            let (session_id, text) = row.ok_or(StoreError("run_not_found"))?;
            let text = text.ok_or(StoreError("result_not_ready"))?;
            Ok(json!({"run_id": run_id, "session_id": session_id, "status": "completed", "result": {"format": "text", "text": text, "truncated": false, "full_result_available": true, "finish_reason": "stop"}}))
        }).await
    }

    // Milestone 1 intentionally uses one serial worker; submission order is durable.
    pub async fn claim(&self) -> anyhow::Result<Option<Work>> {
        self.call(|conn| {
            let tx = conn.transaction()?;
            let work = tx.query_row("SELECT id, input FROM runs WHERE status = 'queued' ORDER BY queue_position LIMIT 1", [],
                |row| Ok(Work { run_id: row.get(0)?, input: row.get(1)? })).optional()?;
            if let Some(work) = &work {
                tx.execute("UPDATE runs SET status = 'running', revision = 2, started_at_ms = ?2 WHERE id = ?1", params![work.run_id, now_ms()])?;
                event(&tx, &work.run_id, 2, "run.started")?;
            }
            tx.commit()?;
            Ok(work)
        }).await
    }

    pub async fn complete(&self, work: Work) -> anyhow::Result<()> {
        self.call(move |conn| {
            let tx = conn.transaction()?;
            let result = format!("[fake] {}", work.input);
            let changed = tx.execute("UPDATE runs SET status = 'completed', revision = 3, result_text = ?2, finished_at_ms = ?3 WHERE id = ?1 AND status = 'running'", params![work.run_id, result, now_ms()])?;
            anyhow::ensure!(changed == 1, "run is no longer running");
            tx.execute("INSERT INTO messages (run_id, role, content) VALUES (?1, 'user', ?2)", params![work.run_id, work.input])?;
            tx.execute("INSERT INTO messages (run_id, role, content) VALUES (?1, 'assistant', ?2)", params![work.run_id, result])?;
            event(&tx, &work.run_id, 3, "run.completed")?;
            tx.commit()?;
            Ok(())
        }).await
    }
}

fn recover(conn: &mut Connection) -> anyhow::Result<()> {
    let tx = conn.transaction()?;
    let interrupted = {
        let mut query = tx.prepare("SELECT id, revision FROM runs WHERE status = 'running'")?;
        query
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (id, revision) in interrupted {
        tx.execute("UPDATE runs SET status = 'failed', revision = revision + 1, error_code = 'daemon_interrupted', finished_at_ms = ?2 WHERE id = ?1", params![id, now_ms()])?;
        event(&tx, &id, revision + 1, "run.failed")?;
    }
    tx.commit()?;
    Ok(())
}

fn event(
    tx: &rusqlite::Transaction<'_>,
    run_id: &str,
    revision: i64,
    kind: &str,
) -> rusqlite::Result<usize> {
    tx.execute(
        "INSERT INTO events (run_id, sequence, kind, created_at_ms) VALUES (?1, ?2, ?3, ?4)",
        params![run_id, revision, kind, now_ms()],
    )
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis() as i64
}
