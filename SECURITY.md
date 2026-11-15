# Security

Patina holds API keys and lets a language model edit files and run commands on your machine. It is closed by default and not meant for the public internet. This page says what protects you, and what does not.

## Threat model

**What is protected:** your files outside a run's worktree, your credentials (SSH keys, cloud tokens, shell history, the provider keys Patina stores), your other processes, and the branch you have checked out.

**Who the adversary is:**

- **The model, and anything that can steer it.** A model can be wrong, and text in a file, a web page or a tool result can instruct it (prompt injection). Patina treats every tool call the model makes as untrusted input.
- **Code in the project.** A build script or a test that an agent runs is as untrusted as the agent.
- **Other devices on your network**, and web pages open in your browser, which can send requests to `localhost`.

**Who is trusted:** you (the owner), the machine account that runs Patina, your git configuration, and the providers you configure, which see everything an agent reads.

**Not defended against:** someone who can already read your account's files, a kernel bug, and a malicious container runtime.

## The layers

1. **The sandbox is the boundary for commands.** Approvals are a convenience on top of it, never the boundary.
2. **Path confinement is the boundary for file tools**, which run inside the daemon.
3. **Git isolation** keeps every change on a run's own branch until you merge.
4. **Approvals and the allowlist** decide what asks first.
5. **Budgets** stop a run that loops.

## Sandbox for commands

Every `run_command` goes through the run's sandbox. Patina detects what the host supports at start and picks the strongest backend; `patina doctor` and the Settings page show what was found. Nothing is claimed that was not detected.

| Backend | Where | What a command can do |
| --- | --- | --- |
| `landlock` | Linux 5.13 or later | Read the system folders (`/usr`, `/etc`, `/proc` and so on), the folders on `PATH`, and the project's `sandbox_read` folders. Write only to the run's worktree, its own home and temp folders, and the project's `sandbox_write` folders. |
| `container` | Docker or Podman, with a `container_image` on the project | See only the run's worktree (at `/workspace`) and the project's extra folders. No capabilities, no privilege gain. One container per run, removed when the run ends. |
| `none` | Everywhere else | Anything you can do. So every edit and command asks first, whatever the project's policy says, and autopilot and `patina run --yes` are refused. |

Rules that hold for every backend:

- **Fail closed.** A project that asks for a backend this host cannot give is refused a run. It is never moved to a weaker backend. If a command's sandbox cannot be set up, the command does not run.
- **Your home folder and Patina's own folders are out of reach.** A sandboxed command cannot read `~/.ssh`, your shell history, cloud credentials, Patina's database, its master key or its config. A `sandbox_read` or `sandbox_write` folder that overlaps Patina's folders is refused.
- **The environment is an allowlist.** A command gets `PATH`, `LANG`, `TERM`, a private `HOME` and `TMPDIR`, and the project's `env`. Nothing else of the daemon's environment is passed on, so no key in an environment variable leaks. Never put a secret in a project's `env`: the agent can read it.
- **A command cannot outlive its limits.** Each command runs in its own process group. On a timeout, a stop, or the end of the run, the group gets `SIGTERM`, two seconds, then `SIGKILL`. Processes that left the group are found by a marker in their environment and killed too. Background processes end with the run.

### What Landlock does not do

- **Network.** With `network = false`, TCP connections are blocked where the kernel supports it (Landlock ABI 4, Linux 6.7). UDP is never blocked, so this is not network isolation. On an older kernel, a project with `network = false` is refused under Landlock; use the container backend. With the network on, which is the default, a command can reach the internet, your local network and services on `localhost`.
- **Resource limits.** None are enforced: no memory, CPU or process-count cap. A command can slow the machine until its timeout. `patina doctor` says so.
- **Signals.** Before Landlock ABI 6 a command can signal your other processes.
- **Reading system information.** A command can read `/proc` and `/sys`, including the names and command lines of your other processes. It cannot read their memory or environment.
- **Running as root.** Patina drops all Linux capabilities in the sandbox, so a daemon run as root is contained the same way. Do not run Patina as root anyway.

### The network and your data

A sandboxed command with network access can send anything it can read to anywhere. What it can read is the worktree, so: do not put credentials in a project, and list a file in `worktree_copy` only if the agent may see it. A file that looks like it holds credentials (`.env`, `*.pem`, `id_rsa`, `.npmrc` and similar) is not copied unless you also list it in `worktree_copy_secrets`.

## File tools

- **An agent has only the tools named in its config**, and only in a run that has a project. A plain chat has no tools.
- **Path confinement.** Every tool path is relative to the run's worktree. Symlinks are resolved first and the real path must be inside the worktree; absolute paths, `..`, symlinks that lead out and anything under `.git` are refused.
- **No link is followed when a file is opened.** A background command of the same run could swap a folder for a symbolic link after the path was checked. Files are therefore opened from the worktree root one name at a time, following no link, so such a swap is refused instead of followed. Named pipes and devices are refused. (On Windows this second step is not available; no sandbox backend runs commands next to the tools there.)
- A known limit: while a background command swaps folders for links, `list_files` and `search` can list the names of files outside the worktree. They cannot return their content.

