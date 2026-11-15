// MCP from the UI against the real binary: add a stdio server, see its tools, give an agent one of them, and
// approve its first call while trusting it, so that the next call runs without asking.
import { expect, test } from "@playwright/test";
import { spawn, type ChildProcess } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { createServer, type Server } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";

const BIN = process.env.PATINA_BIN ?? join(import.meta.dirname, "../../target/debug/patina");
const SERVER = join(import.meta.dirname, "../../crates/patina-tools/tests/fixtures/mcp_stdio_server.py");
const PASSWORD = "a long enough password";
let daemon: ChildProcess;
let fake: Server;
let base = "";
let config = "";

function chunk(delta: object, finish: string | null) {
  const usage = finish ? { prompt_tokens: 5, completion_tokens: 5, total_tokens: 10 } : undefined;
  return `data: ${JSON.stringify({ id: "1", object: "chat.completion.chunk", model: "m", choices: [{ index: 0, delta, finish_reason: finish }], usage })}\n\n`;
}

test.beforeAll(async () => {
  // The model calls the echo tool twice, then answers with what it got back.
  fake = createServer((req, res) => {
    let raw = "";
    req.on("data", (d) => (raw += d));
    req.on("end", () => {
      const messages: any[] = JSON.parse(raw || "{}").messages ?? [];
      const results = messages.filter((m) => m.role === "tool").length;
      res.writeHead(200, { "content-type": "text/event-stream" });
      if (results < 2) {
        const args = JSON.stringify({ text: `echo number ${results + 1}` });
        const call = { tool_calls: [{ index: 0, id: `call_${results}`, type: "function", function: { name: "mcp__local__echo", arguments: args } }] };
        return res.end(chunk(call, null) + chunk({}, "tool_calls") + "data: [DONE]\n\n");
      }
      const last = String(messages.filter((m) => m.role === "tool").pop()?.content ?? "");
      res.end(chunk({ content: `The tool said: ${last.split("\n").pop()}` }, null) + chunk({}, "stop") + "data: [DONE]\n\n");
    });
  });
  await new Promise<void>((r) => fake.listen(0, "127.0.0.1", r));
  const addr = fake.address();
  const fakeUrl = `http://127.0.0.1:${typeof addr === "object" && addr ? addr.port : 0}/v1`;

  const dir = mkdtempSync(join(tmpdir(), "patina-mcp-"));
  config = join(dir, "c");
  daemon = spawn(BIN, ["serve", "--port", "0", "--preview-port", "0", "--config-dir", config, "--data-dir", join(dir, "d")]);
  let out = "";
  const token = await new Promise<string>((resolve, reject) => {
    const t = setTimeout(() => reject(new Error(`daemon did not start: ${out}`)), 10_000);
    daemon.stdout!.on("data", (d) => {
      out += d;
      const url = out.match(/running at (http:\/\/\S+)/);
      const tok = out.match(/setup token[^\n]*\n\s+([0-9a-f]{64})/);
      if (url && tok) {
        base = url[1];
        clearTimeout(t);
        resolve(tok[1]);
      }
    });
    daemon.on("exit", (c) => reject(new Error(`daemon exited with ${c}: ${out}`)));
  });
  const setup = await fetch(`${base}/api/v1/auth/setup`, { method: "POST", headers: { "content-type": "application/json", origin: base }, body: JSON.stringify({ token, password: PASSWORD }) });
  const cookie = setup.headers.getSetCookie()[0].split(";")[0];
  const { csrf } = await setup.json();
  const call = async (method: string, path: string, body: object) => {
    const r = await fetch(`${base}/api/v1${path}`, { method, headers: { "content-type": "application/json", origin: base, cookie, "x-patina-csrf": csrf }, body: JSON.stringify(body) });
    if (!r.ok) throw new Error(`${method} ${path}: ${r.status} ${await r.text()}`);
  };
  await call("POST", "/providers", { id: "local", kind: "openai_compatible", base_url: fakeUrl });
  await call("POST", "/agents", { id: "helper", name: "Helper", provider: "local", model: "m", tools: [] });
});

test.afterAll(() => {
  daemon?.kill("SIGTERM");
  fake?.close();
});

test("add an MCP server, give an agent one tool, and trust it from the approval card", async ({ page }) => {
  await page.goto(base);
  await page.getByLabel("Password", { exact: true }).fill(PASSWORD);
  await page.getByRole("button", { name: "Sign in" }).click();
  await page.getByRole("button", { name: "Settings" }).click();

  // Add the server; a value typed for a variable is stored as a secret, not in mcp.toml.
  const section = page.locator("section", { has: page.getByRole("heading", { name: "MCP servers" }) });
  await section.getByRole("button", { name: "Add MCP server" }).click();
  await section.getByLabel(/^Name/).fill("local");
  await section.getByLabel("Command").fill("python3");
  await section.getByLabel("Arguments, one per line").fill(`-u\n${SERVER}`);
  await section.getByRole("button", { name: "Add", exact: true }).click();
  await section.getByLabel("Environment variables: name").fill("GREETING");
  await section.getByLabel(/^Environment variables: value/).fill("typed-secret-value");
  await section.getByRole("button", { name: "Save" }).click();
  await expect(section.getByText("not started")).toBeVisible();
  expect(readFileSync(join(config, "mcp.toml"), "utf8")).not.toContain("typed-secret-value");

  // Its tools are listed, none trusted.
  await section.getByRole("button", { name: "Show tools" }).click();
  await expect(section.getByText("connected")).toBeVisible();
  await expect(section.locator("code", { hasText: /^echo$/ })).toBeVisible();

  // The agent gets one tool of the server.
  await page.getByRole("button", { name: "Edit Helper" }).click();
  await page.getByRole("button", { name: "Choose tools of local" }).click();
  await page.getByRole("checkbox", { name: "echo", exact: true }).check();
  await page.getByRole("button", { name: "Save" }).click();
  await expect(page.getByText("tools: mcp__local__echo")).toBeVisible();

  // In a plain chat, the first call asks; trusting the tool lets the second run without asking.
  await page.getByRole("button", { name: "Workbench" }).click();
  await page.getByLabel("Prompt", { exact: true }).fill("Use the echo tool");
  await page.getByRole("button", { name: "Send" }).click();
  const approval = page.getByRole("group", { name: "Approval for mcp__local__echo" });
  await expect(approval.getByText(/runs outside the sandbox/)).toBeVisible();
  await approval.getByRole("button", { name: "Approve and trust this tool" }).click();
  await expect(page.getByText("The tool said: echo number 2")).toBeVisible();
  await expect(page.getByRole("group", { name: "Approval for mcp__local__echo" })).toHaveCount(0);
  expect(readFileSync(join(config, "mcp.toml"), "utf8")).toContain('trusted_tools = ["echo"]');

  // Settings shows the trust, and it can be withdrawn there.
  await page.getByRole("button", { name: "Settings" }).click();
  await section.getByRole("button", { name: "Show tools" }).click();
  const trusted = section.getByRole("checkbox", { name: /^echo Return the text/ });
  await expect(trusted).toBeChecked();
  await trusted.uncheck();
  await expect(trusted).not.toBeChecked();
  expect(readFileSync(join(config, "mcp.toml"), "utf8")).not.toContain("trusted_tools");
});
