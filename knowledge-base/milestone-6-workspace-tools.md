# Milestone 6: workspace tools and single-turn sessions

Version 0.1.5, schema 5, wire protocol 1. Implemented 2026-09-25 with separate subagents for filesystem access, provider/transcript integration, and tests. A later diagnostic subagent helped fix database-worker teardown.

## Agreed contract

- One unfinished run occupies a session, including queued and waiting-for-parent states. New submissions return `session_busy`; an exact idempotent retry returns its original receipt first. Resume continues the same run. Active cancellation retains occupancy through cleanup. Previously accepted queues still drain.
- No session version or application MVCC. SQLite transactions make admission atomic; `session_id` selects successful history and run `revision` tracks lifecycle changes.
- Optional top-level TOML `[workspace]` defines one daemon/profile root, read/list/search operations, literal exclusions, and bounds. Policy changes require a new session. `scope` exposes effective permissions and destination without keys or instructions.
- Read-only tools, live-file semantics, SHA-256 hashes. No shell, writes, snapshot isolation, per-session worktrees, retention, or autonomous swarm planner.

## Implementation map

`src/workspace.rs` uses Linux `openat2` beneath a held root descriptor with no symlinks or mount crossings. It obtains an `O_PATH` handle, checks file type, then reopens that held regular-file inode via `/proc/self/fd` for reading. Directory enumeration holds its descriptor and bounds depth/entry counts. A trusted local root is required; pathname exclusions do not prevent same-user hard-link aliases or malicious replacement of allowed content.

Mandatory `.git`, `.env`, `.asyntalc`, config-file and daemon-data exclusions are visible in the effective profile. Config/data exclusions apply when beneath the root; the data directory cannot equal the root. Other private names must be configured explicitly. A single component matches anywhere; slash-separated exclusions match a relative subtree, not a glob.

Configurable maxima are file bytes 262144, visited entries 10000, and result entries 1000. Fixed limits are depth 32, search read budget 4 MiB, serialized tool JSON 256 KiB, and 100 matching line numbers per file. Reads accept UTF-8. Limits bound work/bytes, not latency of a stalled filesystem.

`src/provider.rs` advertises only enabled tools, disables parallel calls, and accepts exactly one bounded call per response. `src/scheduler.rs` performs synchronous bounded filesystem work between model requests, persists the exchange, then continues the same run. Filesystem work is not detached during cancellation. At most 16 workspace calls, 8 questions, and 25 model requests are allowed with workspace enabled; existing profiles keep their 9-request limit.

Schema 5 adds `tool_exchanges` and backfills `questions.model_turn` from the previous ordinal. `src/store/parent.rs` merges both transcript types in model order for resumed context and final history commit. Only completed runs contribute to future history. The migration is embedded with `include_str!`; no distributed SQL companion is needed. Unknown usage remains unknown across interrupted requests.

## Database teardown deadlock found during validation

Repeated unit runs reproduced the older milestone-4 stall, including runs outside the sandbox and without output capture. A GDB-launched run exposed a concrete two-thread lock cycle:

1. A test dropped its last `Store` and immediately reopened the same database. `Store::open → sqlite3_open_v2 → unixOpen → findReusableFd` held SQLite's global Unix mutex and waited for the inode mutex.
2. The old database thread was still dropping its connection. `sqlite3_close → sqlite3WalClose → unixLock → unixIsSharingShmNode` held that inode mutex and waited for the global Unix mutex.

Other databases' workers then blocked on the global mutex, making unrelated tests appear stalled. The existing `barrier()` completed queued jobs but did not join the worker or wait for the connection destructor. This is evidence for the newly reproduced stall; the historical milestone-4 run had no stack trace, so an identical historical cause cannot be proven.

The lifecycle fix makes the final shared worker owner drop its sender and join the database thread. Final `Store` drop therefore waits for connection closure before reopening. Daemon shutdown also drops the store before releasing its directory ownership lock. This preserves async request processing and serializes only final cleanup. Keep this ownership rule when adding new background work; jobs must not capture and finally drop their own worker owner.

## Validation and next work

Local mock-server tests cover read → parent question → restart → resume → final → next-run history, list/search, denied paths/operations, private config/data exclusions, duplicate call IDs, and the 16-call limit. Filesystem tests cover symlink swapping, root replacement, special files, UTF-8, escaping and traversal budgets. Session tests cover simultaneous parents, busy states, exact retries, cancellation cleanup, and legacy queues. No paid API request is needed for these checks; the live test remains opt-in.

Final validation on Rust/Cargo 1.98.1: 22 unit + 39 integration tests passed, one live test ignored. Formatting, Clippy with warnings denied, and the locked offline all-target build passed. After the lifecycle fix, three repeated captured parallel unit runs passed (about 1.6 seconds each), followed by the passing combined suite. The teardown regression verifies WAL/SHM removal after each of 32 final-owner drops. Before the fix, repeated captured and uncaptured runs stalled, and a debugger-launched run captured the mutex cycle. Independent review found no additional blocker in filesystem confinement or cancellation cleanup.

The subsequent user-run [DeepSeek workspace smoke test](deepseek-workspace-live-validation.md) passed, establishing live non-thinking tool-call, parent-resume, and follow-up-history compatibility for this profile. Before enabling mutation or shell execution, define the separate workspace isolation and process sandbox contract. A parent-managed multi-session review demonstration remains a useful next stage.

The [swarm demo](../examples/swarm-demo.md) is now prepared for that user-run stage. Its fake CLI rehearsal and local mock-server test passed; the mock verifies three parent/resume/read workflows and one synthesis run (16 requests total). Busy/cancellation probes deliberately use a separate fake daemon. Live swarm evidence is still pending; do not confuse these local results with the earlier single-session DeepSeek smoke pass. Run `python3 tests/swarm_demo_test.py` separately from Cargo tests to validate this Python example.
