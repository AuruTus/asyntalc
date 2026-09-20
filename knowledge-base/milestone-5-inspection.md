# Milestone 5: run discovery and lifecycle inspection

Version 0.1.4, schema 4, protocol 1. No schema migration or dependency update. The user controls pushing local commits.

## Public contract

- `list [--session ID] [--status STATUS] [--after POSITION] [--limit N]` discovers compact run summaries in ascending submission order. Status accepts all seven current run states. It is a top-level command rather than the earlier plan's proposed `runs list` nesting.
- `logs --run ID [--after-seq SEQUENCE] [--limit N]` returns lifecycle event metadata in ascending per-run sequence order. It is finite JSON, not a continuous stream or raw provider transcript.
- Both default to 50 rows, range 1–100. Cursors are exclusive nonnegative signed 64-bit integers. List returns `runs`, `next_after`, `has_more`; logs returns `run_id`, `events`, `next_after_seq`, `has_more`.
- Next cursors always identify the last delivered item; empty pages preserve the input cursor. Stop paging when `has_more` is false, or retain the cursor for later polling. Unknown runs return `run_not_found`; unmatched filters return an empty successful list.
- List summaries include run/session IDs, queue position, status/revision, timestamps, error code, and pending question ID. Prompt/answer/question text, provider profiles, and credentials are absent. Follow with status/result for detail.
- Pages reflect live state, not a frozen multi-page snapshot. Inserts do not shift durable queue positions. Status changes behind a cursor are not rediscovered by forward-only scans: reset `after` to zero when refreshing a filtered view. Cursors must stay with their directory/run and filters; no encoded filter validation is provided.

## Implementation

`src/store/inspection.rs` uses the existing database worker. Run predicates are assembled from fixed SQL fragments with bound values, allowing existing queue/session/status indexes to be used. Limit-plus-one detects another page without loading all rows. Events use the existing run/sequence primary key. There is no execution task, model request, lifecycle mutation, or new table involved.

Lifecycle events are committed atomically with transitions. Sequences are per-run and monotonic; clients should tolerate gaps in legacy/imported event data. Logs intentionally contain only kind, sequence, and timestamp. Status remains the source for current result/error/question information.

## Demo and validation

```bash
cargo fmt --all -- --check
cargo clippy --locked --offline --all-targets -- -D warnings
cargo build --locked --offline --all-targets
cargo test --locked --offline
bash examples/inspect-demo.sh
```

Validated: 45 local passes (12 store tests + 33 integration tests), one paid live test ignored. Integration tests require Unix and local TCP sockets. The demo test additionally requires Bash and Python 3, which are not runtime dependencies of the shipped binary.

The demo starts a fake daemon in a private temporary directory, checks submit retry identity, discovers run IDs via two list pages, waits/retrieves the exact result, validates event pages, and stops the daemon. It retains JSON/SQLite evidence by default. Tests set `ASYNTALC_DEMO_DIR` to their own disposable private directory.

New tests verify cursor progression across insertion and restart, live filters, cancelled/interrupted errors, event polling across restart, typed invalid-input errors, the 100-item bound/frame size, real CLI JSON, and the script. The existing parent-interaction test now verifies discovery of its pending question and the complete eight-event question/resume lifecycle.

No live provider request was made for this milestone. The earlier milestone-4 stalled test invocation remains documented there; this milestone's initial full suite passed without reproducing it.

## Completion boundary and next work

The five planned milestones for the bounded async text prototype are complete. The broader v0.1 design is not complete: workspace access, tool execution, sandbox isolation, richer configuration/convenience commands, and retention remain.

Do not silently prune records. Runs, question answers, event history, and submission/resume acknowledgements carry retry and audit guarantees. Define an explicit retention/deletion policy before changing that lifetime.

The next bounded slice should define read-only workspace access and its security boundary before mutations or shell execution. Parent clarification does not grant authority to those future tools. Live model-specific tool compatibility also remains separate from the local protocol tests.
