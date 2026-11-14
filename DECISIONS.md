# Decisions

## Owner decisions
- License: dual MIT or Apache-2.0. Ports 7878 (app) and 7879 (preview). Preact with signals. Manual update check only. Defaults from section 19 stand.
- No OS keyring support in v1 (personal home use). Master key lookup will live in one function (file with 0600, or `PATINA_MASTER_KEY`) so a keyring source can be added later.

## M0 choices
- Rust edition 2024, resolver 3. Crates `patina-providers` and `patina-tools` are created in M1 and M2 instead of as empty stubs.
- `ui/dist/index.html` is a committed placeholder so the server compiles. M1 replaces it with the Vite build and gitignores the build output.
- Non-loopback binds are refused until auth exists (M1), to honor section 13 early.
- `--port 0` is allowed, used by the budget script and tests.

## Dependencies (size effect measured on the release binary, 3.0 MB at M0)
- tokio: async runtime, required by the stack.
- axum (http1 only) and tower-http (gzip only): HTTP server and compression.
- rusqlite (bundled): SQLite, about 1 MB of the binary.
- rust-embed: embeds the UI. Negligible.
- clap (derive): CLI. Small.
- tracing and tracing-subscriber (fmt, env-filter, json): logging.
- serde and toml: config files.
- directories: platform config and data paths.
- thiserror and anyhow: error types.
- tempfile (dev only): test folders.

## M1 choices
- **Run without a team:** teams arrive in M5, so `POST /runs` takes `agent_id`. The `team_id` column stores `agent:<id>` until then.
- **One model turn per run** in M1. The loop gains tools in M2 and steering messages in M4.
- **Tool calls from genai arrive whole** at the end of a turn (genai's `capture_tool_calls`), and are emitted as start, one args delta, end. The trait still supports incremental deltas. Revisit if live tool-argument streaming matters.
- **Reasoning parts** are streamed as `reasoning` events but not yet replayed to the provider. Replay is needed only once tool turns exist (M2).
- **Model listing uses plain HTTP**, not genai: genai's list is a static table for most providers.
- **Retries live in the agent loop**, not the provider, so each retry is recorded in the event log. A failed attempt's text stays in the log, tagged with its attempt number, and the UI hides it.
- **`Secure` cookie flag** is set from `X-Forwarded-Proto: https`, since there is no built-in TLS.
- **Origin check** compares the `Origin` authority with the `Host` header, which is already checked against `allowed_hosts`.
- **Minimum password length** is 10 characters.
- **Markdown rendering is deferred to M6.** Model output is shown as plain text until then, which section 13 allows.
- **Extra route** `GET /provider-presets` serves the editable-preset list to the UI.
- **`ui/dist` is not committed.** `patina-server/build.rs` writes a "UI not built" page if it is missing, so `cargo build` works on a fresh clone.
- **On shutdown, running runs are cancelled.** Marking them `interrupted` and resuming is M4.
- **Database access** goes through one writer thread that owns the only connection; reads use it too. Simple, and fast enough for one owner.

## M1 dependencies (stripped release binary: 8.7 MB, up from 3.0 MB)
- genai 0.7: provider protocols. With reqwest and rustls (aws-lc-rs), the bulk of the 5.7 MB growth.
- reqwest 0.13 (rustls): already pulled in by genai; used directly for model listing.
- argon2, chacha20poly1305, sha2, getrandom: password hashing, secret encryption, token hashing, randomness. Small.
- async-trait, futures, tokio-util: the provider trait, streams, cancellation. Small.
- serde_json, uuid (v7): payloads and time-sorted ids. Small.
- ts-rs: generates the UI's API types. Derive only; no measurable size.
- UI: preact, @preact/signals. Dev only: vite, typescript, vitest, @playwright/test, @types/node.

## Open risks
- rustls uses aws-lc-rs, which needs a C toolchain when cross-compiling. Check static musl builds early in M8, and switch to the ring provider if needed.
- Live checks against real hosted providers have not been run (they need the owner's keys).
