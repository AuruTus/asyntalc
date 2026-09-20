use super::*;

fn list(daemon: &Daemon, after: i64, limit: u32) -> Value {
    let result = daemon.rpc(json!({"op":"list","after":after,"limit":limit}));
    assert_eq!(result["ok"], true, "{result}");
    result
}

#[test]
fn list_pages_survive_new_submissions_and_restart_and_filter_current_state() {
    let mut daemon = Daemon::start(30_000);
    let first = daemon.submit("private prompt not included in list");
    daemon.wait_running(&first);
    let second = daemon.submit("second");
    let third = daemon.submit("third");
    let page1 = list(&daemon, 0, 2);
    assert_eq!(page1["runs"][0]["run_id"], first);
    assert_eq!(page1["runs"][1]["run_id"], second);
    assert_eq!(page1["has_more"], true);
    assert!(!page1.to_string().contains("private prompt"));
    let fourth = daemon.submit("arrives between pages");
    let page2 = list(&daemon, page1["next_after"].as_i64().unwrap(), 2);
    assert_eq!(page2["runs"][0]["run_id"], third);
    assert_eq!(page2["runs"][1]["run_id"], fourth);
    assert_eq!(page2["has_more"], false);
    let empty = list(&daemon, page2["next_after"].as_i64().unwrap(), 2);
    assert_eq!(empty["runs"], json!([]));
    assert_eq!(empty["next_after"], page2["next_after"]);
    assert_eq!(empty["has_more"], false);
    let filtered = daemon.rpc(json!({"op":"list","session_id":"test","status":"queued"}));
    assert_eq!(filtered["runs"].as_array().unwrap().len(), 3);
    assert_eq!(
        daemon.rpc(json!({"op":"list","session_id":"missing"}))["runs"],
        json!([])
    );
    daemon.rpc(json!({"op":"cancel","run_id":second}));
    let cancelled = daemon.rpc(json!({"op":"list","status":"cancelled"}));
    assert_eq!(cancelled["runs"].as_array().unwrap().len(), 1);
    assert_eq!(cancelled["runs"][0]["run_id"], second);
    assert_eq!(cancelled["runs"][0]["error_code"], "cancelled");
    daemon.restart(30_000);
    let after_restart = list(&daemon, page1["next_after"].as_i64().unwrap(), 2);
    assert_eq!(after_restart["runs"][0]["run_id"], third);
    assert_eq!(after_restart["runs"][1]["run_id"], fourth);
    let failed = daemon.rpc(json!({"op":"list","status":"failed"}));
    assert_eq!(failed["runs"][0]["run_id"], first);
    assert_eq!(failed["runs"][0]["error_code"], "daemon_interrupted");
}

#[test]
fn logs_are_finite_ordered_pages_and_poll_from_last_sequence() {
    let mut daemon = Daemon::start(30_000);
    let id = daemon.submit("private prompt");
    daemon.wait_running(&id);
    let first = daemon.rpc(json!({"op":"logs","run_id":id,"limit":1}));
    assert_eq!(first["events"][0]["kind"], "run.submitted");
    assert_eq!(first["next_after_seq"], 1);
    assert_eq!(first["has_more"], true);
    let second = daemon.rpc(json!({"op":"logs","run_id":id,"after_seq":1,"limit":1}));
    assert_eq!(second["events"][0]["kind"], "run.started");
    assert_eq!(second["has_more"], false);
    let cursor = second["next_after_seq"].as_i64().unwrap();
    let empty = daemon.rpc(json!({"op":"logs","run_id":id,"after_seq":cursor}));
    assert_eq!(empty["events"], json!([]));
    assert_eq!(empty["next_after_seq"], cursor);
    daemon.rpc(json!({"op":"cancel","run_id":id}));
    let terminal = daemon.rpc(json!({"op":"wait","run_id":id,"timeout_ms":2000}));
    assert_eq!(terminal["status"], "cancelled");
    let remaining = daemon.rpc(json!({"op":"logs","run_id":id,"after_seq":cursor}));
    assert_eq!(remaining["events"][0]["kind"], "run.cancellation_requested");
    assert_eq!(remaining["events"][1]["kind"], "run.cancelled");
    assert_eq!(remaining["next_after_seq"], terminal["revision"]);
    assert!(!remaining.to_string().contains("private prompt"));
    daemon.restart(0);
    assert_eq!(
        daemon.rpc(json!({"op":"logs","run_id":id,"after_seq":cursor})),
        remaining
    );
}

#[test]
fn inspection_cli_bounds_errors_and_maximum_page_remain_valid_json() {
    let daemon = Daemon::start(30_000);
    for i in 0..101 {
        daemon.submit(&format!("prompt {i}"));
    }
    let output = cli(
        daemon.dir.path(),
        &["list", "--session", "test", "--limit", "100"],
    );
    assert!(output.status.success());
    assert!(output.stdout.len() < 1024 * 1024);
    let page: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(page["runs"].as_array().unwrap().len(), 100);
    assert_eq!(page["has_more"], true);
    let id = page["runs"][0]["run_id"].as_str().unwrap();
    let output = cli(
        daemon.dir.path(),
        &["logs", "--run", id, "--after-seq", "0", "--limit", "1"],
    );
    assert!(output.status.success());
    let page: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(page["events"].as_array().unwrap().len(), 1);
    for op in [
        json!({"op":"list","limit":0}),
        json!({"op":"logs","run_id":id,"limit":101}),
    ] {
        assert_eq!(daemon.rpc(op)["error"]["code"], "invalid_limit");
    }
    for op in [
        json!({"op":"list","after":-1}),
        json!({"op":"logs","run_id":id,"after_seq":-1}),
    ] {
        assert_eq!(daemon.rpc(op)["error"]["code"], "invalid_cursor");
    }
    assert_eq!(
        daemon.rpc(json!({"op":"list","status":"unknown"}))["error"]["code"],
        "invalid_status"
    );
    assert_eq!(
        daemon.rpc(json!({"op":"list","session_id":"bad space"}))["error"]["code"],
        "invalid_session_id"
    );
    assert_eq!(
        daemon.rpc(json!({"op":"logs","run_id":"missing"}))["error"]["code"],
        "run_not_found"
    );
    assert_eq!(list(&daemon, i64::MAX, 1)["runs"], json!([]));
    assert_eq!(
        cli(daemon.dir.path(), &["list", "--limit", "0"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        cli(daemon.dir.path(), &["logs", "--run", "missing"])
            .status
            .code(),
        Some(1)
    );
}

#[test]
fn documented_inspection_demo_runs_end_to_end() {
    let dir = private_dir();
    let output = Command::new("bash")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/inspect-demo.sh"
        ))
        .arg(BIN)
        .env("ASYNTALC_DEMO_DIR", dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Inspection demo passed"));
}
