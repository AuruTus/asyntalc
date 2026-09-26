# Parent-managed swarm demonstration

Run from the repository root with Python 3.11+ and the configured API-key environment variable already exported:

```bash
cargo build --locked
python3 examples/swarm-demo.py --config examples/deepseek.toml
```

This is a billable live demonstration. The expected path uses 16 model requests: five for each of three reviewers (parent question, three file reads, final answer), then one synthesis request. Model behavior can vary. Each response is capped at 1200 output tokens; HTTP/run deadlines and observed request/question budgets stop unexpected loops. The polling budget is not a hard billing ceiling.

The Python script is the parent coordinator. It creates an isolated fixture with an intentionally unsafe counter implementation and a minimal test. It launches one daemon with three active slots, submits architecture/concurrency/test-coverage sessions before collecting results, and rediscovers their handles with `list`. When reviewers ask for a priority, it supplies a preconfigured parent answer, visible in stdout and saved evidence. Override that policy with `--parent-answer 'your priority'`.

Each reviewer must ask a parent question and successfully read all three fixture files. Completed reviews are saved and passed to a fourth session acting as the parent's synthesizer. Its output is a model-generated review, not proof that every finding is correct. The script does not execute fixture code or expose your real repository. It copies only endpoint/model/credential-variable and adapter settings from your profile; workspace, instructions, timeouts, output limits, and parent capability are set by the demo.

Before the live portion, a separate fake daemon checks `session_busy`, queued cancellation, and active cancellation. These lifecycle checks incur no provider charge. Their evidence is explicitly labeled fake; they do not establish live cancellation behavior.

The script prints a private `/tmp/asyntalc-swarm-*` evidence directory and stops its daemons on exit. Files remain for review:

- `summary.json`: run IDs, statuses, usage, effective scope, and mode.
- `architecture.md`, `concurrency.md`, `test-coverage.md`, `synthesis.md`: individual and consolidated reviews.
- `parent-questions.json`: questions and the parent's scripted answers.
- `tool-evidence.json`: verified read metadata for each reviewer, obtained through paginated `tools` calls without file contents (v0.1.6+).
- `receipts.json`, `discovered.json`, per-review lifecycle logs, and `lifecycle-probe.json`.
- `state/state.sqlite3`: durable conversation/tool history; temporary config contains only the key's variable name.

Share `summary.json` and `synthesis.md` for review, plus any failure output. No key value is printed or written by the script. On failure, inspect the printed evidence directory; do not repeatedly rerun a billable failure without examining it.

To inspect a retained run interactively, restart its daemon in one terminal (replace `EVIDENCE_DIR` with the printed absolute path; export the configured key):

```bash
target/debug/asyntalc --data-dir EVIDENCE_DIR/state daemon --config EVIDENCE_DIR/provider.toml
```

Then use the same directory in another terminal, replacing `RUN_ID` with a reviewer ID from `summary.json`:

```bash
target/debug/asyntalc --data-dir EVIDENCE_DIR/state tools --run RUN_ID --limit 20
```

The synthesis run normally has no workspace calls and returns an empty page. The CLI requires a running daemon; the run ID alone does not locate its database. Reading saved JSON files needs no daemon. The demo does not query SQLite directly for tool evidence in v0.1.6+.

## Local validation

```bash
# No provider calls; checks CLI coordination and lifecycle probes only.
python3 examples/swarm-demo.py --fake

# Local mock Chat server: checks all three question/resume/read loops and synthesis.
python3 tests/swarm_demo_test.py
```

Both passed during preparation. The mock test verifies 16 requests, three parent interactions, and three sets of file-read evidence. These commands require local Unix/TCP sockets. The fake mode produces echoed prompts, not actual reviews.

The first user-run live attempt reached all three parent questions, but architecture failed on its second model response with `invalid_tool_call`, before any workspace exchange was persisted. Cleanup cancelled the other reviewers. The rejected response was not retained, so batched calls are a hypothesis, not a confirmed cause. The prompt now explicitly reads one file per response, waiting for each result; the parser distinguishes missing/zero/multiple calls from malformed call fields, and failed runs retain their lifecycle logs.

The next user-run live attempt passed against `deepseek-flash`: all four sessions completed, with 16 model requests, 17,932 input tokens, and 2,927 output tokens. Saved evidence verified three parent interactions and nine successful file reads. The initial request attempts started within 10 ms, before any returned, demonstrating overlapping independent-session execution. See the [live evidence and review-quality assessment](../knowledge-base/deepseek-swarm-live-validation.md). This single successful run does not establish reliable formatting or flawless review content.
