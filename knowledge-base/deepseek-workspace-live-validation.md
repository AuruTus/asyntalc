# DeepSeek workspace live validation

The user ran this test and supplied the passing output in the conversation. The agent could not independently issue requests because `DEEPSEEK_API_KEY` was absent from its process environment, even after checking outside the sandbox. No credential value was shared or stored.

```bash
ASYNTALC_LIVE_CONFIG=examples/deepseek.toml \
cargo test --locked --test client_daemon \
  chat_provider::live_workspace::live_workspace_smoke \
  -- --ignored --exact --nocapture
```

## Successful evidence

Endpoint: `https://api.deepseek.com`. Model: `deepseek-flash`. Profile: non-thinking (`reasoning_effort = "none"`), maximum output 1024 tokens per response. Version: 0.1.5. One live test passed in 4.43 seconds.

| Stage | Verified behavior | Requests | Input tokens | Output tokens |
|---|---|---:|---:|---:|
| Workspace and parent | `workspace_list_files`, `workspace_search`, `workspace_read_file`, then `ask_parent` and resume with `beta`; correct final fixture marker and suffix | 5 | 5424 | 244 |
| Same-session follow-up | Fixture removed before submission; identical answer returned from history without a new tool call | 1 | 1400 | 23 |
| Total | Entire live test | 6 | 6824 | 267 |

Read-result SHA-256: `6f67cf4be0e6d8c1400dda3e66dcfa8cc4fc225171475475b9ede6a1fde1a0f4`.

The test verifies persisted tool names, successful tool results, the read path and exact fixture content before resuming. The fixture marker and filename are random and are not supplied in the initial prompt. Exact final text and model-request counts remain asserted. It prints a checkpoint before the formatting assertion and a final evidence summary only after history passes. Temporary workspace and database directories are cleaned up afterward.

## Initial failure and correction

The first user-run attempt completed the workspace/parent workflow but returned a literal `<marker>` prefix before the correct fixture marker. The final exact-text assertion failed, so the history step did not run. This was an answer-formatting failure; the earlier assertions had already verified the three workspace tools and successful resume.

The test prompt was changed from angle-bracket placeholders to explicit concatenation of the file's marker value, a vertical bar, and the parent's suffix. The strict assertion was retained. The user's next run passed. This observation supports the revised test prompt; it does not establish deterministic model formatting across repeated runs.

## Boundaries

Only generated fixture files are exposed, using an isolated temporary daemon. The test selects connection/adapter settings from the provided config, enables parent questions, and replaces workspace and instructions. It expects six requests; an unexpected model path can consume more before the polling guard cancels it. HTTP and run deadlines plus the framework's model/tool limits remain in effect. It is ignored by default and must be explicitly selected for billable execution.

This validates non-thinking Chat tool calls, real parent resume, and same-session history. Thinking-mode reasoning replay, live daemon restart, concurrent sessions, arbitrary repository tasks, shell execution, and file mutation are outside this test. Restart and concurrency still have separate local test coverage. No runtime adapter change was needed for the observed successful run.
