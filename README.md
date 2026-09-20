# Asyntalc

A Rust prototype of a durable local executor for asynchronous subagent tasks.

**Version 0.1.4: run discovery and lifecycle inspection.** Independent sessions can run concurrently with a configurable global limit, while each session keeps FIFO ordering. The CLI supports cancellation, durable run deadlines, and safe submission retries. The daemon can call an OpenAI-compatible Chat endpoint or use the development fake runner. Opt-in parent clarification persists questions across restarts and resumes the same run. Workspace tools and sandbox execution remain future milestones.

See the [design](docs/asyntalc-design-v0.1.md), [prototype plan](docs/asyntalc-prototype-plan.md), and [milestone 5 handoff](knowledge-base/milestone-5-inspection.md).

## Build and run

Requirements: Linux, Rust 1.89 or newer, and a C toolchain for bundled SQLite. Version 0.1.4 was validated with Rust 1.98.1; the declared minimum version has not been tested separately.

```bash
cargo build --locked
cp examples/provider.toml provider.toml

# Edit provider.toml: set base_url and model to your provider's values.
# Set ASYNTALC_API_KEY in this terminal's environment via your secret manager.
# The configuration contains the environment-variable name, never the key itself.

# Terminal 1: foreground daemon; creates a private local data directory.
./target/debug/asyntalc --data-dir .asyntalc daemon --config provider.toml

# Terminal 2: submit a prompt, then use the returned run_id.
echo 'Compare two queue designs' > /tmp/asyntalc-task.md
./target/debug/asyntalc --data-dir .asyntalc submit --session queue-review --input /tmp/asyntalc-task.md
./target/debug/asyntalc --data-dir .asyntalc wait --run RUN_ID --timeout-ms 20000
./target/debug/asyntalc --data-dir .asyntalc result --run RUN_ID --output text
```

Replace `RUN_ID` with the `run_id` from submission. Use the same data directory for every command; relative paths resolve from each process's working directory. Prompts can also come from stdin using `--input -`. An optional `--session NAME` reuses a session; otherwise submission allocates one.

`submit` returns after SQLite commits acceptance, without waiting for execution. `status` reads the current state immediately. `wait` returns on completion, failure, cancellation, run deadline expiry, a parent question, or its own timeout; timeout and client disconnection leave work running. If a wait returns `wait_timeout`, call `wait` again with the same run ID. The daemon defaults to two active runs; set `daemon --max-active-runs N` (1–64) to change this. Each session has at most one active run; eligible sessions rotate in durable dispatch order.

For a follow-up, submit another prompt with `--session queue-review`. The daemon sends the stored successful user/assistant turns followed by the new prompt. A later queued prompt never appears in an earlier request. Failed turns, partial answers, and interrupted runs are retained for inspection but excluded from subsequent model context.

To demonstrate the lifecycle without credentials, use a separate directory:

```bash
./target/debug/asyntalc --data-dir .asyntalc/fake daemon --runner fake
```

Point those clients at `.asyntalc/fake`. This runner echoes `[fake] <input>` and makes no API request. `--fake-delay-ms` remains available for demonstrations.

On daemon restart with the same configuration, completed results remain available, unexpired queued runs continue, waiting questions remain available, persisted stop requests are finalized, expired runs become `timed_out`, and other previously running work becomes `failed` with `daemon_interrupted`. Interrupted requests are not replayed automatically. Use Ctrl-C or SIGTERM to stop the daemon; startup handles stale sockets after an abrupt exit. Dropping the local HTTP request does not guarantee that remote generation or billing stops.

## Cancellation, deadlines, and retries

```bash
# Retry this exact submission with the same key if its reply is lost.
./target/debug/asyntalc submit --session review --input /tmp/asyntalc-task.md \
  --idempotency-key review-001 --run-timeout-ms 600000
./target/debug/asyntalc cancel --run RUN_ID
./target/debug/asyntalc wait --run RUN_ID --timeout-ms 20000
```

