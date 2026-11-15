import { useEffect, useState } from "preact/hooks";
import type { AgentConfig } from "./api/generated/AgentConfig";
import type { McpServerView } from "./api/generated/McpServerView";
import type { McpToolView } from "./api/generated/McpToolView";
import type { McpTransport } from "./api/generated/McpTransport";
import type { McpValueInput } from "./api/generated/McpValueInput";
import type { McpValueView } from "./api/generated/McpValueView";
import type { PolicyMode } from "./api/generated/PolicyMode";
import type { PresetView } from "./api/generated/PresetView";
import type { ProjectView } from "./api/generated/ProjectView";
import type { ResourceLimits } from "./api/generated/ResourceLimits";
import type { SandboxChoice } from "./api/generated/SandboxChoice";
import type { ProviderKind } from "./api/generated/ProviderKind";
import type { ProviderTestResult } from "./api/generated/ProviderTestResult";
import type { ProviderView } from "./api/generated/ProviderView";
import type { TeamConfig } from "./api/generated/TeamConfig";
import type { TeamMode } from "./api/generated/TeamMode";
import type { TemplateView } from "./api/generated/TemplateView";
import { api, ApiError } from "./api";
import { lines, looksLikeCredentials, parseEnv } from "./policy";
import { agents, mcpServers, projects, providers, refresh, report, retentionDays, sandbox, teams, templates, toolNames, view } from "./state";

const KINDS: ProviderKind[] = ["openai_compatible", "openai", "anthropic", "gemini", "ollama"];

function slug(s: string) {
  return s.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
}

function ProviderForm({ existing, onDone }: { existing?: ProviderView; onDone: () => void }) {
  const [presets, setPresets] = useState<PresetView[]>([]);
  const [id, setId] = useState(existing?.id ?? "");
  const [kind, setKind] = useState<ProviderKind>(existing?.kind ?? "openai_compatible");
  const [baseUrl, setBaseUrl] = useState(existing?.base_url ?? "");
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (!existing) api.presets().then(setPresets).catch(report);
  }, []);

  function pick(name: string) {
    const p = presets.find((x) => x.name === name);
    if (!p) return;
    setKind(p.kind);
    setBaseUrl(p.base_url ?? "");
    if (!id) setId(slug(p.name));
  }

  async function save(e: Event) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const input = { id, kind, base_url: baseUrl || null, max_concurrency: null, idle_timeout_s: null, prices: null, api_key_env: null };
      if (existing) await api.updateProvider(input);
      else await api.createProvider(input);
      if (key.trim()) await api.setSecret(id, key.trim());
      setKey("");
      await refresh();
      onDone();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form class="card" onSubmit={save}>
      <h3>{existing ? `Edit ${existing.id}` : "Add a provider"}</h3>
      {!existing && (
        <label>
          Start from a preset
          <select name="preset" onChange={(e) => pick(e.currentTarget.value)}>
            <option value="">Choose…</option>
            {presets.map((p) => (
              <option value={p.name}>{p.name}</option>
            ))}
          </select>
        </label>
      )}
      <label>
        Name (id)
        <input name="id" value={id} onInput={(e) => setId(e.currentTarget.value)} disabled={!!existing} pattern="[A-Za-z0-9_\-]+" required />
      </label>
      <label>
        Protocol
        <select name="kind" value={kind} onChange={(e) => setKind(e.currentTarget.value as ProviderKind)}>
          {KINDS.map((k) => (
            <option value={k}>{k.replace("_", " ")}</option>
          ))}
        </select>
      </label>
      <label>
        Base URL {kind === "openai_compatible" ? "" : "(optional)"}
        <input name="base_url" value={baseUrl} onInput={(e) => setBaseUrl(e.currentTarget.value)} placeholder="http://192.168.1.20:11434/v1" required={kind === "openai_compatible"} />
      </label>
      <label>
        API key {existing?.has_key ? `(stored, ends in ${existing.key_hint ?? "…"}; type to replace)` : "(leave empty if the endpoint needs none)"}
        <input name="key" type="password" value={key} onInput={(e) => setKey(e.currentTarget.value)} autocomplete="off" />
      </label>
      {error && <p class="error">{error}</p>}
      <div class="row">
        <button type="submit" disabled={busy}>
          Save
        </button>
        <button type="button" class="link" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function ProviderRow({ p }: { p: ProviderView }) {
  const [editing, setEditing] = useState(false);
  const [result, setResult] = useState<ProviderTestResult | null>(null);
  const [model, setModel] = useState("");
  const [busy, setBusy] = useState(false);
  async function test() {
    setBusy(true);
    try {
      setResult(await api.testProvider(p.id, model.trim() || null));
    } catch (e) {
      report(e);
    } finally {
      setBusy(false);
    }
  }
  if (editing) return <ProviderForm existing={p} onDone={() => setEditing(false)} />;
  return (
    <li class="card">
      <div class="row">
        <strong>{p.id}</strong>
        <span class="muted">
          {p.kind.replace("_", " ")} {p.base_url ?? ""}
        </span>
      </div>
      <p class="muted">
        Key: {p.key_source === "none" ? "none" : p.key_source === "env" ? `from $${p.key_env}${p.has_key ? "" : " (not set)"}` : p.has_key ? `stored, ends in ${p.key_hint}` : "missing"}
      </p>
      <div class="row">
        <input aria-label={`Model to test on ${p.id}`} placeholder="model id (optional)" value={model} onInput={(e) => setModel(e.currentTarget.value)} list={`models-${p.id}`} />
        <datalist id={`models-${p.id}`}>
          {(result?.models ?? []).map((m) => (
            <option value={m} />
          ))}
        </datalist>
        <button onClick={test} disabled={busy}>
          Test connection
        </button>
        <button class="link" onClick={() => setEditing(true)}>
          Edit
        </button>
        <button
          class="link danger"
          onClick={() =>
            api
              .deleteProvider(p.id)
              .then(refresh)
              .catch(report)
          }
        >
          Delete
        </button>
      </div>
      {result && (
        <p class={result.ok ? "ok" : "error"} role="status">
          {result.ok ? "Connection works." : "Connection failed."}{" "}
          {result.models_error ? `Model list: ${result.models_error}. ` : `${result.models.length} models listed. `}
          {result.reply != null && `Reply: “${result.reply}”. `}
          {result.chat_error && `Chat: ${result.chat_error}`}
        </p>
      )}
    </li>
  );
}

