# DeepSeek swarm live validation

The user ran `python examples/swarm-demo.py` after rebuilding with commit `8f86109`. All four sessions completed against `https://api.deepseek.com`, model `deepseek-flash`, using the example's non-thinking profile. The agent inspected the saved fixture, synthesis, `tool-evidence.json`, `summary.json`, and read-only SQLite events at `/tmp/asyntalc-swarm-ga3c6q41`. This temporary directory is supporting local evidence, not a durable repository artifact.

| Session | Requests | Input tokens | Output tokens |
|---|---:|---:|---:|
| architecture | 5 | 5232 | 763 |
| concurrency | 5 | 5355 | 786 |
| test-coverage | 5 | 5302 | 574 |
| synthesis | 1 | 2043 | 804 |
| Total | 16 | 17932 | 2927 |

Each reviewer asked a parent question, received the configured priority answer, and successfully read `README.md`, `counter.py`, and `test_counter.py` in separate calls. The script recovered handles with `list`, collected final results through the CLI, and submitted the three reviews to a separate synthesis session. The first three `run.model_requested` timestamps were 1790397670618, 1790397670623, and 1790397670628; the earliest response was recorded at 1790397671354. Thus local request attempts overlapped across independent sessions. The span from first submission to synthesis completion was 10.398 seconds, including scripted parent handling; this is an observation, not a performance benchmark.

Busy-session rejection and queued/active cancellation passed in the separate fake-daemon probe. They were not exercised against live DeepSeek requests in this run. No fixture code was executed or modified by the subagents.

## Review quality

The consolidated review correctly identifies the intended unsynchronized read-modify-write race, in-place file truncation risk, and missing concurrent/persisted-state test coverage. Its top actions—serialize the update, replace files atomically, and improve concurrency tests—address the fixture's intended issues, subject to defining the process and durability requirements.

One finding is unsupported as stated: evaluating `count + 1` for writing and again for returning does not itself make the values differ, because both use the same unchanged local `count`. Other writers can change the persisted file, but computing once into a local does not solve that shared-state race. Several line references are also off by a line; the read, write, and return in the actual fixture are lines 6, 7, and 8. A stress test with many threads is useful but does not guarantee a deterministic race reproduction without controlled interleaving.

Successful orchestration and reviewer agreement do not prove semantic correctness. The parent should verify findings against source before turning them into patches; the synthesis model was instructed to use the supplied reviews, not independently inspect the files.

## Earlier failure and remaining scope

The preceding attempt at `/tmp/asyntalc-swarm-kue2sess` failed on architecture's first post-resume response with generic `invalid_tool_call`. No rejected payload was retained, so the exact cause remains unknown. Explicit sequential-read prompting and clearer static parser diagnostics were added; the next attempt passed. Do not infer that the earlier failure was definitively caused by multiple calls or that prompt instructions guarantee protocol compliance.

The parent-managed swarm demonstration is validated for this fixture and run. Tool inspection through bounded CLI output remains the next proposed task. Thinking-mode replay, real repository scale, live restart recovery, file writes, and shell sandboxing remain separate work.