A run deadline includes queue time and defaults to ten minutes from acceptance. `--run-timeout-ms` accepts 1–86,400,000 ms. It is independent of the provider's HTTP timeout and the client's bounded wait. Deadlines persist through restarts and use Unix wall-clock time.

`cancel` returns the current snapshot. Queued work stops immediately; active work may return `running` with `cancellation_requested: true` and phase `cancelling`, then becomes `cancelled` after the local request is dropped. Use `wait` for the terminal state. Repeated cancellation is harmless. Completion, cancellation, and deadline decisions are serialized in SQLite: an already committed terminal result never changes, and a persisted stop decision prevents a late answer from entering session history. Local cancellation cannot guarantee cancellation or prevent billing at the remote provider.

Idempotency keys are scoped to the data directory and retained with their run, without expiration in this prototype. The same key and identical input, session option, run timeout, and effective provider profile return the **original acceptance receipt**, even after completion or restart. Use `status` or `wait` for current state. A changed request returns `idempotency_conflict`. When the original request omitted `--session`, omit it on retries too; the stored receipt returns the same allocated session. Keys contain 1–128 ASCII letters, digits, or `_-.:`. A new request ID alone does not deduplicate a submission; retries without an idempotency key can create another run.

## Discover runs and inspect events

```bash
./target/debug/asyntalc list --limit 50
./target/debug/asyntalc list --session queue-review --status waiting_for_parent
./target/debug/asyntalc list --after CURSOR --limit 50
./target/debug/asyntalc logs --run RUN_ID --after-seq 0 --limit 50
```

`list` returns compact run summaries in ascending submission order. Use `next_after` as the next `--after` with the same filters while `has_more` is true. Summaries contain IDs, queue position, current status/revision, timestamps, error code, and pending question ID. Use `status` to read the question or `result` to retrieve the answer.

`logs` returns durable lifecycle events in ascending per-run sequence order. Each event contains `sequence`, `kind`, and `created_at_ms`. Continue with `next_after_seq` as `--after-seq`. An empty page preserves the supplied cursor, so a caller can poll again later. These are lifecycle events, not raw HTTP logs or conversation transcripts. Missing runs return `run_not_found`.

Both commands default to 50 items and accept 1–100. Cursors are exclusive nonnegative integers, scoped to this data directory (`list`) or run (`logs`). `has_more: false` means no further matches at query time. Pages reflect live state, not a frozen multi-page snapshot: new submissions can appear on later pages, and older runs can change status behind your cursor. Restart from `--after 0` to refresh a status-filtered view, especially pending questions. Neither command follows events continuously.

For a self-checking demo with no credentials:

```bash
cargo build --locked
bash examples/inspect-demo.sh
```

The demo requires Bash and Python 3. It submits/retries work, rediscovers handles through pagination, retrieves a result, checks lifecycle logs, and stops its daemon. It prints the temporary directory containing JSON evidence and SQLite. Records are retained; this prototype has no deletion or automatic retention policy.

## Parent questions and resume

Set `ask_parent = true` under `[provider]` in a profile for a model supporting Chat Completions function calls. This changes the session profile, so use a new session; finish or cancel outstanding work before changing the daemon configuration, or use another data directory. Existing text-only profiles default to `false` and retain their stored identity.

The daemon advertises one tool, `ask_parent({prompt, choices?})`. A validated call pauses the run as `waiting_for_parent`. `wait` returns immediately with `return_reason: "input_required"` and an `input_request` containing `question_id`, `prompt`, optional `choices`, and `allows_free_text: true`. A plain text question from the model does not suspend a run.

```bash
./target/debug/asyntalc wait --run RUN_ID --timeout-ms 20000
printf 'Preserve the existing CLI' > /tmp/asyntalc-answer.md
./target/debug/asyntalc resume --run RUN_ID --question QUESTION_ID \
  --input /tmp/asyntalc-answer.md
./target/debug/asyntalc wait --run RUN_ID --timeout-ms 20000
```

