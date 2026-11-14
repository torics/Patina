// Smoke test against the real binary: set up, add a provider and agent, chat, reload and replay.
import { expect, test } from "@playwright/test";
import { spawn, type ChildProcess } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { createServer, type Server } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";

const KEY = "sk-e2e-SECRETVALUE-4d2e";
const BIN = process.env.PATINA_BIN ?? join(import.meta.dirname, "../../target/debug/patina");
let daemon: ChildProcess;
let fake: Server;
let base = "";
let token = "";
let fakeUrl = "";

test.beforeAll(async () => {
  // A local OpenAI-compatible endpoint that streams a short reply, word by word.
  fake = createServer((req, res) => {
    if (req.url?.startsWith("/v1/models")) {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ object: "list", data: [{ id: "local-model" }] }));
      return;
    }
    res.writeHead(200, { "content-type": "text/event-stream" });
    const words = ["Hello ", "from ", "the ", "local ", "model."];
    let i = 0;
    const timer = setInterval(() => {
      if (i < words.length) {
        const chunk = { id: "1", object: "chat.completion.chunk", model: "local-model", choices: [{ index: 0, delta: { content: words[i++] }, finish_reason: null }] };
        res.write(`data: ${JSON.stringify(chunk)}\n\n`);
      } else {
        clearInterval(timer);
        const end = { id: "1", object: "chat.completion.chunk", model: "local-model", choices: [{ index: 0, delta: {}, finish_reason: "stop" }], usage: { prompt_tokens: 5, completion_tokens: 5, total_tokens: 10 } };
        res.end(`data: ${JSON.stringify(end)}\n\ndata: [DONE]\n\n`);
      }
    }, 80);
  });
  await new Promise<void>((r) => fake.listen(0, "127.0.0.1", r));
  const addr = fake.address();
  fakeUrl = `http://127.0.0.1:${typeof addr === "object" && addr ? addr.port : 0}/v1`;

  const dir = mkdtempSync(join(tmpdir(), "patina-e2e-"));
  daemon = spawn(BIN, ["serve", "--port", "0", "--config-dir", join(dir, "c"), "--data-dir", join(dir, "d")]);
  let out = "";
  await new Promise<void>((resolve, reject) => {
    const t = setTimeout(() => reject(new Error(`daemon did not start: ${out}`)), 10_000);
    daemon.stdout!.on("data", (d) => {
      out += d;
      const url = out.match(/running at (http:\/\/\S+)/);
      const tok = out.match(/setup token[^\n]*\n\s+([0-9a-f]{64})/);
      if (url && tok) {
        base = url[1];
        token = tok[1];
        clearTimeout(t);
        resolve();
      }
    });
    daemon.on("exit", (c) => reject(new Error(`daemon exited with ${c}: ${out}`)));
  });
});

test.afterAll(() => {
  daemon?.kill("SIGTERM");
  fake?.close();
});

test("set up, connect a provider, chat, and replay after a reload", async ({ page }) => {
  await page.goto(base);
  await page.getByLabel("Setup token").fill(token);
  await page.getByLabel("Password", { exact: true }).fill("a long enough password");
  await page.getByLabel("Repeat password").fill("a long enough password");
  await page.getByRole("button", { name: "Set password" }).click();

  // Provider: only a base URL, a key and (later) a model id.
  await page.getByLabel("Start from a preset").selectOption("Custom OpenAI-compatible");
  await page.getByLabel("Name (id)").fill("local");
  await page.getByLabel(/^Base URL/).fill(fakeUrl);
  await page.getByLabel(/^API key/).fill(KEY);
  await page.getByRole("button", { name: "Save" }).click();
  await expect(page.getByText("stored, ends in 4d2e")).toBeVisible();

  await page.getByLabel("Model to test on local").fill("local-model");
  await page.getByRole("button", { name: "Test connection" }).click();
  await expect(page.getByText(/Connection works\./)).toBeVisible();

  await page.getByLabel(/^Model id/).fill("local-model");
  await page.getByRole("button", { name: "Save" }).click();

  // Adding the first agent lands on Chat.
  await expect(page.getByRole("button", { name: "Send" })).toBeVisible();
  await page.getByLabel("Prompt", { exact: true }).fill("Say hello");
  await page.getByRole("button", { name: "Send" }).click();
  await expect(page.locator(".bubble.agent .text")).toHaveText("Hello from the local model.");
  await expect(page.locator(".status")).toHaveText("completed");
  await expect(page.getByText("5 in, 5 out")).toBeVisible();

  // A reload replays the run from the event log.
  await page.reload();
  await expect(page.locator(".bubble.user")).toHaveText("Say hello");
  await expect(page.locator(".bubble.agent .text")).toHaveText("Hello from the local model.");
  await expect(page.locator(".status")).toHaveText("completed");

  // The key is nowhere in the page or in browser storage.
  const html = await page.content();
  expect(html).not.toContain("SECRETVALUE");
  const stored = await page.evaluate(() => JSON.stringify({ ...localStorage }) + JSON.stringify({ ...sessionStorage }) + document.cookie);
  expect(stored).not.toContain("SECRETVALUE");
  expect(stored).not.toContain("patina_session");

  // Sign out, then the app asks for the password again.
  await page.getByRole("button", { name: "Sign out" }).click();
  await expect(page.getByText("Sign in to continue.")).toBeVisible();
});
