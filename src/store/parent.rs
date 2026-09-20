use super::*;
use crate::provider::Question;

impl Store {
    pub async fn pause(&self, work: Work, question: Question) -> anyhow::Result<()> {
        self.call(move |conn| {
            let tx = conn.transaction()?;
            if stop_if_due(&tx, &work.run_id, false)? {
                finish_stop(&tx, &work.run_id)?;
                tx.commit()?;
                return Ok(());
            }
            let count: i64 = tx.query_row("SELECT count(*) FROM questions WHERE run_id=?1", [&work.run_id], |r| r.get(0))?;
            let duplicate: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM questions WHERE run_id=?1 AND call_id=?2)", params![work.run_id,question.call_id], |r| r.get(0))?;
            add_usage(&tx,&work.run_id,&question.usage)?;
            if count >= 8 || duplicate {
                let code = if duplicate { "invalid_parent_question" } else { "question_limit" };
                let revision: i64 = tx.query_row("UPDATE runs SET status='failed',revision=revision+1,finished_at_ms=?2,error_code=?3,error_message='Question limit reached or duplicate tool call ID' WHERE id=?1 RETURNING revision", params![work.run_id,now_ms(),code], |r|r.get(0))?;
                event(&tx,&work.run_id,revision,"run.failed")?;
            } else {
                let id = format!("q_{}",uuid::Uuid::new_v4());
                let public = json!({"question_id":id,"kind":"question","prompt":question.prompt,"choices":question.choices,"allows_free_text":true});
                tx.execute("INSERT INTO questions(id,run_id,ordinal,call_id,question_json,assistant_json) VALUES (?1,?2,?3,?4,?5,?6)", params![id,work.run_id,count+1,question.call_id,public.to_string(),serde_json::to_string(&question.assistant)?])?;
                let revision: i64 = tx.query_row("UPDATE runs SET status='waiting_for_parent',revision=revision+1 WHERE id=?1 RETURNING revision", [&work.run_id], |r|r.get(0))?;
                event(&tx,&work.run_id,revision,"run.waiting_for_parent")?;
            }
            tx.commit()?;
            Ok(())
        }).await
    }

    pub async fn resume(
        &self,
        run_id: String,
        question_id: String,
        answer: String,
    ) -> anyhow::Result<Value> {
        self.call(move |conn| {
            let tx = conn.transaction()?;
            let question: Option<(Option<String>,Option<String>,String)> = tx.query_row("SELECT answer,receipt_json,call_id FROM questions WHERE id=?1 AND run_id=?2", params![question_id,run_id], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            let Some((previous, receipt, _)) = question else { return Err(StoreError("question_not_found").into()); };
            if let Some(previous) = previous {
                if previous != answer { return Err(StoreError("answer_conflict").into()); }
                return Ok(serde_json::from_str(&receipt.expect("answered question has receipt"))?);
            }
            if stop_if_due(&tx,&run_id,false)? {
                tx.commit()?;
                return Err(StoreError("run_not_waiting").into());
            }
            let (status, session): (String,String) = tx.query_row("SELECT status,session_id FROM runs WHERE id=?1", [&run_id], |r|Ok((r.get(0)?,r.get(1)?)))?;
            if status != "waiting_for_parent" { return Err(StoreError("run_not_waiting").into()); }
            let revision: i64 = tx.query_row("UPDATE runs SET status='queued',revision=revision+1 WHERE id=?1 RETURNING revision", [&run_id], |r|r.get(0))?;
            let receipt = json!({"run_id":run_id,"session_id":session,"question_id":question_id,"status":"queued","revision":revision});
            tx.execute("UPDATE questions SET answer=?2,receipt_json=?3 WHERE id=?1", params![question_id,answer,receipt.to_string()])?;
            tx.execute("UPDATE sessions SET scheduler_order=(SELECT max(scheduler_order)+1 FROM sessions) WHERE id=?1", [&session])?;
            event(&tx,&run_id,revision,"run.resumed")?;
            tx.commit()?;
            Ok(receipt)
        }).await
    }

    pub async fn context(&self, work: &Work, max_bytes: usize) -> anyhow::Result<Vec<Message>> {
        let run_id = work.run_id.clone();
        let session_id = work.session_id.clone();
        let input = work.input.clone();
        self.call(move |conn| {
            // Bound both committed content and current run's question/answer transcript before allocation.
            let (history_bytes, history_count): (i64,i64) = conn.query_row("SELECT coalesce(sum(length(CAST(m.content AS BLOB))+coalesce(length(CAST(m.tool_calls AS BLOB)),0)+coalesce(length(m.tool_call_id),0)),0),count(*) FROM messages m JOIN runs r ON r.id=m.run_id WHERE r.session_id=?1 AND r.status='completed' AND r.queue_position<(SELECT queue_position FROM runs WHERE id=?2)", params![session_id,run_id], |r|Ok((r.get(0)?,r.get(1)?)))?;
            let (local_bytes, local_count): (i64,i64) = conn.query_row("SELECT coalesce(sum(length(CAST(assistant_json AS BLOB))+length(CAST(answer AS BLOB))+length(call_id)),0),count(*)*2 FROM questions WHERE run_id=?1 AND answer IS NOT NULL", [&run_id], |r|Ok((r.get(0)?,r.get(1)?)))?;
            if history_bytes as u64 + local_bytes as u64 + input.len() as u64 > max_bytes as u64 || history_count+local_count>=1023 { return Err(StoreError("context_limit").into()); }
            let mut query = conn.prepare("SELECT m.role,m.content,m.tool_calls,m.tool_call_id FROM messages m JOIN runs r ON r.id=m.run_id WHERE r.session_id=?1 AND r.status='completed' AND r.queue_position<(SELECT queue_position FROM runs WHERE id=?2) ORDER BY r.queue_position,m.id")?;
            let mut messages = query.query_map(params![session_id,run_id], |r| Ok(Message {role:r.get(0)?,content:r.get(1)?,tool_calls:r.get::<_,Option<String>>(2)?.map(|s|serde_json::from_str(&s).expect("stored tool calls")),tool_call_id:r.get(3)?}))?.collect::<Result<Vec<_>,_>>()?;
            messages.push(Message::text("user",input));
            messages.extend(question_messages(conn,&run_id)?);
            Ok(messages)
        }).await
    }
}

fn question_messages(conn: &Connection, run_id: &str) -> anyhow::Result<Vec<Message>> {
    let mut query = conn.prepare("SELECT assistant_json,call_id,answer FROM questions WHERE run_id=?1 AND answer IS NOT NULL ORDER BY ordinal")?;
    let rows = query.query_map([run_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut messages = Vec::new();
    for row in rows {
        let (assistant, call_id, answer) = row?;
        messages.push(serde_json::from_str(&assistant)?);
        messages.push(Message {
            role: "tool".into(),
            content: answer,
            tool_calls: None,
            tool_call_id: Some(call_id),
        });
    }
    Ok(messages)
}

pub(super) fn commit_questions(tx: &rusqlite::Transaction<'_>, run_id: &str) -> anyhow::Result<()> {
    for message in question_messages(tx, run_id)? {
        tx.execute("INSERT INTO messages(run_id,role,content,tool_calls,tool_call_id) VALUES (?1,?2,?3,?4,?5)", params![run_id,message.role,message.content,message.tool_calls.map(|v|serde_json::to_string(&v).expect("serializable tool calls")),message.tool_call_id])?;
    }
    Ok(())
}
