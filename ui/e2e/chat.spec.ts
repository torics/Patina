// Smoke tests against the real binary: set up, add a provider and agent, chat, reload and replay; run an agent in
// a project, approve its edit after reading the diff, and merge the result; then let an agent run a command in
// the sandbox and put the command on the project's allowlist.
import { expect, test } from "@playwright/test";
import { execFileSync, spawn, type ChildProcess } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
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
let repo = "";

// The later tests use the provider and agent the first one creates.
test.describe.configure({ mode: "serial" });

function chunk(delta: object, finish: string | null, usage?: object) {
  return `data: ${JSON.stringify({ id: "1", object: "chat.completion.chunk", model: "local-model", choices: [{ index: 0, delta, finish_reason: finish }], usage })}\n\n`;
}

test.beforeAll(async () => {
  // A local OpenAI-compatible endpoint.
  fake = createServer((req, res) => {
    if (req.url?.startsWith("/v1/models")) {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ object: "list", data: [{ id: "local-model" }] }));
      return;
    }
    let raw = "";
    req.on("data", (d) => (raw += d));
    req.on("end", () => {
      const body = JSON.parse(raw || "{}");
      res.writeHead(200, { "content-type": "text/event-stream" });
      const usage = { prompt_tokens: 5, completion_tokens: 5, total_tokens: 10 };
      const messages: { role: string }[] = body.messages ?? [];
      const answered = messages.some((m) => m.role === "tool");
      if (body.tools?.length && JSON.stringify(messages).includes("Run the tests")) {
        // A run that needs a command.
        if (answered) {
          res.write(chunk({ content: "The tests pass." }, null));
          res.end(chunk({}, "stop", usage) + "data: [DONE]\n\n");
        } else {
          const args = JSON.stringify({ command: "echo tests passed" });
          res.write(chunk({ tool_calls: [{ index: 0, id: "call_1", type: "function", function: { name: "run_command", arguments: args } }] }, null));
          res.end(chunk({}, "tool_calls", usage) + "data: [DONE]\n\n");
        }
        return;
      }
      if (body.tools?.length && !answered) {
        // A run in a project: ask to write a file. The daemon must get the owner's approval first.
        const args = JSON.stringify({ path: "NOTES.md", content: "written by the agent\n" });
        res.write(chunk({ tool_calls: [{ index: 0, id: "call_1", type: "function", function: { name: "write_file", arguments: args } }] }, null));
        res.end(chunk({}, "tool_calls", usage) + "data: [DONE]\n\n");
        return;
      }
      if (body.tools?.length) {
        res.write(chunk({ content: "The notes are written." }, null));
        res.end(chunk({}, "stop", usage) + "data: [DONE]\n\n");
        return;
      }
      // A plain chat: stream a short reply, word by word.
      const words = ["Hello ", "from ", "the ", "local ", "model."];
      let i = 0;
      const timer = setInterval(() => {
        if (i < words.length) {
          res.write(chunk({ content: words[i++] }, null));
        } else {
          clearInterval(timer);
          res.end(chunk({}, "stop", usage) + "data: [DONE]\n\n");
        }
      }, 80);
    });
  });
  await new Promise<void>((r) => fake.listen(0, "127.0.0.1", r));
  const addr = fake.address();
  fakeUrl = `http://127.0.0.1:${typeof addr === "object" && addr ? addr.port : 0}/v1`;

  const dir = mkdtempSync(join(tmpdir(), "patina-e2e-"));
  // A real repository for the project test.
  repo = join(dir, "repo");
  mkdirSync(repo);
  writeFileSync(join(repo, "README.md"), "hello\n");
  const git = (...args: string[]) => execFileSync("git", ["-c", "user.name=e2e", "-c", "user.email=e2e@example.com", ...args], { cwd: repo });
  git("init", "-q", "-b", "main");
  git("add", ".");
  git("commit", "-q", "-m", "first");
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

