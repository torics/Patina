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

Team files and templates live in `patina-core` (`team_config`, `team_templates`); team routes in `patina-server::teams`.

## One run

1. `POST /api/v1/runs` (or `patina run`) calls `engine::start_run`. With a project, it first settles the sandbox: the backend the project asks for is resolved against what the host supports, and a backend that is missing refuses the run. Then it creates the worktree `worktrees/<run id>` on branch `patina/run-<id>` from the project's `HEAD`, and gives the agent the tools named in its config, bound to that worktree and that sandbox. Then it spawns a tokio task.
1. The task logs `run_started` with the sandbox level, then prepares the worktree: it copies the project's `worktree_copy` entries and runs `worktree_setup` inside the sandbox. Both are logged as tool calls that belong to no agent, and a failure fails the run.
2. The task calls `team::run_team` in core, which runs each agent with `run_agent` (see Teams). Each step: check cancel and budgets, stream one model turn (with retries), save the assistant message, then handle the tool calls in order.
3. For each call: log `tool_call`, validate the arguments against the tool's schema, ask the policy. If the policy says ask, the host stores an approval row, logs `approval_requested`, sets the run to `waiting_approval` and waits for the owner's answer. Then the tool runs, and `tool_result` is logged. A call always gets a result, even when denied or cancelled.
4. If the step changed files, the host commits the worktree as a checkpoint and logs `checkpoint`. A command counts as a change; the checkpoint finds out whether anything differs.
1. When the loop ends, however it ends, the run's background processes are stopped and its sandbox is released.
5. Everything the loop does to the outside goes through the `RunHost` trait: events are written to SQLite first, then broadcast. Text deltas are broadcast live, and persisted in batches every 250 ms or 2 KB.
6. `GET /runs/:id/events` subscribes to the broadcast, replays the log after `after` or `Last-Event-ID`, then follows live. Events are de-duplicated by `seq`, so there is no gap and no duplicate. The stream ends once the run is inactive and its log is sent.

The event log is the single source of truth for the UI. Each agent's model context is stored separately in `messages`.

## Durability

