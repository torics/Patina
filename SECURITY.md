# Security

Patina holds API keys and, from M3, can run commands. It is closed by default and not meant for the public internet.

## What is in place (M1)

- **Login is always required**, including on localhost. The first run prints a one-time setup token; the setup page needs it to set the owner password. `patina reset-password` issues a new one and signs every device out.
- **Passwords** are hashed with Argon2id. **Sessions** are random 256-bit tokens, stored only as SHA-256 hashes, sent as `HttpOnly; SameSite=Strict` cookies. `Secure` is added when the request arrived over HTTPS (`X-Forwarded-Proto: https`, as `tailscale serve` sends).
- **Login rate limit**: five failures lock sign-in for one minute, doubling on each further lockout. The setup token shares this limit.
- **CSRF**: every state-changing request needs the `X-Patina-CSRF` header matching the session, and an `Origin` that names the same host.
- **Host check**: any request whose `Host` is not in `allowed_hosts` is refused. This defends against DNS rebinding.
- **Bind**: `127.0.0.1` by default. A non-loopback bind is refused unless a password is set and a non-localhost name is in `allowed_hosts`.
- **Headers**: `Content-Security-Policy: default-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'`, plus `nosniff`, `no-referrer` and `X-Frame-Options: DENY`.
- **Keys never reach the browser.** The API accepts a key and never returns it; the UI shows the last four characters. Model output is rendered as text.

## Secrets at rest

API keys are encrypted with XChaCha20-Poly1305 and bound to their name. The master key is random, created at first run in the data folder as `master.key` with `0600` permissions; `PATINA_MASTER_KEY` (64 hex characters) overrides it. There is no OS keyring support in v1.

This protects backups and copied database files. It does not protect against a compromised host account: anyone who can read the data folder as you can read the master key.

## Not in place yet

No command execution exists, so there is no sandbox (M3). MCP servers (M7) will run outside the run sandbox, started by the daemon with a scrubbed environment. The full threat model is written with the sandbox in M3.

## Outbound calls

The daemon calls only the providers you configure.