Replace both IDs from the snapshot and use the same data directory as your daemon. `resume` accepts a UTF-8 file or `--input -`; the answer may be free text even when choices are suggested. The acknowledgement records the same run requeued at its original FIFO position. Identical retries to that question return the original acknowledgement; changed answers return `answer_conflict`. A retry for an earlier answered question never answers a newer question. Unknown/mismatched question IDs return `question_not_found`; an unanswered question on a stopped run returns `run_not_waiting`.

Waiting consumes no execution slot, but blocks later same-session runs and counts toward pending capacity. Cancellation and the original run deadline still apply. The assistant tool call and linked parent answer persist privately with the run; only final success commits the whole turn sequence to future session history. Questions are clarification requests, not permission for filesystem or shell tools. The fake runner remains an echo runner and does not generate questions.

The tool/result message relationship follows the [official function-calling guide](https://developers.openai.com/api/docs/guides/function-calling). Local mock-server tests validate the request shape; live DeepSeek tool compatibility has not been tested.

## Provider configuration

The [example TOML](examples/provider.toml) lists all fields. `base_url` includes the provider's API prefix; the daemon appends `/chat/completions`. Configure `instruction_role` as `system` or `developer`, and `output_token_parameter` as `max_tokens` or `max_completion_tokens`, according to the endpoint/model. Requests use `stream: false` and the default single response choice. Optional `reasoning_effort` is sent only when configured. This supports text answers and the optional `ask_parent` control tool. Other tools and multiple calls in one turn fail explicitly.

The user-selected first live provider is DeepSeek. [examples/deepseek.toml](examples/deepseek.toml) targets `https://api.deepseek.com` with `deepseek-flash`, `max_tokens`, and `reasoning_effort = "none"`, following the current [DeepSeek API reference](https://api-docs.deepseek.com/api/create-chat-completion/). The model choice is configurable. The one-request live smoke test passed on 2026-09-19 using an environment-provided credential.

The daemon reads the named key environment variable at startup. It keeps the credential in memory, uses it as a bearer header, and stores only the non-secret profile in SQLite. HTTP redirects and automatic retries are disabled. Configuration and provider error diagnostics omit raw source lines and response bodies.

Each session retains its complete non-secret profile, including model, URL, instructions, limits, and key-variable name. A submission reusing that session under different settings fails with `session_config_conflict`. Start a new session to change settings. Restart with the original configuration to continue queued work; startup rejects mismatched queued or waiting profiles. Credential values can rotate without changing session identity, but require a daemon restart to reload.

This release supports one configured profile per daemon, supplied explicitly with `--config`. It does not yet implement layered project/user configuration or per-submission model overrides. The local `provider.toml` is ignored by Git.

### Configuration size and safety

Only `base_url`, `model`, and `api_key_env` are required. Other fields override validated defaults, so a normal profile need not repeat every limit. The DeepSeek example retains only those three fields plus its instructions, reasoning setting, and output-token budget. Removing explicitly written default values does not change the effective profile stored in a session.

Endpoints, model IDs, limits, and an environment-variable name are not credentials. Keep the actual key in the daemon's environment or a secret manager; do not place it in TOML, prompts, command arguments, or Git. The parser rejects a raw `api_key` field and credentials embedded in a URL, but it does not identify arbitrary secrets placed inside ordinary text fields.

Treat provider configuration as trusted input: changing `base_url` changes where the daemon sends its bearer credential and conversation. Use HTTPS for a remote endpoint; HTTP is also accepted for local test servers. Do not load an unreviewed profile merely because it contains no key itself.

System instructions may contain private information. Configuration files and SQLite are not encrypted. The daemon's data directory is private (0700), but the owning OS account and privileged processes can access it; an environment variable is not a vault against those processes. A private profile containing sensitive instructions should have restrictive permissions such as 0600 and stay out of Git. The profile, prompts, answers, and selected failure data persist in SQLite; the credential value does not enter those records through the authentication path.

## Output contract

Commands emit one JSON object on stdout by default; diagnostics use stderr. Only `result` supports `--output text`. `ping` reports daemon readiness and the active runner.

```json
{
  "protocol_version": 1,
  "request_id": "req_example",
  "ok": true,
  "run_id": "run_example",
  "session_id": "session_example",
  "status": "queued",
  "revision": 1,
  "deadline_at_ms": 1790000600000
}
```

Receipts describe committed acceptance, so `queued` may already have changed by the time the client receives it. Snapshots carry a revision, phase, Unix-millisecond timestamps (`created_at_ms`, `started_at_ms`, `finished_at_ms`, `deadline_at_ms`), result metadata, and a typed run error when applicable. These timestamp names are an initial implementation detail; the plan's ISO timestamp examples are not implemented yet.

`blocked_by_run_id` identifies the earliest unfinished predecessor in the same session, or is `null`; a global capacity wait has no blocking run ID. `cancellation_requested` reports a persisted client cancellation. `input_request` contains the pending parent question only while waiting; otherwise it is null. Legacy terminal runs may have no deadline.

`return_reason` is `snapshot` for status and cancellation queries, and `terminal`, `input_required`, or `wait_timeout` for waits. Status omits answer text. Wait includes at most 16 KiB of UTF-8 answer text and marks truncation; `result` retrieves the complete stored answer.

Snapshots also include `usage.model_requests`, `usage.input_tokens`, and `usage.output_tokens`. Unreported token counts are `null`. Request counts accumulate attempted initiation across resumes and are not a billing ledger. Token totals become null if any attempted turn has unknown usage or their sum cannot fit an integer. `phase` is `model_request` during a Chat API run. A valid final answer requires an assistant text response with `finish_reason: "stop"`.

Failures are persisted as typed `run_error` values: authentication, rate limit, provider service/request errors, transport/timeout, malformed response, refusal/filtering, unsupported tools, and context/response/output limits. Output-limit failures can expose a bounded `partial_result` with `complete: false`; this is never returned by `result` as a successful final answer. The provider finish reason is retained when valid and available.

Exit codes:

- `0`: the command succeeded, including a snapshot of a failed, cancelled, or timed-out run, or a wait timeout. Inspect `status` to decide whether the task succeeded.
- `1`: input, connection, protocol, or command failure. JSON mode returns an error object when possible.
- `2`: invalid command-line arguments; usage diagnostics go to stderr.

The local wire protocol is private. Each connection accepts one newline-terminated request and returns one response. Both carry protocol version 1 and a request ID. Requests use a nested `operation` object, for example:

```json
{"protocol_version":1,"request_id":"req_example","operation":{"op":"status","run_id":"run_example"}}
```

## Prototype limits

| Resource | Limit |
|---|---|
| Input | 64 KiB, nonempty UTF-8 |
| Protocol frame | 1 MiB including terminating newline |
| Active runs | 1–64, default 2; at most one per session |
| Run lifetime | 1–86,400,000 ms, default 600,000 ms including queue time |
| Inspection page | 1–100 items, default 50; compact summaries/events only |
| Pending runs | 128 queued/running/waiting runs |
| Parent questions per run | 8; at most 9 model requests including the final answer |
| Parent question | Prompt up to 8 KiB; up to 8 choices of 256 bytes; arguments up to 16 KiB |
| Parent answer | 64 KiB, nonempty UTF-8; subject to conversation context limit |
| Connected handlers | 64; excess connections are closed |
| Database work queue | 64 jobs |
| Wait duration | 0–30,000 ms |
| Request read / response write | 5 seconds each |
| Fake execution delay | 0–30,000 ms, default 100 ms |
| HTTP request deadline | Configurable 1–600,000 ms, default 120,000 ms; separate from client wait timeout |
| Conversation content | Default 256 KiB including instructions; maximum 4 MiB and 1,024 messages including the new prompt/instructions |
| HTTP response body | Default 1 MiB, maximum 4 MiB; checked while reading, including chunked responses |
| Decoded final answer | Configurable up to 64 KiB, default 64 KiB |
| Provider output tokens | Default 4,096; parameter name is configurable |

Context byte limits count message content, not model tokens or JSON encoding. Oversized history fails with `context_limit`; it is not silently truncated or summarized. A provider may impose a smaller token-based context window.

The data directory must be private (0700); the socket is 0600. A filesystem lock prevents two daemons from owning the same directory. SQLite changes and their lifecycle events commit together. All four schema migrations are embedded in the executable; no SQL files need to accompany a distributed binary. Version-1 through version-3 databases upgrade to schema 4, preserving sessions, results, messages, and events. Each migration is transactional. Legacy pending runs receive a ten-minute deadline from migration time; historical terminal runs retain a null deadline. Older executables then reject that database as newer than supported. Unknown schema versions are rejected.

This milestone does not implement retention/deletion, continuous log streaming, workspace tools, or sandboxing. Records and retry receipts remain in the data directory until it is managed externally while the daemon is stopped.

## Validation

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Normal tests spawn real daemon/CLI processes and a local HTTP test server, require Unix/TCP loopback sockets, and use a dummy key. They do not call an external provider. An execution sandbox that prohibits sockets must permit these integration tests outside that restriction.

An opt-in live smoke test makes one billable request using your configuration and verifies the retrieved response:

```bash
export ASYNTALC_LIVE_CONFIG="$PWD/provider.toml"
# The API-key variable named in provider.toml must also be set.
cargo test --locked --test client_daemon chat_provider::live_chat_smoke -- --ignored --exact
```

The live test uses a temporary data directory and removes it afterward. It is ignored by default. Local adapter tests are not a substitute for checking compatibility with your actual endpoint and model.

Version 0.1.4 validation on Rust/Cargo 1.98.1: 45 local tests passed; the paid live test stayed ignored. Formatting, the locked all-target build, and Clippy passed. The separately selected live DeepSeek test passed during milestone 2; no additional live request was made for inspection commands. The local suite also runs the Bash/Python inspection demo. No dependency version change was needed for these checks. A package's newer major release is not by itself evidence that the locked version is incompatible with the compiler.

### rust-analyzer after a Rust upgrade

An editor diagnostic on a derive macro such as `mismatched ABI expected: rustc 1.92.0, got: rustc 1.98.1` means the proc-macro host and compiled macro library use different compiler ABIs. This occurred on `serde_derive` in `src/protocol.rs` after the toolchain upgrade, while the locked Cargo build and tests passed. A fresh `rust-analyzer diagnostics . --severity error` scan also completed without errors.

In VS Code, run **rust-analyzer: Restart server** from the command palette; if the diagnostic remains, run **Developer: Reload Window**. If it still persists, inspect any `rust-analyzer.server.path`, `rust-analyzer.procMacro.server`, or toolchain override in local/remote settings for an old toolchain. The installed rustup-managed analyzer can be checked with `rust-analyzer --version` and `rustup which rust-analyzer`. VS Code can use its extension-bundled analyzer rather than that executable, as described in the [official installation guide](https://rust-analyzer.github.io/book/installation.html).

Restarting the editor-side host is the first remedy for this specific mismatch. Changing Serde versions, disabling macro diagnostics, or downgrading Rust is not justified by a passing Cargo build plus an old-host/new-library ABI diagnostic.

For the prepared DeepSeek profile, export `DEEPSEEK_API_KEY` in the environment used to launch the next session, then run:

```bash
export ASYNTALC_LIVE_CONFIG="$PWD/examples/deepseek.toml"
cargo test --locked --test client_daemon chat_provider::live_chat_smoke -- --ignored --exact
```
