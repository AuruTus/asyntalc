use super::*;
use std::{
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

const TEST_KEY: &str = "local-test-key-never-persist";

struct Captured {
    path: String,
    authorization: String,
    body: Value,
}

enum Reply {
    Json(u16, Value),
    Raw(Vec<u8>),
    Chunked(Vec<u8>),
    Redirect(String),
    Disconnect,
}

struct MockApi {
    base_url: String,
    requests: mpsc::Receiver<Captured>,
    replies: mpsc::Sender<Reply>,
    stopped: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockApi {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let (request_tx, requests) = mpsc::channel();
        let (replies, reply_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let (mut socket, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("mock accept: {error}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let request = read_request(&mut socket);
                if request_tx.send(request).is_err() {
                    break;
                }
                let reply = match reply_rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(reply) => reply,
                    Err(_) => break,
                };
                match reply {
                    Reply::Json(status, body) => {
                        send_body(&mut socket, status, &serde_json::to_vec(&body).unwrap())
                    }
                    Reply::Raw(bytes) => send_body(&mut socket, 200, &bytes),
                    Reply::Chunked(bytes) => {
                        let _ = write!(
                            socket,
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
                            bytes.len()
                        );
                        let _ = socket.write_all(&bytes);
                        let _ = socket.write_all(b"\r\n0\r\n\r\n");
                    }
                    Reply::Redirect(location) => {
                        let _ = write!(
                            socket,
                            "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                    }
                    Reply::Disconnect => {}
                }
            }
        });
        Self {
            base_url,
            requests,
            replies,
            stopped,
            thread: Some(thread),
        }
    }

    fn request(&self) -> Captured {
        self.requests.recv_timeout(Duration::from_secs(3)).unwrap()
    }
    fn reply(&self, reply: Reply) {
        self.replies.send(reply).unwrap();
    }
}

impl Drop for MockApi {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        let _ = self.replies.send(Reply::Disconnect);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn read_request(socket: &mut TcpStream) -> Captured {
    let mut reader = BufReader::new(socket);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("POST "));
    let path = line.split_whitespace().nth(1).unwrap().to_owned();
    let mut length = None;
    let mut authorization = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
        if name.eq_ignore_ascii_case("authorization") {
            authorization = value.trim().to_owned();
        }
    }
    let mut bytes = vec![0; length.unwrap()];
    reader.read_exact(&mut bytes).unwrap();
    Captured {
        path,
        authorization,
        body: serde_json::from_slice(&bytes).unwrap(),
    }
}

fn send_body(socket: &mut TcpStream, status: u16, body: &[u8]) {
    let _ = write!(
        socket,
        "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(body);
}

fn answer(text: &str) -> Value {
    json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":text}}], "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}})
}

fn config_text(base_url: &str, extra: &str) -> String {
    format!(
        "[provider]\nbase_url = {base_url:?}\nmodel = \"test-model\"\napi_key_env = \"ASYNTALC_TEST_KEY\"\n{extra}\n"
    )
}

fn spawn_chat(dir: &Path) -> Child {
    spawn_chat_with_limit(dir, 2)
}

