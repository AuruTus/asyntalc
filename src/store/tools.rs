use super::*;
use crate::provider::WorkspaceCall;

impl Store {
    pub async fn tool_allowed(&self, run_id: String, call_id: String) -> anyhow::Result<bool> {
        self.call(move |conn| {
            let tx = conn.transaction()?;
            if stop_if_due(&tx, &run_id, false)? {
                finish_stop(&tx, &run_id)?;
                tx.commit()?;
                return Ok(false);
            }
            check_tool(&tx, &run_id, &call_id)?;
            tx.commit()?;
            Ok(true)
        })
        .await
    }

    pub async fn record_tool(
        &self,
        work: &Work,
        call: WorkspaceCall,
        result: Value,
    ) -> anyhow::Result<bool> {
        let run_id = work.run_id.clone();
        self.call(move |conn| {
            let tx = conn.transaction()?;
            if stop_if_due(&tx, &run_id, false)? {
                finish_stop(&tx, &run_id)?;
                tx.commit()?;
                return Ok(false);
            }
            check_tool(&tx, &run_id, &call.call_id)?;
            add_usage(&tx, &run_id, &call.usage)?;
            tx.execute("INSERT INTO tool_exchanges(run_id,model_turn,call_id,assistant_json,result_json) VALUES (?1,(SELECT model_requests FROM runs WHERE id=?1),?2,?3,?4)", params![run_id,call.call_id,serde_json::to_string(&call.assistant)?,result.to_string()])?;
            let revision: i64 = tx.query_row("UPDATE runs SET revision=revision+1 WHERE id=?1 RETURNING revision", [&run_id], |r| r.get(0))?;
            event(&tx, &run_id, revision, "run.tool_completed")?;
            tx.commit()?;
            Ok(true)
        }).await
    }
}

fn check_tool(conn: &Connection, run_id: &str, call_id: &str) -> anyhow::Result<()> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM tool_exchanges WHERE run_id=?1",
        [run_id],
        |r| r.get(0),
    )?;
    if count >= 16 {
        return Err(StoreError("tool_call_limit").into());
    }
    let duplicate: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM questions WHERE run_id=?1 AND call_id=?2 UNION ALL SELECT 1 FROM tool_exchanges WHERE run_id=?1 AND call_id=?2)", params![run_id,call_id], |r| r.get(0))?;
    if duplicate {
        return Err(StoreError("invalid_tool_call").into());
    }
    Ok(())
}
