# Asyntalc

A Rust prototype of a durable local executor for asynchronous subagent tasks.

**Current milestone: client/daemon contract with a fake runner.** No LLM API requests, workspace tools, or sandbox execution are implemented yet. The daemon requires `--runner fake` and prefixes answers with `[fake]`.

See the [design](docs/asyntalc-design-v0.1.md), [prototype plan](docs/asyntalc-prototype-plan.md), and [milestone 1 handoff](knowledge-base/milestone-1-client-daemon.md).

## Build and run

Requirements: Linux, Rust 1.89 or newer, and a C toolchain for bundled SQLite. Development was validated with Rust 1.92.0; the declared minimum version has not been tested separately.

```bash
cargo build --locked

# Terminal 1: foreground daemon; creates a private local data directory.
./target/debug/asyntalc --data-dir .asyntalc daemon --runner fake

# Terminal 2: submit a prompt, then use the returned run_id.
echo 'Compare two queue designs' > /tmp/asyntalc-task.md
./target/debug/asyntalc --data-dir .asyntalc submit --input /tmp/asyntalc-task.md
./target/debug/asyntalc --data-dir .asyntalc wait --run RUN_ID --timeout-ms 20000
./target/debug/asyntalc --data-dir .asyntalc result --run RUN_ID --output text
```

Replace `RUN_ID` with the `run_id` from submission. Use the same data directory for every command; relative paths resolve from each process's working directory. Prompts can also come from stdin using `--input -`. An optional `--session NAME` reuses a session; otherwise submission allocates one.

`submit` returns after SQLite commits acceptance, without waiting for execution. `status` reads the current state immediately. `wait` returns on completion, failure, or its own timeout; timeout and client disconnection leave work running. The first milestone executes one run at a time in submission order.

On daemon restart, completed results remain available, queued runs continue, and previously running work becomes `failed` with `daemon_interrupted`. Stopping the daemon during a fake run has the same recovery behavior. Use Ctrl-C or SIGTERM to stop it; startup handles stale sockets after an abrupt exit.

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
  "revision": 1
}
```

Receipts describe committed acceptance, so `queued` may already have changed by the time the client receives it. Snapshots carry a revision, phase, Unix-millisecond timestamps (`created_at_ms`, `started_at_ms`, `finished_at_ms`), result metadata, and a typed run error when applicable. These timestamp names are an initial implementation detail; the plan's ISO timestamp examples are not implemented yet.

`return_reason` is `snapshot` for status queries, and `terminal` or `wait_timeout` for waits. Status omits answer text. Wait includes at most 16 KiB of UTF-8 answer text and marks truncation; `result` retrieves the complete stored answer.

Exit codes:

- `0`: the command succeeded, including a snapshot of a failed run or a wait timeout. Inspect `status` to decide whether the task succeeded.
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
| Pending runs | 128 queued/running runs |
| Connected handlers | 64; excess connections are closed |
| Database work queue | 64 jobs |
| Wait duration | 0–30,000 ms |
| Request read / response write | 5 seconds each |
| Fake execution delay | 0–30,000 ms, default 100 ms |

The data directory must be private (0700); the socket is 0600. A filesystem lock prevents two daemons from owning the same directory. SQLite changes and their lifecycle events commit together. Schema version 1 is created transactionally; newer unknown versions are rejected.

This milestone does not implement parallel session execution, conversation replay into a model, idempotency keys, cancellation, run deadlines, parent questions, log/list commands, retention, or provider credentials. These remain in the prototype plan. Avoid blindly retrying an ambiguous submission until idempotency is added.

## Validation

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Tests spawn real daemon and CLI processes in temporary directories, use local Unix sockets, and require no LLM credentials. An execution sandbox that prohibits socket creation must permit these integration tests outside that restriction.
