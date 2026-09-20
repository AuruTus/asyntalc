use super::*;

fn setup() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("state.sqlite3"), &Profile::Fake).unwrap();
    (dir, store)
}
async fn submit(store: &Store, session: &str) -> String {
    store
        .submit(Some(session.into()), "prompt".into(), 600_000, None)
        .await
        .unwrap()["run_id"]
        .as_str()
        .unwrap()
        .into()
}
fn answer() -> Completion {
    Completion {
        text: "answer".into(),
        finish_reason: "stop".into(),
        usage: Usage::default(),
    }
}
async fn expire(store: &Store, id: &str) {
    let id = id.to_owned();
    store
        .call(move |c| {
            c.execute("UPDATE runs SET deadline_at_ms=0 WHERE id=?1", [id])?;
            Ok(())
        })
        .await
        .unwrap();
}
async fn messages(store: &Store) -> i64 {
    store
        .call(|c| Ok(c.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))?))
        .await
        .unwrap()
}

#[tokio::test]
async fn first_stop_wins_and_late_completion_cannot_write_history() {
    let (_dir, store) = setup();
    let id = submit(&store, "a").await;
    let work = store.claim().await.unwrap().unwrap();
    store.mark_requested(id.clone()).await.unwrap();
    store.cancel(id.clone()).await.unwrap();
    expire(&store, &id).await;
    let pending = store.snapshot(id.clone(), true).await.unwrap();
    assert_eq!(pending.status, "running");
    assert!(pending.cancellation_requested);
    store.complete(work, answer()).await.unwrap();
    let done = store.snapshot(id.clone(), true).await.unwrap();
    assert_eq!(done.status, "cancelled");
    assert_eq!(done.usage.model_requests, 1);
    assert!(done.result.is_none());
    assert_eq!(messages(&store).await, 0);
    store.cancel(id.clone()).await.unwrap();
    store
        .fail(id.clone(), Failure::new("late", "late failure"))
        .await
        .unwrap();
    assert_eq!(
        store.snapshot(id, true).await.unwrap().revision,
        done.revision
    );
}

#[tokio::test]
async fn completion_commit_wins_over_later_cancel() {
    let (_dir, store) = setup();
    let id = submit(&store, "a").await;
    let work = store.claim().await.unwrap().unwrap();
    store.complete(work, answer()).await.unwrap();
    let revision = store.snapshot(id.clone(), true).await.unwrap().revision;
    expire(&store, &id).await;
    store.cancel(id.clone()).await.unwrap();
    let done = store.snapshot(id, true).await.unwrap();
    assert_eq!(done.status, "completed");
    assert_eq!(done.revision, revision);
    assert_eq!(messages(&store).await, 2);
}

#[tokio::test]
async fn deadline_is_checked_at_request_and_completion_without_scheduler_sweep() {
    let (_dir, store) = setup();
    for before_request in [true, false] {
        let id = submit(&store, "a").await;
        let work = store.claim().await.unwrap().unwrap();
        expire(&store, &id).await;
        if before_request {
            assert!(!store.mark_requested(id.clone()).await.unwrap());
        }
        store.complete(work, answer()).await.unwrap();
        store.cancel(id.clone()).await.unwrap();
        let done = store.snapshot(id, true).await.unwrap();
        assert_eq!(done.status, "timed_out");
        assert!(!done.cancellation_requested);
    }
    assert_eq!(messages(&store).await, 0);
}

#[tokio::test]
async fn session_fifo_and_round_robin_survive_reopen() {
    let (dir, store) = setup();
    let a1 = submit(&store, "a").await;
    let a2 = submit(&store, "a").await;
    let b1 = submit(&store, "b").await;
    let a = store.claim().await.unwrap().unwrap();
    assert_eq!(a.run_id, a1);
    assert_eq!(
        store
            .snapshot(a2.clone(), false)
            .await
            .unwrap()
            .blocked_by_run_id,
        Some(a1)
    );
    store.complete(a, answer()).await.unwrap();
    store.barrier().await.unwrap();
    drop(store);
    let store = Store::open(&dir.path().join("state.sqlite3"), &Profile::Fake).unwrap();
    assert_eq!(store.claim().await.unwrap().unwrap().run_id, b1);
    assert_eq!(store.claim().await.unwrap().unwrap().run_id, a2);
    assert!(store.claim().await.unwrap().is_none());
}

#[tokio::test]
async fn recovery_preserves_stop_requests_and_expires_queued_runs() {
    let (dir, store) = setup();
    let active = submit(&store, "a").await;
    store.claim().await.unwrap().unwrap();
    store.cancel(active.clone()).await.unwrap();
    let queued = submit(&store, "a").await;
    expire(&store, &queued).await;
    let timed = submit(&store, "b").await;
    store.claim().await.unwrap().unwrap();
    expire(&store, &timed).await;
    store.barrier().await.unwrap();
    drop(store);
    let store = Store::open(&dir.path().join("state.sqlite3"), &Profile::Fake).unwrap();
    assert_eq!(
        store.snapshot(active, true).await.unwrap().status,
        "cancelled"
    );
    assert_eq!(
        store.snapshot(queued, true).await.unwrap().status,
        "timed_out"
    );
    assert_eq!(
        store.snapshot(timed, true).await.unwrap().status,
        "timed_out"
    );
    assert!(store.claim().await.unwrap().is_none());
}

