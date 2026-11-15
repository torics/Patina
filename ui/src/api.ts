// The only place that talks to the daemon. The CSRF token lives in memory, never in storage.
import type { AgentConfig } from "./api/generated/AgentConfig";
import type { ApprovalAnswer } from "./api/generated/ApprovalAnswer";
import type { Budgets } from "./api/generated/Budgets";
import type { CheckpointView } from "./api/generated/CheckpointView";
import type { DiffView } from "./api/generated/DiffView";
import type { MergeResult } from "./api/generated/MergeResult";
import type { NewProject } from "./api/generated/NewProject";
import type { ProjectConfig } from "./api/generated/ProjectConfig";
import type { ProjectView } from "./api/generated/ProjectView";
import type { NewRun } from "./api/generated/NewRun";
import type { PresetView } from "./api/generated/PresetView";
import type { ProviderInput } from "./api/generated/ProviderInput";
import type { ProviderTestResult } from "./api/generated/ProviderTestResult";
import type { ProviderView } from "./api/generated/ProviderView";
import type { RunView } from "./api/generated/RunView";
import type { SessionState } from "./api/generated/SessionState";
import type { SystemInfo } from "./api/generated/SystemInfo";

/** The settings of a project that can change. The id and the folder cannot. */
export type ProjectPatch = { [K in Exclude<keyof ProjectConfig, "id" | "path">]?: ProjectConfig[K] | null };

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
  run: (id: string) => call<RunView>("GET", `/runs/${encodeURIComponent(id)}`),
  setBudgets: (id: string, patch: Partial<Budgets>) => call<RunView>("PATCH", `/runs/${encodeURIComponent(id)}/budgets`, patch),
  answerApproval: (runId: string, approvalId: string, answer: ApprovalAnswer) =>
    call("POST", `/runs/${encodeURIComponent(runId)}/approvals/${encodeURIComponent(approvalId)}`, answer),
  diff: (id: string) => call<DiffView>("GET", `/runs/${encodeURIComponent(id)}/diff`),
  patchUrl: (id: string) => `${BASE}/runs/${encodeURIComponent(id)}/diff?format=patch`,
  checkpoints: (id: string) => call<CheckpointView[]>("GET", `/runs/${encodeURIComponent(id)}/checkpoints`),
  rollback: (id: string, step: number) => call<CheckpointView[]>("POST", `/runs/${encodeURIComponent(id)}/rollback`, { step }),
  merge: (id: string) => call<MergeResult>("POST", `/runs/${encodeURIComponent(id)}/merge`, {}),
  discard: (id: string, keepBranch: boolean) => call<RunView>("POST", `/runs/${encodeURIComponent(id)}/discard`, { keep_branch: keepBranch }),
  projects: () => call<ProjectView[]>("GET", "/projects"),
  createProject: (p: NewProject) => call<ProjectView>("POST", "/projects", p),
  /** Only the settings that are sent change; `null` puts a setting back to its default. */
  updateProject: (id: string, patch: ProjectPatch) => call<ProjectView>("PATCH", `/projects/${encodeURIComponent(id)}`, patch),
  deleteProject: (id: string) => call("DELETE", `/projects/${encodeURIComponent(id)}`),
  eventsUrl: (id: string, after: number) => `${BASE}/runs/${encodeURIComponent(id)}/events?after=${after}`,
};
