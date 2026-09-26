use super::*;

fn tool(call: &str, name: &str, args: Value) -> Value {
    json!({"choices":[{"index":0,"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"tool_calls":[{"id":call,"type":"function","function":{"name":name,"arguments":args.to_string()}}]}}],"usage":{"prompt_tokens":7,"completion_tokens":3}})
}

fn start_workspace(api: &MockApi, root: &Path, operations: &str) -> Daemon {
    start_chat(
        api,
        &format!(
            "ask_parent = true\n[workspace]\nroot = {:?}\noperations = {operations}",
            root.to_str().unwrap()
        ),
    )
}

#[test]
fn workspace_scope_is_explicit_and_disabled_operations_never_execute() {
    let api = MockApi::start();
    let root = tempfile::tempdir().unwrap();
    let daemon = start_workspace(&api, root.path(), "[\"read_file\"]");
    let scope = daemon.rpc(json!({"op":"scope"}));
    assert_eq!(scope["ok"], true, "{scope}");
    assert!(scope.to_string().contains(root.path().to_str().unwrap()));
    assert!(!scope.to_string().contains(TEST_KEY));
    let output = cli(daemon.dir.path(), &["scope"]);
    assert!(output.status.success());
    let _: Value = serde_json::from_slice(&output.stdout).unwrap();
    for name in ["workspace_search", "arbitrary_shell"] {
        let run = daemon.submit("use a tool");
        let request = api.request();
        let names: Vec<_> = request.body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"workspace_read_file"));
        assert!(!names.contains(&"workspace_search"));
        api.reply(Reply::Json(200, tool("disabled", name, json!({}))));
        let result = wait(&daemon, &run);
        assert_eq!(result["status"], "failed", "{result}");
        assert_eq!(result["run_error"]["code"], "unsupported_capability");
        assert!(api.requests.try_recv().is_err());
    }
}

#[test]
fn workspace_read_parent_resume_and_next_run_preserve_ordered_history() {
    let api = MockApi::start();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note.txt"), "workspace history marker").unwrap();
    let mut daemon = start_workspace(&api, root.path(), "[\"read_file\"]");
    let run = daemon.submit("read the note and ask me");
    api.request();
    api.reply(Reply::Json(
        200,
        tool("read_1", "workspace_read_file", json!({"path":"note.txt"})),
    ));
    let read_request = api.request();
    let messages = read_request.body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["tool_calls"][0]["id"], "read_1");
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], "read_1");
    let read_result: Value =
        serde_json::from_str(messages[2]["content"].as_str().unwrap()).unwrap();
    assert_eq!(read_result["ok"], true);
    let page = daemon.rpc(json!({"op":"tools","run_id":run,"limit":1}));
    assert_eq!(page["tools"][0]["name"], "workspace_read_file");
    assert_eq!(page["tools"][0]["bytes"], "workspace history marker".len());
    assert_eq!(page["tools"][0]["sha256"], read_result["sha256"]);
    assert_eq!(page["tools"][0]["truncated"], false);
    assert!(!page.to_string().contains("workspace history marker"));
    assert_eq!(page["next_after"], 1);
    assert_eq!(page["has_more"], false);
    assert_eq!(read_result["sha256"].as_str().unwrap().len(), 64);
    assert!(
        read_result.to_string().contains("workspace history marker"),
        "{read_result}"
    );
    api.reply(Reply::Json(
        200,
        tool("ask_1", "ask_parent", json!({"prompt":"Continue?"})),
    ));
    let paused = wait(&daemon, &run);
    assert_eq!(paused["status"], "waiting_for_parent");
    restart_chat(&mut daemon);
    let restored = daemon.rpc(json!({"op":"tools","run_id":run}));
    assert_eq!(restored["tools"], page["tools"]);
    assert_eq!(
        wait(&daemon, &run)["input_request"],
        paused["input_request"]
    );
    let resumed = daemon.rpc(json!({"op":"resume","run_id":run,"question_id":paused["input_request"]["question_id"],"input":"yes"}));
    assert_eq!(resumed["ok"], true, "{resumed}");
    let request = api.request();
    let transcript = request.body["messages"].as_array().unwrap();
    assert_eq!(transcript.len(), 5);
    assert_eq!(&transcript[..3], &messages[..]);
    assert_eq!(transcript[3]["tool_calls"][0]["id"], "ask_1");
    assert_eq!(transcript[4]["tool_call_id"], "ask_1");
    assert_eq!(transcript[4]["content"], "yes");
    api.reply(Reply::Json(200, answer("read confirmed")));
    let completed = wait(&daemon, &run);
    assert_eq!(completed["status"], "completed", "{completed}");
    assert_eq!(
        completed["usage"],
        json!({"model_requests":3,"input_tokens":21,"output_tokens":9})
    );
    let next = daemon.submit("continue history");
    let request = api.request();
    let history = request.body["messages"].as_array().unwrap();
    assert_eq!(history.len(), 7);
    assert_eq!(&history[..5], &transcript[..]);
    assert_eq!(history[5]["content"], "read confirmed");
    api.reply(Reply::Json(200, answer("done")));
    assert_eq!(wait(&daemon, &next)["status"], "completed");
}

