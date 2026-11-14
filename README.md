# Patina

A lean, self-hosted, model-agnostic agent workbench in one Rust binary. The daemon serves a browser UI, holds your API keys, runs agent loops in sandboxed git worktrees and stores every run so you can watch, pause and review it from any device.

Status: **M0 (skeleton)**. `patina serve` starts, creates its folders and database, and serves a placeholder page and `/healthz`. See `DECISIONS.md` for the milestone plan and choices.

## Build and run

```sh
cargo build --release
./target/release/patina serve          # http://127.0.0.1:7878
./target/release/patina serve --port 9000 --config-dir ./c --data-dir ./d
```

Config lives in `~/.config/patina/` and data in `~/.local/share/patina/` unless overridden.

## Checks

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all
./scripts/check-budgets.sh   # release size and idle memory budgets (needs a release build)
```

Licensed under MIT or Apache-2.0, at your option.
