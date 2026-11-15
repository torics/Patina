# Agent guide

- Follow `Patina build brief prompt for a coding model.md` (milestones M0 to M8).
- Before finishing a change: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`.
- Log every new dependency and every decision the brief leaves open in `DECISIONS.md`.
- Never hardcode provider or model names in logic. No `todo!()` or stubs in finished work.
- After changing a type in `patina-core::api`, run `cargo xtask types` and commit `ui/src/api/generated`.
- UI checks: `cd ui && npx tsc --noEmit && npm test && npx playwright test --trace off`.
- A tool must take every path through `Workspace::resolve`. Never join a model-supplied path to a folder yourself.
- A tool opens files only with `Workspace::read` and `Workspace::write`, which follow no link. Never `std::fs` on a path in a worktree.
- Run git only through `patina-tools::git`, which disables hooks and signing.
- A run's worktree is untrusted. Never run `git add`, `status`, `commit` or `worktree remove` on one, and never let git find the repository from inside it. Use `git::checkpoint`, `git::rollback` and `git::remove_worktree`. Read diffs from the repository with `git::diff`.
- Every command an agent starts goes through the run's `Commands`, and so through its `Sandbox`. Nothing else spawns a process for an agent.
- A sandbox backend that cannot enforce what was asked returns an error. It never falls back to something weaker.
- Sandbox and escape tests live in `crates/patina/tests/sandbox.rs` and drive the real binary. A new way out gets a test there first.
- New event types or payload fields need a case in `ui/src/reducer.ts` and a test in `ui/src/reducer.test.ts`.
- Never add a message to an agent's stored history from outside the loop: it could land between a tool call and its result. Queue it with `store::add_steering`; the loop delivers it before the next model turn.
- An agent's history may only be cut in front of an assistant message (`compact::turn_start`). Any new way of shortening history needs the pairing test in `compact.rs`.
- Anything a run does that must survive a restart is written before it takes effect: the approval row before the event, `delivered` before the call runs, usage with every `usage` event.