- **Ownership.** The task that drives a run writes its process identity (pid and start time, from `process::identity`) to `runs.owner`, and clears it when the run ends. A run whose owner is alive belongs to that process, for example a `patina run` in a terminal; nobody else resumes or recovers it.
- **Pause** sets a flag in the run's `Signals`. The loop checks it before each model turn, so the step in progress, including a command or an approval, finishes first. The run ends as `paused` and `run_finished` carries `reason: owner`. The loop breaker ends a run the same way with `reason: loop`.
- **Shutdown** sets `Signals.shutdown` and cancels every run. The loop then returns `Interrupted` without writing results for the calls of the turn in progress, so the log says where each one stood. A run that was waiting for an approval is recorded as still `waiting_approval`, with no `run_finished`; any other becomes `interrupted`. `serve` waits up to 30 seconds for runs to record this.
- **Recovery** (`engine::recover`, at `serve` start): every `queued`, `running` or `waiting_approval` run whose owner is dead has its leftover processes killed (`process::sweep_run` finds them by the run id in their `PATINA_CMD` marker; the run's container is removed). Waiting runs keep waiting; the rest become `interrupted` with a `run_finished`.
- **Resume** (`engine::resume_run`) rebuilds the plan from the run row, the agent's current config and the project's current settings, so the sandbox is resolved again and fails closed. Usage is restored from `runs.usage_json`, which every `usage` event updates. History comes from `messages`. If the last stored message is an assistant turn with tool calls, `call_states` reads the events after that turn's `usage` event and the run's approvals to tell for each call: finished (its result is in the log), interrupted (it started and never reported back), decided (the owner answered while no task was there), logged (asked and not answered: asked again), or not started. Approvals carry a `delivered` flag once the loop received the answer, which is what separates "decided" from "may have run".
- **Messages** from the owner go to the `steering` table. The loop takes them before each model turn, logs `user_message`, and appends them to the history after the tool results, so they never split a call from its result. A rollback note uses the same path.
- **Compaction** (`patina-core::compact`) runs before a model turn when the estimated history passes 70% of `context_window`. The estimate is four characters per token, scaled by the ratio the provider's own counts showed so far. Stage one replaces tool outputs of 2 KB or more older than the latest three turns with a pointer to an artifact. Stage two asks the same model, with no tools, to summarize everything between the prompt and the latest ten turns (fewer if ten do not fit); the prompt stays word for word, followed by the summary. History is only cut in front of an assistant message, so a call and its result stay together. The new history replaces the stored one in one transaction, and a `compaction` event records the numbers.
- **Retention** (`engine::prune`) deletes events, messages, approvals, delivered messages and artifacts of runs that ended before the cutoff and marks them `pruned_at`.

## Teams

`patina-core::team` composes every mode from the single-agent loop. Before a run starts, the engine resolves the team (`team:<id>`) or agent (`agent:<id>`) stored on the run into a `Team` and a `Roster`: every agent the run can reach, delegation followed to depth two, each with its provider and the tools its file names. A missing agent or provider stops the run before anything is created.

- **Ids.** An agent the run starts itself has its own id as its instance id; a sub-agent is `<agent>#<n>`, numbered past every id the run already used. Events and stored messages use the instance id, and `agent_started` carries `agent` (the config id) and, for a sub-agent, `parent` and `tool_call_id`.
- **Shared, and not.** All agents share the run's `RunEnv`: host, policy, sandbox, allowlist, budget, cancel and signals. Each has its own `RunCtx`: provider, tools, whether the owner's messages reach it (`steerable`), and its place in the team (`TeamLink`).
- **Sequential** runs members in order; each gets a hand-off message: the prompt, then each earlier member's final message. **Parallel** runs members concurrently with only `ToolKind::Read` tools, then the synthesizer gets the prompt and every answer. **Delegate** adds a `delegate` tool to the lead's request. The loop handles a `delegate` call itself: the sub-agent runs its own loop inside the call, and its final message becomes the call's result. If the sub-agent stops for any other reason (pause, budget, shutdown, failure), the run stops with it and the lead's turn stays open.
- **Resume.** `run_team` asks the host for each agent's stored state (`RunHost::load_agent`): history, where an unfinished turn stands, and whether the agent already gave its final answer (its latest start or finish event says `completed`, or its history ends with an answer). Finished agents are not run again; their final message is read from their history. For a `delegate` call that was logged without a result, `call_states` finds the sub-agent it started and the call continues with that agent (`CallState::Delegated`).
- **Pause** is sticky in a team: the flag stays set until the run ends, so every agent stops at its next turn. Resume makes a new one.
- **Messages** from the owner reach the agent the owner talks to: the current member in sequential mode, the lead in delegate mode, the synthesizer in parallel mode. The last agent answers a message that arrives as it finishes.

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

## Workbench and preview

The file routes (`/projects/:id/tree` and `/projects/:id/file`, with `?run=` for a run's worktree) use the same `Workspace` confinement as the agent's tools. A save in a run's worktree is committed with the same untrusting checkpoint code as an agent's step and logged as `owner_edit`. `@path` attachments are read by the daemon and stored in the run's prompt, so every agent and a resumed run see them.

`patina-server::preview` runs a second axum listener on loopback. It holds the selected run, a port per run (picked once, kept for the daemon's lifetime), and the dev command the owner started, which runs in a sandbox built like the run's own. Requests are streamed to `127.0.0.1:<port>` with `reqwest`, without cookies or Patina headers, and responses come back without `Set-Cookie` or CORS headers. The run's sandbox setup carries the port, so commands get `PORT`, Landlock allows binding exactly that port when TCP is blocked, and the container publishes it on loopback.

## MCP

`patina-tools::mcp` is the client, on `rmcp` with client features only. `McpHub` holds one connection per server, shared by every run and made on first use: a child process over stdio (in its own process group, with a scrubbed environment) or Streamable HTTP over `reqwest`. A connection is made again when it closed or its settings changed. `McpTool` wraps one tool of a server as an agent `Tool` of kind `External { trusted }`; trust is read from `mcp.toml` at each call, so trusting a tool applies to a run already going. The policy decides an external tool by trust alone.

`patina-server::mcp` reads `mcp.toml`, resolves `env:` and `secret:` values for the hub, and serves the settings routes. When a run starts or resumes, `engine::resolve` gives each agent the MCP tools its `tools` list grants (`mcp__<server>` or `mcp__<server>__<tool>`); a server that cannot be reached stops the run before it starts.

## UI

Preact with signals. CodeMirror 6 provides the editor and the per-file diff; it is loaded on demand and mounted in a shadow root so its styles need no `unsafe-inline`. `ui/src/reducer.ts` is a pure function from events to the timeline, and is unit tested. `ui/src/policy.ts` previews the allowlist as the daemon applies it, also unit tested. `ui/dist` is embedded with `rust-embed`.
