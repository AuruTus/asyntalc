# Milestone 4: persisted parent questions and resume

Version 0.1.3, schema 4, protocol 1. Implemented on 2026-09-20. The user controls pushing the local commits.

## Contract

`[provider] ask_parent = true` opts a Chat profile into the single supported function. False is omitted during profile serialization, preserving schema-3 profile identity and submission fingerprints. Enabling it changes session identity; outstanding queued/waiting work requires the original daemon profile.

The provider advertises `ask_parent({prompt, choices?})` with `parallel_tool_calls: false`. It validates exactly one function call, a bounded call ID and arguments, the prompt, and optional choices. Arbitrary tools and multiple calls fail. Plain text questions do not pause. The fake runner remains unchanged.

A question commits its daemon-owned ID, provider call ID, public prompt/choices, assistant message, event, and usage atomically with `waiting_for_parent`. Waiting releases its active slot, retains original FIFO position, blocks followers, and counts toward pending capacity. Status includes `input_request`; wait returns `input_required`. Free text is allowed even when choices exist.

`resume --run ID --question ID --input FILE` (or stdin `-`) commits an answer and original acknowledgement before requeuing the same run. Identical retries return that acknowledgement regardless of later state; a changed answer conflicts. An old answered question never answers a new pending one. Unknown/run-mismatched IDs return `question_not_found`; stopped unanswered questions return `run_not_waiting`.

Deadlines include waiting time, survive restart, and are checked transactionally on resume. Cancellation of waiting work is immediate. A stop winning before question persistence prevents suspension. Restart preserves unexpired waiting questions and answered transcripts. Other interrupted active requests remain failed without automatic replay.

## Implementation details worth preserving

- `src/store/parent.rs` owns pause, resume, transcript assembly, and successful transcript commit. Questions/answers remain run-local until success. Committed messages now include typed `tool_calls` and `tool_call_id` metadata. Failed transcripts never enter later session context.
- A resumed run may become queued before its previous task has exited. `claim_available` excludes run IDs still owned by the scheduler's active map. This prevents duplicate task ownership and loss of cancellation signals.
- Claim increments revision and preserves the first `started_at_ms`; it must never reset revision to 2 on resume.
- `mark_requested` accumulates attempted model requests. Completed turns add reported tokens; any unreported attempted turn or sum overflow makes the total null. Cancelling an in-flight resumed request also invalidates prior partial token totals.
- Bounds: 8 questions / 9 model requests per run, question prompt 8 KiB, arguments 16 KiB, at most 8 choices of 256 bytes, parent answer 64 KiB. Tool metadata and answered run-local content count toward the context budget before allocation/request.
- Embedded migration 4 rebuilds runs/messages to extend CHECK constraints and creates questions with unique run/ordinal, run/call ID, and one unanswered question per run. Foreign keys are verified in the transaction. Existing IDs, results, histories, and idempotency receipts survive.
- SQLite rows can retain unanswered questions for terminal runs as audit data; snapshots expose questions only while waiting. Answers already accepted remain retryable even after cancellation, but no new answer is accepted for a stopped question.

## Validation

```bash
cargo fmt --all -- --check
cargo clippy --locked --offline --all-targets -- -D warnings
cargo build --locked --offline --all-targets
cargo test --locked --offline
```

Final clean run: 41 local passes (12 store tests + 29 integration tests), one paid live test ignored. Local sockets need permission outside a socket-restricted sandbox. Parent tool compatibility has not been live-tested with DeepSeek; the earlier text-only live smoke evidence remains unchanged.

One earlier full-suite invocation stalled in database tests and was terminated. Fresh parallel and serial unit runs and the complete suite passed; the stall was not reproduced or attributed to a confirmed code defect. If it recurs, inspect database connection/thread shutdown during reopen tests; attaching a debugger was unavailable in this environment.

The new local HTTP tests cover a one-slot waiting run freeing capacity, same-session blocking, restart, exact tool/result linkage, full successful history, cumulative/unknown usage, repeated questions, stale/duplicate answers, CLI answer files, cancellation/expiry, malformed or excessive questions, and bounded question count. Store tests add stop-before-question races, fast resume exclusion, pending capacity, context/turn limits, profile mismatch, deadline checks on resume, and schema-3 retry preservation.

The OpenAI Docs skill was used to check the Chat function-call/result relationship against https://developers.openai.com/api/docs/guides/function-calling. This is a portable compatibility subset and does not implement model-specific reasoning continuation fields.

## Next milestone

Add paginated `list` and lifecycle `logs` so operators can discover run IDs, pending questions, and failure events. Decide retention behavior for runs and their submit/resume retry receipts. Parent clarification is not authorization for future privileged tools. Workspace access, shell tools, sandbox isolation, and provider-specific reasoning/tool extensions remain outside this milestone.
