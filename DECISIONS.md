# Decisions

## Owner decisions
- License: dual MIT or Apache-2.0. Ports 7878 (app) and 7879 (preview). Preact with signals. Manual update check only. Defaults from section 19 stand.
- No OS keyring support in v1 (personal home use). Master key lookup will live in one function (file with 0600, or `PATINA_MASTER_KEY`) so a keyring source can be added later.

## M0 choices
- Rust edition 2024, resolver 3. Crates `patina-providers` and `patina-tools` are created in M1 and M2 instead of as empty stubs.
- `ui/dist/index.html` is a committed placeholder so the server compiles. M1 replaces it with the Vite build and gitignores the build output.
- Non-loopback binds are refused until auth exists (M1), to honor section 13 early.
- `--port 0` is allowed, used by the budget script and tests.

## Dependencies (size effect measured on the release binary, 3.0 MB at M0)
- tokio: async runtime, required by the stack.
- axum (http1 only) and tower-http (gzip only): HTTP server and compression.
- rusqlite (bundled): SQLite, about 1 MB of the binary.
- rust-embed: embeds the UI. Negligible.
- clap (derive): CLI. Small.
- tracing and tracing-subscriber (fmt, env-filter, json): logging.
- serde and toml: config files.
- directories: platform config and data paths.
- thiserror and anyhow: error types.
- tempfile (dev only): test folders.