#[test]
fn denied_paths_return_tool_errors_and_list_search_continue() {
    let api = MockApi::start();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note.txt"), "find this literal\n").unwrap();
    std::fs::write(root.path().join(".env"), "private-marker").unwrap();
    let daemon = start_workspace(
        &api,
        root.path(),
        "[\"read_file\",\"list_files\",\"search\"]",
    );
    let run = daemon.submit("inspect workspace");
    api.request();
    for (index, path) in ["../outside", ".env"].into_iter().enumerate() {
        api.reply(Reply::Json(
            200,
            tool(
                &format!("denied_{index}"),
                "workspace_read_file",
                json!({"path":path}),
            ),
        ));
        let request = api.request();
        let result: Value = serde_json::from_str(
            request.body["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["ok"], false, "{result}");
        assert!(!request.body.to_string().contains("private-marker"));
    }
    api.reply(Reply::Json(
        200,
        tool("list_1", "workspace_list_files", json!({"path":"."})),
    ));
    let request = api.request();
    let result: Value = serde_json::from_str(
        request.body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["entries"].as_array().unwrap().len(), 1);
    assert_eq!(result["entries"][0]["path"], "note.txt");
    api.reply(Reply::Json(
        200,
        tool(
            "search_1",
            "workspace_search",
            json!({"path":".","query":"literal"}),
        ),
    ));
    let request = api.request();
    let result: Value = serde_json::from_str(
        request.body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["entries"][0]["path"], "note.txt");
    assert_eq!(result["entries"][0]["lines"], json!([1]));
    api.reply(Reply::Json(200, answer("inspected")));
    let done = wait(&daemon, &run);
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["usage"]["model_requests"], 5);
    let first = daemon.rpc(json!({"op":"tools","run_id":run,"limit":2}));
    assert_eq!(first["tools"].as_array().unwrap().len(), 2);
    assert_eq!(first["has_more"], true);
    assert_eq!(first["tools"][0]["path"], Value::Null);
    assert_eq!(first["tools"][0]["error_code"], "invalid_path");
    assert_eq!(first["tools"][1]["path"], ".env");
    assert_eq!(first["tools"][1]["error_code"], "path_excluded");
    let second =
        daemon.rpc(json!({"op":"tools","run_id":run,"after":first["next_after"],"limit":2}));
    assert_eq!(second["has_more"], false);
    assert_eq!(second["tools"][0]["returned_entries"], 1);
    assert_eq!(
        second["tools"][1]["scanned_bytes"],
        "find this literal\n".len()
    );
    assert!(!second.to_string().contains("literal"));
    let output = cli(
        daemon.dir.path(),
        &["tools", "--run", &run, "--after", "4", "--limit", "1"],
    );
    assert!(output.status.success());
    let empty: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(empty["tools"], json!([]));
    assert_eq!(empty["next_after"], 4);
}

#[test]
fn tool_limit_and_duplicate_ids_fail_without_committing_history() {
    let api = MockApi::start();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note"), "observed").unwrap();
    let daemon = start_workspace(&api, root.path(), "[\"read_file\"]");
    for duplicate in [false, true] {
        let run = daemon.submit("bounded tools");
        let first = api.request();
        assert_eq!(first.body["messages"].as_array().unwrap().len(), 1);
        let count = if duplicate { 1 } else { 16 };
        for index in 0..count {
            api.reply(Reply::Json(
                200,
                tool(
                    &format!("read_{index}"),
                    "workspace_read_file",
                    json!({"path":"note"}),
                ),
            ));
            api.request();
        }
        api.reply(Reply::Json(
            200,
            tool(
                if duplicate { "read_0" } else { "too_many" },
                "workspace_read_file",
                json!({"path":"note"}),
            ),
        ));
        let failed = wait(&daemon, &run);
        assert_eq!(failed["status"], "failed", "{failed}");
        assert_eq!(
            failed["run_error"]["code"],
            if duplicate {
                "invalid_tool_call"
            } else {
                "tool_call_limit"
            }
        );
        assert_eq!(failed["usage"]["model_requests"], count + 1);
        assert_eq!(failed["usage"]["input_tokens"], 7 * (count + 1));
        assert!(api.requests.try_recv().is_err());
    }
}

#[test]
fn effective_scope_excludes_private_data_and_config() {
    let api = MockApi::start();
    let root = private_dir();
    let dir = tempfile::tempdir_in(root.path()).unwrap();
    std::fs::set_permissions(
        dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("provider.toml"),
        config_text(
            &api.base_url,
            &format!(
                "[workspace]\nroot = {:?}\nexclude = []",
                root.path().to_str().unwrap()
            ),
        ),
    )
    .unwrap();
    let child = spawn_chat(dir.path());
    let mut daemon = Daemon { child, dir };
    daemon.ready();
    let scope = daemon.rpc(json!({"op":"scope"}));
    let excluded = scope["workspace"]["exclude"].as_array().unwrap();
    let private = daemon.dir.path().file_name().unwrap().to_str().unwrap();
    for path in [
        ".git",
        ".env",
        ".asyntalc",
        private,
        &format!("{private}/provider.toml"),
    ] {
        assert!(excluded.contains(&json!(path)), "{scope}");
    }
    let run = daemon.submit("read private state");
    api.request();
    for (index, path) in [
        format!("{private}/provider.toml"),
        format!("{private}/state.sqlite3"),
    ]
    .into_iter()
    .enumerate()
    {
        api.reply(Reply::Json(
            200,
            tool(
                &format!("private_{index}"),
                "workspace_read_file",
                json!({"path":path}),
            ),
        ));
        let request = api.request();
        let result: Value = serde_json::from_str(
            request.body["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["error"]["code"], "path_excluded");
    }
    api.reply(Reply::Json(200, answer("private files unavailable")));
    assert_eq!(wait(&daemon, &run)["status"], "completed");
}
