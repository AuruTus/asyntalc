# Milestone 1: durable CLI and daemon handoff

Date: 2026-09-19. Status: implemented and validated.

## Scope and decisions

The user approved the prototype plan and requested initial commits followed by implementation. The first delivery implements milestone 1 only. OpenAI-compatible Chat Completions remains the selected provider for milestone 2.

- Initial commits separate repository instructions, original design/notes, and the prototype plan.
- Linux-only binary spelling is `asyntalc`, matching the design, despite the checkout directory being named `asynctalc`.
- The fake runner is explicitly selected by `daemon --runner fake`; it echoes input with a `[fake]` prefix and has a configurable delay for lifecycle tests.
- A single worker processes the durable FIFO. Session concurrency and a fair scheduler belong to milestone 3.
- SQLite is accessed through one bounded database-worker channel. No synchronous SQL runs on an async socket-handler thread after startup.
- Submission, state changes, messages, and lifecycle events are committed before acknowledgement/notification. `watch` subscriptions precede status reads to avoid losing completion wakeups.
- Snapshots currently use explicit Unix-millisecond timestamp fields. Provider/model usage, questions, and cancellation fields will be introduced with the corresponding behavior.
- Restart recovery is included early because it is necessary to demonstrate durability honestly: queued work is recovered; active work fails visibly; completed work survives.

## Validation and baseline

Hypothesis: a short-lived client can submit work that remains inspectable and retrievable by a later process, including after daemon restart. The baseline repository contained documentation only and had no executable behavior.

Validation uses fixed prompts, an explicit fake delay, and observed lifecycle states. Restart scenarios wait until a run is actually `running` before killing the daemon. They compare the durable states of active and queued work under the same interruption. No performance improvement or statistical attribution is claimed; algorithm/configuration ablation is not applicable to this initial functional slice.

Validated with Rust 1.92.0:

- `cargo check`: passed.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --locked --all-targets -- -D warnings`: passed.
- `cargo test --locked`: 8 integration tests passed, 0 failed.

The tests cover separate-process submission and result retrieval; persisted lifecycle events and messages; completed-result survival across restart; queued recovery without active replay; timeout/client disconnection semantics; multiple waiters; UTF-8-safe inline truncation with full-result retrieval; explicit fake selection; duplicate-daemon exclusion; private socket permissions; preservation of a non-socket file at the socket path; invalid frames, versions, inputs, operations, and wait durations; and command/run exit-code distinction.

Initial failures: test temporary directories needed explicit 0700 permissions; the execution sandbox denied Unix socket creation. The fixtures now set permissions, and integration tests passed with sandbox escalation. Dependency downloads also required escalation for registry access. No live provider request has been made.

## Next delivery

Implement milestone 2 using the selected OpenAI-compatible Chat Completions API. Read the OpenAI Docs skill and current official schema before implementation. Configure endpoint/model and resolve the key only in the daemon. Keep the fake runner and add a fake HTTP endpoint for reproducible adapter tests.

Read committed messages in session order for model context, keep incomplete run history out of later requests, enforce response/context limits, and map finish reasons and provider errors explicitly. A real smoke test requires a configured endpoint, model, and credential; normal tests must remain offline.

Do not describe this milestone as a functioning LLM agent or sandbox. Outstanding work includes model calls, concurrency, deduplication, cancellation/deadlines, parent clarification/resume, observability commands, tools, and isolation. Use the [prototype plan](../docs/asyntalc-prototype-plan.md) as the remaining work sequence.
