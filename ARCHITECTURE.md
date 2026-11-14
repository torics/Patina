# Architecture

One Cargo workspace, one binary.

- `patina-core`: config, paths and (later) domain types, traits, agent loop. Never depends on axum, SQLite or provider SDKs.
- `patina-store`: SQLite schema, migrations (`PRAGMA user_version`), WAL mode, foreign keys on.
- `patina-server`: axum router, embedded UI (`rust-embed` of `ui/dist`).
- `patina`: the CLI binary that wires them together.

`patina-providers` and `patina-tools` are added in the milestones that need them (M1 and M2) rather than as empty crates.

Startup: resolve folders, create them (data folder is 0700), load or write `config.toml`, open and migrate the database, bind loopback only, serve until SIGTERM or Ctrl-C.
