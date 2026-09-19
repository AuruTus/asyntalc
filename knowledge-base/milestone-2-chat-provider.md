# Milestone 2: Chat Completions and session history

Date: 2026-09-19. Binary version: 0.1.1. Status: implemented; local validation complete; live DeepSeek check pending.

## User decision and next-session handoff

The user approved the next version after milestone 1 and selected `https://api.deepseek.com` for the first live check. They will end this session and export a temporary key before continuing. Do not look for or print secret values. The prepared configuration names `DEEPSEEK_API_KEY`; it contains no credential.

`examples/deepseek.toml` selects `deepseek-flash`, based on the current official API reference, with `max_tokens`, system instructions, and `reasoning_effort = "none"`. The user specified DeepSeek generally, not a particular model identifier; this model choice is an implementation default and remains configurable. Account access and live behavior are unverified.

Resume by confirming the key is available to the process environment without exposing its value, then run:

```bash
export ASYNTALC_LIVE_CONFIG="$PWD/examples/deepseek.toml"
cargo test --locked --test client_daemon chat_provider::live_chat_smoke -- --ignored --exact
```

This explicitly selected ignored test makes one billable request, checks successful retrieval, and removes its temporary database. It expects the final text `asyntalc smoke ok`. If it fails, inspect the typed error, correct configuration/compatibility if needed, and record the actual result. Do not silently retry an uncertain paid request. The test caps its own wait at 150 seconds; the example provider timeout is 120 seconds. A network-restricted execution environment may require escalation.

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

The opt-in live test is ignored by default. Local HTTP validation does not establish DeepSeek account access, model quality, HTTPS connectivity, or provider latency. Update this file and prototype-plan section 12 with live results in the next session.

Final local checks with Rust 1.98.1: `cargo clippy --locked --all-targets -- -D warnings` and `cargo fmt --all -- --check` passed. The integration suite contains 18 passing local tests and 1 ignored live test. Documentation JSON examples, local file links, and both example TOML files were checked programmatically. The configuration enum uses a boxed Chat profile to keep the enum small; this does not change its persisted JSON representation.

Official schema references used during implementation: [OpenAI Chat Completions](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create) and [DeepSeek Chat Completions](https://api-docs.deepseek.com/api/create-chat-completion/).