fn spawn_chat_with_limit(dir: &Path, slots: u64) -> Child {
    Command::new(BIN)
        .arg("--data-dir")
        .arg(dir)
        .args(["daemon", "--config"])
        .arg(dir.join("provider.toml"))
        .arg("--max-active-runs")
        .arg(slots.to_string())
        .env("ASYNTALC_TEST_KEY", TEST_KEY)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn start_chat(api: &MockApi, extra: &str) -> Daemon {
    let dir = private_dir();
    std::fs::write(
        dir.path().join("provider.toml"),
        config_text(&api.base_url, extra),
    )
    .unwrap();
    let child = spawn_chat(dir.path());
    let mut daemon = Daemon { child, dir };
    daemon.ready();
    assert_eq!(daemon.rpc(json!({"op":"ping"}))["runner"], "chat");
    daemon
}

fn restart_chat(daemon: &mut Daemon) {
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
    daemon.child = spawn_chat(daemon.dir.path());
    daemon.ready();
}

fn wait(daemon: &Daemon, run: &str) -> Value {
    daemon.rpc(json!({"op":"wait","run_id":run,"timeout_ms":2000}))
}

#[test]
fn chat_uses_committed_history_and_survives_restart() {
    let api = MockApi::start();
    let mut daemon = start_chat(
        &api,
        "system_prompt = \"Be concise\"\ninstruction_role = \"developer\"\noutput_token_parameter = \"max_completion_tokens\"\nmax_output_tokens = 123",
    );
    let first = daemon.submit("Remember alpha");
    let request = api.request();
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(request.authorization, format!("Bearer {TEST_KEY}"));
    assert_eq!(
        request.body,
        json!({"model":"test-model","stream":false,"max_completion_tokens":123,"messages":[{"role":"developer","content":"Be concise"},{"role":"user","content":"Remember alpha"}]})
    );
    // The real request is gated at the mock server, not by timing assumptions.
    assert_eq!(
        daemon.rpc(json!({"op":"submit","session_id":"test","input":"What did I say?"}))["error"]["code"],
        "session_busy"
    );
    let pending = daemon.rpc(json!({"op":"wait","run_id":first,"timeout_ms":0}));
    assert_eq!(pending["phase"], "model_request");
    assert_eq!(pending["return_reason"], "wait_timeout");
    assert_eq!(pending["usage"]["model_requests"], 1);
    api.reply(Reply::Json(200, answer("I remember alpha")));
    let first_result = wait(&daemon, &first);
    assert_eq!(first_result["result"]["text"], "I remember alpha");
    assert_eq!(
        first_result["usage"],
        json!({"model_requests":1,"input_tokens":7,"output_tokens":3})
    );
    let second = daemon.submit("What did I say?");
    let request = api.request();
    assert_eq!(
        request.body["messages"],
        json!([
            {"role":"developer","content":"Be concise"},
            {"role":"user","content":"Remember alpha"},
            {"role":"assistant","content":"I remember alpha"},
            {"role":"user","content":"What did I say?"}
        ])
    );
    api.reply(Reply::Json(200, answer("alpha")));
    assert_eq!(wait(&daemon, &second)["status"], "completed");
    restart_chat(&mut daemon);
    assert_eq!(
        daemon.rpc(json!({"op":"result","run_id":second}))["result"]["text"],
        "alpha"
    );
    let third = daemon.submit("Continue");
    assert_eq!(api.request().body["messages"].as_array().unwrap().len(), 6);
    api.reply(Reply::Json(200, answer("continuing")));
    assert_eq!(wait(&daemon, &third)["status"], "completed");
    let independent = daemon.rpc(json!({"op":"submit","session_id":"other","input":"Fresh"}));
    assert_eq!(api.request().body["messages"].as_array().unwrap().len(), 2);
    api.reply(Reply::Json(200, answer("fresh answer")));
    assert_eq!(
        wait(&daemon, independent["run_id"].as_str().unwrap())["status"],
        "completed"
    );
    let db = rusqlite::Connection::open(daemon.dir.path().join("state.sqlite3")).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        5
    );
    let profile: String = db
        .query_row(
            "SELECT profile_json FROM sessions WHERE id='test'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!profile.contains(TEST_KEY));
    assert!(profile.contains("ASYNTALC_TEST_KEY"));
}

#[test]
fn provider_failures_are_typed_and_do_not_poison_session_history() {
    let api = MockApi::start();
    let daemon = start_chat(&api, "");
    let mut length = answer("partial answer");
    length["choices"][0]["finish_reason"] = json!("length");
    let mut filtered = answer("");
    filtered["choices"][0]["finish_reason"] = json!("content_filter");
    let mut refused = answer("");
    refused["choices"][0]["message"]["refusal"] = json!("refused");
    let mut tools = answer("");
    tools["choices"][0]["finish_reason"] = json!("tool_calls");
    tools["choices"][0]["message"]["tool_calls"] = json!([{"id":"call_1","type":"function"}]);
    let mut huge_usage = answer("answer");
    huge_usage["usage"]["prompt_tokens"] = json!(u64::MAX);
    let cases = [
        (
            Reply::Json(401, json!({"error":{"message":TEST_KEY}})),
            "provider_auth_error",
        ),
        (
            Reply::Json(429, json!({"error":"rate limit"})),
            "provider_rate_limited",
        ),
        (
            Reply::Json(503, json!({"error":"down"})),
            "provider_unavailable",
        ),
        (
            Reply::Json(400, json!({"error":"bad model"})),
            "provider_request_error",
        ),
        (Reply::Raw(b"not json".to_vec()), "provider_protocol_error"),
        (
            Reply::Json(200, json!({"choices":[]})),
            "provider_protocol_error",
        ),
        (Reply::Json(200, huge_usage), "provider_protocol_error"),
        (Reply::Json(200, length), "output_limit"),
        (Reply::Json(200, filtered), "provider_content_filter"),
        (Reply::Json(200, refused), "provider_refusal"),
        (Reply::Json(200, tools), "unsupported_capability"),
        (Reply::Disconnect, "provider_transport_error"),
    ];
    for (reply, expected) in cases {
        api.reply(reply);
        let run = daemon.submit("failing turn");
        let request = api.request();
        assert_eq!(
            request.body["messages"],
            json!([{"role":"user","content":"failing turn"}])
        );
        let failed = wait(&daemon, &run);
        assert_eq!(failed["status"], "failed", "{failed}");
        assert_eq!(failed["run_error"]["code"], expected, "{failed}");
        assert_eq!(failed["usage"]["model_requests"], 1);
        assert!(failed["result"].is_null());
        assert!(!failed.to_string().contains(TEST_KEY));
        if expected == "output_limit" {
            assert_eq!(failed["partial_result"]["text"], "partial answer");
            assert_eq!(failed["run_error"]["finish_reason"], "length");
        }
        assert!(
            api.requests.try_recv().is_err(),
            "automatic retry must be disabled"
        );
    }
    let mut response = answer("recovered");
    response.as_object_mut().unwrap().remove("usage");
    api.reply(Reply::Json(200, response));
    let run = daemon.submit("fresh successful turn");
    assert_eq!(api.request().body["messages"].as_array().unwrap().len(), 1);
    let complete = wait(&daemon, &run);
    assert_eq!(complete["status"], "completed");
    assert!(complete["usage"]["input_tokens"].is_null());
    assert!(complete["usage"]["output_tokens"].is_null());
    let db = rusqlite::Connection::open(daemon.dir.path().join("state.sqlite3")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    let mut query = db
        .prepare("SELECT coalesce(error_message,'') FROM runs")
        .unwrap();
    for error in query.query_map([], |r| r.get::<_, String>(0)).unwrap() {
        assert!(!error.unwrap().contains(TEST_KEY));
    }
}

#[test]
fn response_and_context_limits_fail_before_unbounded_work() {
    let api = MockApi::start();
    let daemon = start_chat(
        &api,
        "max_context_bytes = 10\nmax_response_bytes = 300\nmax_output_bytes = 4",
    );
    let over_context = daemon.submit("longer than ten bytes");
    let failed = wait(&daemon, &over_context);
    assert_eq!(failed["run_error"]["code"], "context_limit");
    assert_eq!(failed["usage"]["model_requests"], 0);
    assert!(api.requests.try_recv().is_err());
    for reply in [Reply::Raw(vec![b' '; 301]), Reply::Chunked(vec![b' '; 301])] {
        api.reply(reply);
        let run = daemon.submit("short");
        api.request();
        assert_eq!(wait(&daemon, &run)["run_error"]["code"], "response_limit");
    }
    api.reply(Reply::Json(200, answer("🌱🌱")));
    let run = daemon.submit("short");
    api.request();
    let failed = wait(&daemon, &run);
    assert_eq!(failed["run_error"]["code"], "output_limit");
    assert_eq!(failed["partial_result"]["text"], "🌱");
    api.reply(Reply::Json(200, answer("yes")));
    let success = daemon.submit("short");
    api.request();
    assert_eq!(wait(&daemon, &success)["status"], "completed");
    let next = daemon.submit("next");
    assert_eq!(wait(&daemon, &next)["run_error"]["code"], "context_limit");
    assert!(api.requests.try_recv().is_err());
}

#[test]
fn timeout_and_redirect_do_not_replay_or_forward_credentials() {
    let api = MockApi::start();
    let other = MockApi::start();
    let daemon = start_chat(&api, "request_timeout_ms = 100");
    let run = daemon.submit("timeout");
    api.request();
    assert_eq!(wait(&daemon, &run)["run_error"]["code"], "provider_timeout");
    api.reply(Reply::Disconnect);
    api.reply(Reply::Redirect(format!(
        "{}/chat/completions",
        other.base_url
    )));
    let run = daemon.submit("redirect");
    api.request();
    assert_eq!(
        wait(&daemon, &run)["run_error"]["code"],
        "provider_redirect"
    );
    assert!(other.requests.try_recv().is_err());
    assert!(api.requests.try_recv().is_err());
}

#[test]
fn changed_profile_cannot_reuse_existing_session() {
    let api = MockApi::start();
    let mut daemon = start_chat(&api, "");
    api.reply(Reply::Json(200, answer("first answer")));
    let run = daemon.submit("first");
    api.request();
    assert_eq!(wait(&daemon, &run)["status"], "completed");
    std::fs::write(
        daemon.dir.path().join("provider.toml"),
        config_text(&api.base_url, "system_prompt = \"changed\""),
    )
    .unwrap();
    restart_chat(&mut daemon);
    let conflict = daemon.rpc(json!({"op":"submit","session_id":"test","input":"second"}));
    assert_eq!(conflict["error"]["code"], "session_config_conflict");
    assert!(api.requests.try_recv().is_err());
    assert_eq!(
        daemon.rpc(json!({"op":"result","run_id":run}))["result"]["text"],
        "first answer"
    );
}

#[test]
fn embedded_migration_preserves_v1_results_and_sessions() {
    let dir = private_dir();
    let db = rusqlite::Connection::open(dir.path().join("state.sqlite3")).unwrap();
    db.execute_batch(include_str!("../../migrations/001_initial.sql"))
        .unwrap();
    db.execute_batch("INSERT INTO sessions VALUES ('legacy',1); INSERT INTO runs (id,session_id,input,status,revision,created_at_ms,result_text) VALUES ('old','legacy','prompt','completed',3,1,'[fake] prompt'); INSERT INTO events VALUES ('old',3,'run.completed',1);").unwrap();
    drop(db);
    let child = spawn(dir.path(), 0);
    let mut daemon = Daemon { child, dir };
    daemon.ready();
    assert_eq!(
        daemon.rpc(json!({"op":"result","run_id":"old"}))["result"]["text"],
        "[fake] prompt"
    );
    let run = daemon.rpc(json!({"op":"submit","session_id":"legacy","input":"new"}));
    assert_eq!(
        wait(&daemon, run["run_id"].as_str().unwrap())["status"],
        "completed"
    );
    let db = rusqlite::Connection::open(daemon.dir.path().join("state.sqlite3")).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        5
    );
}

#[test]
fn configuration_requires_credentials_and_rejects_embedded_secrets() {
    let dir = private_dir();
    let file = dir.path().join("provider.toml");
    std::fs::write(&file, config_text("https://api.example.invalid/v1", "")).unwrap();
    let output = Command::new(BIN)
        .arg("--data-dir")
        .arg(dir.path())
        .args(["daemon", "--config"])
        .arg(&file)
        .env_remove("ASYNTALC_TEST_KEY")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("environment variable"));
    for base in [
        "https://secret:password@example.invalid/v1",
        "https://example.invalid/v1?key=secret",
        "https://example.invalid/v1/chat/completions",
    ] {
        std::fs::write(&file, config_text(base, "")).unwrap();
        let output = cli(dir.path(), &["daemon", "--config", file.to_str().unwrap()]);
        assert_eq!(output.status.code(), Some(1));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("password"));
    }
    std::fs::write(
        &file,
        config_text(
            "https://example.invalid/v1",
            "api_key = \"never-echo-this-secret\"",
        ),
    )
    .unwrap();
    let output = cli(dir.path(), &["daemon", "--config", file.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("never-echo-this-secret"));
}

