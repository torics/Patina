import { useEffect, useState } from "preact/hooks";
import type { PresetView } from "./api/generated/PresetView";
import type { ProviderKind } from "./api/generated/ProviderKind";
import type { ProviderTestResult } from "./api/generated/ProviderTestResult";
import type { ProviderView } from "./api/generated/ProviderView";
import { api } from "./api";
import { agents, providers, refresh, report, view } from "./state";

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

function AgentForm({ onDone }: { onDone: () => void }) {
  const [name, setName] = useState("Assistant");
  const [provider, setProvider] = useState(providers.value[0]?.id ?? "");
  const [model, setModel] = useState("");
  const [models, setModels] = useState<string[]>([]);
  const [system, setSystem] = useState("");
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    setModels([]);
    if (provider) api.models(provider).then(setModels).catch(() => setModels([]));
  }, [provider]);
  async function save(e: Event) {
    e.preventDefault();
    setError(null);
    try {
      await api.createAgent({ id: slug(name), name, provider, model: model.trim(), system_prompt: system });
      await refresh();
      onDone();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }
  return (
    <form class="card" onSubmit={save}>
      <h3>Add an agent</h3>
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
        System prompt (optional)
        <textarea name="system" rows={3} value={system} onInput={(e) => setSystem(e.currentTarget.value)} />
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

export function Settings() {
  const [addingProvider, setAddingProvider] = useState(false);
  const [addingAgent, setAddingAgent] = useState(false);
  const noProviders = providers.value.length === 0;
  const noAgents = agents.value.length === 0;
  return (
    <div class="settings">
      {(noProviders || noAgents) && (
        <ol class="card steps">
          <li class={noProviders ? "current" : "done"}>Add a provider and test the connection.</li>
          <li class={noProviders ? "" : noAgents ? "current" : "done"}>Add an agent that uses it.</li>
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
              <li class="card row" key={a.id}>
                <strong>{a.name}</strong>
                <span class="muted">
                  {a.provider} · {a.model}
                </span>
                <button
                  class="link danger"
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
    </div>
  );
}