function AgentForm({ existing, onDone }: { existing?: AgentConfig; onDone: () => void }) {
  const [name, setName] = useState(existing?.name ?? "Assistant");
  const [provider, setProvider] = useState(existing?.provider ?? providers.value[0]?.id ?? "");
  const [model, setModel] = useState(existing?.model ?? "");
  const [models, setModels] = useState<string[]>([]);
  const [system, setSystem] = useState(existing?.system_prompt ?? "");
  const [role, setRole] = useState(existing?.role ?? "");
  const [delegates, setDelegates] = useState<string[]>(existing?.can_delegate_to ?? []);
  const others = agents.value.filter((a) => a.id !== existing?.id);
  // A new agent starts with every tool; tools only do anything in a run that has a project.
  const [tools, setTools] = useState<string[]>(existing?.tools ?? toolNames.value);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    setModels([]);
    if (provider) api.models(provider).then(setModels).catch(() => setModels([]));
  }, [provider]);
  async function save(e: Event) {
    e.preventDefault();
    setError(null);
    try {
      const fields = {
        name,
        role,
        provider,
        model: model.trim(),
        system_prompt: system,
        tools: [...toolNames.value.filter((t) => tools.includes(t)), ...tools.filter((t) => t.startsWith("mcp__"))],
        can_delegate_to: others.map((a) => a.id).filter((id) => delegates.includes(id)),
      };
      if (existing) await api.updateAgent({ ...existing, ...fields });
      else await api.createAgent({ id: slug(name), ...fields });
      await refresh();
      onDone();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }
  return (
    <form class="card" onSubmit={save}>
      <h3>{existing ? `Edit ${existing.name}` : "Add an agent"}</h3>
      <label>
        Name
        <input name="name" value={name} onInput={(e) => setName(e.currentTarget.value)} required />
      </label>
      <label>
        Provider
        <select name="provider" value={provider} onChange={(e) => setProvider(e.currentTarget.value)} required>
          {providers.value.map((p) => (
            <option value={p.id}>{p.id}</option>
          ))}
        </select>
      </label>
      <label>
        Model id {models.length ? `(${models.length} offered by the provider)` : "(type it; the provider offered no list)"}
        <input name="model" value={model} onInput={(e) => setModel(e.currentTarget.value)} list="agent-models" required />
        <datalist id="agent-models">
          {models.map((m) => (
            <option value={m} />
          ))}
        </datalist>
      </label>
      <label>
        Role (optional; other agents of a team see it)
        <input name="role" value={role} onInput={(e) => setRole(e.currentTarget.value)} />
      </label>
      <label>
        System prompt (optional)
        <textarea name="system" rows={3} value={system} onInput={(e) => setSystem(e.currentTarget.value)} />
      </label>
      {others.length > 0 && (
        <fieldset>
          <legend>Agents it may hand tasks to (with the delegate tool)</legend>
          {others.map((a) => (
            <label class="check">
              <input type="checkbox" checked={delegates.includes(a.id)} onChange={(e) => setDelegates(e.currentTarget.checked ? [...delegates, a.id] : delegates.filter((x) => x !== a.id))} />
              {a.name}
            </label>
          ))}
        </fieldset>
      )}
      <fieldset>
        <legend>Tools the agent may use in a project</legend>
        {toolNames.value.map((t) => (
          <label class="check">
            <input type="checkbox" checked={tools.includes(t)} onChange={(e) => setTools(e.currentTarget.checked ? [...tools, t] : tools.filter((x) => x !== t))} />
            {t}
          </label>
        ))}
      </fieldset>
      {mcpServers.value.length > 0 && <McpToolPicker tools={tools} setTools={setTools} />}
      {error && <p class="error">{error}</p>}
      <div class="row">
        <button type="submit">Save</button>
        <button type="button" class="link" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function AgentRow({ a }: { a: AgentConfig }) {
  const [editing, setEditing] = useState(false);
  if (editing) return <AgentForm existing={a} onDone={() => setEditing(false)} />;
  return (
    <li class="card row">
      <strong>{a.name}</strong>
      <span class="muted">
        {a.provider} · {a.model} · {a.tools.length ? `tools: ${a.tools.join(", ")}` : "no tools"}
      </span>
      <button class="link" onClick={() => setEditing(true)} aria-label={`Edit ${a.name}`}>
        Edit
      </button>
      <button
        class="link danger"
        aria-label={`Delete ${a.name}`}
        onClick={() =>
          api
            .deleteAgent(a.id)
            .then(refresh)
            .catch(report)
        }
      >
        Delete
      </button>
    </li>
  );
}

export const POLICIES: { value: PolicyMode; label: string }[] = [
  { value: "confirm_all", label: "Ask before every edit" },
  { value: "auto_edit", label: "Apply edits without asking" },
  { value: "autopilot", label: "Never ask" },
];

/** The policies to offer. "Never ask" is offered only where commands are contained; a project that already has it keeps showing it. */
export function policyChoices(contained: boolean, current?: PolicyMode) {
  return POLICIES.filter((p) => p.value !== "autopilot" || contained || current === "autopilot");
}

const SANDBOXES: { value: SandboxChoice; label: string }[] = [
  { value: "auto", label: "Automatic (the strongest one this machine has)" },
  { value: "landlock", label: "Landlock" },
  { value: "container", label: "Container" },
  { value: "none", label: "None (every edit and command asks)" },
];

const LEVEL_NAME: Record<string, string> = { landlock: "Landlock", container: "a container", none: "no sandbox" };

/** What contains commands on the machine that runs Patina. Only what was detected is listed. */
function SandboxCard() {
  const s = sandbox.value;
  if (!s) return null;
  return (
    <div class="card" role="region" aria-label="Sandbox">
      <h3>
        Sandbox on this machine: <span class={`badge ${s.level === "none" ? "off" : "on"}`}>{LEVEL_NAME[s.level] ?? s.level}</span>
      </h3>
      <p>{s.summary}</p>
      <ul class="facts">
        <li>Landlock file confinement: {s.landlock_abi === null ? "not available on this machine" : `available (ABI ${s.landlock_abi}${s.landlock_abi >= 6 ? " or later" : ""})`}</li>
        <li>Blocking TCP for a project without network: {s.tcp_block ? "available. UDP is never blocked, so this is not full network isolation" : s.landlock_abi === null ? "not available (it needs Landlock)" : "not available under Landlock on this kernel"}</li>
        <li>Container runtime: {s.container_runtime ?? "none found"}</li>
        <li>Resource limits enforced here: {s.resource_limits.length ? s.resource_limits.join("; ") : "none"}</li>
      </ul>
      <p class="muted">
        Run history: {retentionDays.value === 0 ? "kept until you delete it." : `the logs of runs that ended more than ${retentionDays.value} days ago are removed. Their changes and checkpoints stay.`} Set{" "}
        <code>retention_days</code> in <code>config.toml</code> to change this.
      </p>
    </div>
  );
}

function ProjectForm({ onDone }: { onDone: () => void }) {
  const [path, setPath] = useState("");
  const [name, setName] = useState("");
  const [policy, setPolicy] = useState<PolicyMode>("auto_edit");
  const [offerInit, setOfferInit] = useState(false);
  const [initGit, setInitGit] = useState(false);
  const [error, setError] = useState<string | null>(null);
  async function save(e: Event) {
    e.preventDefault();
    setError(null);
    try {
      await api.createProject({ id: null, name: name.trim() || null, path: path.trim(), policy, init_git: initGit });
      await refresh();
      onDone();
    } catch (err) {
      // A folder that is not a repository yet can become one, but only if the owner says so.
      if (err instanceof ApiError && err.code === "not_a_repository") setOfferInit(true);
      setError(err instanceof Error ? err.message : String(err));
    }
  }
  return (
    <form class="card" onSubmit={save}>
      <h3>Add a project</h3>
      <label>
        Folder (an absolute path on the machine that runs Patina)
        <input name="path" value={path} onInput={(e) => setPath(e.currentTarget.value)} placeholder="/home/you/code/my-project" required />
      </label>
      <label>
        Name (optional)
        <input name="project_name" value={name} onInput={(e) => setName(e.currentTarget.value)} />
      </label>
      <label>
        Approvals
        <select name="policy" value={policy} onChange={(e) => setPolicy(e.currentTarget.value as PolicyMode)}>
          {policyChoices(sandbox.value !== null && sandbox.value.level !== "none").map((p) => (
            <option value={p.value}>{p.label}</option>
          ))}
        </select>
      </label>
      {offerInit && (
        <label class="check">
          <input type="checkbox" checked={initGit} onChange={(e) => setInitGit(e.currentTarget.checked)} />
          Create a git repository in that folder
        </label>
      )}
      {error && <p class="error">{error}</p>}
      <div class="row">
        <button type="submit">Save</button>
        <button type="button" class="link" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}

/** The sandbox, command and worktree settings of one project. */
function ProjectSettings({ p, saved, setSaved }: { p: ProjectView; saved: boolean; setSaved: (v: boolean) => void }) {
  const [choice, setChoice] = useState<SandboxChoice>(p.sandbox);
  const [image, setImage] = useState(p.container_image ?? "");
  const [network, setNetwork] = useState(p.network);
  const [allow, setAllow] = useState(p.allow_commands.join("\n"));
  const [env, setEnv] = useState(
    Object.entries(p.env)
      .map(([k, v]) => `${k}=${v}`)
      .join("\n"),
  );
  const [read, setRead] = useState(p.sandbox_read.join("\n"));
  const [write, setWrite] = useState(p.sandbox_write.join("\n"));
  const [copy, setCopy] = useState(p.worktree_copy.join("\n"));
  const [secrets, setSecrets] = useState<string[]>(p.worktree_copy_secrets);
  const [setup, setSetup] = useState(p.worktree_setup ?? "");
  const [dev, setDev] = useState(p.dev_command ?? "");
  const [limits, setLimits] = useState(p.limits);
  const limit = (key: keyof ResourceLimits) => (e: { currentTarget: { value: string } }) => {
    const n = Number(e.currentTarget.value);
    setLimits({ ...limits, [key]: Number.isFinite(n) && n > 0 ? n : 0 });
    setSaved(false);
  };
  const [error, setError] = useState<string | null>(null);
  const risky = lines(copy).filter(looksLikeCredentials);
  const input = (set: (v: string) => void) => (e: { currentTarget: { value: string } }) => (set(e.currentTarget.value), setSaved(false));
  async function save(e: Event) {
    e.preventDefault();
    setError(null);
    const parsed = parseEnv(env);
    if (parsed.bad !== null) return setError(`"${parsed.bad}" is not in the form NAME=value.`);
    const copied = lines(copy);
    try {
      await api.updateProject(p.id, {
        sandbox: choice,
        container_image: image.trim() || null,
        network,
        allow_commands: lines(allow),
        env: parsed.env,
        sandbox_read: lines(read),
        sandbox_write: lines(write),
        worktree_copy: copied,
        worktree_copy_secrets: secrets.filter((s) => copied.includes(s)),
        worktree_setup: setup.trim() || null,
        dev_command: dev.trim() || null,
        limits,
      });
      await refresh();
      setSaved(true);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }
  return (
      <form onSubmit={save}>
        <label>
          Sandbox
          <select name="sandbox" value={choice} onChange={(e) => (setChoice(e.currentTarget.value as SandboxChoice), setSaved(false))}>
            {SANDBOXES.map((o) => (
              <option value={o.value}>{o.label}</option>
            ))}
          </select>
        </label>
        {(choice === "container" || choice === "auto") && (
          <label>
            Container image{choice === "auto" ? " (used only when this machine has no Landlock)" : ""}
            <input name="container_image" value={image} onInput={input(setImage)} placeholder="docker.io/library/node:22" />
          </label>
        )}
        <label class="check">
          <input type="checkbox" name="network" checked={network} onChange={(e) => (setNetwork(e.currentTarget.checked), setSaved(false))} />
          Commands may use the network
        </label>
        {!network && p.effective_sandbox === "landlock" && <p class="muted">Under Landlock this blocks TCP connections. UDP is not blocked.</p>}
        <label>
          Commands that run without asking, one pattern per line. <code>*</code> stands for anything.
          <textarea name="allow_commands" rows={3} value={allow} onInput={input(setAllow)} placeholder={"cargo test*\nnpm run *"} />
        </label>
        <label>
          Environment variables for commands, one NAME=value per line. Never put a secret here: the agent can read them.
          <textarea name="env" rows={2} value={env} onInput={input(setEnv)} />
        </label>
        <label>
          Extra folders commands may read, one per line (for example a toolchain outside the system folders)
          <textarea name="sandbox_read" rows={2} value={read} onInput={input(setRead)} />
        </label>
        <label>
          Extra folders commands may write to, one per line (for example a shared package cache)
          <textarea name="sandbox_write" rows={2} value={write} onInput={input(setWrite)} />
        </label>
        <label>
          Ignored files and folders to copy into each run's copy of the project, one per line (for example node_modules)
          <textarea name="worktree_copy" rows={2} value={copy} onInput={input(setCopy)} />
        </label>
        {risky.map((path) => (
          <div class="approval">
            <p class="warn">
              <code>{path}</code> looks like a file with credentials. The agent can read a copied file, and what it reads can be sent to your model provider.
            </p>
            <label class="check">
              <input
                type="checkbox"
                aria-label={`Copy ${path} anyway`}
                checked={secrets.includes(path)}
                onChange={(e) => (setSecrets(e.currentTarget.checked ? [...secrets, path] : secrets.filter((s) => s !== path)), setSaved(false))}
              />
              Copy it anyway
            </label>
          </div>
        ))}
        <label>
          Command to run in each new copy before the agent starts (for example npm ci). It runs inside the sandbox.
          <input name="worktree_setup" value={setup} onInput={input(setSetup)} />
        </label>
        <label>
          Dev server command for Preview, run in a run's copy inside the sandbox. <code>{"{port}"}</code> becomes the run's port, which is also in <code>PORT</code>.
          <input name="dev_command" value={dev} onInput={input(setDev)} placeholder="npm run dev -- --host 127.0.0.1 --port {port} --strictPort" />
        </label>
        <fieldset>
          <legend>Limits for each command (0 is no limit)</legend>
          <p class="muted">
            {sandbox.value?.resource_limits.some((l) => l.includes("cgroup"))
              ? "Memory, processes and CPU are capped per command on this machine."
              : "On this machine memory, processes and CPU are capped only for a project on the container backend, where the limits hold for the run's container as a whole."}{" "}
            The file size cap applies to every command.
          </p>
          <div class="limits">
            <label>
              Memory (MiB)
              <input name="memory_mb" type="number" min="0" value={limits.memory_mb} onInput={limit("memory_mb")} />
            </label>
            <label>
              Processes
              <input name="processes" type="number" min="0" value={limits.processes} onInput={limit("processes")} />
            </label>
            <label>
              CPU cores
              <input name="cpus" type="number" min="0" step="0.1" value={limits.cpus} onInput={limit("cpus")} />
            </label>
            <label>
              Largest file (MiB)
              <input name="file_size_mb" type="number" min="0" value={limits.file_size_mb} onInput={limit("file_size_mb")} />
            </label>
          </div>
        </fieldset>
        {error && <p class="error">{error}</p>}
        <div class="row">
          <button type="submit">Save settings</button>
          {saved && <span class="ok">Saved.</span>}
        </div>
      </form>
  );
}

function ProjectRow({ p }: { p: ProjectView }) {
  const contained = p.effective_sandbox !== null && p.effective_sandbox !== "none";
  const [open, setOpen] = useState(false);
  const [saved, setSaved] = useState(false);
  return (
    <li class="card">
      <div class="row">
        <strong>{p.name}</strong>
        <span class="muted">{p.path}</span>
        <span class={`badge ${contained ? "on" : "off"}`}>{p.sandbox_error ? "sandbox unavailable" : `sandbox: ${p.effective_sandbox}`}</span>
        <select
          aria-label={`Approvals for ${p.name}`}
          value={p.policy}
          onChange={(e) =>
            api
              .updateProject(p.id, { policy: e.currentTarget.value as PolicyMode })
              .then(refresh)
              .catch(report)
          }
        >
          {policyChoices(contained, p.policy).map((o) => (
            <option value={o.value}>{o.label}</option>
          ))}
        </select>
        <button
          class="link danger"
          aria-label={`Remove ${p.name}`}
          onClick={() =>
            api
              .deleteProject(p.id)
              .then(refresh)
              .catch(report)
          }
        >
          Remove
        </button>
      </div>
      {p.sandbox_error && <p class="error">Runs in this project are refused: {p.sandbox_error}.</p>}
      {!contained && !p.sandbox_error && <p class="muted">Without a sandbox, every edit and command asks for approval whatever is chosen here.</p>}
      <details class="project-settings" open={open} onToggle={(e) => setOpen(e.currentTarget.open)}>
        <summary>Sandbox and commands</summary>
        {/* The form starts again from the saved settings whenever they change, for example when an approval adds a pattern. */}
        {open && <ProjectSettings key={JSON.stringify(p)} p={p} saved={saved} setSaved={setSaved} />}
      </details>
    </li>
  );
}


const MODES: { value: TeamMode; label: string }[] = [
  { value: "single", label: "Single: one agent" },
  { value: "delegate", label: "Delegate: the first member leads and hands tasks to the others" },
  { value: "sequential", label: "Sequential: members work one after another" },
  { value: "parallel", label: "Parallel: members answer at once without changing files, then a synthesizer" },
];

/** A provider and model picker, with the models the provider offers when it lists them. */
function ModelPicker({ label, provider, model, onChange }: { label: string; provider: string; model: string; onChange: (provider: string, model: string) => void }) {
  const [models, setModels] = useState<string[]>([]);
  useEffect(() => {
    setModels([]);
    if (provider) api.models(provider).then(setModels).catch(() => setModels([]));
  }, [provider]);
  const listId = `models-${label.replace(/[^a-z0-9]/gi, "")}`;
  return (
    <div class="grid">
      <label>
        {label}: provider
        <select value={provider} onChange={(e) => onChange(e.currentTarget.value, model)} required>
          {providers.value.map((p) => (
            <option value={p.id}>{p.id}</option>
          ))}
        </select>
      </label>
      <label>
        {label}: model id
        <input value={model} onInput={(e) => onChange(provider, e.currentTarget.value)} list={listId} required />
        <datalist id={listId}>
          {models.map((m) => (
            <option value={m} />
          ))}
        </datalist>
      </label>
    </div>
  );
}

function TemplateForm({ onDone }: { onDone: () => void }) {
  const [id, setId] = useState(templates.value[0]?.id ?? "");
  const t: TemplateView | undefined = templates.value.find((x) => x.id === id);
  const [teamId, setTeamId] = useState("");
  const [choice, setChoice] = useState({ provider: providers.value[0]?.id ?? "", model: "" });
  const [own, setOwn] = useState<Record<string, { provider: string; model: string }>>({});
  const [conflict, setConflict] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  async function save(e: Event, overwrite = false) {
    e.preventDefault();
    setError(null);
    try {
      const models = Object.fromEntries(Object.entries(own).filter(([k, v]) => t?.distinct_models.includes(k) && v.model.trim()));
      await api.fromTemplate(id, { team_id: teamId.trim() || null, provider: choice.provider, model: choice.model.trim(), models, overwrite });
      await refresh();
      onDone();
    } catch (err) {
      if (err instanceof ApiError && err.code === "exists") setConflict(err.message);
      else setError(err instanceof Error ? err.message : String(err));
    }
  }
  return (
    <form class="card" onSubmit={(e) => save(e)}>
      <h3>Make a team from a template</h3>
      <label>
        Template
        <select name="template" value={id} onChange={(e) => (setId(e.currentTarget.value), setConflict(null))}>
          {templates.value.map((x) => (
            <option value={x.id}>{x.name}</option>
          ))}
        </select>
      </label>
      {t && (
        <p class="muted">
          {t.description} Agents: {t.agents.map((a) => `${a.name} (${a.role.toLowerCase()})`).join("; ")}.
        </p>
      )}
      <label>
        Team id (optional; the agents are named after it)
        <input name="team_id" value={teamId} placeholder={id} onInput={(e) => setTeamId(slug(e.currentTarget.value))} />
      </label>
      <ModelPicker label="Every agent" provider={choice.provider} model={choice.model} onChange={(provider, model) => setChoice({ provider, model })} />
      {t?.distinct_models.map((agent) => {
        const name = t.agents.find((a) => a.id === agent)?.name ?? agent;
        const v = own[agent] ?? { provider: choice.provider, model: "" };
        return <ModelPicker label={name} provider={v.provider} model={v.model} onChange={(provider, model) => setOwn({ ...own, [agent]: { provider, model } })} />;
      })}
      {t && t.distinct_models.length > 0 && <p class="muted">Give these agents different models; that is what the template compares. Left empty, an agent uses the model above.</p>}
      {conflict && (
        <div class="warn">
          <p>{conflict}</p>
          <button type="button" class="danger" onClick={(e) => save(e, true)}>
            Replace them
          </button>
        </div>
      )}
      {error && <p class="error">{error}</p>}
      <div class="row">
        <button type="submit">Create team</button>
        <button type="button" class="link" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function TeamForm({ existing, onDone }: { existing?: TeamConfig; onDone: () => void }) {
  const [name, setName] = useState(existing?.name ?? "");
  const [mode, setMode] = useState<TeamMode>(existing?.mode ?? "sequential");
  const [members, setMembers] = useState(existing?.members.join(", ") ?? "");
  const [synthesizer, setSynthesizer] = useState(existing?.synthesizer ?? "");
  const [error, setError] = useState<string | null>(null);
  async function save(e: Event) {
    e.preventDefault();
    setError(null);
    const list = members.split(",").map((m) => m.trim()).filter(Boolean);
    const team: TeamConfig = { schema_version: 1, id: existing?.id ?? slug(name), name, mode, members: list, synthesizer: mode === "parallel" ? synthesizer : "" };
    try {
      if (existing) await api.updateTeam(team);
      else await api.createTeam(team);
      await refresh();
      onDone();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }
  return (
    <form class="card" onSubmit={save}>
      <h3>{existing ? `Edit ${existing.name || existing.id}` : "Add a team"}</h3>
      <label>
        Name
        <input name="team_name" value={name} onInput={(e) => setName(e.currentTarget.value)} required />
      </label>
      <label>
        Mode
        <select name="mode" value={mode} onChange={(e) => setMode(e.currentTarget.value as TeamMode)}>
          {MODES.map((m) => (
            <option value={m.value}>{m.label}</option>
          ))}
        </select>
      </label>
      <label>
        Members, in order, as agent ids separated by commas
        <input name="members" value={members} onInput={(e) => setMembers(e.currentTarget.value)} required />
      </label>
      <p class="muted">Agents: {agents.value.map((a) => a.id).join(", ")}</p>
      {mode === "parallel" && (
        <label>
          Synthesizer (not a member; it gets every member's answer)
          <select name="synthesizer" value={synthesizer} onChange={(e) => setSynthesizer(e.currentTarget.value)} required>
            <option value="">Choose an agent</option>
            {agents.value.map((a) => (
              <option value={a.id}>{a.name}</option>
            ))}
          </select>
        </label>
      )}
      {error && <p class="error">{error}</p>}
      <div class="row">
        <button type="submit">Save</button>
        <button type="button" class="link" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function ImportForm({ onDone }: { onDone: () => void }) {
  const [bundle, setBundle] = useState("");
  const [provider, setProvider] = useState("");
  const [conflict, setConflict] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  async function save(e: Event, overwrite = false) {
    e.preventDefault();
    setError(null);
    try {
      await api.importTeam({ bundle, provider: provider || null, overwrite });
      await refresh();
      onDone();
    } catch (err) {
      if (err instanceof ApiError && err.code === "exists") setConflict(err.message);
      else setError(err instanceof Error ? err.message : String(err));
    }
  }
  return (
    <form class="card" onSubmit={(e) => save(e)}>
      <h3>Import a team</h3>
      <label>
        Team file
        <input type="file" accept=".toml,text/plain" onChange={(e) => e.currentTarget.files?.[0]?.text().then(setBundle)} />
      </label>
      <label>
        Or paste it
        <textarea name="bundle" rows={6} value={bundle} onInput={(e) => setBundle(e.currentTarget.value)} required />
      </label>
      <label>
        Provider for agents whose provider does not exist here
        <select name="import_provider" value={provider} onChange={(e) => setProvider(e.currentTarget.value)}>
          <option value="">None: refuse such agents</option>
          {providers.value.map((p) => (
            <option value={p.id}>{p.id}</option>
          ))}
        </select>
      </label>
      {conflict && (
        <div class="warn">
          <p>{conflict}</p>
          <button type="button" class="danger" onClick={(e) => save(e, true)}>
            Replace them
          </button>
        </div>
      )}
      {error && <p class="error">{error}</p>}
      <div class="row">
        <button type="submit">Import</button>
        <button type="button" class="link" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function TeamRow({ t }: { t: TeamConfig }) {
  const [editing, setEditing] = useState(false);
  if (editing) return <TeamForm existing={t} onDone={() => setEditing(false)} />;
  const label = t.name || t.id;
  return (
    <li class="card row">
      <strong>{label}</strong>
      <span class="muted">
        {t.mode} · {t.members.join(", ")}
        {t.synthesizer ? ` · synthesizer ${t.synthesizer}` : ""}
      </span>
      <button class="link" onClick={() => setEditing(true)} aria-label={`Edit ${label}`}>
        Edit
      </button>
      <a class="link" href={api.exportTeamUrl(t.id)} download={`team-${t.id}.toml`} aria-label={`Export ${label}`}>
        Export
      </a>
      <button class="link danger" aria-label={`Delete ${label}`} onClick={() => api.deleteTeam(t.id).then(refresh).catch(report)}>
        Delete
      </button>
    </li>
  );
}

function TeamsSection() {
  const [adding, setAdding] = useState<"template" | "team" | "import" | null>(null);
  const done = () => setAdding(null);
  return (
    <section>
      <h2>Teams</h2>
      <p class="muted">A team runs several agents on one prompt, each with its own model. They share the run's limits and see only each other's final messages.</p>
      <ul class="list">
        {teams.value.map((t) => (
          <TeamRow key={t.id} t={t} />
        ))}
      </ul>
      {adding === "template" && <TemplateForm onDone={done} />}
      {adding === "team" && <TeamForm onDone={done} />}
      {adding === "import" && <ImportForm onDone={done} />}
      {adding === null && (
        <div class="row">
          <button onClick={() => setAdding("template")}>Team from a template</button>
          {agents.value.length > 0 && <button onClick={() => setAdding("team")}>Add team</button>}
          <button onClick={() => setAdding("import")}>Import team</button>
        </div>
      )}
    </section>
  );
}


/** The MCP tools an agent may use, in or out of a project: every tool of a server, or chosen ones. */
function McpToolPicker({ tools, setTools }: { tools: string[]; setTools: (t: string[]) => void }) {
  return (
    <fieldset>
      <legend>MCP tools the agent may use</legend>
      {mcpServers.value.map((s) => (
        <McpServerPick key={s.id} server={s} tools={tools} setTools={setTools} />
      ))}
    </fieldset>
  );
}

function McpServerPick({ server, tools, setTools }: { server: McpServerView; tools: string[]; setTools: (t: string[]) => void }) {
  const all = `mcp__${server.id}`;
  const prefix = `${all}__`;
  const chosen = tools.filter((t) => t.startsWith(prefix));
  const [list, setList] = useState<McpToolView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  async function load() {
    setLoading(true);
    setError(null);
    try {
      setList(await api.mcpTools(server.id));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }
  const toggle = (name: string, on: boolean) => setTools(on ? [...tools, name] : tools.filter((x) => x !== name));
  return (
    <div class="mcp-pick">
      <label class="check">
        <input type="checkbox" checked={tools.includes(all)} onChange={(e) => toggle(all, e.currentTarget.checked)} />
        Every tool of {server.id}
        {!server.enabled && <span class="muted"> (disabled)</span>}
      </label>
      {!tools.includes(all) && (
        <div class="indent">
          {chosen.length > 0 && !list && <p class="muted">Chosen: {chosen.map((t) => t.slice(prefix.length)).join(", ")}</p>}
          {list ? (
            list.map((t) => (
              <label class="check" title={t.description}>
                <input type="checkbox" checked={tools.includes(t.exposed)} onChange={(e) => toggle(t.exposed, e.currentTarget.checked)} />
                {t.name}
              </label>
            ))
          ) : (
            <button type="button" class="link" onClick={load} disabled={loading || !server.enabled}>
              {loading ? "Starting the server…" : `Choose tools of ${server.id}`}
            </button>
          )}
          {error && <p class="error">{error}</p>}
        </div>
      )}
    </div>
  );
}

type ValueRow = { name: string; value: string; existing?: McpValueView };

function sourceLabel(v: McpValueView) {
  return v.source === "secret" ? "stored secret" : v.source === "env" ? `from $${v.var}` : "set in mcp.toml";
}

/** Names and values for a server's environment or headers. Stored values are never shown; empty keeps them. */
function ValuesEditor({ label, rows, setRows, placeholder }: { label: string; rows: ValueRow[]; setRows: (r: ValueRow[]) => void; placeholder: string }) {
  const set = (i: number, patch: Partial<ValueRow>) => setRows(rows.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  return (
    <fieldset>
      <legend>{label}</legend>
      {rows.map((r, i) => (
        <div class="row">
          <input aria-label={`${label}: name`} placeholder={placeholder} value={r.name} disabled={!!r.existing} onInput={(e) => set(i, { name: e.currentTarget.value })} />
          <input
            aria-label={`${label}: value of ${r.name || "new entry"}`}
            type="password"
            autocomplete="off"
            placeholder={r.existing ? `${sourceLabel(r.existing)}; type to replace` : "value, or env:VARIABLE"}
            value={r.value}
            onInput={(e) => set(i, { value: e.currentTarget.value })}
          />
          <button type="button" class="link danger" onClick={() => setRows(rows.filter((_, j) => j !== i))}>
            Remove
          </button>
        </div>
      ))}
      <button type="button" class="link" onClick={() => setRows([...rows, { name: "", value: "" }])}>
        Add
      </button>
    </fieldset>
  );
}

function toInput(rows: ValueRow[]): McpValueInput[] {
  return rows.filter((r) => r.name.trim()).map((r) => ({ name: r.name.trim(), value: r.value || null }));
}

function McpForm({ existing, onDone }: { existing?: McpServerView; onDone: () => void }) {
  const [id, setId] = useState(existing?.id ?? "");
  const [transport, setTransport] = useState<McpTransport>(existing?.transport ?? "stdio");
  const [command, setCommand] = useState(existing?.command ?? "");
  const [args, setArgs] = useState((existing?.args ?? []).join("\n"));
  const [cwd, setCwd] = useState(existing?.cwd ?? "");
  const [url, setUrl] = useState(existing?.url ?? "");
  const [env, setEnv] = useState<ValueRow[]>((existing?.env ?? []).map((v) => ({ name: v.name, value: "", existing: v })));
  const [headers, setHeaders] = useState<ValueRow[]>((existing?.headers ?? []).map((v) => ({ name: v.name, value: "", existing: v })));
  const [timeout, setTimeout_] = useState(String(existing?.timeout_s ?? 120));
  const [enabled, setEnabled] = useState(existing?.enabled ?? true);
  const [error, setError] = useState<string | null>(null);
  async function save(e: Event) {
    e.preventDefault();
    setError(null);
    const stdio = transport === "stdio";
    const input = {
      id: id.trim(),
      transport,
      command: stdio ? command : null,
      args: stdio ? lines(args) : [],
      cwd: stdio ? cwd : null,
      url: stdio ? null : url,
      env: stdio ? toInput(env) : [],
      headers: stdio ? [] : toInput(headers),
      trusted_tools: existing?.trusted_tools ?? [],
      timeout_s: Number(timeout) || 120,
      enabled,
    };
    try {
      if (existing) await api.updateMcpServer(input);
      else await api.createMcpServer(input);
      await refresh();
      onDone();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }
  return (
    <form class="card" onSubmit={save}>
      <h3>{existing ? `Edit ${existing.id}` : "Add an MCP server"}</h3>
      {!existing && (
        <label>
          Name (letters, digits, - and _; tools appear as mcp__name__tool)
          <input name="mcp_id" value={id} onInput={(e) => setId(e.currentTarget.value)} required pattern="[A-Za-z0-9_\-]{1,32}" />
        </label>
      )}
      <label>
        Connection
        <select name="transport" value={transport} onChange={(e) => setTransport(e.currentTarget.value as McpTransport)}>
          <option value="stdio">A program on this machine (stdio)</option>
          <option value="http">A server at a URL (Streamable HTTP)</option>
        </select>
      </label>
      {transport === "stdio" ? (
        <>
          <label>
            Command
            <input name="command" value={command} onInput={(e) => setCommand(e.currentTarget.value)} placeholder="npx" required />
          </label>
          <label>
            Arguments, one per line
            <textarea name="args" rows={3} value={args} onInput={(e) => setArgs(e.currentTarget.value)} />
          </label>
          <label>
            Folder to start in (optional)
            <input name="cwd" value={cwd} onInput={(e) => setCwd(e.currentTarget.value)} />
          </label>
          <ValuesEditor label="Environment variables" rows={env} setRows={setEnv} placeholder="NAME" />
          <p class="muted">
            The program runs outside the sandbox, as you, with only these variables and a minimal base ({"PATH, HOME, LANG and a few more"}). A value is stored encrypted, unless it is <code>env:NAME</code>, which takes the
            daemon's own variable.
          </p>
        </>
      ) : (
        <>
          <label>
            URL
            <input name="url" type="url" value={url} onInput={(e) => setUrl(e.currentTarget.value)} placeholder="https://example.com/mcp" required />
          </label>
          <ValuesEditor label="Headers" rows={headers} setRows={setHeaders} placeholder="Authorization" />
          <p class="muted">Header values are stored encrypted, unless a value is <code>env:NAME</code>.</p>
        </>
      )}
      <label>
        Longest a call may take, in seconds
        <input name="timeout_s" type="number" min={1} max={600} value={timeout} onInput={(e) => setTimeout_(e.currentTarget.value)} />
      </label>
      <label class="check">
        <input type="checkbox" checked={enabled} onChange={(e) => setEnabled(e.currentTarget.checked)} />
        Enabled
      </label>
      {error && <p class="error">{error}</p>}
      <div class="row">
        <button type="submit">Save</button>
        <button type="button" class="link" onClick={onDone}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function McpRow({ s }: { s: McpServerView }) {
  const [editing, setEditing] = useState(false);
  const [list, setList] = useState<McpToolView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  async function load() {
    setBusy(true);
    setError(null);
    try {
      setList(await api.mcpTools(s.id));
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }
  async function trust(tool: string, on: boolean) {
    try {
      await api.trustMcpTool(s.id, tool, on);
      setList((l) => l && l.map((t) => (t.name === tool ? { ...t, trusted: on } : t)));
      await refresh();
    } catch (e) {
      report(e);
    }
  }
  if (editing) return <McpForm existing={s} onDone={() => setEditing(false)} />;
  const all = s.trusted_tools.includes("*");
  return (
    <li class="card">
      <div class="row">
        <strong>{s.id}</strong>
        <span class="muted">{s.transport === "stdio" ? [s.command, ...s.args].join(" ") : s.url}</span>
        <span class="muted">{!s.enabled ? "disabled" : s.connected ? "connected" : "not started"}</span>
      </div>
      <div class="row">
        <button onClick={load} disabled={busy || !s.enabled}>
          {busy ? "Starting…" : list ? "Refresh tools" : "Show tools"}
        </button>
        <button class="link" onClick={() => setEditing(true)} aria-label={`Edit ${s.id}`}>
          Edit
        </button>
        <button
          class="link danger"
          aria-label={`Delete ${s.id}`}
          onClick={() =>
            api
              .deleteMcpServer(s.id)
              .then(refresh)
              .catch(report)
          }
        >
          Delete
        </button>
      </div>
      {error && (
        <pre class="error" role="status">
          {error}
        </pre>
      )}
      {list && (
        <fieldset>
          <legend>Tools that run without asking</legend>
          <label class="check">
            <input type="checkbox" checked={all} onChange={(e) => trust("*", e.currentTarget.checked)} />
            Trust every tool of {s.id}, including tools it adds later
          </label>
          {list.map((t) => (
            <label class="check" title={t.description}>
              <input type="checkbox" checked={all || t.trusted} disabled={all} onChange={(e) => trust(t.name, e.currentTarget.checked)} />
              <code>{t.name}</code> <span class="muted">{t.description}</span>
            </label>
          ))}
          {list.length === 0 && <p class="muted">The server offers no tools.</p>}
        </fieldset>
      )}
    </li>
  );
}

function McpSection() {
  const [adding, setAdding] = useState(false);
  return (
    <section>
      <h2>MCP servers</h2>
      <p class="muted">
        An MCP server gives agents more tools, such as web search. Give an agent a server's tools in its settings. A tool asks before each call until you trust it, because the server runs outside the sandbox and
        what it returns comes from outside Patina.
      </p>
      <ul class="list">
        {mcpServers.value.map((s) => (
          <McpRow key={s.id} s={s} />
        ))}
      </ul>
      {adding ? <McpForm onDone={() => setAdding(false)} /> : <button onClick={() => setAdding(true)}>Add MCP server</button>}
    </section>
  );
}

export function Settings() {
  const [addingProvider, setAddingProvider] = useState(false);
  const [addingAgent, setAddingAgent] = useState(false);
  const [addingProject, setAddingProject] = useState(false);
  const noProviders = providers.value.length === 0;
  const noAgents = agents.value.length === 0;
  // Settings can change outside this page: an approval adds an allowlist pattern, or a file is edited by hand.
  useEffect(() => {
    refresh().catch(report);
  }, []);
  return (
    <div class="settings">
      {(noProviders || noAgents) && (
        <ol class="card steps">
          <li class={noProviders ? "current" : "done"}>Add a provider and test the connection.</li>
          <li class={noProviders ? "" : noAgents ? "current" : "done"}>Add an agent that uses it, or make a team from a template.</li>
          <li>Open Chat and send a prompt.</li>
        </ol>
      )}
      <section>
        <h2>Providers</h2>
        <ul class="list">
          {providers.value.map((p) => (
            <ProviderRow key={p.id} p={p} />
          ))}
        </ul>
        {addingProvider || noProviders ? <ProviderForm onDone={() => setAddingProvider(false)} /> : <button onClick={() => setAddingProvider(true)}>Add provider</button>}
      </section>
      {!noProviders && (
        <section>
          <h2>Agents</h2>
          <ul class="list">
            {agents.value.map((a) => (
              <AgentRow key={a.id} a={a} />
            ))}
          </ul>
          {addingAgent || noAgents ? (
            <AgentForm
              onDone={() => {
                setAddingAgent(false);
                if (agents.value.length === 1) view.value = "chat";
              }}
            />
          ) : (
            <button onClick={() => setAddingAgent(true)}>Add agent</button>
          )}
        </section>
      )}
      {!noProviders && <TeamsSection />}
      {!noProviders && <McpSection />}
      {!noAgents && (
        <section>
          <h2>Projects</h2>
          <p class="muted">
            A project is a folder with a git repository. An agent works in its own copy on a separate branch, and nothing reaches your branch until you merge it.
          </p>
          <SandboxCard />
          <ul class="list">
            {projects.value.map((p) => (
              <ProjectRow key={p.id} p={p} />
            ))}
          </ul>
          {addingProject ? <ProjectForm onDone={() => setAddingProject(false)} /> : <button onClick={() => setAddingProject(true)}>Add project</button>}
        </section>
      )}
    </div>
  );
}