## M1 choices
- **Run without a team:** teams arrive in M5, so `POST /runs` takes `agent_id`. The `team_id` column stores `agent:<id>` until then.
- **One model turn per run** in M1. The loop gains tools in M2 and steering messages in M4.
- **Tool calls from genai arrive whole** at the end of a turn (genai's `capture_tool_calls`), and are emitted as start, one args delta, end. The trait still supports incremental deltas. Revisit if live tool-argument streaming matters.
- **Reasoning parts** are streamed as `reasoning` events but not yet replayed to the provider. Replay is needed only once tool turns exist (M2).
- **Model listing uses plain HTTP**, not genai: genai's list is a static table for most providers.
- **Retries live in the agent loop**, not the provider, so each retry is recorded in the event log. A failed attempt's text stays in the log, tagged with its attempt number, and the UI hides it.
- **`Secure` cookie flag** is set from `X-Forwarded-Proto: https`, since there is no built-in TLS.
- **Origin check** compares the `Origin` authority with the `Host` header, which is already checked against `allowed_hosts`.
- **Minimum password length** is 10 characters.
- **Markdown rendering is deferred to M6.** Model output is shown as plain text until then, which section 13 allows.
- **Extra route** `GET /provider-presets` serves the editable-preset list to the UI.
- **`ui/dist` is not committed.** `patina-server/build.rs` writes a "UI not built" page if it is missing, so `cargo build` works on a fresh clone.
- **On shutdown, running runs are cancelled.** Marking them `interrupted` and resuming is M4.
- **Database access** goes through one writer thread that owns the only connection; reads use it too. Simple, and fast enough for one owner.

## M1 dependencies (stripped release binary: 8.7 MB, up from 3.0 MB)
- genai 0.7: provider protocols. With reqwest and rustls (aws-lc-rs), the bulk of the 5.7 MB growth.
- reqwest 0.13 (rustls): already pulled in by genai; used directly for model listing.
- argon2, chacha20poly1305, sha2, getrandom: password hashing, secret encryption, token hashing, randomness. Small.
- async-trait, futures, tokio-util: the provider trait, streams, cancellation. Small.
- serde_json, uuid (v7): payloads and time-sorted ids. Small.
- ts-rs: generates the UI's API types. Derive only; no measurable size.
- UI: preact, @preact/signals. Dev only: vite, typescript, vitest, @playwright/test, @types/node.

## M2 choices
- **`StreamEvent::ReasoningBlock` added to the provider trait.** The brief's trait has no way to carry a provider's opaque reasoning block, and section 8 requires replaying those blocks verbatim next to tool calls. The genai provider emits them and replays them as thought signatures. Verified against the mock and the local endpoint only; the hosted providers need the owner's keys.
- **Own JSON-Schema subset validator** (`patina-core::tool::validate`: object, required, type, enum, array items, no unknown properties) instead of a schema crate. The built-in tools need nothing more, and it keeps the binary small. MCP tool schemas (M7) may need more.
- **Tool calls in one turn run one after another.** The brief allows reads to run concurrently; sequential is simpler and keeps the log in call order. Revisit if it is slow.
- **`.git` is out of reach** for every file tool, in addition to path confinement, so an agent cannot rewrite history or hooks.
- **Patina's own git commands disable hooks and commit signing** (`core.hooksPath=/dev/null`, `commit.gpgsign=false`), so a checkpoint never runs repository code or asks for a passphrase.
- **Without a sandbox, every edit asks**, whatever the project's policy says. There is no sandbox before M3, so M2 behaves as `confirm_all` for every project. Reads never ask.
- **A write to a path outside the worktree is asked about before it is refused.** The approval comes first in the loop; the tool then refuses the path. Nothing is written either way.
- **Rollback, merge and discard need an idle run.** They return 409 while an agent is working in the worktree.
- **Rollback to step 0** means the base commit, which undoes everything.
- **Patch export** is `GET /runs/:id/diff?format=patch`, not a separate route.
- **`runs.disposition`** (migration 3) records what happened to a run's changes: `merged`, `kept` or `discarded`. Merging removes the worktree and the run branch; keeping removes only the worktree.
- **The event stream ends when the run is inactive and its log is sent**, not at `run_finished`, so a rollback made after the run is replayed too.
- **Text and reasoning events carry `step`**, so the UI can order text around tool calls and discard only the failed attempt of that step.
- **Checkpoint summaries** are the first line of each changing tool's result, for example `created NOTES.md (21 bytes)`.
- **`patina run --team <id>` names an agent** until teams exist (M5); `--agent` is an alias. It runs the engine in its own process, without the daemon, on the same config and data folders. A folder that is not a registered project is registered with the default policy; it never runs `git init`.
- **`patina run` approvals need a terminal.** Answers are read only from an interactive terminal, so `yes | patina run` cannot approve. Without a terminal, a run that needs an approval is stopped (exit code 3). `--yes` is refused until a sandbox exists (M3). Other exit codes: 0 completed, 1 failed, 4 budget reached, 130 stopped.
- **A daemon and a `patina run` can share the data folder**, but each knows only its own live runs: the daemon's UI shows a terminal run's log, and cannot stop it or answer its approvals.
- **Agent `tools` are checked on save** against the built-in tool names.
- **Deferred, with their milestone:** resuming after raising a budget and answering an approval after a daemon restart (M4, with pause and recovery; until then a stopped run stays stopped and such an answer gets 409); "approve and add an allowlist pattern" (M3, with commands); `worktree_copy` and `worktree_setup` (M3: setup must run in the sandbox); project tree and file routes (M6).

## M2 dependencies (stripped release binary: 10.2 MB, up from 8.7 MB; idle memory 9.4 MB; UI 19.6 KB gzipped)
- ignore: file walking that respects `.gitignore`, for `list_files` and `search`. Named by the brief.
- regex: `search` patterns. Named by the brief; already in the tree through `ignore`.
- similar: unified diffs for edit approvals. Named by the brief. Small.
- Test only: util-linux `script` gives the `patina run` test a real terminal (Linux only).

## M3 choices
- **Landlock is applied by a hidden helper subcommand** (`patina sandbox-exec`), not in a `pre_exec` closure. The daemon starts its own binary, which restricts itself, drops every Linux capability and then execs the shell. This keeps allocation and error reporting out of the forked child, and a daemon run as root is contained the same way.
- **`sandbox = "auto"` picks Landlock, else a container when the project has a `container_image` and Docker or Podman is found, else none.** A project that names a backend the host cannot give is refused a run (`sandbox_unavailable`); it is never moved to a weaker backend.
- **`network = false` under Landlock needs ABI 4 and blocks TCP only.** On an older kernel such a project is refused; the container backend (`--network none`) is the real network isolation. SECURITY.md says UDP is not blocked.
- **One container per run, not per project** (deviation from the brief). Two runs of one project must not see each other's worktree, and the container ends with the run. It starts with `--cap-drop ALL --security-opt no-new-privileges` and mounts only the worktree and the project's extra folders.
- **The `none` backend still scrubs the environment** and uses process groups, and keeps the real `HOME`. Because nothing contains it, every edit and command asks, the API refuses `autopilot` (409 `needs_sandbox`, on create and on update), and `patina run --yes` is refused.
- **No resource limits yet** (deviation from section 20, which asks for cgroup v2 limits where a delegated subtree exists). Planned for M8. `patina doctor`, the Settings page and SECURITY.md say "none enforced" instead of implying a limit.
- **Ending a command:** each command is its own process group; on timeout, stop or the end of the run the group gets `SIGTERM`, two seconds, then `SIGKILL`. Processes that left the group (`setsid`) are found by a `PATINA_CMD` marker in their environment and killed. What a foreground command leaves behind is killed when its shell exits; only `background: true` keeps a process, until the run ends.
- **The environment is an allowlist:** `PATH`, `LANG`, `TERM`, a private `HOME` and `TMPDIR` under `data/sandbox/<run>`, and the project's `env`. The private folders are removed when the run is merged or discarded.
- **An allowlist wildcard never matches a command with a shell operator** (`;`, `&`, `|`, `>`, `<`, backticks, `$(`). Such a command matches only a pattern equal to it. The allowlist is documented as a convenience, not a control.
- **"Approve and always allow" saves the pattern before the answer is delivered**, so a failed save never runs the command. Only for `run_command`, one line, at most 500 characters.
- **`ProjectView`** is the project config plus `effective_sandbox` and `sandbox_error`. The API always sends every setting; `projects.toml` keeps only what differs from the defaults. `PATCH /projects/:id` merges keys, `null` resets one, an unknown key is 400.
- **`sandbox_read` and `sandbox_write` may not overlap Patina's config or data folder**, and neither may a project folder.
- **Worktree preparation is part of the run's log:** `worktree_copy` and `worktree_setup` appear as tool calls without an agent. Setup runs inside the sandbox with a ten-minute limit; if it fails, the run fails. What it changes lands in the first checkpoint. A copied file that looks like credentials needs `worktree_copy_secrets`.
- **Patina's git never trusts the worktree** (found in review, with attack tests). A command could plant git metadata that names programs to run (`core.fsmonitor`, filters, hooks), and Patina's own git, outside the sandbox, would have run them. Now git is always given the admin folder and the work tree explicitly, a checkpoint uses plumbing commands only (`ls-files`, `update-index`, `write-tree`, `commit-tree`, `update-ref`), the `.git` pointer is restored and the metadata of nested repositories is removed before each checkpoint (and reported in its summary), diffs are read from commits in the owner's repository, and a worktree is removed by deleting the folder and pruning. Needs git 2.31 or later.
- **A run's own repository is not usable from its commands:** its `.git` folder is outside the sandbox. Agents do not commit; Patina does.
- **File tools open files without following links** (found in review, with attack tests). A background command could swap a folder for a symbolic link between the path check and the open. `Workspace::read` and `Workspace::write` walk from the worktree root with `openat(O_NOFOLLOW)` and accept regular files only. Unix only; on other systems no sandbox backend runs commands next to the tools.
- **`patina doctor`** checks git, the sandbox backends, the config files, the ports, each project's sandbox and the database, creates nothing, prints what is not covered, and exits 1 on a problem. `--json` for scripts. Landlock is reported as "ABI 6 or later" because the crate knows no newer one.
- **Sandbox tests fail instead of skipping in CI:** `PATINA_REQUIRE_LANDLOCK=1` makes a missing Landlock a failure, `PATINA_TEST_CONTAINER=1` runs the container test (needs Docker or Podman and `alpine:3`).

## M3 dependencies (stripped release binary: 10.6 MB, up from 10.2 MB; idle memory 9.4 MB; UI 21.9 KB gzipped)
- landlock 0.4: the Landlock rules. Named by the brief. Linux only.
- rustix 1 (`fs`, `process`, `thread`): process groups and signals, capability drop, no-follow opens. Safe wrappers over the system calls, so `patina-tools` needs no `unsafe`. Unix only. Small.
- tracing and tokio-util in `patina-tools`, serde in the `patina` crate: already in the workspace.
- Test only: Docker or Podman with `alpine:3` for the container test; a Rust toolchain for the build-and-test run.

## M4 choices
- **A run records the process that drives it** (`runs.owner`: pid and start time). A restarted daemon recovers only runs whose owner is dead, so a `patina run` in a terminal keeps its run, and a reused pid is not mistaken for the old process.
- **Pause takes effect before the next model turn.** A command or an approval in progress finishes first, so no call is left half done. Pausing ends the run's background processes, like any end of a run; a resumed agent starts them again if it needs them. The brief does not say; keeping processes across a pause would mean keeping a sandbox alive with no task watching it.
- **Paused is a resumable end, not a live state.** The run's task ends, `run_finished` is logged with `status: paused` and a `reason` (`owner` or `loop`), and Resume starts a new task. Resumable: paused, interrupted, failed, budget_exceeded, and waiting_approval with no task. Not resumable: completed and cancelled (Stop means stop), and runs whose history was pruned.
- **A failed run is resumable** (the brief: a provider failure "fails the run in a resumable state"). A run that failed before its agent started, for example in `worktree_setup`, prepares its worktree again when resumed.
- **Resuming uses the current agent config, project policy and sandbox.** The sandbox is resolved again and fails closed. A budget-stopped run resumes only once its limit leaves room (409 `budget` otherwise).
- **A turn cut off by a shutdown or crash is finished call by call on resume**, from the event log and approvals: a logged result is reused; a call that started and never reported back gets an error result saying it was interrupted and may have run in part (it is never repeated by Patina); an approval answered while no task ran is carried out; an unanswered approval is asked again; a call that never started runs. Approvals record `delivered` when the loop receives the answer, which separates "answered, not yet acted on" from "may have run".
- **A clean shutdown leaves the turn open** instead of writing "not run" results, so a run waiting for an approval keeps waiting (status `waiting_approval`, no `run_finished`), and answering it after the restart resumes the run. Stop still writes results for every open call.
- **A step cut off by a crash is not counted.** Usage is stored with every `usage` event, which comes after the model's answer; a clean shutdown stores the tracker's total, which counts it.
- **Messages to a run** are a new event type, `user_message`, logged when the agent reads them. They are stored in a `steering` table (migration 4) until then. A message to a paused or interrupted run queues it and resumes the run; to a finished run it is refused (409 `finished`); a message that could not be delivered is taken back. A message that arrives as the agent gives its final answer still gets one more turn. Rollback notes now use the same queue, so they cannot split a tool call from its result.
- **Loop breaker:** the same tool name with equal arguments three times in a row, across turns, pauses the run after the third call has its result. The count restarts on resume.
- **Compaction** follows the brief: past 70% of `context_window`, first old large outputs (2 KB or more, older than the latest three turns) become a pointer to an artifact, then the same model summarizes the oldest turns with no tools and a summarizer system prompt, keeping the latest ten turns (fewer only if ten do not fit, down to one). The owner's prompt stays word for word; an earlier summary is folded into the next. History is cut only in front of an assistant message. Token counts are estimated at four characters per token, scaled by what the provider's counts showed (never below 1). A summary request is counted against the token budget, is not a step, and is tried three times; if it cannot be made, the run goes on with a logged error.
- **Retention** deletes events, agent contexts, approvals, delivered messages and artifacts of runs that ended more than `retention_days` ago, at start and every six hours, and marks them `pruned_at`. Run rows, branches, worktrees and checkpoints stay: they are the owner's work. Paused and waiting runs are never pruned. The setting stays in `config.toml`; the Settings page shows it.
- **Shutdown** waits up to 30 seconds for runs to record that they were interrupted, and removes their containers.
- **New routes:** `POST /runs/:id/pause`, `/resume`, `/messages` (as the brief lists). `RunView` gained `can_resume` and `pruned`; `SystemInfo` gained `retention_days`.
- **`patina run` exits with 5** when the run pauses (the loop breaker); it cannot resume, the web UI can.
- **The UI keeps an agent's block across a resume**, and the timeline shows notes for pauses, resumes and compactions, and the owner's messages where the agent read them.

## M4 dependencies (stripped release binary: 10.9 MB, up from 10.6 MB; idle memory 10.1 MB; UI 23.1 KB gzipped)
- None new. Test only: `reqwest` in the `patina` crate's tests, already used by the server tests, to drive a real daemon across a restart.

## M5 choices
- **Every mode is a composition of the one loop** (`patina-core::team`). The engine resolves the team into a roster of agents, each with its provider and tools, before anything is created, so a missing agent or provider refuses the run at once.
- **A run names an agent or a team.** `NewRun` gained `team_id`; the run row keeps `agent:<id>` or `team:<id>`. `RunView` gained `team_id`. A one-agent run is a team in `single` mode, so there is one code path.
- **Delegation is handled by the loop, not a `Tool`.** The `delegate` tool spec is added to the lead's request; the call runs the sub-agent's loop inside it, so a sub-agent that pauses, hits a budget or is interrupted stops the whole run and leaves the lead's call open. On resume the same sub-agent continues (found by `parent` and `tool_call_id` on its `agent_started`). It is always allowed: the sub-agent's own calls go through the policy. Depth: a lead may delegate, its sub-agent once more, no deeper (the brief's "maximum depth 2").
- **Whom an agent may delegate to:** in `delegate` mode the lead may use the agents its `can_delegate_to` names, or else the other members. In `single` and `sequential` mode an agent with `can_delegate_to` gets the tool too. Parallel members never do, because they are read-only. A target that does not exist is left out.
- **Parallel members are read-only** (only `ToolKind::Read` tools), as the brief allows; separate worktrees for parallel writers are not built. The synthesizer may write. A synthesizer cannot also be a member.
- **Sub-agent ids** are `<agent>#<n>`, numbered past every id the run used, so stored histories never collide. Agent ids cannot contain `#`.
- **Hand-offs** say plainly that earlier agents' messages are notes from colleagues, not the owner's instructions.
- **Owner messages** go to one agent: the current member in sequential mode, the lead in delegate mode, the synthesizer in parallel mode (members running at once would race for them). Sub-agents never take them.
- **Pause stays set** until the run ends, so every agent of a team stops; resuming makes fresh signals. (M4 cleared it when one agent saw it.)
- **Resuming a team** skips agents whose last start or finish event says `completed`, or whose history ends with an answer (a crash between the answer and the event). It uses the team's current file; agents are matched by id.
- **Templates** are built in (`patina-core::team_templates`) and name no provider or model. `POST /team-templates/:id` takes a provider and model for every agent, optional per-agent choices, and a team id; agents are named `<team id>-<template agent id>`, so a template can be used for several teams. *Compare models* asks for different models for its two members.
- **Import and export** use one TOML file: `[team]` plus one `[[agent]]` per agent the team can reach, delegation included. It names providers by id and holds no secret. Import checks everything before writing anything; a team or agent that exists with other settings is a 409 unless `overwrite`; identical ones are left alone. Agents whose provider is missing can be given another with `provider`.
- **An agent in a team cannot be deleted** (409 `in_use`) until the team stops naming it.
- **`patina run --team <id>`** runs a team, or the agent with that id when there is no such team. Other agents' text and tool lines are labeled with their id.
- **An empty final answer is flagged.** The live check showed a reasoning model spending a small output limit on thinking and ending the turn with no text, reported as a normal stop. The loop now logs an error that suggests raising `max_output_tokens`; the run still completes. The live smoke test asks for up to 2000 tokens instead of 100 for the same reason.
- **UI (kept minimal, per the guardrail):** a Teams section in Settings (from a template, by hand, import, export, delete), a role and delegation targets on the agent form, a team or agent picker in the composer, and sub-agent blocks nested under the `delegate` call that started them.

## M5 dependencies (stripped release binary: 11.2 MB, up from 10.9 MB; idle memory 10.3 MB; UI 25.4 KB gzipped)
- None new.

## M6 choices
- **Layout.** Three panes on a wide screen: runs, the file tree and a link to the roster on the left; the composer or the timeline in the center; Changes, Editor and Preview tabs on the right. The header holds the project switcher, the project's approval mode, a count of approvals waiting across all runs, the theme switch, and, for the open run, its status, sandbox, usage against its limits, Pause, Stop, Resume, Export and New chat. The composer's own project picker is gone: the header's switcher sets the project for new runs and the tree.
- **Phone.** Under 760 px one pane shows at a time, chosen by a tab row (Run, Browse, Changes, Editor, Preview). Run comes first, the header is not sticky so the timeline gets the room, and controls are at least 40 px tall. An end-to-end test at 360 by 740 starts, steers, approves and merges a run by touch and checks nothing is wider than the screen.
- **Files API.** `GET /projects/:id/tree` lists up to 5000 files, honoring `.gitignore`. `GET` and `PUT /projects/:id/file?path=` read and write one text file of up to 1 MB. Both take `?run=` for the run's worktree. Paths go through the same `Workspace` confinement as the agent's tools, so `..`, absolute paths, `.git` and symbolic links out are refused. Binary files are refused with 415.
- **Editing a run's copy.** Allowed only while no agent is working (409 `run_active`). The save is committed to the run's branch as `patina: owner edit, <path>`, logged as an `owner_edit` event, and the agent gets a note before its next turn, as after a rollback. So the Changes tab, the patch and a merge all include it.
- **`@path` mentions** autocomplete from the project's tree. The daemon, not the browser, reads each mentioned file from the owner's checkout (same confinement, at most 256 KB each and 512 KB together) and appends it to the prompt as `<attached_file path="...">` blocks. The run stores that full prompt, so every agent of a team and a resumed run see the same text. The timeline shows the typed prompt and lists the attachments.
- **Diff view.** The Changes tab keeps the whole patch and adds a CodeMirror unified diff per file, from `GET /runs/:id/diff/file?path=`, which reads the file at the base commit and on the run's branch with `git cat-file` (no filters or textconv).
- **CodeMirror under the CSP.** CodeMirror normally injects a `<style>` element, which `default-src 'self'` blocks. The editor and the diff view are mounted inside a shadow root instead, where CodeMirror uses constructed style sheets, so the CSP stays as section 13 gives it, with no `unsafe-inline`. Colors come from the page's custom properties, which cross the shadow boundary. CodeMirror and each language load on demand: the first load is 32.6 KB gzipped.
- **Preview.** A second listener, on 127.0.0.1 only, reverse-proxies the dev server of the one run the owner selects. Each run gets a free port for the daemon's lifetime; its commands get it as `PORT`, and `{port}` in the project's new `dev_command` becomes it. Under Landlock with the network off, commands may still listen on that one port (a `NetPort` bind rule); the container backend publishes it on the host's loopback when the network is on. The owner starts and stops `dev_command` from the Preview tab; it runs in the run's sandbox (a separate container, for the container backend) and is stopped on merge, discard, another selection and shutdown. The frame is sandboxed without `allow-same-origin`, and the app's CSP gains `frame-src` for the preview origin only. `preview_origin` in `config.toml` names it when a tunnel serves it elsewhere. The frame reloads after every step that changes files.
- **The preview has no login of its own.** Cookies are shared across ports, but a sandboxed frame's requests are cross-site, so the strict session cookie would not reach the proxy anyway, and third-party cookie blocking would break any other cookie. Instead the listener is loopback-only whatever `--bind` says, checks `Host` against `allowed_hosts`, serves nothing until a run is selected, forwards no `Cookie`, `Authorization` or `X-Patina-*` header, and passes on no `Set-Cookie` or `Access-Control-*` header, so other web pages cannot read it and the dev server cannot set cookies for the app's host. Remote use goes through `tailscale serve`, like the app.
- **No WebSocket in the proxy.** Hot reload through the preview does not work; the frame reloads after each changed step instead.
- **Run export.** `GET /runs/:id/export?format=md` renders the prompt, each agent's text, tool calls with arguments, results (clipped at 4000 bytes), approvals, messages, checkpoints, edits and errors, leaving out text of retried attempts. `format=json` is the run view and every event. Events never hold secrets, so neither does an export.
- **Theme.** Dark by default, with a Light switch in the header, remembered in `localStorage` with the last project. Nothing else is stored in the browser.
- **`patina serve --preview-port`** overrides `preview_port`; 0 picks a free port, which tests use. `preview_port = 0` in `config.toml` turns the preview off. A preview port that is taken turns the preview off with a warning instead of stopping the daemon.

## M6 dependencies (stripped release binary: 12.1 MB, up from 11.2 MB; idle memory 10.8 MB; UI first load 32.6 KB gzipped, all assets 271 KB)
- CodeMirror 6, as section 4 names it: `@codemirror/state`, `view`, `commands`, `language`, `search`, `merge`, `@lezer/highlight`, and the languages JavaScript and TypeScript, JSON, Rust, Python, Markdown, CSS and HTML. Exact versions, all over two weeks old. Loaded on demand.
- `reqwest` with `stream` in `patina-server`, for the preview proxy. It was already in the binary for providers.

## M7 choices
- **Client.** `rmcp` 3.10, the official Rust SDK, with client features only: `client`, `transport-child-process`, `transport-streamable-http-client-reqwest` and its rustls `reqwest` feature. No server, macros or OAuth code is in the binary. The connection uses the `initialize` handshake (rmcp's default), which every server in use today accepts; the newer `server/discover` start is not tried.
- **`mcp.toml`.** One `[[server]]` per server: `id`, `transport` (`stdio` or `http`), `command`, `args`, `cwd`, `env` for stdio, `url`, `headers` for HTTP, `trusted_tools`, `timeout_s` (120 by default, at most 600) and `enabled`. Server ids are at most 32 characters and may not contain `__`, so a tool name splits back into server and tool unambiguously.
- **Tool names.** A model sees `mcp__<server>__<tool>`. Characters a model API refuses become `_`, and the name is cut at 64 characters; a tool whose cleaned name clashes with an earlier one of the same server is left out with a warning.
- **Per-agent opt-in** reuses the agent's `tools` list: `mcp__<server>` grants every tool of a server, `mcp__<server>__<tool>` one tool. MCP tools work without a project, in a plain chat.
- **Trust.** A new `ToolKind::External { trusted }`. The policy decides it by trust alone: an untrusted MCP tool asks in every mode, including Autopilot, and a trusted one never asks, also without a sandbox, because the sandbox never contained it. Trust is per tool, by the server's own tool name, stored in `trusted_tools` (`*` for all). It is read at each call, so trusting a tool from the approval card ("Approve and trust this tool", `trust_tool` on the answer) applies to the run at once. Settings lists the tools with a trust checkbox.
- **One connection per server, shared by all runs.** `McpHub` connects on first use and reconnects when the connection closed or the server's settings changed. A failed call is not retried, since a tool may have side effects; the next call reconnects.
- **Starting a run.** The MCP servers an agent names are reached before the run is created. If one cannot be reached, the run does not start (502 `mcp_unavailable`, naming the server). A missing or disabled server gives no tools, with a warning in the log. The same applies on resume.
- **Stdio servers** get an empty environment plus `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_ALL`, `TZ` and `TMPDIR` (on Windows: `PATH`, `PATHEXT`, `SystemRoot`, `SystemDrive`, `ComSpec`, `TEMP`, `TMP`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, `ProgramData`, `ProgramFiles`, `USERNAME`), and the server's `env`. A server without `PATH` and `HOME` could not start `npx` or `uvx`, which most servers use. Each runs in its own process group (a job object on Windows). Ending a connection closes its input, waits two seconds as the protocol asks, then sends SIGTERM and SIGKILL to what is left of the group, so a child the server started does not outlive it. Its error output is kept (the last 4 KB) to explain a failed start.
- **Values.** `env` and `headers` values are literal text, `env:<VAR>` or `secret:<name>`. Anything typed in the UI that is not `env:` is stored as the secret `mcp/<server>/<name>`, so tokens never land in `mcp.toml` in clear text, and the API shows only names and sources. Secrets no longer used are deleted on save and with the server. Headers the transport owns (`Accept`, `Mcp-Session-Id` and others) are refused.
- **Results.** Text content is passed on; images, audio and binary resources are described in one line; embedded text resources are included with their URI; with no content, `structuredContent` is passed as JSON. Each result starts with a line saying it comes from outside Patina and is data, not instructions. The loop's usual 30 KB cut applies. `isError` makes the result an error for the model.
- **Schemas.** The server's input schema is passed on as given, without `$schema`, and with `type: object` and `properties` added when missing. Patina's argument check validates the subset it knows and ignores the rest.
- **Not in v1:** MCP resources, prompts, sampling and elicitation (a server that asks for them gets rmcp's default refusal); OAuth for remote servers; the legacy SSE transport.
- **Tests.** The client is tested against a stdio server written in Python without any MCP library (`crates/patina-tools/tests/fixtures/mcp_stdio_server.py`, speaking protocol 2025-06-18) and a Streamable HTTP server made with rmcp's own server side, in-process. CI's Ubuntu image has Python; the stdio tests skip with a message where it is missing. A browser test drives the whole flow from Settings to a trusted second call.

## M7 dependencies (stripped release binary: 14.2 MB, up from 12.1 MB; idle memory 11.2 MB; UI first load 34.6 KB gzipped, all assets 271 KB)
- `rmcp` 3.10 (Apache-2.0), client features only, as section 4 names it. Most of the size increase is its protocol model.
- `process-wrap` 10 (Apache-2.0 or MIT), already a dependency of rmcp's stdio transport, used directly to start a server in its own process group or job object.
- `http` 1, already in the tree, for header names and values.
- Test only: rmcp's server side and macros, to run a Streamable HTTP server in the tests. Not in the binary.

## M8 choices
- **Resource limits** are a project setting, `limits = { memory_mb, processes, cpus, file_size_mb }`, with generous defaults (8192 MiB, 2048 processes and threads, no CPU cap, 8192 MiB per file) and `0` to turn one off. They stop a runaway command, not a normal build.
- **cgroup v2 where delegated.** At start, `serve` and `run` call `cgroup::init()`. It acts only when `/sys/fs/cgroup` is the unified hierarchy, the process's own cgroup is writable by it, it has at least one of `memory`, `pids` and `cpu`, and no other process shares it, which is what a systemd unit with `Delegate=yes` gives. It moves the daemon into `<own>/daemon` (a cgroup with processes cannot give controllers to children), enables the controllers for `<own>` and `<own>/commands`, and removes leftovers of an earlier daemon. Each command gets `<own>/commands/<marker>` with `memory.max`, `memory.swap.max = 0` (so a command that runs out is stopped instead of paging the host), `pids.max` and `cpu.max`; the child joins it between fork and exec by writing `0` to its `cgroup.procs`, opened beforehand. Stopping a command writes `cgroup.kill` (Linux 5.14; before that each listed process is killed), which reaches what left the group and cleared the marker. A cgroup that cannot be created fails the command: it never runs without the limits it was given. Any failing setup step is undone and the daemon runs without cgroup limits, saying why in its log and in `patina doctor`.
- **Container backend:** `--memory` and `--memory-swap` (equal), `--pids-limit`, `--cpus` and `--ulimit fsize`, on the run's container, so its commands share them.
- **`RLIMIT_FSIZE`** for every Landlock and unsandboxed command on Unix, capped at the daemon's own hard limit. `RLIMIT_AS` and `RLIMIT_NPROC` are not used, as section 20 says.
- **Only what is enforced is listed.** `SandboxStatus.resource_limits` names each mechanism in force on this host (cgroup v2 with its controllers, the file size cap, the container backend), for the badge and `patina doctor`. `doctor` runs as its own process, so it reports whether its own cgroup is delegated, which predicts a daemon started the same way.
- **Redaction.** Every value in the secret store and every variable a provider or MCP server takes with `env:` is replaced with `[secret removed by Patina]` in tool results (in the agent loop, before the event, the history and the model see them), in the `worktree_setup` log and in the preview's dev server log. Values under eight characters are skipped. The set is read when a run starts, so a key added during a run is covered from the next run.
- **Security suite** (`crates/patina-server/tests/security.rs`). It reads `router()` from `src/lib.rs`, one `.route(...)` per line, and checks every route and method: 401 without a session; 403 for a change without the CSRF token, with a wrong one, from another Origin or without one; 403 for a foreign `Host`; and no stored provider key or MCP secret in any GET response. A parser check fails if fewer than 55 routes are found. It found that the sign-in routes judged a cross-site request's body (422) before its Origin; they now pass through a `same_origin` layer first.
- **`patina export <run id>`** writes the same Markdown or JSON as the UI's Export, to stdout or `--output`. The last characters of an id are enough when one run matches. It reads the folders without creating anything.
- **`patina version`** prints the version, the commit (`PATINA_COMMIT` at build time, set by the release workflow) and the target. `--check` asks `https://api.github.com/repos/torics/Patina/releases/latest` (`PATINA_RELEASE_API` at build time or `PATINA_RELEASE_URL` at run time to change it) and compares versions by number. It is the only outbound call that is not configured, and only on demand; the daemon never makes it. While the repository is private the feed answers 404, and the message says so.
- **Binary size.** `cargo bloat --profile release-symbols --crates`: of 9.5 MiB of code, rmcp 1.1 MiB (serde code for its protocol model), std 0.8, patina-server 0.7, genai 0.66, similar 0.57, axum 0.48, patina-tools 0.46, aws-lc 0.41, patina-core 0.37, rustls 0.33, reqwest 0.32, regex 0.45 with its parts. No single cut saves much without dropping a feature, and the binary is inside the goal (under 20 MB, aim 8 to 15), so nothing was trimmed.
- **Release workflow** (`.github/workflows/release.yml`), a hand-written matrix rather than cargo-dist: it is short, needs no generated config to keep in step, and does not depend on a tool whose upkeep is uncertain. On a `v*` tag: the UI is built once; Linux x86_64 and ARM64 are static musl builds with `cargo zigbuild` (this answers the M7 risk: aws-lc-rs builds and the binary runs, with no switch to ring); macOS x86_64 and ARM64 build on a macOS runner; Windows x86_64 with MSVC. Each archive (`patina-<target>.tar.gz`, `.zip` on Windows, without the version so that `releases/latest/download/` links work) holds the binary, the README, licenses, `SECURITY.md`, the systemd unit and the notices. Then `SHA256SUMS`, `install.sh`, a GitHub release with generated notes, after checking that the tag matches `Cargo.toml`, and a multi-arch image `ghcr.io/<owner>/patina` (Alpine, git, tini; it binds to loopback, so it runs with `--network host`). A manual run builds and checks without publishing.
- **Third-party notices:** `cargo about` with the license list from `deny.toml` writes `THIRD_PARTY_NOTICES.html` for the crates in the binary; `scripts/ui-notices.mjs` writes `THIRD_PARTY_NOTICES_UI.txt` with the license text of every package bundled in the UI.
- **Install script** (`packaging/install.sh`): POSIX sh, Linux and macOS, picks the target, downloads the archive and `SHA256SUMS`, refuses a mismatch, installs to `~/.local/bin`. No code signing yet: `docs/install.md` covers checksums, macOS quarantine and Windows SmartScreen.
- **Docs:** `docs/install.md` (download to first run), `providers.md`, `teams.md`, `remote.md` (Tailscale, `allowed_hosts`, `preview_origin`), `systemd.md` with `packaging/patina.service` (`Delegate=yes`, `KillMode=mixed`), `backup.md`. `patina doctor` now prints the config and data folders it uses.

## M8 dependencies (stripped release binary: 14.3 MB, up from 14.2 MB; idle memory 11.4 MB; UI first load 35.1 KB gzipped, all assets 278 KB)
- `reqwest` becomes a direct dependency of the `patina` binary crate for `version --check`. It was already linked through the providers, so the size did not change.
- Build tools only, not in the binary: `cargo-about` 0.9 and `cargo-zigbuild` with Zig in the release workflow, `cargo-bloat` for the size review.

## Windows choices

- On Windows a command runs with `cmd /C`, and `run_command`'s description tells the model so (and whether a sandbox contains it), so it writes `dir` and `type`, not `ls` and `cat`. Git Bash or PowerShell would suit models better but are not always installed; `cmd` always is.
- Without a sandbox, a command on Windows keeps the variables a program needs to start (`SystemRoot`, `ComSpec`, `PATHEXT`, the profile folders and a few more); `TEMP` and `TMP` point at the run's temp folder, like `TMPDIR`. None of them holds a secret.
- File paths in the API and in tool results always use `/`, so a path the UI or an agent got back can be sent again and compared with git's.
- A run's owning process is identified by its PID and creation time on Windows too, so a live process is never mistaken for a dead one at the next start.
- Ctrl+C, closing the console window, signing out and shutting down stop the daemon cleanly; running runs are recorded as interrupted.
- doctor, Settings and the sandbox summary say on Windows that Landlock and cgroup limits are not available on that operating system, rather than "on this host".

## Open risks
- Live checks passed on 2026-11-15 for Gemini (streaming, a tool call, reasoning replay) and for a hosted OpenAI-compatible endpoint (Meta's Muse API: streaming and a tool call). Anthropic, OpenAI and a local endpoint have not been checked live yet.
- Memory, process and CPU limits per command need a delegated cgroup (a systemd unit with `Delegate=yes`). Started from a shell, Patina caps only file size outside a container, and says so. The cgroup code is tested in CI only (a scope from `systemd-run`, as root); this build machine has the legacy hierarchy.
- Under the container backend a rootless runtime without delegated controllers cannot apply `--memory` or `--cpus`, so the container does not start; the error says why, and setting the limit to 0 goes on without it.
- Redaction matches exact values only. A secret that a command encodes (base64, split across lines) is not caught.
- Under Landlock a command can read `/proc` (names and command lines of other processes), can use UDP even with `network = false`, and before ABI 6 can signal other processes.
- While a background command swaps folders for links, `list_files` and `search` can list file names outside the worktree, never content.
- Windows has no process-tree kill and no no-follow open yet; the container backend is the only sandbox there and is untested on Windows.
- The container backend is tested against Docker only, not Podman.
- Resuming after a crash relies on the event log describing each tool call. A tool whose effect lands outside the worktree (a command that calls a remote API) may have taken effect before the crash; the agent is told the call was interrupted and may have run in part.
- Compaction quality depends on the model's summary. It is tested with scripted models only.
- Team modes are tested with scripted models and fake endpoints. How well real models use `delegate` and hand-offs is not measured.
- Members of a parallel team that share a provider share its concurrency limit, so they may run one after another.
- The preview proxy has no WebSocket support, so dev servers' hot reload does not reach the frame. Pages that need their origin (storage, cookies, service workers) do not work in the sandboxed frame; "Open in a new tab" shows them unsandboxed on the preview origin.
- A preview container left behind by a crash is not removed on restart; `docker rm -f patina-<run>-preview` does it.
- Under the container backend, a dev server must listen on all of the container's addresses (for example `--host 0.0.0.0`) to be reachable through the published port.
- MCP is tested against a hand-written stdio server and rmcp's own HTTP server, not yet against third-party servers or a real model choosing MCP tools. Gemini rejects some JSON Schema keywords; a server whose schema uses them may fail with that provider.
- A stdio MCP server's own children that leave its process group are not stopped with it, and on Windows the job object is untested.
- Windows x64 is built and tested in CI on every push: clippy, the Rust tests and the browser tests against `patina.exe` (the browser test of a sandboxed command is skipped, since Windows has no sandbox). The Windows binary has no process-tree kill and no sandbox but the container backend, which is untested there. macOS builds compile in the release workflow only. Binaries are not code-signed.
- `patina version --check` and the install script's links need the repository to be public; while it is private, use `gh release download`.