test("run an agent in a project, approve its edit after reading the diff, and merge", async ({ page }) => {
  await page.goto(base);
  await page.getByLabel("Password", { exact: true }).fill("a long enough password");
  await page.getByRole("button", { name: "Sign in" }).click();

  // Register the repository as a project.
  await page.getByRole("button", { name: "Settings" }).click();
  await page.getByRole("button", { name: "Add project" }).click();
  await page.getByLabel(/^Folder/).fill(repo);
  await page.getByRole("button", { name: "Save" }).click();
  // The default policy applies edits without asking. This test is about approving an edit, so ask for each one.
  await expect(page.getByLabel("Approvals for repo")).toHaveValue("auto_edit");
  await page.getByLabel("Approvals for repo").selectOption("confirm_all");
  // The page says what contains commands on this machine, and claims no limit that is not enforced.
  const sandbox = page.getByRole("region", { name: "Sandbox" });
  await expect(sandbox.getByRole("heading")).toContainText("Sandbox on this machine");
  await expect(sandbox).toContainText("Memory, CPU and process limits: none enforced");

  // Start a run in it.
  await page.getByRole("button", { name: "Chat" }).click();
  await page.getByLabel("Project").selectOption("repo");
  await page.getByLabel("Prompt", { exact: true }).fill("Write the notes");
  await page.getByRole("button", { name: "Send" }).click();

  // The edit waits for the owner: the diff is on screen and nothing is written yet.
  const approval = page.getByRole("group", { name: "Approval for write_file" });
  await expect(approval).toBeVisible();
  await expect(page.locator(".status")).toHaveText("waiting approval");
  await expect(page.locator(".tool pre.diff")).toContainText("+written by the agent");
  expect(existsSync(join(repo, "NOTES.md"))).toBe(false);

  await approval.getByRole("button", { name: "Approve", exact: true }).click();
  await expect(page.getByText("You approved this.")).toBeVisible();
  await expect(page.locator(".bubble.agent .text")).toHaveText("The notes are written.");
  await expect(page.locator(".status")).toHaveText("completed");
  await expect(page.getByText(/^Checkpoint after step 1/)).toBeVisible();

  // The change is on the run's branch, shown as a diff, and still not in the owner's checkout.
  const changes = page.getByRole("region", { name: "Changes" });
  await expect(changes.locator(".files")).toHaveText("added NOTES.md");
  await expect(changes.locator("pre.diff")).toContainText("+written by the agent");
  expect(existsSync(join(repo, "NOTES.md"))).toBe(false);

  // A reload replays the whole run, approval included.
  await page.reload();
  await expect(page.getByText("You approved this.")).toBeVisible();
  await expect(page.locator(".status")).toHaveText("completed");

  await page.getByRole("region", { name: "Changes" }).getByRole("button", { name: "Merge into your branch" }).click();
  await expect(page.getByText("Merged into main.")).toBeVisible();
  await expect(page.getByText(/These changes were merged/)).toBeVisible();
  expect(readFileSync(join(repo, "NOTES.md"), "utf8")).toBe("written by the agent\n");
});

test("a command runs in the sandbox, and approving it can put it on the allowlist", async ({ page }) => {
  await page.goto(base);
  await page.getByLabel("Password", { exact: true }).fill("a long enough password");
  await page.getByRole("button", { name: "Sign in" }).click();
  await page.getByRole("button", { name: "Settings" }).click();

  // This needs a sandbox. CI has one and says so; elsewhere the test is skipped where the machine has none.
  const heading = await page.getByRole("region", { name: "Sandbox" }).getByRole("heading").textContent();
  const contained = !heading?.includes("no sandbox");
  if (process.env.PATINA_REQUIRE_LANDLOCK === "1") expect(heading).toContain("Landlock");
  test.skip(!contained, "this machine has no sandbox");

  // With a sandbox, "Never ask" is offered. Edits are automatic and commands ask, unless they are on the allowlist.
  const approvals = page.getByLabel("Approvals for repo");
  await expect(approvals.locator("option")).toHaveCount(3);
  await approvals.selectOption("auto_edit");
  await expect(page.locator("li.card", { hasText: "repo" }).locator(".badge")).toHaveText(/sandbox: (landlock|container)/);

  await page.getByRole("button", { name: "Chat" }).click();
  await page.getByLabel("Project").selectOption("repo");
  await page.getByLabel("Prompt", { exact: true }).fill("Run the tests");
  await page.getByRole("button", { name: "Send" }).click();

  const approval = page.getByRole("group", { name: "Approval for run_command" });
  await expect(approval).toBeVisible();
  await expect(page.locator(".timeline .badge")).toHaveText(/^Sandbox: /);
  await approval.getByText("Always allow commands like this").click();
  await expect(approval.getByLabel(/^Pattern for this project/)).toHaveValue("echo tests *");
  await approval.getByRole("button", { name: "Approve and always allow" }).click();
  await expect(page.locator(".bubble.agent .text")).toHaveText("The tests pass.");
  await expect(page.locator(".status")).toHaveText("completed");
  await page.locator(".tool", { hasText: "run_command" }).getByText("Result").click();
  await expect(page.locator(".tool", { hasText: "run_command" }).locator("pre").last()).toHaveText("exit code 0\ntests passed");

  // The pattern is in the project's settings now.
  await page.getByRole("button", { name: "Settings" }).click();
  await page.getByText("Sandbox and commands").click();
  await expect(page.locator("textarea[name=allow_commands]")).toHaveValue("echo tests *");

  // The same command no longer asks.
  await page.getByRole("button", { name: "Chat" }).click();
  await page.getByRole("button", { name: "New chat" }).click();
  await page.getByLabel("Project").selectOption("repo");
  await page.getByLabel("Prompt", { exact: true }).fill("Run the tests again");
  await page.getByRole("button", { name: "Send" }).click();
  await expect(page.locator(".bubble.agent .text")).toHaveText("The tests pass.");
  await expect(page.locator(".status")).toHaveText("completed");
  await expect(page.getByRole("group", { name: "Approval for run_command" })).toHaveCount(0);
  await expect(page.getByText("You approved this.")).toHaveCount(0);

  // A setting the daemon refuses is shown as an error and is not saved: a credentials file needs an explicit yes.
  await page.getByRole("button", { name: "Settings" }).click();
  await page.getByText("Sandbox and commands").click();
  await page.locator("textarea[name=worktree_copy]").fill(".env");
  await expect(page.getByText("looks like a file with credentials")).toBeVisible();
  await page.getByRole("button", { name: "Save settings" }).click();
  await expect(page.locator(".project-settings .error")).toContainText("looks like a file with credentials");
  await page.getByLabel("Copy .env anyway").check();
  await page.getByRole("button", { name: "Save settings" }).click();
  await expect(page.getByText("Saved.")).toBeVisible();
});
