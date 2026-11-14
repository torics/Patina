// The only place that talks to the daemon. The CSRF token lives in memory, never in storage.
import type { AgentConfig } from "./api/generated/AgentConfig";
import type { NewRun } from "./api/generated/NewRun";
import type { PresetView } from "./api/generated/PresetView";
import type { ProviderInput } from "./api/generated/ProviderInput";
import type { ProviderTestResult } from "./api/generated/ProviderTestResult";
import type { ProviderView } from "./api/generated/ProviderView";
import type { RunView } from "./api/generated/RunView";
import type { SessionState } from "./api/generated/SessionState";
import type { SystemInfo } from "./api/generated/SystemInfo";

const BASE = "/api/v1";
let csrf = "";

export class ApiError extends Error {
  constructor(public status: number, public code: string, message: string) {
    super(message);
  }
}

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {};
  if (body !== undefined) headers["content-type"] = "application/json";
  if (method !== "GET") headers["x-patina-csrf"] = csrf;
  const res = await fetch(BASE + path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body), credentials: "same-origin" });
  const text = await res.text();
  let data: any = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = null;
  }
  if (!res.ok) throw new ApiError(res.status, data?.error?.code ?? "error", data?.error?.message ?? `request failed (${res.status})`);
  return data as T;
}

export const api = {
  async session(): Promise<SessionState> {
    const s = await call<SessionState>("GET", "/auth/session");
    csrf = s.csrf ?? "";
    return s;
  },
  async setup(token: string, password: string) {
    csrf = (await call<{ csrf: string }>("POST", "/auth/setup", { token, password })).csrf;
  },
  async login(password: string) {
    csrf = (await call<{ csrf: string }>("POST", "/auth/login", { password })).csrf;
  },
  async logout() {
    await call("POST", "/auth/logout", {});
    csrf = "";
  },
  system: () => call<SystemInfo>("GET", "/system"),
  presets: () => call<PresetView[]>("GET", "/provider-presets"),
  providers: () => call<ProviderView[]>("GET", "/providers"),
  createProvider: (p: ProviderInput) => call<ProviderView>("POST", "/providers", p),
  updateProvider: (p: ProviderInput) => call<ProviderView>("PUT", `/providers/${encodeURIComponent(p.id)}`, p),
  deleteProvider: (id: string) => call("DELETE", `/providers/${encodeURIComponent(id)}`),
  setSecret: (id: string, value: string) => call<ProviderView>("PUT", `/providers/${encodeURIComponent(id)}/secret`, { value }),
  testProvider: (id: string, model: string | null) => call<ProviderTestResult>("POST", `/providers/${encodeURIComponent(id)}/test`, { model }),
  models: (id: string) => call<string[]>("GET", `/providers/${encodeURIComponent(id)}/models`),
  agents: () => call<AgentConfig[]>("GET", "/agents"),
  createAgent: (a: Partial<AgentConfig>) => call<AgentConfig>("POST", "/agents", a),
  updateAgent: (a: Partial<AgentConfig> & { id: string }) => call<AgentConfig>("PUT", `/agents/${encodeURIComponent(a.id)}`, a),
  deleteAgent: (id: string) => call("DELETE", `/agents/${encodeURIComponent(id)}`),
  runs: () => call<RunView[]>("GET", "/runs"),
  createRun: (r: NewRun) => call<RunView>("POST", "/runs", r),
  cancelRun: (id: string) => call("POST", `/runs/${encodeURIComponent(id)}/cancel`, {}),
  eventsUrl: (id: string, after: number) => `${BASE}/runs/${encodeURIComponent(id)}/events?after=${after}`,
};
