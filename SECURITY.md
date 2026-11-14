# Security

Patina holds API keys and, from M3, can run commands. It is closed by default and not meant for the public internet.

## What is in place (M2)

- **Login is always required**, including on localhost. The first run prints a one-time setup token; the setup page needs it to set the owner password. `patina reset-password` issues a new one and signs every device out.
- **Passwords** are hashed with Argon2id. **Sessions** are random 256-bit tokens, stored only as SHA-256 hashes, sent as `HttpOnly; SameSite=Strict` cookies. `Secure` is added when the request arrived over HTTPS (`X-Forwarded-Proto: https`, as `tailscale serve` sends).
- **Login rate limit**: five failures lock sign-in for one minute, doubling on each further lockout. The setup token shares this limit.
- **CSRF**: every state-changing request needs the `X-Patina-CSRF` header matching the session, and an `Origin` that names the same host.
- **Host check**: any request whose `Host` is not in `allowed_hosts` is refused. This defends against DNS rebinding.
- **Bind**: `127.0.0.1` by default. A non-loopback bind is refused unless a password is set and a non-localhost name is in `allowed_hosts`.
- **Headers**: `Content-Security-Policy: default-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'`, plus `nosniff`, `no-referrer` and `X-Frame-Options: DENY`.
- **Keys never reach the browser.** The API accepts a key and never returns it; the UI shows the last four characters. Model output is rendered as text.

## Agents and files (M2)

- **An agent has only the tools named in its config**, and only in a run that has a project. A plain chat has no tools.
- **Path confinement**: every tool path is relative to the run's worktree. Symlinks are resolved first and the real path must be inside the worktree; absolute paths, `..`, symlinks that lead out and anything under `.git` are refused. This is enforced in one place and unit tested.
- **Isolated changes**: a run works on its own branch in its own worktree under Patina's data folder. Your checkout changes only when you merge, and Patina never pushes.
- **Approvals**: with no sandbox, every edit waits for your answer with its diff, whatever the project's policy says. An approval can be answered once; a second answer is refused. Stopping a run expires its open approvals.
- **Patina's folders cannot be a project**: a folder that contains or sits inside the config or data folder is refused, so no agent gets a workspace over the keys or the database.
- **Git safety**: Patina's git commands run with hooks and commit signing disabled, so a checkpoint or merge never runs code from the repository.
- **Budgets** stop a run at its step, token or time limit, and at its cost limit where the model has a configured price.
- **Model output and file content are data.** The system preamble tells the model that text in files and tool results is not an instruction, and the UI renders all of it as plain text.
- **`patina run`** takes approvals only from an interactive terminal and refuses `--yes` while there is no sandbox.

What this does not cover yet: an approved edit can still write anything inside the worktree, including files that run later (a build script, a test). Review diffs before you merge and before you run the result.

## Secrets at rest

API keys are encrypted with XChaCha20-Poly1305 and bound to their name. The master key is random, created at first run in the data folder as `master.key` with `0600` permissions; `PATINA_MASTER_KEY` (64 hex characters) overrides it. There is no OS keyring support in v1.

This protects backups and copied database files. It does not protect against a compromised host account: anyone who can read the data folder as you can read the master key.

## Not in place yet

No command execution exists, so there is no sandbox (M3). MCP servers (M7) will run outside the run sandbox, started by the daemon with a scrubbed environment. The full threat model is written with the sandbox in M3.

## Outbound calls

The daemon calls only the providers you configure.
