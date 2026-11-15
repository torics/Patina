# Architecture

One Cargo workspace, one binary. The browser is a thin view over the daemon's API.

| Crate | Holds |
| --- | --- |
| `patina-core` | Config and file formats, the `ChatProvider`, `Tool`, `RunHost` and `Sandbox` traits, the agent loop, budgets, policy and the command allowlist, event and API types. No axum, SQLite or genai types. |
| `patina-tools` | The file tools and path confinement (`Workspace`), `run_command` and background processes (`Commands`, `process`), the sandbox backends and host detection (`sandbox`), worktree preparation (`setup`), and git through the `git` command line. |
| `patina-providers` | `GenaiProvider` (the `genai` crate), `MockProvider`, the `Guarded` wrapper (concurrency limit, idle timeout, cancel), model listing. |
| `patina-store` | SQLite: migrations, the writer thread, runs, events, messages, sessions, encrypted secrets, master key lookup. |
| `patina-server` | axum routes under `/api/v1`, auth and security middleware, the run engine, SSE, the embedded UI. |
| `patina` | The CLI binary that wires them together: `serve`, `run`, `doctor`, `reset-password`, `version`, and the hidden `sandbox-exec` helper. |
| `xtask` | Build helper: `cargo xtask ui`. Not part of the binary. |

## One run

1. `POST /api/v1/runs` (or `patina run`) calls `engine::start_run`. With a project, it first settles the sandbox: the backend the project asks for is resolved against what the host supports, and a backend that is missing refuses the run. Then it creates the worktree `worktrees/<run id>` on branch `patina/run-<id>` from the project's `HEAD`, and gives the agent the tools named in its config, bound to that worktree and that sandbox. Then it spawns a tokio task.
1. The task logs `run_started` with the sandbox level, then prepares the worktree: it copies the project's `worktree_copy` entries and runs `worktree_setup` inside the sandbox. Both are logged as tool calls that belong to no agent, and a failure fails the run.
2. The task calls `run_agent` in core. Each step: check cancel and budgets, stream one model turn (with retries), save the assistant message, then handle the tool calls in order.
3. For each call: log `tool_call`, validate the arguments against the tool's schema, ask the policy. If the policy says ask, the host stores an approval row, logs `approval_requested`, sets the run to `waiting_approval` and waits for the owner's answer. Then the tool runs, and `tool_result` is logged. A call always gets a result, even when denied or cancelled.
4. If the step changed files, the host commits the worktree as a checkpoint and logs `checkpoint`. A command counts as a change; the checkpoint finds out whether anything differs.
1. When the loop ends, however it ends, the run's background processes are stopped and its sandbox is released.
5. Everything the loop does to the outside goes through the `RunHost` trait: events are written to SQLite first, then broadcast. Text deltas are broadcast live, and persisted in batches every 250 ms or 2 KB.
6. `GET /runs/:id/events` subscribes to the broadcast, replays the log after `after` or `Last-Event-ID`, then follows live. Events are de-duplicated by `seq`, so there is no gap and no duplicate. The stream ends once the run is inactive and its log is sent.

The event log is the single source of truth for the UI. Each agent's model context is stored separately in `messages`.

## Workspaces

`Workspace::resolve` is the only way a tool turns a model-supplied path into a real one: it refuses absolute paths, `..`, anything under `.git`, and any path whose real location (after resolving symlinks) is outside the worktree. `Workspace::read` and `Workspace::write` then open the file from the worktree root one name at a time and follow no link, because a background command could swap a folder for a link after the check. `edit_file` and `write_file` remember a hash of what the agent last read and refuse to overwrite a file that changed since.

## Sandbox

`patina_core::sandbox::Sandbox` is the contract: `prepare(command, marker)` turns a shell command into a program, arguments, environment and working folder; `kill(marker)` stops what a command left behind; `shutdown()` releases the sandbox. `patina-tools::sandbox` has three backends and `Caps::detect()`, which probes the Landlock ABI and looks for Docker or Podman once per process.

- **Landlock.** The policy is applied by the Patina binary itself: the command is started as `patina sandbox-exec --rw <folder> --ro <folder> ... -- /bin/sh -c <command>`. The helper restricts itself before any thread exists, drops every Linux capability, then replaces itself with the shell. Each right is requested for the ABI that introduced it, and a requested protection that cannot be enforced is an error, not a warning.
- **Container.** One container per run (`docker run -d --rm --init --cap-drop ALL ...`), started at the first command with the worktree mounted at `/workspace`. Commands are `docker exec`. The container is removed when the run ends.
- **None.** The command runs as the daemon's user with the same scrubbed environment. `RunCtx.sandboxed` is false, so the policy engine confirms every edit and command.

`process::run` starts each command in its own process group with a unique `PATINA_CMD` marker in its environment, captures at most 2 MB of output, and on timeout or cancel sends `SIGTERM`, waits two seconds, sends `SIGKILL`, then sweeps `/proc` for processes that carry the marker. `Commands` holds a run's background processes, at most eight at a time.

The policy engine is `patina_core::policy::decide(mode, kind, command_allowed, sandboxed)`. The allowlist lives on the project, and a pattern added while approving reaches the live run through a shared `Allowlist`.

## Git and the worktree

Git work goes through the `git` command line (`patina-tools::git`) with hooks, signing, the file-system monitor and submodule recursion disabled. The owner's checkout is touched only by an explicit merge, which needs a clean checkout and fast-forwards when it can.

A worktree is written by the agent's commands, so git is never allowed to trust it:

- Commands that need the worktree pass `--git-dir` (the repository's own record of the worktree) and `--work-tree`. Git never discovers the repository from inside the worktree.
- `git::checkpoint` uses plumbing: `ls-files` to find what changed, `update-index` to stage it, then `write-tree`, `commit-tree` and `update-ref`. `git add`, `status` and `commit` are not used on a worktree, because they start a second git inside nested repositories, which obeys that repository's config.
- Before a checkpoint or a rollback, the worktree's `.git` pointer is put back, and the git metadata of repositories a command created inside the worktree is removed, so their files are committed as ordinary files.
- Diffs and file lists are read from commits in the owner's repository.
- A worktree is removed by deleting its folder and pruning, not with `git worktree remove`.

## Storage

One thread owns the SQLite connection (WAL, foreign keys on). Async code sends it closures and awaits the result, so writes are serialized and never block the runtime.

## API types

Types in `patina-core::api` derive `ts-rs`. `cargo xtask ui` exports them to `ui/src/api/generated` before the Vite build, and CI fails if the committed files differ.

## UI

Preact with signals. `ui/src/reducer.ts` is a pure function from events to the timeline, and is unit tested. `ui/src/policy.ts` previews the allowlist as the daemon applies it, also unit tested. `ui/dist` is embedded with `rust-embed`.
