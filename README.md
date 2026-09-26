# Asyntalc

A Rust prototype for running asynchronous subagent tasks through an OpenAI-compatible Chat API. A local daemon executes tasks and persists sessions in SQLite; the CLI submits work, waits for updates, and retrieves results.

Independent sessions run concurrently. Each session accepts one unfinished run at a time and carries successful conversation history into follow-up prompts. Optional tools let agents ask their parent for clarification and read a configured workspace. File writes and shell execution are not supported yet.

## Quick start

Requires Linux, Rust 1.89+, and a C toolchain for bundled SQLite. Workspace tools additionally require Linux 5.6+ and mounted procfs.

```bash
cargo build --locked
cp examples/provider.toml provider.toml
```

Edit `provider.toml` with your endpoint, model, and API-key environment-variable name. Set the actual key in the daemon's environment, not in TOML. A [DeepSeek profile](examples/deepseek.toml) is also provided.

Start the daemon in one terminal:

```bash
./target/debug/asyntalc --data-dir .asyntalc daemon --config provider.toml
```

Submit work from another terminal:

```bash
printf 'Compare two queue designs' |
  ./target/debug/asyntalc --data-dir .asyntalc submit --session review --input -

# Replace RUN_ID with the run_id returned by submit.
./target/debug/asyntalc --data-dir .asyntalc wait --run RUN_ID --timeout-ms 20000
./target/debug/asyntalc --data-dir .asyntalc result --run RUN_ID --output text
```

**Keep the daemon running and use the same data directory for every command.** A run ID alone cannot locate its database. Relative data paths resolve from each terminal's working directory.

`submit` returns as soon as acceptance is saved. If `wait` returns `wait_timeout`, wait again; the task continues running. Once the run finishes, submit another prompt with `--session review` to continue the conversation. Submitting while that session has unfinished work returns `session_busy`.

To try the lifecycle without an API key, start the daemon with `--runner fake` instead of `--config provider.toml`. The fake runner echoes the prompt without contacting a provider.

## CLI reference

Append these commands to `./target/debug/asyntalc --data-dir .asyntalc`:

| Command | Purpose |
|---|---|
| `scope` | Show the effective provider and workspace permissions |
| `list --limit 20` | Discover runs |
| `status --run RUN_ID` | Read the current state |
| `logs --run RUN_ID --limit 20` | Inspect lifecycle events |
| `tools --run RUN_ID --limit 20` | Inspect workspace call metadata, without file contents |
| `cancel --run RUN_ID` | Request cancellation |
| `resume --run RUN_ID --question QUESTION_ID --input answer.txt` | Answer a pending parent question |

Commands return JSON by default; `result --output text` returns the final answer. A successful command can describe a failed run, so check the returned `status`. Use `--help` on any command for its options.

Enable `ask_parent = true` under `[provider]` to allow clarification requests. When `wait` returns `input_required`, answer the supplied question ID with `resume`, then wait again. Configure a top-level `[workspace]` to allow read, list, and search operations; the [example configuration](examples/provider.toml) shows the settings. Workspace content may be sent to the configured provider and saved in local history. Use a new session after changing provider or workspace settings.

## Examples and documentation

- [Swarm demo](examples/swarm-demo.md): three concurrent reviewers, parent questions, and a synthesis run; includes live and no-API instructions.
- [Inspection demo](examples/inspect-demo.sh): run `bash examples/inspect-demo.sh` after building for a self-checking lifecycle demo without credentials.
- [Design](docs/asyntalc-design-v0.1.md): goals and original v0.1 design.
- [Prototype plan](docs/asyntalc-prototype-plan.md): implemented architecture, contracts, validation, and next steps.
- [Engineering notes](knowledge-base/): milestone handoffs and live-test evidence.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

The default tests use local daemons and mock HTTP servers; they require local sockets and make no external API calls. Billable live tests are ignored by default.
