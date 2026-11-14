# Architecture

One Cargo workspace, one binary. The browser is a thin view over the daemon's API.

| Crate | Holds |
| --- | --- |
| `patina-core` | Config and file formats, the `ChatProvider`, `Tool` and `RunHost` traits, the agent loop, budgets, policy, event and API types. No axum, SQLite or genai types. |
| `patina-tools` | The five file tools, path confinement (`Workspace`), and git worktree operations through the `git` command line. |
| `patina-providers` | `GenaiProvider` (the `genai` crate), `MockProvider`, the `Guarded` wrapper (concurrency limit, idle timeout, cancel), model listing. |
| `patina-store` | SQLite: migrations, the writer thread, runs, events, messages, sessions, encrypted secrets, master key lookup. |
| `patina-server` | axum routes under `/api/v1`, auth and security middleware, the run engine, SSE, the embedded UI. |
| `patina` | The CLI binary that wires them together: `serve`, `run`, `reset-password`, `version`. |
| `xtask` | Build helper: `cargo xtask ui`. Not part of the binary. |

## One run

1. `POST /api/v1/runs` (or `patina run`) calls `engine::start_run`. With a project, it creates the worktree `worktrees/<run id>` on branch `patina/run-<id>` from the project's `HEAD`, and gives the agent the tools named in its config, bound to that worktree. Then it spawns a tokio task.
2. The task calls `run_agent` in core. Each step: check cancel and budgets, stream one model turn (with retries), save the assistant message, then handle the tool calls in order.
3. For each call: log `tool_call`, validate the arguments against the tool's schema, ask the policy. If the policy says ask, the host stores an approval row, logs `approval_requested`, sets the run to `waiting_approval` and waits for the owner's answer. Then the tool runs, and `tool_result` is logged. A call always gets a result, even when denied or cancelled.
4. If the step changed files, the host commits the worktree as a checkpoint and logs `checkpoint`.
5. Everything the loop does to the outside goes through the `RunHost` trait: events are written to SQLite first, then broadcast. Text deltas are broadcast live, and persisted in batches every 250 ms or 2 KB.
6. `GET /runs/:id/events` subscribes to the broadcast, replays the log after `after` or `Last-Event-ID`, then follows live. Events are de-duplicated by `seq`, so there is no gap and no duplicate. The stream ends once the run is inactive and its log is sent.

The event log is the single source of truth for the UI. Each agent's model context is stored separately in `messages`.

## Workspaces

`Workspace::resolve` is the only way a tool turns a model-supplied path into a real one: it refuses absolute paths, `..`, anything under `.git`, and any path whose real location (after resolving symlinks) is outside the worktree. `edit_file` and `write_file` remember a hash of what the agent last read and refuse to overwrite a file that changed since.

Git work goes through the `git` command line (`patina-tools::git`) with hooks and signing disabled. The owner's checkout is touched only by an explicit merge, which needs a clean checkout and fast-forwards when it can.

## Storage

One thread owns the SQLite connection (WAL, foreign keys on). Async code sends it closures and awaits the result, so writes are serialized and never block the runtime.

## API types

Types in `patina-core::api` derive `ts-rs`. `cargo xtask ui` exports them to `ui/src/api/generated` before the Vite build, and CI fails if the committed files differ.

## UI

Preact with signals. `ui/src/reducer.ts` is a pure function from events to the timeline, and is unit tested. `ui/dist` is embedded with `rust-embed`.
