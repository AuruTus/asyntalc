use super::*;

fn observe(daemon: &Daemon, run: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(150);
    loop {
        let snapshot = daemon.rpc(json!({"op":"wait","run_id":run,"timeout_ms":1000}));
        assert_eq!(snapshot["ok"], true, "{snapshot}");
        match snapshot["status"].as_str().unwrap() {
            "completed" | "waiting_for_parent" => return snapshot,
            "failed" | "cancelled" | "timed_out" => panic!("live run stopped: {snapshot}"),
            _ => {}
        }
        if Instant::now() >= deadline || snapshot["usage"]["model_requests"].as_u64().unwrap() >= 8
        {
            daemon.rpc(json!({"op":"cancel","run_id":run}));
            panic!("live smoke safety budget reached: {snapshot}");
        }
    }
}

#[test]
#[ignore = "billable workspace/parent/history smoke; requires ASYNTALC_LIVE_CONFIG and its key environment variable"]
fn live_workspace_smoke() {
    let path = std::env::var("ASYNTALC_LIVE_CONFIG").expect("set ASYNTALC_LIVE_CONFIG");
    let source: toml::Value = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let provider = source["provider"].as_table().unwrap();
    let key_env = provider["api_key_env"].as_str().unwrap();
    assert!(
        std::env::var(key_env).is_ok_and(|key| !key.is_empty()),
        "configured key environment variable is missing"
    );
    let mut selected = toml::map::Map::new();
    for field in [
        "base_url",
        "model",
        "api_key_env",
        "reasoning_effort",
        "output_token_parameter",
        "instruction_role",
    ] {
        if let Some(value) = provider.get(field) {
            selected.insert(field.into(), value.clone());
        }
    }
    selected.insert("ask_parent".into(), true.into());
    selected.insert("request_timeout_ms".into(), 60000.into());
    selected.insert("max_output_tokens".into(), 1024.into());
    selected.insert("system_prompt".into(), "Follow the test instructions exactly. Use one tool call at a time. Do not guess file contents. After a parent answer, finish with only the requested text.".into());
    let root = private_dir();
    std::fs::create_dir(root.path().join("records")).unwrap();
    let marker = format!("fixture_{}", uuid::Uuid::new_v4().simple());
    let file = format!("records/{}.txt", uuid::Uuid::new_v4().simple());
    std::fs::write(
        root.path().join("README.txt"),
        "The records directory contains the smoke fixture.\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join(&file),
        format!("SMOKE_RECORD\nmarker={marker}\n"),
    )
    .unwrap();
    let dir = private_dir();
    let mut config = toml::map::Map::new();
    config.insert("provider".into(), toml::Value::Table(selected));
    let mut workspace = toml::map::Map::new();
    workspace.insert("root".into(), root.path().to_str().unwrap().into());
    config.insert("workspace".into(), toml::Value::Table(workspace));
    std::fs::write(
        dir.path().join("provider.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();
    let child = Command::new(BIN)
        .arg("--data-dir")
        .arg(dir.path())
        .args(["daemon", "--config"])
        .arg(dir.path().join("provider.toml"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut daemon = Daemon { child, dir };
    daemon.ready();
    let scope = daemon.rpc(json!({"op":"scope"}));
    assert_eq!(scope["workspace"]["root"], root.path().to_str().unwrap());
    let receipt = daemon.rpc(json!({"op":"submit","session_id":"live-workspace","run_timeout_ms":180000,"input":"Perform these steps in order, with exactly one call for each: (1) workspace_list_files with path .; (2) workspace_search with path records and query SMOKE_RECORD; (3) workspace_read_file on the matching file; (4) ask_parent with prompt 'Choose a suffix' and choices ['alpha','beta']. After the parent answers, construct your final answer by concatenating exactly three pieces: the value after marker= on the file's marker line, a single vertical bar character |, and the parent's chosen suffix. Copy the value exactly; it starts with fixture_. Output only this concatenation on one line, with no labels, tags, spaces, quotes, or Markdown."}));
    assert_eq!(receipt["ok"], true, "{receipt}");
    let run = receipt["run_id"].as_str().unwrap();
    let paused = observe(&daemon, run);
    assert_eq!(paused["status"], "waiting_for_parent", "{paused}");
    let db = rusqlite::Connection::open(daemon.dir.path().join("state.sqlite3")).unwrap();
    let mut query = db.prepare("SELECT assistant_json,result_json FROM tool_exchanges WHERE run_id=?1 ORDER BY model_turn").unwrap();
    let exchanges: Vec<(Value, Value)> = query
        .query_map([run], |row| {
            let assistant: String = row.get(0)?;
            let result: String = row.get(1)?;
            Ok((
                serde_json::from_str(&assistant).unwrap(),
                serde_json::from_str(&result).unwrap(),
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let names: Vec<_> = exchanges
        .iter()
        .map(|(assistant, result)| {
            assert_eq!(result["ok"], true, "{result}");
            assistant["tool_calls"][0]["function"]["name"]
                .as_str()
                .unwrap()
        })
        .collect();
    assert_eq!(
        names,
        [
            "workspace_list_files",
            "workspace_search",
            "workspace_read_file"
        ]
    );
    assert_eq!(exchanges[2].1["path"], file);
    assert_eq!(
        exchanges[2].1["content"],
        format!("SMOKE_RECORD\nmarker={marker}\n")
    );
    let question = &paused["input_request"]["question_id"];
    let resumed =
        daemon.rpc(json!({"op":"resume","run_id":run,"question_id":question,"input":"beta"}));
    assert_eq!(resumed["ok"], true, "{resumed}");
    let completed = observe(&daemon, run);
    assert_eq!(completed["status"], "completed", "{completed}");
    println!(
        "live workspace checkpoint: {}",
        json!({"provider":scope["provider"],"tools":names,"parent_resume":true,
            "run_usage":completed["usage"],"final_text":completed["result"]["text"],
            "history_followup":"not_yet_tested"})
    );
    assert_eq!(
        completed["result"]["text"].as_str().unwrap().trim(),
        format!("{marker}|beta"),
        "tool execution and parent resume succeeded, but final-answer formatting did not match; history follow-up has not run"
    );
    assert_eq!(completed["usage"]["model_requests"], 5);
    std::fs::remove_file(root.path().join(&file)).unwrap();
    let followup = daemon.rpc(json!({"op":"submit","session_id":"live-workspace","run_timeout_ms":90000,"input":"Without using tools, repeat your previous final answer exactly. Use our conversation history."}));
    assert_eq!(followup["ok"], true, "{followup}");
    let followup = observe(&daemon, followup["run_id"].as_str().unwrap());
    assert_eq!(followup["status"], "completed", "{followup}");
    assert_eq!(followup["result"]["text"], completed["result"]["text"]);
    assert_eq!(followup["usage"]["model_requests"], 1);
    println!(
        "live workspace evidence: {}",
        json!({
            "provider":scope["provider"],"tools":names,"parent_resume":true,
            "history_after_file_removed":true,"run_usage":completed["usage"],
            "followup_usage":followup["usage"],"read_sha256":exchanges[2].1["sha256"]
        })
    );
}
