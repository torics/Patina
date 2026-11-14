# Architecture

One Cargo workspace, one binary. The browser is a thin view over the daemon's API.

| Crate | Holds |
| --- | --- |
| `patina-core` | Config and file formats, the `ChatProvider` and `EventSink` traits, the agent loop, event and API types. No axum, SQLite or genai types. |
| `patina-providers` | `GenaiProvider` (the `genai` crate), `MockProvider`, the `Guarded` wrapper (concurrency limit, idle timeout, cancel), model listing. |
| `patina-store` | SQLite: migrations, the writer thread, runs, events, messages, sessions, encrypted secrets, master key lookup. |
| `patina-server` | axum routes under `/api/v1`, auth and security middleware, the run task, SSE, the embedded UI. |
| `patina` | The CLI binary that wires them together. |
| `xtask` | Build helper: `cargo xtask ui`. Not part of the binary. |

`patina-tools` is added in M2.

## One run

1. `POST /api/v1/runs` creates the run row and spawns a tokio task.
2. The task calls `run_agent` in core, which streams a model turn from the provider.
3. Every step goes through the `EventSink`: the event is written to SQLite first, then broadcast.
4. Text deltas are broadcast live, and persisted in batches every 250 ms or 2 KB.
5. `GET /runs/:id/events` subscribes to the broadcast, replays the log after `after` or `Last-Event-ID`, then follows live. Events are de-duplicated by `seq`, so there is no gap and no duplicate. The stream ends after `run_finished`.

The event log is the single source of truth for the UI. Each agent's model context is stored separately in `messages`.

## Storage

One thread owns the SQLite connection (WAL, foreign keys on). Async code sends it closures and awaits the result, so writes are serialized and never block the runtime.

## API types

Types in `patina-core::api` derive `ts-rs`. `cargo xtask ui` exports them to `ui/src/api/generated` before the Vite build, and CI fails if the committed files differ.

## UI

Preact with signals. `ui/src/reducer.ts` is a pure function from events to the timeline, and is unit tested. `ui/dist` is embedded with `rust-embed`.
