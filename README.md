# Patina

A lean, self-hosted, model-agnostic agent workbench in one Rust binary. The daemon serves a browser UI, holds your API keys, calls any LLM provider on your behalf and stores every run so you can watch and review it from any device.

Status: **M2 (tools and worktrees)**. You can set a password, add providers and agents, and chat with streaming. An agent can also work in a project: it reads, searches and edits files in its own git worktree, every edit waits for your approval with a diff, each changing step is a checkpoint you can roll back to, and nothing reaches your branch until you merge. Commands and the sandbox arrive in M3. See `DECISIONS.md`.

## Build and run

Building needs Rust and Node. Running the binary needs neither.

```sh
cargo xtask ui            # export API types, build ui/dist
cargo build --release
./target/release/patina serve
```

The first run prints the address (`http://127.0.0.1:7878`) and a one-time setup token. Open the address, enter the token, choose a password, then add a provider and an agent in Settings.

Other commands: `patina reset-password` prints a new setup token, `patina version` prints the version. `serve` accepts `--bind`, `--port`, `--config-dir`, `--data-dir` and `--log-format human|json`.

## Work in a project

A project is a folder that is the top level of a git repository. Add one in Settings, give an agent tools, then pick the project in the composer. The run gets a worktree under the data folder on a new branch `patina/run-<id>`, based on your current commit. Uncommitted changes in your checkout are not included.

Every edit shows its diff and waits for Approve, Deny (with a reason the agent sees) or Approve with changed arguments. When the run stops, the Changes section shows the diff and offers: merge into your branch (your checkout must be clean), download a patch, keep the branch, or discard. Patina never pushes.

The same from a terminal, without the daemon:

```sh
patina run --project ~/code/my-project --team assistant "Add a section about testing to the README"
```

It prints the run's events and asks for each approval on the terminal. `--team` takes an agent id until teams arrive. Limits: `--max-steps`, `--max-tokens`, `--max-cost-usd`, `--max-wall-clock-s`. `--json` prints one event per line. `--yes` is refused until a sandbox exists. Exit codes: 0 completed, 1 failed, 3 an approval was needed and no terminal was attached, 4 a limit was reached, 130 stopped.

Config lives in `~/.config/patina/` and data in `~/.local/share/patina/` unless overridden. See `docs/config.md`.

## Checks

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all
(cd ui && npm test && npx playwright test --trace off)   # reducer tests, browser smoke test
./scripts/check-budgets.sh                                # binary size, idle memory, UI size
```

Live provider tests are opt-in and need your keys. They stream a reply, then make a tool call and use its result. The variables are listed at the top of `crates/patina-providers/tests/live.rs`.

## Development

Run `patina serve` and, in `ui/`, `npm run dev`. The Vite dev server proxies `/api` to the daemon.

Licensed under MIT or Apache-2.0, at your option.