#[tokio::test]
async fn idempotency_returns_original_receipt_even_at_capacity_and_after_restart() {
    let (dir, store) = setup();
    let receipt = store
        .submit(None, "prompt".into(), 600_000, Some("key".into()))
        .await
        .unwrap();
    let work = store.claim().await.unwrap().unwrap();
    store.complete(work, answer()).await.unwrap();
    for i in 0..MAX_PENDING {
        submit(&store, &format!("s{i}")).await;
    }
    assert_eq!(
        store
            .submit(None, "prompt".into(), 600_000, Some("key".into()))
            .await
            .unwrap(),
        receipt
    );
    let error = store
        .submit(None, "other".into(), 600_000, Some("key".into()))
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<StoreError>().unwrap().0,
        "idempotency_conflict"
    );
    let error = store
        .submit(None, "prompt".into(), 1000, Some("key".into()))
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<StoreError>().unwrap().0,
        "idempotency_conflict"
    );
    let error = store
        .submit(
            Some(receipt["session_id"].as_str().unwrap().into()),
            "prompt".into(),
            600_000,
            Some("key".into()),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<StoreError>().unwrap().0,
        "idempotency_conflict"
    );
    store.barrier().await.unwrap();
    drop(store);
    let store = Store::open(&dir.path().join("state.sqlite3"), &Profile::Fake).unwrap();
    assert_eq!(
        store
            .submit(None, "prompt".into(), 600_000, Some("key".into()))
            .await
            .unwrap(),
        receipt
    );
}

#[tokio::test]
async fn migration_preserves_children_and_assigns_legacy_pending_deadlines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite3");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("../../migrations/001_initial.sql"))
        .unwrap();
    conn.execute_batch(include_str!("../../migrations/002_chat_provider.sql"))
        .unwrap();
    conn.execute_batch("INSERT INTO sessions(id,created_at_ms) VALUES ('a',1); INSERT INTO runs(id,session_id,input,status,revision,created_at_ms) VALUES ('old','a','input','completed',3,1),('queued','a','input','queued',1,1); INSERT INTO messages(run_id,role,content) VALUES ('old','user','input'),('old','assistant','answer'); INSERT INTO events VALUES ('old',3,'run.completed',1);").unwrap();
    drop(conn);
    let before = now_ms();
    let store = Store::open(&path, &Profile::Fake).unwrap();
    assert_eq!(messages(&store).await, 2);
    assert!(
        store
            .snapshot("queued".into(), false)
            .await
            .unwrap()
            .deadline_at_ms
            .unwrap()
            > before + 590_000
    );
    store
        .call(|c| {
            assert_eq!(
                c.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
                    .get::<_, i64>(
                    0
                ))?,
                0
            );
            assert_eq!(
                c.query_row("SELECT count(*) FROM events WHERE run_id='old'", [], |r| {
                    r.get::<_, i64>(0)
                })?,
                1
            );
            assert!(
                c.execute(
                    "INSERT INTO messages(run_id,role,content) VALUES ('missing','user','bad')",
                    []
                )
                .is_err()
            );
            Ok(())
        })
        .await
        .unwrap();
}

fn question(call: &str) -> crate::provider::Question {
    crate::provider::Question {
        call_id: call.into(),
        prompt: "Choose?".into(),
        choices: None,
        assistant: Message {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: Some(vec![
                json!({"id":call,"type":"function","function":{"name":"ask_parent","arguments":"{\"prompt\":\"Choose?\"}"}}),
            ]),
            tool_call_id: None,
        },
        usage: Usage {
            model_requests: 1,
            input_tokens: Some(7),
            output_tokens: Some(3),
        },
    }
}
async fn pause(store: &Store, id: &str) -> String {
    let work = store.claim().await.unwrap().unwrap();
    assert_eq!(work.run_id, id);
    store.mark_requested(id.into()).await.unwrap();
    store.pause(work, question("call")).await.unwrap();
    store
        .snapshot(id.into(), true)
        .await
        .unwrap()
        .input_request
        .unwrap()["question_id"]
        .as_str()
        .unwrap()
        .into()
}

