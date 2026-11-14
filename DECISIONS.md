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

## M2 choices
- **`StreamEvent::ReasoningBlock` added to the provider trait.** The brief's trait has no way to carry a provider's opaque reasoning block, and section 8 requires replaying those blocks verbatim next to tool calls. The genai provider emits them and replays them as thought signatures. Verified against the mock and the local endpoint only; the hosted providers need the owner's keys.
- **Own JSON-Schema subset validator** (`patina-core::tool::validate`: object, required, type, enum, array items, no unknown properties) instead of a schema crate. The built-in tools need nothing more, and it keeps the binary small. MCP tool schemas (M7) may need more.
- **Tool calls in one turn run one after another.** The brief allows reads to run concurrently; sequential is simpler and keeps the log in call order. Revisit if it is slow.
- **`.git` is out of reach** for every file tool, in addition to path confinement, so an agent cannot rewrite history or hooks.
- **Patina's own git commands disable hooks and commit signing** (`core.hooksPath=/dev/null`, `commit.gpgsign=false`), so a checkpoint never runs repository code or asks for a passphrase.
- **Without a sandbox, every edit asks**, whatever the project's policy says. There is no sandbox before M3, so M2 behaves as `confirm_all` for every project. Reads never ask.
- **A write to a path outside the worktree is asked about before it is refused.** The approval comes first in the loop; the tool then refuses the path. Nothing is written either way.
- **Rollback, merge and discard need an idle run.** They return 409 while an agent is working in the worktree.
- **Rollback to step 0** means the base commit, which undoes everything.
- **Patch export** is `GET /runs/:id/diff?format=patch`, not a separate route.
- **`runs.disposition`** (migration 3) records what happened to a run's changes: `merged`, `kept` or `discarded`. Merging removes the worktree and the run branch; keeping removes only the worktree.
- **The event stream ends when the run is inactive and its log is sent**, not at `run_finished`, so a rollback made after the run is replayed too.
- **Text and reasoning events carry `step`**, so the UI can order text around tool calls and discard only the failed attempt of that step.
- **Checkpoint summaries** are the first line of each changing tool's result, for example `created NOTES.md (21 bytes)`.
- **`patina run --team <id>` names an agent** until teams exist (M5); `--agent` is an alias. It runs the engine in its own process, without the daemon, on the same config and data folders. A folder that is not a registered project is registered with the default policy; it never runs `git init`.
- **`patina run` approvals need a terminal.** Answers are read only from an interactive terminal, so `yes | patina run` cannot approve. Without a terminal, a run that needs an approval is stopped (exit code 3). `--yes` is refused until a sandbox exists (M3). Other exit codes: 0 completed, 1 failed, 4 budget reached, 130 stopped.
- **A daemon and a `patina run` can share the data folder**, but each knows only its own live runs: the daemon's UI shows a terminal run's log, and cannot stop it or answer its approvals.
- **Agent `tools` are checked on save** against the built-in tool names.
- **Deferred, with their milestone:** resuming after raising a budget and answering an approval after a daemon restart (M4, with pause and recovery; until then a stopped run stays stopped and such an answer gets 409); "approve and add an allowlist pattern" (M3, with commands); `worktree_copy` and `worktree_setup` (M3: setup must run in the sandbox); project tree and file routes (M6).

## M2 dependencies (stripped release binary: 10.2 MB, up from 8.7 MB; idle memory 9.4 MB; UI 19.6 KB gzipped)
- ignore: file walking that respects `.gitignore`, for `list_files` and `search`. Named by the brief.
- regex: `search` patterns. Named by the brief; already in the tree through `ignore`.
- similar: unified diffs for edit approvals. Named by the brief. Small.
- Test only: util-linux `script` gives the `patina run` test a real terminal (Linux only).

## Open risks
- rustls uses aws-lc-rs, which needs a C toolchain when cross-compiling. Check static musl builds early in M8, and switch to the ring provider if needed.
- Live checks against real hosted providers have not been run (they need the owner's keys). This now includes tool calls and reasoning replay on those providers.
- A run that waits for approval when the daemon restarts cannot continue until M4 adds recovery.
