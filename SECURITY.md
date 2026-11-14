# Security

Patina holds API keys and can run commands, so it is closed by default and not meant for the public internet.

Current state (M0): the daemon binds only to loopback and refuses any non-loopback address until authentication exists (M1). The data folder is created with 0700 permissions. No authentication, secrets or sandbox exist yet; they arrive in M1 and M3.
