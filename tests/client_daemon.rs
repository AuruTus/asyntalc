use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_asyntalc");

#[path = "support/chat_provider.rs"]
mod chat_provider;
#[path = "support/inspection.rs"]
mod inspection;
#[path = "support/scheduler.rs"]
mod scheduler;

fn private_dir() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

struct Daemon {
    child: Child,
    dir: TempDir,
}

impl Daemon {
    fn start(delay_ms: u64) -> Self {
        let dir = private_dir();
        let child = spawn(dir.path(), delay_ms);
        let mut daemon = Self { child, dir };
        daemon.ready();
        daemon
    }

    fn ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                let mut stderr = String::new();
                self.child
                    .stderr
                    .as_mut()
                    .unwrap()
                    .read_to_string(&mut stderr)
                    .unwrap();
                panic!("daemon exited {status}: {stderr}");
            }
            if UnixStream::connect(self.dir.path().join("daemon.sock")).is_ok() {
                assert_eq!(self.rpc(json!({"op":"ping"}))["ready"], true);
                return;
            }
            assert!(Instant::now() < deadline, "daemon readiness timeout");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn restart(&mut self, delay_ms: u64) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        self.child = spawn(self.dir.path(), delay_ms);
        self.ready();
    }

    fn rpc(&self, operation: Value) -> Value {
        rpc(self.dir.path(), operation)
    }

    fn submit(&self, text: &str) -> String {
        let response = self.rpc(json!({"op":"submit", "session_id":"test", "input":text}));
        assert_eq!(response["ok"], true, "{response}");
        response["run_id"].as_str().unwrap().to_owned()
    }

    fn wait_running(&self, run: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if self.rpc(json!({"op":"status", "run_id":run}))["status"] == "running" {
                return;
            }
            assert!(Instant::now() < deadline, "run did not start");
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn(dir: &Path, delay_ms: u64) -> Child {
    Command::new(BIN)
        .arg("--data-dir")
        .arg(dir)
        .args([
            "daemon",
            "--runner",
            "fake",
            "--fake-delay-ms",
            &delay_ms.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn rpc(dir: &Path, operation: Value) -> Value {
    raw(
        dir,
        &serde_json::to_vec(
            &json!({"protocol_version":1,"request_id":"test-request","operation":operation}),
        )
        .unwrap(),
        true,
    )
}

fn raw(dir: &Path, bytes: &[u8], newline: bool) -> Value {
    let mut stream = UnixStream::connect(dir.join("daemon.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    // Oversized frames can be rejected before the writer has sent every byte.
    let _ = stream.write_all(bytes);
    if newline {
        let _ = stream.write_all(b"\n");
    }
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn cli(dir: &Path, args: &[&str]) -> Output {
    let mut child = Command::new(BIN)
        .arg("--data-dir")
        .arg(dir)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!("CLI timed out: {output:?}");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn separate_clients_submit_wait_and_retrieve_durable_result() {
    let mut daemon = Daemon::start(100);
    let input = daemon.dir.path().join("task.md");
    std::fs::write(&input, "hello\nworld").unwrap();
    let output = cli(
        daemon.dir.path(),
        &[
            "submit",
            "--input",
            input.to_str().unwrap(),
            "--output",
            "json",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty());
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["status"], "queued");
    assert_eq!(receipt["protocol_version"], 1);
    let run = receipt["run_id"].as_str().unwrap();
    let output = cli(
        daemon.dir.path(),
        &["wait", "--run", run, "--timeout-ms", "2000"],
    );
    assert!(output.status.success());
    let completed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["return_reason"], "terminal");
    assert_eq!(completed["result"]["text"], "[fake] hello\nworld");
    let db = rusqlite::Connection::open(daemon.dir.path().join("state.sqlite3")).unwrap();
    let events: i64 = db
        .query_row("SELECT count(*) FROM events WHERE run_id=?1", [run], |r| {
            r.get(0)
        })
        .unwrap();
    let messages: i64 = db
        .query_row(
            "SELECT count(*) FROM messages WHERE run_id=?1",
            [run],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!((events, messages), (3, 2));
    drop(db);
    daemon.restart(0);
    let output = cli(
        daemon.dir.path(),
        &["result", "--run", run, "--output", "text"],
    );
    assert!(output.status.success());
    assert_eq!(output.stdout, b"[fake] hello\nworld");
}

#[test]
fn timeout_and_disconnected_waiter_do_not_cancel_run() {
    let mut daemon = Daemon::start(30_000);
    let run = daemon.submit("slow task");
    daemon.wait_running(&run);
    let output = cli(
        daemon.dir.path(),
        &["wait", "--run", &run, "--timeout-ms", "0"],
    );
    assert!(output.status.success());
    let snapshot: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(snapshot["return_reason"], "wait_timeout");
    assert_eq!(snapshot["status"], "running");
    let mut socket = UnixStream::connect(daemon.dir.path().join("daemon.sock")).unwrap();
    writeln!(socket, "{}", json!({"protocol_version":1,"request_id":"abandoned","operation":{"op":"wait","run_id":run,"timeout_ms":100}})).unwrap();
    drop(socket);
    assert_eq!(
        daemon.rpc(json!({"op":"status","run_id":run}))["status"],
        "running"
    );
    daemon.restart(0);
    let output = cli(
        daemon.dir.path(),
        &["wait", "--run", &run, "--timeout-ms", "0"],
    );
    assert!(
        output.status.success(),
        "querying a failed run must succeed"
    );
    let snapshot: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(snapshot["status"], "failed");
    assert_eq!(snapshot["run_error"]["code"], "daemon_interrupted");
}

#[test]
fn restart_recovers_queued_work_without_replaying_active_work() {
    let mut daemon = Daemon::start(30_000);
    let active = daemon.submit("active");
    daemon.wait_running(&active);
    let blocker = daemon.rpc(json!({"op":"submit","session_id":"blocker","input":"blocker"}));
    daemon.wait_running(blocker["run_id"].as_str().unwrap());
    let queued =
        daemon.rpc(json!({"op":"submit","session_id":"queued","input":"queued"}))["run_id"]
            .as_str()
            .unwrap()
            .to_owned();
    assert_eq!(
        daemon.rpc(json!({"op":"status","run_id":queued}))["status"],
        "queued"
    );
    daemon.restart(0);
    assert_eq!(
        daemon.rpc(json!({"op":"status","run_id":active}))["status"],
        "failed"
    );
    assert_eq!(
        daemon.rpc(json!({"op":"wait","run_id":queued,"timeout_ms":2000}))["result"]["text"],
        "[fake] queued"
    );
}

#[test]
fn duplicate_daemon_cannot_replace_live_socket() {
    let daemon = Daemon::start(0);
    let output = cli(daemon.dir.path(), &["daemon", "--runner", "fake"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("another daemon"));
    assert_eq!(daemon.rpc(json!({"op":"ping"}))["ready"], true);
    let mode = std::fs::metadata(daemon.dir.path().join("daemon.sock"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn rejects_invalid_frames_versions_inputs_and_operations() {
    let daemon = Daemon::start(0);
    assert_eq!(
        raw(daemon.dir.path(), b"not JSON", true)["error"]["code"],
        "invalid_request"
    );
    assert_eq!(
        raw(daemon.dir.path(), &vec![b'x'; 1024 * 1024 + 1], false)["error"]["code"],
        "invalid_frame"
    );
    let version =
        json!({"protocol_version":99,"request_id":"bad-version","operation":{"op":"ping"}});
    assert_eq!(
        raw(
            daemon.dir.path(),
            &serde_json::to_vec(&version).unwrap(),
            true
        )["error"]["code"],
        "unsupported_version"
    );
    assert_eq!(
        daemon.rpc(json!({"op":"unknown"}))["error"]["code"],
        "invalid_request"
    );
    assert_eq!(
        daemon.rpc(json!({"op":"submit","session_id":null,"input":" "}))["error"]["code"],
        "invalid_input"
    );
    assert_eq!(
        daemon.rpc(json!({"op":"submit","session_id":"../escape","input":"hello"}))["error"]["code"],
        "invalid_session_id"
    );
    assert_eq!(
        daemon.rpc(json!({"op":"wait","run_id":"none","timeout_ms":30001}))["error"]["code"],
        "invalid_timeout"
    );
    assert_eq!(daemon.rpc(json!({"op":"ping"}))["ready"], true);
}

#[test]
fn command_errors_have_json_and_nonzero_exit_codes() {
    let daemon = Daemon::start(30_000);
    let output = cli(daemon.dir.path(), &["status", "--run", "missing"]);
    assert_eq!(output.status.code(), Some(1));
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "run_not_found");
    let run = daemon.submit("slow");
    let output = cli(daemon.dir.path(), &["result", "--run", &run]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["error"]["code"],
        "result_not_ready"
    );
    let output = cli(
        daemon.dir.path(),
        &["wait", "--run", &run, "--timeout-ms", "30001"],
    );
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn multiple_waiters_observe_completion_and_large_result_is_retrievable() {
    let daemon = Daemon::start(100);
    let text = "🌱".repeat(5_000);
    let run = daemon.submit(&text);
    let waiters: Vec<_> = (0..4)
        .map(|_| {
            let dir = daemon.dir.path().to_owned();
            let run = run.clone();
            thread::spawn(move || rpc(&dir, json!({"op":"wait","run_id":run,"timeout_ms":2000})))
        })
        .collect();
    for waiter in waiters {
        let response = waiter.join().unwrap();
        assert_eq!(response["status"], "completed");
        assert_eq!(response["result"]["truncated"], true);
        assert!(response["result"]["text"].as_str().unwrap().len() <= 16 * 1024);
    }
    let result = daemon.rpc(json!({"op":"result","run_id":run}));
    assert_eq!(result["result"]["text"], format!("[fake] {text}"));
    let status = daemon.rpc(json!({"op":"status","run_id":run}));
    assert!(status["result"]["text"].is_null());
}

#[test]
fn daemon_requires_explicit_fake_runner_and_preserves_non_socket_files() {
    let dir = private_dir();
    let output = cli(dir.path(), &["daemon"]);
    assert_eq!(output.status.code(), Some(2));
    let path = dir.path().join("daemon.sock");
    std::fs::write(&path, "keep me").unwrap();
    let output = cli(dir.path(), &["daemon", "--runner", "fake"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "keep me");
}
