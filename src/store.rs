use std::{
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::{
    config::Profile,
    provider::{Completion, Failure, Message, Usage},
};

const MAX_PENDING: i64 = 128;
type Job = Box<dyn FnOnce(&mut Connection) + Send>;

#[derive(Clone)]
pub struct Store {
    worker: Arc<DatabaseWorker>,
    profile_json: String,
}

struct DatabaseWorker {
    sender: Option<mpsc::Sender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for DatabaseWorker {
    fn drop(&mut self) {
        // Closing SQLite must finish before this database can be reopened.
        drop(self.sender.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
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
    pub usage: Usage,
    pub partial_result: Option<Value>,
    pub deadline_at_ms: Option<i64>,
    pub cancellation_requested: bool,
    pub blocked_by_run_id: Option<String>,
    pub input_request: Option<Value>,
}

pub struct Work {
    pub run_id: String,
    pub session_id: String,
    pub input: String,
}

impl Store {
    pub fn open(path: &Path, profile: &Profile) -> anyhow::Result<Self> {
        let mut conn = Connection::open(path)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        anyhow::ensure!(
            version <= 5,
            "database schema is newer than this executable"
        );
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        if version == 0 {
            conn.execute_batch(include_str!("../migrations/001_initial.sql"))?;
        }
        if version < 2 {
            conn.execute_batch(include_str!("../migrations/002_chat_provider.sql"))?;
        }
        if version < 3 {
            conn.pragma_update(None, "foreign_keys", "OFF")?;
            let tx = conn.transaction()?;
            tx.execute_batch(include_str!("../migrations/003_scheduler.sql"))?;
            let violations: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                    r.get(0)
                })?;
            anyhow::ensure!(violations == 0, "migration would violate foreign keys");
            tx.commit()?;
            conn.pragma_update(None, "foreign_keys", "ON")?;
        }
        if version < 4 {
            conn.pragma_update(None, "foreign_keys", "OFF")?;
            let tx = conn.transaction()?;
            tx.execute_batch(include_str!("../migrations/004_parent_questions.sql"))?;
            let violations: i64 =
                tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                    r.get(0)
                })?;
            anyhow::ensure!(violations == 0, "migration would violate foreign keys");
            tx.commit()?;
            conn.pragma_update(None, "foreign_keys", "ON")?;
        }
        if version < 5 {
            let tx = conn.transaction()?;
            tx.execute_batch(include_str!("../migrations/005_workspace_tools.sql"))?;
            tx.commit()?;
        }
        let profile_json = serde_json::to_string(profile)?;
        let mismatched: i64 = conn.query_row("SELECT count(*) FROM runs r JOIN sessions s ON s.id=r.session_id WHERE r.status IN ('queued','waiting_for_parent') AND r.deadline_at_ms > ?2 AND s.profile_json != ?1", params![profile_json, now_ms()], |r| r.get(0))?;
        anyhow::ensure!(
            mismatched == 0,
            "queued or waiting runs require their original provider configuration; restore it or use a separate data directory"
        );
        recover(&mut conn)?;
        let (sender, mut receiver) = mpsc::channel::<Job>(64);
        let thread = std::thread::Builder::new()
            .name("asyntalc-store".into())
            .spawn(move || {
                while let Some(job) = receiver.blocking_recv() {
                    job(&mut conn);
                }
            })?;
        Ok(Self {
            worker: Arc::new(DatabaseWorker {
                sender: Some(sender),
                thread: Some(thread),
            }),
            profile_json,
        })
    }

    async fn call<T, F>(&self, job: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> anyhow::Result<T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.worker
            .sender
            .as_ref()
            .expect("live database worker")
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

    pub fn scope(&self) -> anyhow::Result<Value> {
        Ok(serde_json::from_str::<Profile>(&self.profile_json)?.scope())
    }

    pub async fn submit(
        &self,
        session_id: Option<String>,
        input: String,
        run_timeout_ms: u64,
        idempotency_key: Option<String>,
    ) -> anyhow::Result<Value> {
        let profile = self.profile_json.clone();
        self.call(move |conn| {
            let tx = conn.transaction()?;
            let request = json!({"session_id":session_id,"input":input,"run_timeout_ms":run_timeout_ms,"profile":profile}).to_string();
            if let Some(key) = &idempotency_key {
                let previous: Option<(String,String)> = tx.query_row("SELECT request_json, receipt_json FROM submissions WHERE key=?1", [key], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
                if let Some((original, receipt)) = previous {
                    if original != request { return Err(StoreError("idempotency_conflict").into()); }
                    return Ok(serde_json::from_str(&receipt)?);
                }
            }
            let session_id = session_id.unwrap_or_else(|| format!("session_{}", uuid::Uuid::new_v4()));
            let run_id = format!("run_{}", uuid::Uuid::new_v4());
            let now = now_ms();
            tx.execute("INSERT OR IGNORE INTO sessions (id, created_at_ms, profile_json) VALUES (?1, ?2, ?3)", params![session_id, now, profile])?;
            let stored_profile: String = tx.query_row("SELECT profile_json FROM sessions WHERE id=?1", [&session_id], |r| r.get(0))?;
            if stored_profile != profile { return Err(StoreError("session_config_conflict").into()); }
            let busy: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE session_id=?1 AND status IN ('queued','running','waiting_for_parent'))", [&session_id], |r| r.get(0))?;
            if busy { return Err(StoreError("session_busy").into()); }
            let pending: i64 = tx.query_row("SELECT count(*) FROM runs WHERE status IN ('queued','running','waiting_for_parent')", [], |r| r.get(0))?;
            if pending >= MAX_PENDING { return Err(StoreError("capacity_exceeded").into()); }
            tx.execute("UPDATE sessions SET scheduler_order=(SELECT coalesce(max(scheduler_order),0)+1 FROM sessions) WHERE id=?1 AND NOT EXISTS (SELECT 1 FROM runs WHERE session_id=?1 AND status IN ('queued','running','waiting_for_parent'))", [&session_id])?;
            let deadline = now + run_timeout_ms as i64;
            tx.execute("INSERT INTO runs (id, session_id, input, status, revision, created_at_ms, deadline_at_ms) VALUES (?1, ?2, ?3, 'queued', 1, ?4, ?5)", params![run_id, session_id, input, now, deadline])?;
            event(&tx, &run_id, 1, "run.submitted")?;
            let receipt = json!({"run_id": run_id, "session_id": session_id, "status": "queued", "revision": 1, "deadline_at_ms":deadline});
            if let Some(key) = idempotency_key {
                tx.execute("INSERT INTO submissions VALUES (?1,?2,?3,?4)", params![key,request,receipt.to_string(),run_id])?;
            }
            tx.commit()?;
            Ok(receipt)
        }).await
    }

    pub async fn cancel(&self, run_id: String) -> anyhow::Result<()> {
        self.call(move |conn| {
            let tx = conn.transaction()?;
            stop_if_due(&tx, &run_id, true)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn maintain(&self) -> anyhow::Result<(Vec<String>, Option<i64>, bool)> {
        self.call(|conn| {
            let tx = conn.transaction()?;
            let before = tx.total_changes();
            let ids = {
                let mut query = tx.prepare("SELECT id FROM runs WHERE status IN ('queued','running','waiting_for_parent') AND (stop_reason IS NOT NULL OR deadline_at_ms<=?1)")?;
                query.query_map([now_ms()], |r| r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?
            };
            for id in &ids { stop_if_due(&tx, id, false)?; }
            let next = tx.query_row("SELECT min(deadline_at_ms) FROM runs WHERE status IN ('queued','running','waiting_for_parent') AND stop_reason IS NULL", [], |r| r.get(0))?;
            let changed = tx.total_changes() != before;
            tx.commit()?;
            Ok((ids, next, changed))
        }).await
    }

    pub async fn snapshot(&self, run_id: String, include_text: bool) -> anyhow::Result<Snapshot> {
        self.call(move |conn| {
            let snapshot = conn.query_row(
                "SELECT r.id, r.session_id, r.status, r.revision, r.created_at_ms, r.started_at_ms, r.finished_at_ms, r.result_text, r.error_code, r.finish_reason, r.error_message, r.model_requests, r.input_tokens, r.output_tokens, r.partial_text, json_extract(s.profile_json, '$.runner'), r.deadline_at_ms, r.stop_reason, (SELECT p.id FROM runs p WHERE r.status='queued' AND p.session_id=r.session_id AND p.queue_position<r.queue_position AND p.status IN ('queued','running','waiting_for_parent') ORDER BY p.queue_position LIMIT 1), (SELECT question_json FROM questions WHERE run_id=r.id AND answer IS NULL AND r.status='waiting_for_parent') FROM runs r JOIN sessions s ON s.id=r.session_id WHERE r.id = ?1",
                [&run_id], |row| {
                    let status: String = row.get(2)?;
                    let text: Option<String> = row.get(7)?;
                    let error: Option<String> = row.get(8)?;
                    let finish_reason: Option<String> = row.get(9)?;
                    let error_message: Option<String> = row.get(10)?;
                    let partial_text: Option<String> = row.get(14)?;
                    let runner: String = row.get(15)?;
                    let stop_reason: Option<String> = row.get(17)?;
                    let result = text.map(|text| {
                        let mut end = text.len().min(16 * 1024);
                        while !text.is_char_boundary(end) { end -= 1; }
                        json!({"format": "text", "text": if include_text { Some(&text[..end]) } else { None }, "truncated": include_text && end < text.len(), "full_result_available": true, "finish_reason": finish_reason})
                    });
                    Ok(Snapshot {
                        input_request: row.get::<_,Option<String>>(19)?.map(|s| serde_json::from_str(&s).expect("stored question JSON")),
                        run_id: row.get(0)?, session_id: row.get(1)?,
                        phase: match status.as_str() { "queued" => "queued", "waiting_for_parent" => "waiting_for_parent", "running" if stop_reason.as_deref() == Some("cancelled") => "cancelling", "running" if stop_reason.is_some() => "timing_out", "running" if runner == "chat" => "model_request", "running" => "fake_execution", _ => "finished" },
                        deadline_at_ms: row.get(16)?, cancellation_requested: stop_reason.as_deref() == Some("cancelled"), blocked_by_run_id: row.get(18)?,
                        status, revision: row.get(3)?, created_at_ms: row.get(4)?, started_at_ms: row.get(5)?, finished_at_ms: row.get(6)?, result,
                        run_error: error.map(|code| json!({"code": code, "message": error_message, "finish_reason": finish_reason})),
                        usage: Usage { model_requests: row.get(11)?, input_tokens: row.get(12)?, output_tokens: row.get(13)? },
                        partial_result: partial_text.map(|text| json!({"format":"text", "text": if include_text { Some(text) } else { None }, "complete":false, "finish_reason":finish_reason})),
                    })
                }).optional()?.ok_or(StoreError("run_not_found"))?;
            Ok(snapshot)
        }).await
    }

    pub async fn result(&self, run_id: String) -> anyhow::Result<Value> {
        self.call(move |conn| {
            let row: Option<(String, Option<String>, Option<String>)> = conn.query_row(
                "SELECT session_id, result_text, finish_reason FROM runs WHERE id = ?1", [&run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
            let (session_id, text, finish_reason) = row.ok_or(StoreError("run_not_found"))?;
            let text = text.ok_or(StoreError("result_not_ready"))?;
            Ok(json!({"run_id": run_id, "session_id": session_id, "status": "completed", "result": {"format": "text", "text": text, "truncated": false, "full_result_available": true, "finish_reason": finish_reason}}))
        }).await
    }

    // Round-robin across eligible sessions; FIFO within each session.
    #[cfg(test)]
    pub async fn claim(&self) -> anyhow::Result<Option<Work>> {
        self.claim_available(Vec::new()).await
    }

    pub async fn claim_available(&self, active: Vec<String>) -> anyhow::Result<Option<Work>> {
        self.call(move |conn| {
            let tx = conn.transaction()?;
            let work = tx.query_row("SELECT r.id, r.input, r.session_id FROM runs r JOIN sessions s ON s.id=r.session_id WHERE r.status='queued' AND r.deadline_at_ms>?1 AND r.id NOT IN (SELECT value FROM json_each(?2)) AND NOT EXISTS (SELECT 1 FROM runs p WHERE p.session_id=r.session_id AND p.status IN ('queued','running','waiting_for_parent') AND p.queue_position<r.queue_position) ORDER BY s.scheduler_order, r.queue_position LIMIT 1", params![now_ms(),serde_json::to_string(&active)?],
                |row| Ok(Work { run_id: row.get(0)?, input: row.get(1)?, session_id: row.get(2)? })).optional()?;
            if let Some(work) = &work {
                let revision: i64 = tx.query_row("UPDATE runs SET status='running', revision=revision+1, started_at_ms=coalesce(started_at_ms,?2) WHERE id=?1 RETURNING revision", params![work.run_id,now_ms()], |r| r.get(0))?;
                event(&tx, &work.run_id, revision, "run.started")?;
                tx.execute("UPDATE sessions SET scheduler_order=(SELECT max(scheduler_order)+1 FROM sessions) WHERE id=?1", [&work.session_id])?;
            }
            tx.commit()?;
            Ok(work)
        }).await
    }

    pub async fn mark_requested(&self, run_id: String) -> anyhow::Result<bool> {
        let profile: Profile = serde_json::from_str(&self.profile_json)?;
        let limit = if matches!(profile, Profile::Chat(config) if config.workspace.is_some()) {
            25
        } else {
            9
        };
        self.call(move |conn| {
            let tx = conn.transaction()?;
            if stop_if_due(&tx, &run_id, false)? {
                tx.commit()?;
                return Ok(false);
            }
            let attempts: i64 = tx.query_row("SELECT model_requests FROM runs WHERE id=?1", [&run_id], |r| r.get(0))?;
            if attempts >= limit { return Err(StoreError("model_turn_limit").into()); }
            let revision: i64 = tx.query_row("UPDATE runs SET model_requests=model_requests+1, revision=revision+1 WHERE id=?1 AND status='running' RETURNING revision", [&run_id], |r| r.get(0))?;
            event(&tx, &run_id, revision, "run.model_requested")?;
            tx.commit()?;
            Ok(true)
        }).await
    }

    pub async fn complete(&self, work: Work, completion: Completion) -> anyhow::Result<()> {
        self.call(move |conn| {
            let tx = conn.transaction()?;
            if stop_if_due(&tx, &work.run_id, false)? {
                finish_stop(&tx, &work.run_id)?;
                tx.commit()?;
                return Ok(());
            }
            add_usage(&tx, &work.run_id, &completion.usage)?;
            let result = completion.text;
            let revision: i64 = tx.query_row("UPDATE runs SET status = 'completed', revision = revision+1, result_text = ?2, finished_at_ms = ?3, finish_reason=?4 WHERE id = ?1 AND status = 'running' RETURNING revision", params![work.run_id, result, now_ms(), completion.finish_reason], |r| r.get(0))?;
            tx.execute("INSERT INTO messages (run_id, role, content) VALUES (?1, 'user', ?2)", params![work.run_id, work.input])?;
            commit_questions(&tx, &work.run_id)?;
            tx.execute("INSERT INTO messages (run_id, role, content) VALUES (?1, 'assistant', ?2)", params![work.run_id, result])?;
            event(&tx, &work.run_id, revision, "run.completed")?;
            tx.commit()?;
            Ok(())
        }).await
    }

    pub async fn fail(&self, run_id: String, failure: Failure) -> anyhow::Result<()> {
        self.call(move |conn| {
            let tx = conn.transaction()?;
            if stop_if_due(&tx, &run_id, false)? {
                finish_stop(&tx, &run_id)?;
                tx.commit()?;
                return Ok(());
            }
            add_usage(&tx, &run_id, &failure.usage)?;
            let revision: i64 = tx.query_row("UPDATE runs SET status='failed', revision=revision+1, finished_at_ms=?2, error_code=?3, error_message=?4, finish_reason=?5, partial_text=?6 WHERE id=?1 AND status='running' RETURNING revision",
                params![run_id, now_ms(), failure.code, failure.message, failure.finish_reason, failure.partial_text], |r| r.get(0))?;
            event(&tx, &run_id, revision, "run.failed")?;
            tx.commit()?;
            Ok(())
        }).await
    }
}

// Persist the first stop decision. Active runs retain their session slot until cleanup.
fn stop_if_due(tx: &rusqlite::Transaction<'_>, id: &str, cancel: bool) -> anyhow::Result<bool> {
    let (status, stop, deadline): (String, Option<String>, Option<i64>) = tx
        .query_row(
            "SELECT status,stop_reason,deadline_at_ms FROM runs WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .ok_or(StoreError("run_not_found"))?;
    if !matches!(status.as_str(), "queued" | "running" | "waiting_for_parent") {
        return Ok(true);
    }
    let reason = stop.or_else(|| {
        if deadline.is_some_and(|d| d <= now_ms()) {
            Some("timed_out".into())
        } else if cancel {
            Some("cancelled".into())
        } else {
            None
        }
    });
    let Some(reason) = reason else {
        return Ok(false);
    };
    let revision: Option<i64> = tx.query_row("UPDATE runs SET stop_reason=?2,revision=revision+1 WHERE id=?1 AND stop_reason IS NULL RETURNING revision", params![id,reason], |r| r.get(0)).optional()?;
    if let Some(revision) = revision {
        event(
            tx,
            id,
            revision,
            if reason == "cancelled" {
                "run.cancellation_requested"
            } else {
                "run.timeout_requested"
            },
        )?;
    }
    if status != "running" {
        finish_stop(tx, id)?;
    }
    Ok(true)
}

fn finish_stop(tx: &rusqlite::Transaction<'_>, id: &str) -> anyhow::Result<()> {
    let changed: Option<(i64,String)> = tx.query_row("UPDATE runs SET status=stop_reason,revision=revision+1,finished_at_ms=?2,input_tokens=CASE WHEN model_requests>((SELECT count(*) FROM questions WHERE run_id=?1)+(SELECT count(*) FROM tool_exchanges WHERE run_id=?1)) THEN NULL ELSE input_tokens END,output_tokens=CASE WHEN model_requests>((SELECT count(*) FROM questions WHERE run_id=?1)+(SELECT count(*) FROM tool_exchanges WHERE run_id=?1)) THEN NULL ELSE output_tokens END,error_code=stop_reason,error_message=CASE stop_reason WHEN 'cancelled' THEN 'Run cancelled by client' ELSE 'Run deadline exceeded' END WHERE id=?1 AND status IN ('queued','running','waiting_for_parent') AND stop_reason IS NOT NULL RETURNING revision,status", params![id,now_ms()], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((revision, status)) = changed {
        event(tx, id, revision, &format!("run.{status}"))?;
    }
    Ok(())
}

fn recover(conn: &mut Connection) -> anyhow::Result<()> {
    let tx = conn.transaction()?;
    let interrupted = {
        let mut query =
            tx.prepare("SELECT id, revision FROM runs WHERE status IN ('queued','running','waiting_for_parent')")?;
        query
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (id, revision) in interrupted {
        if stop_if_due(&tx, &id, false)? {
            finish_stop(&tx, &id)?;
            continue;
        }
        let running: bool = tx.query_row(
            "SELECT status='running' FROM runs WHERE id=?1",
            [&id],
            |r| r.get(0),
        )?;
        if !running {
            continue;
        }
        tx.execute("UPDATE runs SET status = 'failed', revision = revision + 1, error_code = 'daemon_interrupted', input_tokens=NULL, output_tokens=NULL, error_message = 'Daemon stopped before the run completed', finished_at_ms = ?2 WHERE id = ?1", params![id, now_ms()])?;
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

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis() as i64
}

#[cfg(test)]
mod tests;

// Unknown usage in any attempted turn makes the aggregate unknown.
fn add_usage(tx: &rusqlite::Transaction<'_>, id: &str, usage: &Usage) -> anyhow::Result<()> {
    if usage.model_requests > 0 {
        tx.execute("UPDATE runs SET input_tokens=CASE WHEN model_requests=1 THEN ?2 WHEN input_tokens IS NULL OR ?2 IS NULL OR input_tokens>9223372036854775807-?2 THEN NULL ELSE input_tokens+?2 END, output_tokens=CASE WHEN model_requests=1 THEN ?3 WHEN output_tokens IS NULL OR ?3 IS NULL OR output_tokens>9223372036854775807-?3 THEN NULL ELSE output_tokens+?3 END WHERE id=?1", params![id,usage.input_tokens,usage.output_tokens])?;
    }
    Ok(())
}

mod parent;
use parent::commit_questions;

mod inspection;
mod tools;