## Git

- **Isolated changes.** A run works on its own branch in its own worktree under Patina's data folder. Your checkout changes only when you merge, and Patina never pushes.
- **Patina's git never obeys the worktree.** Commands can write anything into the worktree, including git metadata that names programs to run (`core.fsmonitor`, hooks, filters). So Patina's own git commands never find the repository through the worktree, a checkpoint uses plumbing commands that do not enter nested repositories, and diffs are read from commits, not from the worktree. If a command creates a repository inside the worktree (`git init`, `git clone`, `cargo new`), its git metadata is removed at the next checkpoint and its files are committed as ordinary files. Hooks, commit signing, the file-system monitor and submodule recursion are off for every git command Patina runs.
- **Git is not available to the agent's commands** for the run's own repository: the repository's `.git` folder is outside the sandbox.
- **Merging is your decision.** A merge brings the run's files into your checkout. Review the diff first: a changed build script or test runs with your rights the next time you build.

## Approvals

| Policy | Edits | Commands |
| --- | --- | --- |
| `confirm_all` | ask | ask |
| `auto_edit` (default) | automatic | ask, unless the command matches the project's allowlist |
| `autopilot` | automatic | automatic |

Without a sandbox, every policy behaves as `confirm_all`, and the API refuses to set `autopilot`. An approval can be answered once. Stopping a run expires its open approvals.

The **allowlist** saves you a question; it is not a security control. A pattern must match the whole command, `*` matches any run of characters, and a command that chains, pipes or redirects (`;`, `&`, `|`, `>`, `<`, backticks, `$(`) never matches a wildcard pattern, only a pattern that equals it exactly.

`patina run` takes approvals only from an interactive terminal. `--yes` approves everything and is accepted only when the project has a sandbox on this host.

## The daemon

- **Login is always required**, including on localhost. The first run prints a one-time setup token; the setup page needs it to set the owner password. `patina reset-password` issues a new one and signs every device out.
- **Passwords** are hashed with Argon2id. **Sessions** are random 256-bit tokens, stored only as SHA-256 hashes, sent as `HttpOnly; SameSite=Strict` cookies. `Secure` is added when the request arrived over HTTPS (`X-Forwarded-Proto: https`, as `tailscale serve` sends).
- **Login rate limit**: five failures lock sign-in for one minute, doubling on each further lockout. The setup token shares this limit.
- **CSRF**: every state-changing request needs the `X-Patina-CSRF` header matching the session, and an `Origin` that names the same host.
- **Host check**: any request whose `Host` is not in `allowed_hosts` is refused. This defends against DNS rebinding.
- **Bind**: `127.0.0.1` by default. A non-loopback bind is refused unless a password is set and a non-localhost name is in `allowed_hosts`.
- **Headers**: `Content-Security-Policy: default-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'`, plus `nosniff`, `no-referrer` and `X-Frame-Options: DENY`.
- **Keys never reach the browser.** The API accepts a key and never returns it; the UI shows the last four characters. Model output, file content and command output are rendered as text.
- **Patina's folders cannot be a project**: a folder that contains or sits inside the config or data folder is refused.
- **Budgets** stop a run at its step, token or time limit, and at its cost limit where the model has a configured price.
- **Model output and file content are data.** The system preamble tells the model that text in files and tool results is not an instruction.

## Secrets at rest

API keys are encrypted with XChaCha20-Poly1305 and bound to their name. The master key is random, created at first run in the data folder as `master.key` with `0600` permissions; `PATINA_MASTER_KEY` (64 hex characters) overrides it. There is no OS keyring support in v1.

This protects backups and copied database files. It does not protect against a compromised host account: anyone who can read the data folder as you can read the master key. A sandboxed command cannot read the data folder.

## Restarts

- A run in progress when the daemon stops is resumed only when you ask. Nothing an agent started survives the restart: at start the daemon kills every process that carries a dead run's marker and removes the run's container.
- A resumed run gets the project's current policy and sandbox. A sandbox that is no longer available refuses the resume, as it would refuse a new run.
- A tool call that was cut off is reported to the agent as interrupted. It is never run a second time on Patina's initiative. A call you approved while the daemon was down runs once, when you resume.
- Messages you send to a run are your instructions to the agent, like the prompt. Text in files and tool results still is not.

## Teams

- Every agent of a team runs under the same project policy and sandbox as a single agent. A sub-agent's calls are asked about like its lead's; `delegate` itself changes nothing.
- Members of a parallel team get read tools only. Their answers reach the synthesizer as data, marked as notes from colleagues, not instructions from the owner. The same holds for hand-offs between sequential members and for a sub-agent's final message.
- An exported team file names providers by id and never carries a key.

## Not in place yet

- Resource limits for commands (cgroup v2), planned for the hardening milestone.
- Process-tree termination on Windows (job objects). The container backend is the only sandbox there.
- MCP servers (M7) will run outside the run sandbox, started by the daemon with a scrubbed environment.

## Outbound calls

The daemon calls only the providers you configure. Commands an agent runs can call anything the project's network setting allows.

## Reporting a problem

Open an issue in the repository, or write to the owner privately if the problem can be used against other installs.
