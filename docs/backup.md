# Backup and restore

Patina keeps everything in two folders. By default these are `~/.config/patina` (config) and `~/.local/share/patina` (data); `patina doctor` prints the ones in use.

| What | Where | Back it up? |
| --- | --- | --- |
| Settings, providers, agents, teams, projects, MCP servers | config folder, `*.toml` | Yes. Plain text, no secrets |
| Runs, events, sessions, encrypted keys | `data/patina.db` (with `-wal` and `-shm` while running) | Yes |
| The master key that decrypts stored keys | `data/master.key`, or `PATINA_MASTER_KEY` | Yes, but store it apart from the database |
| Full outputs of long tool results | `data/artifacts/` | Optional |
| Run worktrees and private temp folders | `data/worktrees/`, `data/sandbox/` | No. A run's work is on its branch `patina/run-<id>` in your project |

The database alone holds keys only in encrypted form. Keeping `master.key` in a different place from database backups means a lost backup does not give away your keys.

## Taking a backup

While Patina runs, copy the database with SQLite's backup command, which gives a consistent copy:

```sh
sqlite3 ~/.local/share/patina/patina.db ".backup '/backups/patina-$(date +%F).db'"
cp -r ~/.config/patina /backups/patina-config-$(date +%F)
```

Or stop Patina and copy both folders as they are.

## Restoring

Stop Patina, put the config folder, `patina.db` and `master.key` back (delete any old `patina.db-wal` and `patina.db-shm` first), and start it. Runs that were running when the backup was taken show as interrupted and can be resumed if their worktrees still exist; otherwise their branches are in the projects.

Without the master key, stored keys cannot be decrypted: enter them again in Settings. Everything else works.

## Moving to another machine

Restore as above on the new machine. Project paths in `projects.toml` must point at the projects there; edit them if they moved. If you forgot the password, `patina reset-password` prints a new setup token.
