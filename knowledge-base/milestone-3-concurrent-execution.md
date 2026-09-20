# Milestone 3: concurrent durable execution

Implemented for version 0.1.2 on 2026-09-20. Schema is version 3; private wire protocol remains version 1. The user will push the local commits after this delivery.

## Decisions and invariants

- `daemon --max-active-runs` defaults to 2, accepts 1–64, and is independent of the immutable provider profile. Each session has at most one active run. Durable session dispatch order rotates eligible session heads; run queue position preserves FIFO.
- `submit --run-timeout-ms` defaults to 600,000 and accepts 1–86,400,000. The persisted absolute deadline includes queue time. Client wait timeout and provider HTTP timeout remain separate. Wall-clock changes affect run deadlines; the scheduler periodically rechecks the clock.
- `submit --idempotency-key` stores the original request and receipt atomically with acceptance. Keys are directory-scoped and have no expiry yet. Exact retries use the same input, original optional session, normalized timeout, and effective provider profile. Credential values are never included. Retries return the original receipt even at queue capacity and after restart. Changed requests conflict.
- SQLite owns the first stop decision. Queued stops terminalize immediately. Active stops persist `stop_reason` while retaining `running`, preventing the session's next run from starting until cleanup. The task drops its provider future before terminalization. Completion/failure recheck stop/deadline within their transactions. Only successful completion adds conversation messages.
- Snapshots expose `deadline_at_ms`, `cancellation_requested`, and `blocked_by_run_id`. Cancellation can return a still-running snapshot with `phase: cancelling`; callers wait for a terminal status. Deadline cleanup uses `timing_out`. Repeated cancellation never changes terminal runs.
- The scheduler owns and drains all run tasks on shutdown/error before the daemon releases the directory lock. Store barriers drain accepted database jobs. Startup finalizes stored stops/expired deadlines; other previously active requests become `daemon_interrupted`, never automatically replayed.
- Migration 3 rebuilds the runs table transactionally to extend its status CHECK constraint. Foreign keys are disabled outside the transaction, checked before commit, then restored. IDs and dependent messages/events survive. Legacy pending runs receive ten minutes from migration time; historical terminal records have a null deadline. All migrations are embedded with `include_str!`; distribute only the binary.

## Validation

Rust/Cargo 1.98.1:

```bash
cargo fmt --all -- --check
cargo clippy --locked --offline --all-targets -- -D warnings
cargo build --locked --offline --all-targets
cargo test --locked --offline
```

Expected: **32 local tests pass, one live test ignored** (7 store unit tests + 25 daemon/CLI/HTTP integration tests). Integration tests need permission to bind local Unix and TCP sockets; sandbox socket denial is environmental, not a provider failure. The live DeepSeek test was not rerun or billed for milestone 3.

The local HTTP concurrency test holds response sockets as deterministic barriers: A and B are simultaneously in flight, A2 is blocked by A, and C waits for a free global slot. Cancelling A closes its local socket before A2 starts; A2 excludes abandoned A history; A3 replays successful A2. Separate tests cover a one-slot baseline, active HTTP deadline cleanup, queued expiry, eight concurrent duplicate submits, restart receipts, capacity retries, both cancellation/completion commit orders, fair dispatch after reopen, and foreign-key integrity across migration.

The prototype plan section 13 contains current architecture, CLI examples, output semantics, acceptance evidence, and the remaining work. Section 10's historical serial demo now explicitly sets `--max-active-runs 1` so it still reproduces the original baseline.

## Next work

Milestone 4 is parent interaction: validated `ask_parent` tool call, persisted question and run-local messages, `waiting_for_parent`, and a `resume` command. A waiting run must block later same-session turns without consuming an execution slot. Extend deadline/cancel/recovery handling to waiting runs. Persist question IDs/revisions for deterministic duplicate/stale answers. Only successful final completion commits the full conversation segment for later turns.

Milestone 5 adds paginated list/log inspection and retention decisions. Tool execution, workspace access, and sandboxing remain later work. No automatic remote retries or rate limiter have been added. Local request cancellation does not guarantee remote cancellation or prevent billing. Request counts represent attempted initiation; unknown token counts remain null.
