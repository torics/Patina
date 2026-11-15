# Config reference

Files live in the config folder (`~/.config/patina/` on Linux, or `--config-dir`). They are plain TOML, edited by hand or through the UI; the daemon reloads them when they change. Every file carries `schema_version = 1`.

## config.toml

| Key | Default | Meaning |
| --- | --- | --- |
| `bind` | `"127.0.0.1"` | Listen address |
| `port` | `7878` | App port |
| `preview_port` | `7879` | Preview proxy port (used from M6) |
| `allowed_hosts` | `["localhost", "127.0.0.1", "[::1]"]` | Accepted `Host` names |
| `retention_days` | `0` | Remove the event log, agent context and saved outputs of runs that ended more than this many days ago, at start and every six hours. The run, its branch and its checkpoints stay; a pruned run cannot be resumed. Paused and waiting runs are never pruned. 0 keeps everything |

## providers.toml

```toml
schema_version = 1

[[provider]]
id = "home-ollama"
kind = "openai_compatible"   # openai_compatible | openai | anthropic | gemini | ollama
base_url = "http://192.168.1.20:11434/v1"
api_key_ref = "secret:home-ollama"   # or "env:SOME_VAR", or omitted
max_concurrency = 4
idle_timeout_s = 120

[provider.prices."some-model-id"]   # optional, USD per million tokens
input = 0.0
output = 0.0
```

A `secret:` reference points at a key stored encrypted in the database; set it in the UI. Cost is shown only for models with a price.

## agents/<id>.toml

```toml
schema_version = 1
id = "assistant"
name = "Assistant"
provider = "home-ollama"
model = "some-model-id"
system_prompt = ""
temperature = 0.2          # optional
max_output_tokens = 8000   # optional
context_window = 128000    # tokens; history is compacted before it passes 70% of this
tools = ["list_files", "read_file", "search", "edit_file", "write_file", "run_command"]
can_delegate_to = []
```

`tools` lists what the agent may use in a run that has a project: `list_files`, `read_file`, `search`, `edit_file`, `write_file`, `run_command`. `run_command` brings `process_output` and `stop_process` for background processes. An empty list means the agent can only talk.

## projects.toml

```toml
schema_version = 1

[[project]]
id = "my-project"
name = "My project"
path = "/home/you/code/my-project"   # absolute; the top level of a git repository
policy = "auto_edit"                 # confirm_all | auto_edit | autopilot
```

Only what differs from the defaults is written to the file. Everything below is optional.

| Key | Default | Meaning |
| --- | --- | --- |
| `policy` | `"auto_edit"` | What asks for approval. `confirm_all`: every edit and command. `auto_edit`: commands, unless they match `allow_commands`. `autopilot`: nothing; needs a sandbox |
| `sandbox` | `"auto"` | `auto` picks Landlock, else a container if `container_image` is set and a runtime answers, else none. `landlock`, `container` and `none` ask for exactly that. A backend the host cannot give refuses the project's runs |
| `container_image` | none | The image for the container backend, for example `"docker.io/library/node:22"` |
| `network` | `true` | Whether commands may use the network. `false` blocks TCP under Landlock (kernel 6.7 or later; UDP stays open) and gives a container no network |
| `allow_commands` | `[]` | Patterns of commands that run without asking under `auto_edit`. `*` matches any run of characters and the whole command must match. A command with `;`, `&`, `|`, `>`, `<`, backticks or `$(` matches only a pattern that equals it |
| `env` | `{}` | Extra environment variables for commands, as a `[project.env]` table. `HOME`, `TMPDIR` and `PATINA_*` cannot be set. Never put a secret here |
| `sandbox_read` | `[]` | Extra absolute folders commands may read and run programs from |
| `sandbox_write` | `[]` | Extra absolute folders commands may write to, for example a shared package cache |
| `worktree_copy` | `[]` | Gitignored files and folders to copy from the project into each new worktree, for example `node_modules` or `config/local.toml`. Paths are relative to the project. Only ignored entries are accepted, so a checkpoint never commits them |
| `worktree_copy_secrets` | `[]` | The entries of `worktree_copy` that hold credentials. An entry that looks like a credentials file (`.env`, `*.pem`, `id_rsa`, `.npmrc` and similar) is refused unless it is also listed here |
| `worktree_setup` | none | A command to run in each new worktree before the agent starts, inside the sandbox, with a ten-minute limit. If it fails, the run fails |

Without a sandbox, every edit and command asks whatever `policy` says. The file is read on every use, so a hand edit applies to the next run. "Approve and always allow" in the UI appends to `allow_commands`.

### What a command gets

A command runs through `/bin/sh -c` in the worktree with `PATH`, `LANG`, `TERM=dumb`, a private `HOME` and `TMPDIR` (under the data folder, per run), and the project's `env`. Nothing else of the daemon's environment is passed on.

Under Landlock, a command can read the system folders, every folder on its `PATH`, and `sandbox_read`. A toolchain that lives in your home folder must be opened explicitly. Recipes:

```toml
# Rust installed with rustup. RUSTUP_HOME is needed because HOME is private.
sandbox_read = ["/home/you/.rustup", "/home/you/.cargo"]
[project.env]
PATH = "/home/you/.cargo/bin:/usr/local/bin:/usr/bin:/bin"
RUSTUP_HOME = "/home/you/.rustup"
```

```toml
# Node installed with nvm, with dependencies copied from your checkout
sandbox_read = ["/home/you/.nvm"]
worktree_copy = ["node_modules"]
[project.env]
PATH = "/home/you/.nvm/versions/node/v22.11.0/bin:/usr/local/bin:/usr/bin:/bin"
```

Package caches are private to each run, because `HOME` is. To share one, set its location with `env` and open it with `sandbox_write`. Git commands do not work on the run's own repository inside a command: Patina commits the agent's changes itself.

Run `patina doctor` to see which sandbox each project gets on this machine.
