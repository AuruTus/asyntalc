use super::*;
use crate::protocol::{MAX_PAGE_LIMIT, RUN_STATUSES};

fn validate_page(after: i64, limit: u32) -> anyhow::Result<()> {
    if after < 0 {
        return Err(StoreError("invalid_cursor").into());
    }
    if !(1..=MAX_PAGE_LIMIT).contains(&limit) {
        return Err(StoreError("invalid_limit").into());
    }
    Ok(())
}

impl Store {
    pub async fn list(
        &self,
        session: Option<String>,
        status: Option<String>,
        after: i64,
        limit: u32,
    ) -> anyhow::Result<Value> {
        validate_page(after, limit)?;
        if status
            .as_deref()
            .is_some_and(|s| !RUN_STATUSES.contains(&s))
        {
            return Err(StoreError("invalid_status").into());
        }
        if session.as_ref().is_some_and(|s| {
            s.is_empty()
                || s.len() > 128
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        }) {
            return Err(StoreError("invalid_session_id").into());
        }
        self.call(move |conn| {
            // Choose predicates explicitly so SQLite can use session/status queue indexes.
            let mut sql = String::from("SELECT r.queue_position,r.id,r.session_id,r.status,r.revision,r.created_at_ms,r.started_at_ms,r.finished_at_ms,r.deadline_at_ms,r.error_code,(SELECT id FROM questions WHERE run_id=r.id AND answer IS NULL AND r.status='waiting_for_parent') FROM runs r WHERE r.queue_position>?");
            let mut args: Vec<rusqlite::types::Value> = vec![after.into()];
            if let Some(session) = session { sql.push_str(" AND r.session_id=?"); args.push(session.into()); }
            if let Some(status) = status { sql.push_str(" AND r.status=?"); args.push(status.into()); }
            sql.push_str(" ORDER BY r.queue_position LIMIT ?");
            args.push(i64::from(limit+1).into());
            let mut query = conn.prepare(&sql)?;
            let mut runs = query.query_map(rusqlite::params_from_iter(args), |r| Ok(json!({
                "queue_position":r.get::<_,i64>(0)?,"run_id":r.get::<_,String>(1)?,"session_id":r.get::<_,String>(2)?,
                "status":r.get::<_,String>(3)?,"revision":r.get::<_,i64>(4)?,"created_at_ms":r.get::<_,i64>(5)?,
                "started_at_ms":r.get::<_,Option<i64>>(6)?,"finished_at_ms":r.get::<_,Option<i64>>(7)?,
                "deadline_at_ms":r.get::<_,Option<i64>>(8)?,"error_code":r.get::<_,Option<String>>(9)?,
                "question_id":r.get::<_,Option<String>>(10)?
            })))?.collect::<Result<Vec<_>,_>>()?;
            let has_more = runs.len()>limit as usize;
            runs.truncate(limit as usize);
            let next = runs.last().map_or(after,|r|r["queue_position"].as_i64().expect("queue position"));
            Ok(json!({"runs":runs,"next_after":next,"has_more":has_more}))
        }).await
    }

    pub async fn logs(&self, run_id: String, after_seq: i64, limit: u32) -> anyhow::Result<Value> {
        validate_page(after_seq, limit)?;
        self.call(move |conn| {
            let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE id=?1)",[&run_id],|r|r.get(0))?;
            if !exists { return Err(StoreError("run_not_found").into()); }
            let mut query = conn.prepare("SELECT sequence,kind,created_at_ms FROM events WHERE run_id=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3")?;
            let mut events = query.query_map(params![run_id,after_seq,i64::from(limit+1)], |r|Ok(json!({"sequence":r.get::<_,i64>(0)?,"kind":r.get::<_,String>(1)?,"created_at_ms":r.get::<_,i64>(2)?})))?.collect::<Result<Vec<_>,_>>()?;
            let has_more = events.len()>limit as usize;
            events.truncate(limit as usize);
            let next = events.last().map_or(after_seq,|e|e["sequence"].as_i64().expect("event sequence"));
            Ok(json!({"run_id":run_id,"events":events,"next_after_seq":next,"has_more":has_more}))
        }).await
    }
}
