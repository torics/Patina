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
- **UI (kept minimal, per the guardrail):** a Teams section in Settings (from a template, by hand, import, export, delete), a role and delegation targets on the agent form, a team or agent picker in the composer, and sub-agent blocks nested under the `delegate` call that started them.

## M5 dependencies (stripped release binary: 11.2 MB, up from 10.9 MB; idle memory 10.3 MB; UI 25.4 KB gzipped)
- None new.

## Open risks
- rustls uses aws-lc-rs, which needs a C toolchain when cross-compiling. Check static musl builds early in M8, and switch to the ring provider if needed.
- Live checks: Gemini passed (streaming, a tool call, reasoning replay) on 2026-11-15. Anthropic, OpenAI and a local OpenAI-compatible endpoint have not been checked live yet.
- Commands have no memory, CPU or process-count limit until M8. A command can slow the machine until its timeout.
- Under Landlock a command can read `/proc` (names and command lines of other processes), can use UDP even with `network = false`, and before ABI 6 can signal other processes.
- While a background command swaps folders for links, `list_files` and `search` can list file names outside the worktree, never content.
- Windows has no process-tree kill and no no-follow open yet; the container backend is the only sandbox there and is untested on Windows.
- The container backend is tested against Docker only, not Podman.
- Resuming after a crash relies on the event log describing each tool call. A tool whose effect lands outside the worktree (a command that calls a remote API) may have taken effect before the crash; the agent is told the call was interrupted and may have run in part.
- Compaction quality depends on the model's summary. It is tested with scripted models only.
- Team modes are tested with scripted models and fake endpoints. How well real models use `delegate` and hand-offs is not measured.
- Members of a parallel team that share a provider share its concurrency limit, so they may run one after another.
