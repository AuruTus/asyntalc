# Milestone 2: Chat Completions and session history

Date: 2026-09-19. Binary version: 0.1.1. Status: implemented; local validation and one-request live DeepSeek check passed.

## User decision and next-session handoff

The user approved the next version after milestone 1 and selected `https://api.deepseek.com` for the first live check. They subsequently exported the temporary key and authorized its use. The live smoke test passed; do not repeat paid requests merely to reconfirm the same result. The configuration names `DEEPSEEK_API_KEY`; it contains no credential, and the key value must not be printed.

`examples/deepseek.toml` selects `deepseek-flash`, based on the current official API reference, with `max_tokens`, system instructions, and `reasoning_effort = "none"`. The user specified DeepSeek generally, not a particular model identifier; this model choice is an implementation default and remains configurable. Basic account access and request/result behavior were verified by the smoke test; live multi-turn behavior has not been tested.

The successful live invocation was:

```bash
export ASYNTALC_LIVE_CONFIG="$PWD/examples/deepseek.toml"
cargo test --locked --test client_daemon chat_provider::live_chat_smoke -- --ignored --exact
```

This explicitly selected ignored test made one billable request, checked the retrieved final text `asyntalc smoke ok`, and removed its temporary database. It reported 1 passed, 0 failed, in 0.64 seconds. That single observation is not a latency benchmark. The test remains ignored by default and needs network/socket escalation in the current execution environment. Its own wait cap is 150 seconds; the example provider timeout is 120 seconds.

## Implementation decisions

- `daemon --config FILE` selects Chat Completions; `daemon --runner fake` preserves the previous offline workflow. They are mutually exclusive. One profile per daemon; no automatic config discovery or per-run model overrides.
- `src/config.rs` validates a bounded TOML profile. Raw keys and unknown configuration fields are rejected; diagnostics do not quote potentially secret source lines.
- `src/provider.rs` uses reqwest with Rustls, one non-streaming response, explicit limits, no automatic retries, and no redirects. Optional fields are sent only when configured. HTTP failure bodies/headers are never persisted or printed.
- Sessions bind the full non-secret configuration, including limits and the key-variable name. Reuse under another profile fails. Queued work prevents startup with a mismatched profile. Credential values are loaded at daemon startup and are not persisted.
- The store checks history size before loading it, selects completed earlier turns in session order, and excludes queued, failed, and interrupted turns. There is no automatic history truncation/summarization.
- Schema 2 is an embedded, transactional upgrade from schema 1. Legacy sessions remain fake profiles. New fields store usage, finish reasons, error messages, and bounded partial output. Older binaries reject the upgraded database.
- The request-initiation event is persisted before sending HTTP; usage counters are not a provider billing guarantee. Missing usage is `null`. Only valid final assistant text with `stop` is successful. `length` or a decoded output bound produces `output_limit` with separately labeled partial text.
- Execution is still global FIFO with one worker. No concurrency, durable deadlines, cancellation, idempotency, parent questions, tool calls, or sandboxing was added.

## Validation

Baseline: all 8 milestone 1 integration tests remain unchanged except for adding the new test module. The new tests use a localhost HTTP server with controlled response gates, fixed prompts, and a dummy credential. They inspect exact request messages and state transitions rather than infer correctness from latency. No performance claim or algorithm ablation is made.

Coverage includes successful request serialization, same-session history and restart, session isolation, configuration conflicts, interrupted-request recovery without replay, profile protection for queued work, v1 database upgrade, authentication/rate-limit/service/request errors, malformed response, refusal/filtering, unsupported tools, disconnect, timeout, redirect rejection, missing/invalid usage, context budgets, response-body limits including chunked delivery, UTF-8-safe partial output, and the checked-in DeepSeek request settings.

The opt-in live test is ignored by default. Local HTTP validation covers the edge cases; the separate live run establishes the basic HTTPS request/result path for the configured DeepSeek account/model. Neither establishes model quality, provider performance, or all real-provider error cases.

Final local checks with Rust 1.98.1: `cargo clippy --locked --all-targets -- -D warnings` and `cargo fmt --all -- --check` passed. The integration suite contains 18 passing local tests and 1 ignored live test. Documentation JSON examples, local file links, and both example TOML files were checked programmatically. The configuration enum uses a boxed Chat profile to keep the enum small; this does not change its persisted JSON representation.

Official schema references used during implementation: [OpenAI Chat Completions](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create) and [DeepSeek Chat Completions](https://api-docs.deepseek.com/api/create-chat-completion/).

## Configuration and compiler follow-up

The user asked whether the verbose config was safe and reported dependency incompatibilities after upgrading Rust. The example was shortened by omitting fields equal to validated defaults; its effective settings are unchanged. README now documents required fields, trusted endpoint configuration, environment-variable limitations, sensitive instructions, and unencrypted SQLite persistence.

The active toolchain reports Rust/Cargo 1.98.1. `cargo check --locked --all-targets` and Clippy with warnings denied both passed. `cargo report future-incompatibilities` reported that no reports are available; this command's nonzero exit is not a dependency compilation error. No incompatible package was reproduced and no dependency versions were changed. After shortening the example config, all 18 local tests passed again; the live test was not repeated.

The user clarified the problem as a rust-analyzer diagnostic on the Serde derive in `src/protocol.rs`: the proc-macro expander expected `rustc 1.92.0 (ded5c06cf 2025-12-08)` but the library was built with `rustc 1.98.1 (48a229cea 2026-09-01)`. This identifies an editor proc-macro host/library ABI mismatch, not a Cargo dependency compilation failure. The rustup-managed analyzer reports 1.98.1; the installed remote VS Code extension includes `rust-analyzer 0.3.3049-standalone`. No relevant server-path override was found in the checked workspace/remote-machine settings. Other editor-side settings or stale running hosts are not ruled out.

The user was advised to run `rust-analyzer: Restart server`, then `Developer: Reload Window` if needed. No dependencies were changed to hide this diagnostic. Confirmation that the active editor cleared it must come from the editor; a successful Cargo check alone cannot establish that.

A separate fresh `rust-analyzer diagnostics . --severity error` process using the installed rustup-managed analyzer scanned all 9 project/test source files and completed with no errors, including `src/protocol.rs`. This supports the stale or differently configured editor-host diagnosis without modifying dependencies or deleting build artifacts.