#[tokio::test]
async fn waiting_recovery_deadline_and_pending_capacity() {
    let (dir, store) = setup();
    let id = submit(&store, "a").await;
    let q = pause(&store, &id).await;
    for i in 1..MAX_PENDING {
        submit(&store, &format!("s{i}")).await;
    }
    let error = store
        .submit(None, "full".into(), 600_000, None)
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<StoreError>().unwrap().0,
        "capacity_exceeded"
    );
    store.barrier().await.unwrap();
    drop(store);
    let store = Store::open(&dir.path().join("state.sqlite3"), &Profile::Fake).unwrap();
    assert_eq!(
        store.snapshot(id.clone(), true).await.unwrap().status,
        "waiting_for_parent"
    );
    expire(&store, &id).await;
    let error = store
        .resume(id.clone(), q, "too late".into())
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<StoreError>().unwrap().0,
        "run_not_waiting"
    );
    assert_eq!(store.snapshot(id, true).await.unwrap().status, "timed_out");
    let count: i64 = store
        .call(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM questions WHERE answer IS NOT NULL",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn cancel_before_question_commit_prevents_suspension() {
    let (_dir, store) = setup();
    let id = submit(&store, "a").await;
    let work = store.claim().await.unwrap().unwrap();
    store.mark_requested(id.clone()).await.unwrap();
    store.cancel(id.clone()).await.unwrap();
    store.pause(work, question("late")).await.unwrap();
    let snapshot = store.snapshot(id, true).await.unwrap();
    assert_eq!(snapshot.status, "cancelled");
    assert!(snapshot.input_request.is_none());
    let count: i64 = store
        .call(|c| Ok(c.query_row("SELECT count(*) FROM questions", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn fast_resume_waits_for_old_task_and_keeps_fifo_position() {
    let (_dir, store) = setup();
    let id = submit(&store, "a").await;
    let q = pause(&store, &id).await;
    let next = submit(&store, "a").await;
    store.resume(id.clone(), q, "answer".into()).await.unwrap();
    assert!(
        store
            .claim_available(vec![id.clone()])
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.snapshot(next, false).await.unwrap().blocked_by_run_id,
        Some(id.clone())
    );
    let work = store.claim().await.unwrap().unwrap();
    assert_eq!(work.run_id, id);
    store.mark_requested(id.clone()).await.unwrap();
    store.cancel(id.clone()).await.unwrap();
    store
        .fail(id.clone(), Failure::new("stopped", "stopped"))
        .await
        .unwrap();
    let snapshot = store.snapshot(id, true).await.unwrap();
    assert_eq!(snapshot.usage.model_requests, 2);
    assert!(snapshot.usage.input_tokens.is_none());
    assert_eq!(messages(&store).await, 0);
}

#[tokio::test]
async fn resumed_context_is_bounded_and_waiting_profile_cannot_change() {
    let (dir, store) = setup();
    let id = submit(&store, "a").await;
    let q = pause(&store, &id).await;
    let profile: Profile = serde_json::from_value(json!({"runner":"chat","config":{"base_url":"http://localhost/v1","model":"test","api_key_env":"TEST_KEY"}})).unwrap();
    assert!(Store::open(&dir.path().join("state.sqlite3"), &profile).is_err());
    store.resume(id.clone(), q, "x".repeat(1024)).await.unwrap();
    let work = store.claim().await.unwrap().unwrap();
    let error = match store.context(&work, 1024).await {
        Ok(_) => panic!("context limit not enforced"),
        Err(error) => error,
    };
    assert_eq!(
        error.downcast_ref::<StoreError>().unwrap().0,
        "context_limit"
    );
    store
        .call(move |c| {
            c.execute("UPDATE runs SET model_requests=9 WHERE id=?1", [id.clone()])?;
            Ok(())
        })
        .await
        .unwrap();
    let error = store.mark_requested(work.run_id).await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<StoreError>().unwrap().0,
        "model_turn_limit"
    );
}

#[tokio::test]
async fn schema_three_upgrade_preserves_retry_receipts_and_chat_profile() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite3");
    let mut conn = Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("../../migrations/001_initial.sql"))
        .unwrap();
    conn.execute_batch(include_str!("../../migrations/002_chat_provider.sql"))
        .unwrap();
    let tx = conn.transaction().unwrap();
    tx.execute_batch(include_str!("../../migrations/003_scheduler.sql"))
        .unwrap();
    tx.commit().unwrap();
    let profile: Profile = serde_json::from_value(json!({"runner":"chat","config":{"base_url":"http://localhost/v1","model":"test","api_key_env":"TEST_KEY"}})).unwrap();
    let profile_json = serde_json::to_string(&profile).unwrap();
    assert!(!profile_json.contains("ask_parent"));
    let request =
        json!({"session_id":"a","input":"prompt","run_timeout_ms":600_000,"profile":profile_json})
            .to_string();
    let receipt = json!({"run_id":"legacy","session_id":"a","status":"queued","revision":1,"deadline_at_ms":now_ms()+600_000});
    conn.execute(
        "INSERT INTO sessions(id,created_at_ms,profile_json) VALUES ('a',1,?1)",
        [profile_json],
    )
    .unwrap();
    conn.execute("INSERT INTO runs(id,session_id,input,status,revision,created_at_ms,deadline_at_ms) VALUES ('legacy','a','prompt','queued',1,1,?1)",[now_ms()+600_000]).unwrap();
    conn.execute(
        "INSERT INTO submissions VALUES ('key',?1,?2,'legacy')",
        params![request, receipt.to_string()],
    )
    .unwrap();
    drop(conn);
    let store = Store::open(&path, &profile).unwrap();
    assert_eq!(
        store
            .submit(
                Some("a".into()),
                "prompt".into(),
                600_000,
                Some("key".into())
            )
            .await
            .unwrap(),
        receipt
    );
}
