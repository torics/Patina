# Agent guide

- Follow `Patina build brief prompt for a coding model.md` (milestones M0 to M8).
- Before finishing a change: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`.
- Log every new dependency and every decision the brief leaves open in `DECISIONS.md`.
- Never hardcode provider or model names in logic. No `todo!()` or stubs in finished work.
- After changing a type in `patina-core::api`, run `cargo xtask types` and commit `ui/src/api/generated`.
- UI checks: `cd ui && npx tsc --noEmit && npm test && npx playwright test --trace off`.
- A tool must take every path through `Workspace::resolve`. Never join a model-supplied path to a folder yourself.
- Run git only through `patina-tools::git`, which disables hooks and signing.
- New event types or payload fields need a case in `ui/src/reducer.ts` and a test in `ui/src/reducer.test.ts`.
