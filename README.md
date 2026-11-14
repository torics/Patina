# Patina

A lean, self-hosted, model-agnostic agent workbench in one Rust binary. The daemon serves a browser UI, holds your API keys, calls any LLM provider on your behalf and stores every run so you can watch and review it from any device.

Status: **M1 (auth and chat)**. You can set a password, add providers and agents, chat with streaming, cancel, and replay any run after a reload. Tools, worktrees and the sandbox arrive in M2 and M3. See `DECISIONS.md`.

## Build and run

Building needs Rust and Node. Running the binary needs neither.

```sh
cargo xtask ui            # export API types, build ui/dist
cargo build --release
./target/release/patina serve
```

The first run prints the address (`http://127.0.0.1:7878`) and a one-time setup token. Open the address, enter the token, choose a password, then add a provider and an agent in Settings.

Other commands: `patina reset-password` prints a new setup token, `patina version` prints the version. `serve` accepts `--bind`, `--port`, `--config-dir`, `--data-dir` and `--log-format human|json`.

Config lives in `~/.config/patina/` and data in `~/.local/share/patina/` unless overridden. See `docs/config.md`.

## Checks

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all
(cd ui && npm test && npx playwright test --trace off)   # reducer tests, browser smoke test
./scripts/check-budgets.sh                                # binary size, idle memory, UI size
```

Live provider smoke tests are opt-in and need your keys. The variables are listed at the top of `crates/patina-providers/tests/live.rs`.

## Development

Run `patina serve` and, in `ui/`, `npm run dev`. The Vite dev server proxies `/api` to the daemon.

Licensed under MIT or Apache-2.0, at your option.
