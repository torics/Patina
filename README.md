# Patina

A lean, self-hosted, model-agnostic agent workbench in one Rust binary. The daemon serves a browser UI, holds your API keys, calls any LLM provider on your behalf and stores every run so you can watch and review it from any device.

Status: **M3 (sandbox and commands)**. You can set a password, add providers and agents, and chat with streaming. An agent can work in a project: it reads, searches and edits files and runs commands in its own git worktree. Commands run in a sandbox (Landlock on Linux, or a container), each changing step is a checkpoint you can roll back to, and nothing reaches your branch until you merge. Pause and resume, teams, the file workbench and MCP arrive in later milestones. See `DECISIONS.md`.

## Build and run

Building needs Rust and Node. Running the binary needs neither.

```sh
cargo xtask ui            # export API types, build ui/dist
cargo build --release
./target/release/patina serve
```

The first run prints the address (`http://127.0.0.1:7878`) and a one-time setup token. Open the address, enter the token, choose a password, then add a provider and an agent in Settings.

Other commands: `patina doctor` checks git, the sandbox, the container runtime, ports, config files and leftover worktrees; `patina reset-password` prints a new setup token; `patina version` prints the version. `serve` accepts `--bind`, `--port`, `--config-dir`, `--data-dir` and `--log-format human|json`.

## Work in a project

A project is a folder that is the top level of a git repository. Add one in Settings, give an agent tools, then pick the project in the composer. The run gets a worktree under the data folder on a new branch `patina/run-<id>`, based on your current commit. Uncommitted changes in your checkout are not included.

What asks for your approval depends on the project's policy: every edit and command (`confirm_all`), commands only (`auto_edit`, the default), or nothing (`autopilot`). An edit shows its diff, a command shows its command line, and you answer Approve, Deny (with a reason the agent sees) or Approve with changed arguments. For a command you can also choose "Approve and always allow", which adds a pattern such as `cargo test *` to the project's allowlist.

When the run stops, the Changes section shows the diff and offers: merge into your branch (your checkout must be clean), download a patch, keep the branch, or discard. Patina never pushes.

## Commands and the sandbox

An agent that has the `run_command` tool can run shell commands in its worktree, in the foreground with a timeout (120 seconds by default, 600 at most) or in the background. Every command runs in the run's sandbox:

- **Landlock** (Linux 5.13 or later): commands read the system and the toolchain, and write only to the worktree and to a home and temp folder of their own. They cannot read your home folder or Patina's data.
- **Container** (Docker or Podman): set `container_image` on the project. Commands see only the worktree.
- **None**: on a host with neither, every edit and command asks first, whatever the policy says.

`patina doctor` and the Settings page show which one your machine has and what it does not cover. Read `SECURITY.md` before you choose `autopilot`.

A toolchain outside the system folders must be opened to the sandbox. For Rust installed with rustup, in the project's "Sandbox and commands" settings or in `projects.toml`:

```toml
sandbox_read = ["/home/you/.rustup", "/home/you/.cargo"]
[project.env]
PATH = "/home/you/.cargo/bin:/usr/local/bin:/usr/bin:/bin"
RUSTUP_HOME = "/home/you/.rustup"
```

`RUSTUP_HOME` is needed because a command's `HOME` is a private folder, not yours.

`docs/config.md` lists every project setting, including `worktree_copy` (bring `node_modules` or a local config file into each worktree) and `worktree_setup` (a command such as `npm ci` that prepares each new worktree).

The same from a terminal, without the daemon:

```sh
patina run --project ~/code/my-project --team assistant "Add a section about testing to the README"
```

It prints the run's events and asks for each approval on the terminal. `--team` takes an agent id until teams arrive. Limits: `--max-steps`, `--max-tokens`, `--max-cost-usd`, `--max-wall-clock-s`. `--json` prints one event per line. `--yes` approves everything and is accepted only when the project has a sandbox on this machine. Exit codes: 0 completed, 1 failed, 3 an approval was needed and no terminal was attached, 4 a limit was reached, 130 stopped.

Config lives in `~/.config/patina/` and data in `~/.local/share/patina/` unless overridden. See `docs/config.md`.

## Checks

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all
(cd ui && npm test && npx playwright test --trace off)   # reducer and policy tests, browser smoke tests
./scripts/check-budgets.sh                                # binary size, idle memory, UI size
```

The sandbox tests in `crates/patina/tests/sandbox.rs` drive the real binary with a scripted model that tries to get out. They need Landlock; on a host without it they print a notice, unless `PATINA_REQUIRE_LANDLOCK=1` is set, as in CI. The container test runs with `PATINA_TEST_CONTAINER=1` and needs a running Docker or Podman with the `alpine:3` image.

Live provider tests are opt-in and need your keys. They stream a reply, then make a tool call and use its result. The variables are listed at the top of `crates/patina-providers/tests/live.rs`.

## Development

Run `patina serve` and, in `ui/`, `npm run dev`. The Vite dev server proxies `/api` to the daemon.

Licensed under MIT or Apache-2.0, at your option.
