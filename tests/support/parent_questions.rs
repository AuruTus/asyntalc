use super::*;

fn question(call: &str, prompt: &str) -> Value {
    json!({"choices":[{"index":0,"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"tool_calls":[{"id":call,"type":"function","function":{"name":"ask_parent","arguments":json!({"prompt":prompt,"choices":["yes","no"]}).to_string()}}]}}],"usage":{"prompt_tokens":7,"completion_tokens":3}})
}
fn ask(api: &MockApi, daemon: &Daemon, run: &str, call: &str) -> Value {
    api.request();
    api.reply(Reply::Json(200, question(call, "Which option?")));
    let snapshot = wait(daemon, run);
    assert_eq!(snapshot["status"], "waiting_for_parent", "{snapshot}");
    assert_eq!(snapshot["return_reason"], "input_required");
    snapshot["input_request"].clone()
}
fn resume(daemon: &Daemon, run: &str, question: &Value, input: &str) -> Value {
    daemon.rpc(
        json!({"op":"resume","run_id":run,"question_id":question["question_id"],"input":input}),
    )
}

#[test]
fn parent_question_survives_restart_and_resume_commits_full_history() {
    let api = MockApi::start();
    let dir = private_dir();
    std::fs::write(
        dir.path().join("provider.toml"),
        config_text(&api.base_url, "ask_parent = true"),
    )
    .unwrap();
    let child = spawn_chat_with_limit(dir.path(), 1);
    let mut daemon = Daemon { child, dir };
    daemon.ready();
    let a = daemon.submit("first");
    let request = api.request();
    assert_eq!(request.body["parallel_tool_calls"], false);
    assert_eq!(request.body["tools"][0]["function"]["name"], "ask_parent");
    api.reply(Reply::Json(200, question("call_a", "Which option?")));
    let waiting = wait(&daemon, &a);
    assert_eq!(waiting["return_reason"], "input_required");
    let q = waiting["input_request"].clone();
    let a2 = daemon.submit("second");
    assert_eq!(
        daemon.rpc(json!({"op":"status","run_id":a2}))["blocked_by_run_id"],
        a
    );
    let b = chat_submit(&daemon, "independent", "other");
    assert_eq!(
        api.request().body["messages"],
        json!([{"role":"user","content":"other"}])
    );
    api.reply(Reply::Json(200, answer("other done")));
    assert_eq!(wait(&daemon, &b)["status"], "completed");
    restart_chat(&mut daemon);
    assert_eq!(wait(&daemon, &a)["input_request"], q);
    let receipt = resume(&daemon, &a, &q, "yes");
    assert_eq!(receipt["ok"], true, "{receipt}");
    let request = api.request();
    assert_eq!(request.body["messages"][0]["content"], "first");
    assert_eq!(request.body["messages"][1]["tool_calls"][0]["id"], "call_a");
    assert_eq!(
        request.body["messages"][2],
        json!({"role":"tool","tool_call_id":"call_a","content":"yes"})
    );
    assert_eq!(request.body["messages"].as_array().unwrap().len(), 3);
    assert_eq!(resume(&daemon, &a, &q, "yes"), receipt);
    assert_eq!(
        resume(&daemon, &a, &q, "no")["error"]["code"],
        "answer_conflict"
    );
    api.reply(Reply::Json(200, answer("final")));
    let done = wait(&daemon, &a);
    assert_eq!(done["status"], "completed");
    assert_eq!(done["started_at_ms"], waiting["started_at_ms"]);
    assert!(done["revision"].as_i64() > waiting["revision"].as_i64());
    assert_eq!(
        done["usage"],
        json!({"model_requests":2,"input_tokens":14,"output_tokens":6})
    );
    let next = api.request();
    assert_eq!(next.body["messages"].as_array().unwrap().len(), 5);
    assert_eq!(next.body["messages"][2]["tool_call_id"], "call_a");
    assert_eq!(next.body["messages"][3]["content"], "final");
    assert_eq!(next.body["messages"][4]["content"], "second");
    api.reply(Reply::Json(200, answer("second done")));
    assert_eq!(wait(&daemon, &a2)["status"], "completed");
    restart_chat(&mut daemon);
    assert_eq!(resume(&daemon, &a, &q, "yes"), receipt);
}

#[test]
fn waiting_cancel_and_deadline_reject_answers_without_poisoning_history() {
    let api = MockApi::start();
    let daemon = start_chat(&api, "ask_parent = true");
    let id = daemon.submit("cancel");
    let q = ask(&api, &daemon, &id, "cancel_call");
    let cancelled = daemon.rpc(json!({"op":"cancel","run_id":id}));
    assert_eq!(cancelled["status"], "cancelled");
    assert!(cancelled["input_request"].is_null());
    assert_eq!(
        resume(&daemon, &id, &q, "yes")["error"]["code"],
        "run_not_waiting"
    );
    let receipt = daemon
        .rpc(json!({"op":"submit","session_id":"test","input":"expire","run_timeout_ms":300}));
    let id = receipt["run_id"].as_str().unwrap();
    let q = ask(&api, &daemon, id, "expire_call");
    thread::sleep(Duration::from_millis(350));
    assert_eq!(
        resume(&daemon, id, &q, "yes")["error"]["code"],
        "run_not_waiting"
    );
    assert_eq!(wait(&daemon, id)["status"], "timed_out");
    let next = daemon.submit("healthy");
    assert_eq!(
        api.request().body["messages"],
        json!([{"role":"user","content":"healthy"}])
    );
    api.reply(Reply::Json(200, answer("done")));
    assert_eq!(wait(&daemon, &next)["status"], "completed");
}

#[test]
fn second_question_rejects_stale_answers_and_cli_resume_reads_file() {
    let api = MockApi::start();
    let daemon = start_chat(&api, "ask_parent = true");
    let id = daemon.submit("two questions");
    let q1 = ask(&api, &daemon, &id, "first_call");
    let receipt = resume(&daemon, &id, &q1, "first answer");
    let q2 = ask(&api, &daemon, &id, "second_call");
    assert_ne!(q1["question_id"], q2["question_id"]);
    assert_eq!(resume(&daemon, &id, &q1, "first answer"), receipt);
    assert_eq!(wait(&daemon, &id)["input_request"], q2);
    assert_eq!(
        resume(&daemon, &id, &q1, "changed")["error"]["code"],
        "answer_conflict"
    );
    assert_eq!(
        resume(&daemon, &id, &json!({"question_id":"missing"}), "answer")["error"]["code"],
        "question_not_found"
    );
    let path = daemon.dir.path().join("answer.txt");
    std::fs::write(&path, "second answer").unwrap();
    let result = cli(
        daemon.dir.path(),
        &[
            "resume",
            "--run",
            &id,
            "--question",
            q2["question_id"].as_str().unwrap(),
            "--input",
            path.to_str().unwrap(),
        ],
    );
    assert!(result.status.success(), "{:?}", result.stderr);
    let request = api.request();
    assert_eq!(request.body["messages"][4]["tool_call_id"], "second_call");
    assert_eq!(request.body["messages"][4]["content"], "second answer");
    let mut final_answer = answer("done");
    final_answer.as_object_mut().unwrap().remove("usage");
    api.reply(Reply::Json(200, final_answer));
    let done = wait(&daemon, &id);
    assert_eq!(done["usage"]["model_requests"], 3);
    assert!(done["usage"]["input_tokens"].is_null());
}

#[test]
fn invalid_questions_fail_and_question_count_is_bounded() {
    let api = MockApi::start();
    let daemon = start_chat(&api, "ask_parent = true");
    let mut cases = Vec::new();
    let mut bad = question("call", "prompt");
    bad["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] = json!("not json");
    cases.push(bad);
    let mut bad = question("call", "prompt");
    bad["choices"][0]["message"]["tool_calls"][0]["function"]["name"] = json!("shell");
    cases.push(bad);
    let mut bad = question("call", "prompt");
    let call = bad["choices"][0]["message"]["tool_calls"][0].clone();
    bad["choices"][0]["message"]["tool_calls"]
        .as_array_mut()
        .unwrap()
        .push(call);
    cases.push(bad);
    cases.push(question("call", ""));
    cases.push(question("call", &"x".repeat(8193)));
    for body in cases {
        let id = daemon.submit("invalid");
        api.request();
        api.reply(Reply::Json(200, body));
        assert_eq!(
            wait(&daemon, &id)["run_error"]["code"],
            "invalid_parent_question"
        );
    }
    let id = daemon.submit("too many questions");
    for i in 0..8 {
        let q = ask(&api, &daemon, &id, &format!("call_{i}"));
        assert_eq!(resume(&daemon, &id, &q, "answer")["ok"], true);
    }
    api.request();
    api.reply(Reply::Json(200, question("call_9", "one more")));
    let failed = wait(&daemon, &id);
    assert_eq!(failed["run_error"]["code"], "question_limit");
    assert_eq!(failed["usage"]["model_requests"], 9);
}
