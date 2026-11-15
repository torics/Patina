# Patina

A lean, self-hosted, model-agnostic agent workbench in one Rust binary. The daemon serves a browser UI, holds your API keys, calls any LLM provider on your behalf and stores every run so you can watch and review it from any device.

Status: **M6 (workbench)**. You can set a password, add providers and agents, and chat with streaming. An agent can work in a project: it reads, searches and edits files and runs commands in its own git worktree. Commands run in a sandbox (Landlock on Linux, or a container), each changing step is a checkpoint you can roll back to, and nothing reaches your branch until you merge. Runs can be paused, resumed and steered with messages, survive a restart of the daemon, and keep long histories inside the model's context. Teams run several agents, each on its own provider and model, from three built-in templates or your own. The workbench shows a project's files, an editor, each run's changes as diffs, and a live preview of the run's dev server, and it works from a phone. MCP arrives in the next milestone. See `DECISIONS.md`.

## Build and run

Building needs Rust and Node. Running the binary needs neither.

```sh
cargo xtask ui            # export API types, build ui/dist
cargo build --release
./target/release/patina serve
```

The first run prints the address (`http://127.0.0.1:7878`) and a one-time setup token. Open the address, enter the token, choose a password, then add a provider and an agent in Settings.

`serve` also starts the preview listener on `127.0.0.1:7879` (see [The workbench](#the-workbench)).

Other commands: `patina doctor` checks git, the sandbox, the container runtime, ports, config files and leftover worktrees; `patina reset-password` prints a new setup token; `patina version` prints the version. `serve` accepts `--bind`, `--port`, `--preview-port`, `--config-dir`, `--data-dir` and `--log-format human|json`.

## Work in a project

A project is a folder that is the top level of a git repository. Add one in Settings, give an agent tools, then pick the project with the switcher at the top. The run gets a worktree under the data folder on a new branch `patina/run-<id>`, based on your current commit. Uncommitted changes in your checkout are not included.

What asks for your approval depends on the project's policy: every edit and command (`confirm_all`), commands only (`auto_edit`, the default), or nothing (`autopilot`). An edit shows its diff, a command shows its command line, and you answer Approve, Deny (with a reason the agent sees) or Approve with changed arguments. For a command you can also choose "Approve and always allow", which adds a pattern such as `cargo test *` to the project's allowlist.

While a run works you can **message the agent**: it reads your message before its next step. **Pause** stops the run after the step in progress, **Resume** continues it from where it was, and sending a message to a paused run resumes it. An agent that makes the same tool call with the same arguments three times in a row is paused for you, with a note saying so. A run stopped by a limit resumes once you raise the limit under "Change limits". **Stop** ends a run for good.

If the daemon stops (a clean shutdown, a crash, a reboot), a run in progress is marked *interrupted* and anything its commands left running is stopped at the next start. Resume it: the agent continues from its stored context, and a tool call that was cut off is reported to it as interrupted, not repeated. A run that was waiting for your approval keeps waiting, and answering resumes it.

When a run's history grows past 70% of the agent's `context_window`, Patina first replaces old large tool outputs with a pointer to a saved copy, then has the same model summarize the oldest turns, keeping the latest ten word for word. The timeline notes each time it does.

## Teams

A team runs several agents on one prompt, each with its own provider and model. They share the run's limits, and each keeps its own history: agents see only each other's final messages, never their transcripts.

| Mode | What happens |
| --- | --- |
| `single` | One agent. |
| `sequential` | Members run in order. Each gets your prompt and the final message of every member before it. |
| `parallel` | Members answer at once and cannot change files, then the synthesizer gets every answer and replies. Your messages reach the synthesizer. |
| `delegate` | The first member leads. It gets a `delegate` tool to hand a task to another member (or to the agents its `can_delegate_to` names). The helper starts with a fresh context, sees only the task, and returns only its final message. A helper may delegate once more, no deeper. |

In Settings, **Team from a template** makes one of three teams: *Solo builder*, *Plan, build, review*, and *Compare models* (two agents on different models and a judge). You pick the provider and model; a template names none. Teams can also be made by hand, exported as one TOML file with their agents (it holds no secret), and imported on another machine, with a provider of your choice for agents whose provider does not exist there. In the composer, pick the team instead of an agent. `patina run --team <id>` runs a team, or a single agent when no team has that id.

A helper's work is shown inside the `delegate` call that started it. Pausing a team stops every agent; resuming continues each one where it stopped, and agents that had finished are not run again.

When the run stops, the Changes tab shows the diff and offers: merge into your branch (your checkout must be clean), download a patch, keep the branch, or discard. Patina never pushes.

## The workbench

On a wide screen the workbench has three panes. On the left are your runs and the project's files. The center holds the prompt box, or the timeline of the open run. On the right are three tabs:

- **Changes**: the files the run changed, each with an inline diff, the whole patch, checkpoints with rollback, and merge, keep or discard.
- **Editor**: open a file from the tree, edit it and save with the button or Ctrl+S. The tree shows your checkout, or the run's copy when a run is open. A file in the run's copy can be saved only while the run is paused or stopped; the save is committed to the run's branch, so the Changes tab and a merge include it, and the agent is told before its next step.
- **Preview**: the run's dev server, in a sandboxed frame on its own origin. Each run has its own port, given to its commands as `PORT`. Set `dev_command` on the project (for example `npm run dev -- --host 127.0.0.1 --port {port} --strictPort`) to start and stop it from this tab, or ask the agent to start one on `$PORT`. The frame reloads after every step that changes files. Hot reload over WebSocket does not pass through.

In the prompt box, `@` suggests files from the project. Each file you mention goes with the prompt, read from your checkout. The header shows the open run's status, sandbox and usage against its limits, with Pause, Stop, Resume and Export (Markdown or JSON), and how many approvals wait across all runs. The theme is dark; Light is one click away.

On a phone the panes become tabs (Run, Browse, Changes, Editor, Preview), with the run first, so a run can be started, steered, approved and merged by touch.

To use Patina from another device, put `tailscale serve` in front of the app port and, for the preview, the preview port (for example `tailscale serve --https=8443 http://127.0.0.1:7879`), then set `preview_origin` in `config.toml` to that address.

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

It prints the run's events and asks for each approval on the terminal. `--team` takes an agent id until teams arrive. Limits: `--max-steps`, `--max-tokens`, `--max-cost-usd`, `--max-wall-clock-s`. `--json` prints one event per line. `--yes` approves everything and is accepted only when the project has a sandbox on this machine. Exit codes: 0 completed, 1 failed, 3 an approval was needed and no terminal was attached, 4 a limit was reached, 5 paused (the agent repeated itself; resume it in the web UI), 130 stopped.

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
