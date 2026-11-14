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
