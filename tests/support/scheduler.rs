use super::*;

fn submit(daemon: &Daemon, session: &str, deadline: u64) -> String {
    let receipt = daemon.rpc(
        json!({"op":"submit","session_id":session,"input":"prompt","run_timeout_ms":deadline}),
    );
    assert_eq!(receipt["ok"], true, "{receipt}");
    receipt["run_id"].as_str().unwrap().into()
}
fn status(daemon: &Daemon, id: &str) -> Value {
    daemon.rpc(json!({"op":"status","run_id":id}))
}
fn wait(daemon: &Daemon, id: &str) -> Value {
    daemon.rpc(json!({"op":"wait","run_id":id,"timeout_ms":2000}))
}
fn cancel(daemon: &Daemon, id: &str) {
    assert_eq!(daemon.rpc(json!({"op":"cancel","run_id":id}))["ok"], true);
    assert_eq!(wait(daemon, id)["status"], "cancelled");
}

#[test]
fn concurrent_sessions_respect_fifo_and_global_limit_and_cancel_frees_slot() {
    let daemon = Daemon::start(30_000);
    let a = submit(&daemon, "a", 600_000);
    daemon.wait_running(&a);
    let a2 = submit(&daemon, "a", 600_000);
    let b = submit(&daemon, "b", 600_000);
    daemon.wait_running(&b);
    let c = submit(&daemon, "c", 600_000);
    assert_eq!(status(&daemon, &a2)["status"], "queued");
    assert_eq!(status(&daemon, &a2)["blocked_by_run_id"], a);
    assert_eq!(status(&daemon, &c)["status"], "queued");
    assert!(status(&daemon, &c)["blocked_by_run_id"].is_null());
    cancel(&daemon, &a2);
    cancel(&daemon, &a);
    daemon.wait_running(&c);
    assert_eq!(status(&daemon, &b)["status"], "running");
    let terminal = status(&daemon, &a);
    cancel(&daemon, &a);
    assert_eq!(status(&daemon, &a)["revision"], terminal["revision"]);
    assert_eq!(
        daemon.rpc(json!({"op":"result","run_id":a}))["error"]["code"],
        "result_not_ready"
    );
}

#[test]
fn one_slot_option_serializes_independent_sessions() {
    let dir = private_dir();
    let child = Command::new(BIN)
        .arg("--data-dir")
        .arg(dir.path())
        .args([
            "daemon",
            "--runner",
            "fake",
            "--fake-delay-ms",
            "30000",
            "--max-active-runs",
            "1",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut daemon = Daemon { child, dir };
    daemon.ready();
    let a = submit(&daemon, "a", 600_000);
    daemon.wait_running(&a);
    let b = submit(&daemon, "b", 600_000);
    assert_eq!(status(&daemon, &b)["status"], "queued");
    cancel(&daemon, &a);
    daemon.wait_running(&b);
}

#[test]
fn queued_and_active_deadlines_are_distinct_from_wait_timeouts() {
    let daemon = Daemon::start(30_000);
    let blocker = submit(&daemon, "a", 600_000);
    daemon.wait_running(&blocker);
    let queued = submit(&daemon, "a", 150);
    let active = submit(&daemon, "b", 300);
    daemon.wait_running(&active);
    let snapshot = daemon.rpc(json!({"op":"wait","run_id":active,"timeout_ms":0}));
    assert_eq!(snapshot["return_reason"], "wait_timeout");
    let queued = wait(&daemon, &queued);
    assert_eq!(queued["status"], "timed_out");
    assert!(queued["started_at_ms"].is_null());
    let active = wait(&daemon, &active);
    assert_eq!(active["status"], "timed_out");
    assert!(!active["started_at_ms"].is_null());
    assert_eq!(status(&daemon, &blocker)["status"], "running");
}

#[test]
fn simultaneous_duplicate_submissions_create_one_run_and_survive_restart() {
    let mut daemon = Daemon::start(30_000);
    let operation = json!({"op":"submit","input":"same prompt","idempotency_key":"retry:1"});
    let replies = thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|_| scope.spawn(|| daemon.rpc(operation.clone())))
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().unwrap())
            .collect::<Vec<_>>()
    });
    for reply in &replies {
        assert_eq!(reply, &replies[0]);
    }
    let receipt = &replies[0];
    let id = receipt["run_id"].as_str().unwrap();
    daemon.wait_running(id);
    daemon.restart(0);
    assert_eq!(daemon.rpc(operation.clone()), *receipt);
    assert_eq!(status(&daemon, id)["status"], "failed");
    let db = rusqlite::Connection::open(daemon.dir.path().join("state.sqlite3")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM runs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let mut conflict = operation;
    conflict["input"] = json!("changed");
    assert_eq!(
        daemon.rpc(conflict)["error"]["code"],
        "idempotency_conflict"
    );
}

#[test]
fn cli_exposes_deadlines_idempotency_cancel_and_validates_new_fields() {
    let daemon = Daemon::start(30_000);
    let input = daemon.dir.path().join("prompt.txt");
    std::fs::write(&input, "prompt").unwrap();
    let args = [
        "submit",
        "--input",
        input.to_str().unwrap(),
        "--run-timeout-ms",
        "60000",
        "--idempotency-key",
        "cli-key",
    ];
    let first = cli(daemon.dir.path(), &args);
    assert!(first.status.success());
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    let second = cli(daemon.dir.path(), &args);
    let second: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(first["run_id"], second["run_id"]);
    assert_eq!(first["deadline_at_ms"], second["deadline_at_ms"]);
    let id = first["run_id"].as_str().unwrap();
    assert!(
        cli(daemon.dir.path(), &["cancel", "--run", id])
            .status
            .success()
    );
    assert_eq!(wait(&daemon, id)["status"], "cancelled");
    for timeout in [0, 86_400_001] {
        assert_eq!(
            daemon.rpc(json!({"op":"submit","input":"x","run_timeout_ms":timeout}))["error"]["code"],
            "invalid_run_timeout"
        );
    }
    for key in ["", "spaces invalid"] {
        assert_eq!(
            daemon.rpc(json!({"op":"submit","input":"x","idempotency_key":key}))["error"]["code"],
            "invalid_idempotency_key"
        );
    }
    assert_eq!(
        daemon.rpc(json!({"op":"cancel","run_id":"missing"}))["error"]["code"],
        "run_not_found"
    );
    assert!(
        !cli(
            daemon.dir.path(),
            &["daemon", "--runner", "fake", "--max-active-runs", "0"]
        )
        .status
        .success()
    );
}