#[test]
fn interrupted_chat_is_not_replayed_and_queued_chat_keeps_its_profile() {
    let api = MockApi::start();
    let mut daemon = start_chat(&api, "");
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
    daemon.child = spawn_chat_with_limit(daemon.dir.path(), 1);
    daemon.ready();
    let active = daemon.submit("interrupted");
    api.request();
    let queued = chat_submit(&daemon, "queued", "queued");
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
    api.reply(Reply::Disconnect);
    // A restart under another profile must refuse to run the queued prompt.
    let output = cli(daemon.dir.path(), &["daemon", "--runner", "fake"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("original provider configuration"));
    assert!(api.requests.try_recv().is_err());
    daemon.child = spawn_chat(daemon.dir.path());
    daemon.ready();
    let request = api.request();
    assert_eq!(
        request.body["messages"],
        json!([{"role":"user","content":"queued"}])
    );
    let failed = wait(&daemon, &active);
    assert_eq!(failed["run_error"]["code"], "daemon_interrupted");
    assert_eq!(failed["usage"]["model_requests"], 1);
    api.reply(Reply::Json(200, answer("resumed queue")));
    assert_eq!(wait(&daemon, &queued)["result"]["text"], "resumed queue");
    assert!(api.requests.try_recv().is_err());
}

#[test]
fn context_instructions_and_trailing_base_slash_are_handled() {
    let api = MockApi::start();
    let dir = private_dir();
    std::fs::write(
        dir.path().join("provider.toml"),
        config_text(
            &format!("{}/", api.base_url),
            "system_prompt = \"hello\"\nmax_context_bytes = 8",
        ),
    )
    .unwrap();
    let child = spawn_chat(dir.path());
    let mut daemon = Daemon { child, dir };
    daemon.ready();
    let too_long = daemon.submit("four");
    assert_eq!(
        wait(&daemon, &too_long)["run_error"]["code"],
        "context_limit"
    );
    assert!(api.requests.try_recv().is_err());
    api.reply(Reply::Json(200, answer("ok")));
    let run = daemon.submit("yes");
    let request = api.request();
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(
        request.body["messages"],
        json!([{"role":"system","content":"hello"},{"role":"user","content":"yes"}])
    );
    assert_eq!(request.body["max_tokens"], 4096);
    assert!(request.body.get("max_completion_tokens").is_none());
    assert_eq!(wait(&daemon, &run)["status"], "completed");
}

#[test]
#[ignore = "makes one billable API request; requires ASYNTALC_LIVE_CONFIG and its key environment variable"]
fn live_chat_smoke() {
    let config = std::env::var("ASYNTALC_LIVE_CONFIG")
        .expect("set ASYNTALC_LIVE_CONFIG to a provider TOML file");
    let config = std::fs::canonicalize(config).unwrap();
    let dir = private_dir();
    let child = Command::new(BIN)
        .arg("--data-dir")
        .arg(dir.path())
        .args(["daemon", "--config"])
        .arg(config)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut daemon = Daemon { child, dir };
    daemon.ready();
    let run = daemon.submit("Reply with exactly: asyntalc smoke ok");
    let deadline = Instant::now() + Duration::from_secs(150);
    loop {
        let snapshot = wait(&daemon, &run);
        match snapshot["status"].as_str().unwrap() {
            "completed" => {
                let result = cli(
                    daemon.dir.path(),
                    &["result", "--run", &run, "--output", "text"],
                );
                assert!(result.status.success());
                assert_eq!(
                    String::from_utf8(result.stdout).unwrap().trim(),
                    "asyntalc smoke ok"
                );
                assert_eq!(snapshot["usage"]["model_requests"], 1);
                return;
            }
            "failed" => panic!("live request failed: {}", snapshot["run_error"]),
            _ => assert!(
                Instant::now() < deadline,
                "live request did not finish within 150 seconds"
            ),
        }
    }
}

#[test]
fn deepseek_example_uses_root_endpoint_and_optional_reasoning_setting() {
    let api = MockApi::start();
    let dir = private_dir();
    let profile = include_str!("../../examples/deepseek.toml")
        .replace(
            "https://api.deepseek.com",
            api.base_url.trim_end_matches("/v1"),
        )
        .replace("DEEPSEEK_API_KEY", "ASYNTALC_TEST_KEY");
    std::fs::write(dir.path().join("provider.toml"), profile).unwrap();
    let child = spawn_chat(dir.path());
    let mut daemon = Daemon { child, dir };
    daemon.ready();
    api.reply(Reply::Json(200, answer("ready")));
    let run = daemon.submit("smoke");
    let request = api.request();
    assert_eq!(request.path, "/chat/completions");
    assert_eq!(request.body["model"], "deepseek-flash");
    assert_eq!(request.body["reasoning_effort"], "none");
    assert_eq!(request.body["max_tokens"], 1024);
    assert!(request.body.get("n").is_none());
    assert_eq!(wait(&daemon, &run)["status"], "completed");
}

// Keep response sockets in the test thread: each accepted request is an explicit barrier.
fn gated_chat() -> (TcpListener, Daemon) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let dir = private_dir();
    std::fs::write(
        dir.path().join("provider.toml"),
        config_text(&format!("http://{}/v1", listener.local_addr().unwrap()), ""),
    )
    .unwrap();
    let child = spawn_chat(dir.path());
    let mut daemon = Daemon { child, dir };
    daemon.ready();
    (listener, daemon)
}
fn accept_request(listener: &TcpListener) -> (TcpStream, Captured) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match listener.accept() {
            Ok((mut socket, _)) => {
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let request = read_request(&mut socket);
                return (socket, request);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "HTTP request did not start");
                thread::sleep(Duration::from_millis(2));
            }
            Err(e) => panic!("accept: {e}"),
        }
    }
}
fn chat_submit(daemon: &Daemon, session: &str, input: &str) -> String {
    let response = daemon.rpc(json!({"op":"submit","session_id":session,"input":input}));
    assert_eq!(response["ok"], true);
    response["run_id"].as_str().unwrap().into()
}

#[test]
fn concurrent_http_requests_preserve_fifo_history_and_release_cancelled_session() {
    let (listener, daemon) = gated_chat();
    let a = chat_submit(&daemon, "a", "a1");
    let (mut a_socket, _) = accept_request(&listener);
    assert_eq!(
        daemon.rpc(json!({"op":"submit","session_id":"a","input":"a2"}))["error"]["code"],
        "session_busy"
    );
    let b = chat_submit(&daemon, "b", "b1");
    let (mut b_socket, b_request) = accept_request(&listener);
    assert_eq!(
        b_request.body["messages"],
        json!([{"role":"user","content":"b1"}])
    );
    let c = chat_submit(&daemon, "c", "c1");
    assert_eq!(
        daemon.rpc(json!({"op":"status","run_id":c}))["status"],
        "queued"
    );
    send_body(
        &mut b_socket,
        200,
        &serde_json::to_vec(&answer("b answer")).unwrap(),
    );
    assert_eq!(wait(&daemon, &b)["status"], "completed");
    let (mut c_socket, c_request) = accept_request(&listener);
    assert_eq!(c_request.body["messages"][0]["content"], "c1");
    daemon.rpc(json!({"op":"cancel","run_id":a}));
    let cancelled = wait(&daemon, &a);
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["usage"]["model_requests"], 1);
    // The local request has been dropped before A2 may claim the session.
    assert_eq!(a_socket.read(&mut [0_u8; 1]).unwrap(), 0);
    let a2 = chat_submit(&daemon, "a", "a2");
    let (mut a2_socket, a2_request) = accept_request(&listener);
    assert_eq!(
        a2_request.body["messages"],
        json!([{"role":"user","content":"a2"}])
    );
    // Even a server trying to return the abandoned answer cannot change durable state.
    send_body(
        &mut a_socket,
        200,
        &serde_json::to_vec(&answer("late a answer")).unwrap(),
    );
    send_body(
        &mut a2_socket,
        200,
        &serde_json::to_vec(&answer("a2 answer")).unwrap(),
    );
    send_body(
        &mut c_socket,
        200,
        &serde_json::to_vec(&answer("c answer")).unwrap(),
    );
    assert_eq!(wait(&daemon, &a2)["status"], "completed");
    assert_eq!(wait(&daemon, &c)["status"], "completed");
    assert_eq!(wait(&daemon, &a)["revision"], cancelled["revision"]);
    let a3 = chat_submit(&daemon, "a", "a3");
    let (mut a3_socket, a3_request) = accept_request(&listener);
    assert_eq!(
        a3_request.body["messages"],
        json!([{"role":"user","content":"a2"},{"role":"assistant","content":"a2 answer"},{"role":"user","content":"a3"}])
    );
    send_body(
        &mut a3_socket,
        200,
        &serde_json::to_vec(&answer("a3 answer")).unwrap(),
    );
    assert_eq!(wait(&daemon, &a3)["status"], "completed");
}

#[test]
fn run_deadline_drops_active_http_request_without_history() {
    let (listener, daemon) = gated_chat();
    let receipt =
        daemon.rpc(json!({"op":"submit","session_id":"a","input":"expire","run_timeout_ms":500}));
    let id = receipt["run_id"].as_str().unwrap();
    let (mut socket, _) = accept_request(&listener);
    let expired = wait(&daemon, id);
    assert_eq!(expired["status"], "timed_out");
    assert_eq!(expired["usage"]["model_requests"], 1);
    assert_eq!(socket.read(&mut [0_u8; 1]).unwrap(), 0);
    let next = chat_submit(&daemon, "a", "next");
    let (mut socket, request) = accept_request(&listener);
    assert_eq!(
        request.body["messages"],
        json!([{"role":"user","content":"next"}])
    );
    send_body(
        &mut socket,
        200,
        &serde_json::to_vec(&answer("healthy")).unwrap(),
    );
    assert_eq!(wait(&daemon, &next)["status"], "completed");
}

#[path = "parent_questions.rs"]
mod parent_questions;

#[path = "workspace_tools.rs"]
mod workspace_tools;
