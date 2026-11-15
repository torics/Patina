// Smoke tests against the real binary: set up, add a provider and agent, chat, reload and replay; run an agent in
// a project, approve its edit after reading the diff, and merge the result; then let an agent run a command in
// the sandbox and put the command on the project's allowlist; pause an agent that repeats itself, resume it,
// and send it a message; and make teams from a template and by hand, run them, and see a sub-agent nested under
// the call that started it.
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
      const all = JSON.stringify(messages);
      if (all.includes("Plan and build")) {
        // Team runs: each agent answers by its role, which its system prompt states.
        const reply = (text: string) => {
          res.write(chunk({ content: text }, null));
          res.end(chunk({}, "stop", usage) + "data: [DONE]\n\n");
        };
        if (body.tools?.some((t: any) => t.function?.name === "delegate")) {
          if (answered) return reply("The builder is done.");
          const args = JSON.stringify({ agent: "plan-build-review-builder", task: "Plan and build: the build part" });
          res.write(chunk({ tool_calls: [{ index: 0, id: "call_d", type: "function", function: { name: "delegate", arguments: args } }] }, null));
          res.end(chunk({}, "tool_calls", usage) + "data: [DONE]\n\n");
          return;
        }
        if (all.includes("You plan changes")) return reply("1. Build it.");
        if (all.includes("You implement changes")) return reply("Built it.");
        return reply("Looks good.");
      }
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
      if (body.tools?.length && JSON.stringify(messages).includes("Keep reading")) {
        // An agent that repeats itself until the owner tells it to stop.
        if (JSON.stringify(messages.at(-1)).includes("Stop reading")) {
          res.write(chunk({ content: "I stopped reading." }, null));
          res.end(chunk({}, "stop", usage) + "data: [DONE]\n\n");
        } else {
          const args = JSON.stringify({ path: "README.md" });
          res.write(chunk({ tool_calls: [{ index: 0, id: "call_1", type: "function", function: { name: "read_file", arguments: args } }] }, null));
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
  // Only enforced limits are listed; without a delegated cgroup that is the file size cap (and a container's, if any).
  if (process.platform === "win32") {
    // Windows has neither Landlock, cgroups nor RLIMIT_FSIZE, and the page says so instead of implying a sandbox.
    await expect(sandbox).toContainText("The Linux sandbox (Landlock) and cgroup limits are not available on this operating system");
    await expect(sandbox).not.toContainText("RLIMIT_FSIZE");
  } else {
    await expect(sandbox).toContainText("Resource limits enforced here: largest file a command may write (RLIMIT_FSIZE)");
    await expect(sandbox).not.toContainText("cgroup");
  }

  // Start a run in it.
  await page.getByRole("button", { name: "Workbench" }).click();
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

  await page.getByRole("button", { name: "Workbench" }).click();
  await page.getByLabel("Project").selectOption("repo");
  await page.getByLabel("Prompt", { exact: true }).fill("Run the tests");
  await page.getByRole("button", { name: "Send" }).click();

  const approval = page.getByRole("group", { name: "Approval for run_command" });
  await expect(approval).toBeVisible();
  await expect(page.locator(".runbar .badge")).toHaveText(/^Sandbox: /);
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
  await page.getByRole("button", { name: "Workbench" }).click();
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

test("an agent that repeats itself is paused, and a message sends it on", async ({ page }) => {
  await page.goto(base);
  await page.getByLabel("Password", { exact: true }).fill("a long enough password");
  await page.getByRole("button", { name: "Sign in" }).click();
  if (await page.getByRole("button", { name: "New chat" }).count()) await page.getByRole("button", { name: "New chat" }).click();
  await page.getByLabel("Project").selectOption("repo");
  await page.getByLabel("Prompt", { exact: true }).fill("Keep reading the readme");
  await page.getByRole("button", { name: "Send" }).click();

  // The same read three times in a row pauses the run.
  await expect(page.locator(".status")).toHaveText("paused");
  await expect(page.locator(".note.warn")).toContainText("same read_file call 3 times in a row");
  await expect(page.getByRole("button", { name: "Stop" })).toHaveCount(0);

  // Resuming as it is lets it go on, until it repeats itself again.
  await page.getByRole("button", { name: "Resume", exact: true }).click();
  await expect(page.locator(".note.warn")).toHaveCount(2);
  await expect(page.locator(".status")).toHaveText("paused");
  await expect(page.locator(".note", { hasText: "Resumed." })).toHaveCount(1);

  // A message resumes it, and the agent reads it before its next turn.
  await page.getByLabel(/^Message the agent/).fill("Stop reading; the file will not change.");
  await page.getByRole("button", { name: "Send and resume" }).click();
  await expect(page.locator(".status")).toHaveText("completed");
  await expect(page.locator(".bubble.user.steer")).toContainText("Stop reading; the file will not change.");
  await expect(page.locator(".bubble.agent .text").last()).toHaveText("I stopped reading.");
  // Everything happened in one block: the agent picked up where it was. (The other block prepared the worktree.)
  await expect(page.locator(".bubble.agent:not([aria-label='Preparing the worktree'])")).toHaveCount(1);
  await expect(page.getByRole("button", { name: "Resume", exact: true })).toHaveCount(0);

  // A reload replays the same timeline.
  await page.reload();
  await expect(page.locator(".bubble.user.steer")).toHaveCount(1);
  await expect(page.locator(".note.warn")).toHaveCount(2);
  await expect(page.locator(".bubble.agent .text").last()).toHaveText("I stopped reading.");
});

test("teams: from a template and by hand, with a sub-agent shown under its delegate call", async ({ page }) => {
  await page.goto(base);
  await page.getByLabel("Password", { exact: true }).fill("a long enough password");
  await page.getByRole("button", { name: "Sign in" }).click();
  await page.getByRole("button", { name: "Settings" }).click();

  // A template names no model: the owner picks the provider and model.
  await page.getByRole("button", { name: "Team from a template" }).click();
  await page.getByLabel("Template").selectOption({ label: "Plan, build, review" });
  await page.getByLabel("Every agent: provider").selectOption("local");
  await page.getByLabel("Every agent: model id").fill("local-model");
  await page.getByRole("button", { name: "Create team" }).click();
  await expect(page.locator("li.card", { hasText: "Plan, build, review" })).toContainText("sequential");

  // A team by hand: the planner leads and hands the build to the builder.
  await page.getByRole("button", { name: "Add team" }).click();
  await page.getByLabel("Name", { exact: true }).fill("Lead");
  await page.locator("select[name=mode]").selectOption("delegate");
  await page.getByLabel(/^Members, in order/).fill("plan-build-review-planner, plan-build-review-builder");
  await page.getByRole("button", { name: "Save" }).click();
  await expect(page.locator("li.card", { hasText: "Lead" })).toContainText("delegate");
  await expect(page.getByRole("link", { name: "Export Lead" })).toHaveAttribute("href", "/api/v1/teams/lead/export");

  // The sequential team: three agents, one after another, each in its own block.
  await page.getByRole("button", { name: "Workbench" }).click();
  if (await page.getByRole("button", { name: "New chat" }).count()) await page.getByRole("button", { name: "New chat" }).click();
  await page.getByLabel("Team or agent").selectOption("team:plan-build-review");
  await expect(page.getByText(/3 agents working one after another/)).toBeVisible();
  await page.getByLabel("Project").selectOption("repo");
  await page.getByLabel("Prompt", { exact: true }).fill("Plan and build a feature");
  await page.getByRole("button", { name: "Send" }).click();
  await expect(page.locator(".status")).toHaveText("completed");
  const blocks = page.locator(".bubble.agent:not([aria-label='Preparing the worktree'])");
  await expect(blocks.locator("h3")).toContainText(["Planner", "Builder", "Reviewer"]);
  await expect(blocks.locator(".text")).toHaveText(["1. Build it.", "Built it.", "Looks good."]);
  await expect(page.getByText("Team plan-build-review, one after another.")).toBeVisible();

  // The delegate team: the builder's work appears inside the planner's delegate call.
  await page.getByRole("button", { name: "New chat" }).click();
  await page.getByLabel("Team or agent").selectOption("team:lead");
  await page.getByLabel("Project").selectOption("repo");
  await page.getByLabel("Prompt", { exact: true }).fill("Plan and build with help");
  await page.getByRole("button", { name: "Send" }).click();
  await expect(page.locator(".status")).toHaveText("completed");
  const call = page.locator(".tool", { hasText: "delegate" });
  await expect(call.locator(".task")).toHaveText("Plan and build: the build part");
  const sub = call.getByRole("article", { name: "Sub-agent Builder" });
  await expect(sub.locator(".text")).toHaveText("Built it.");
  await expect(page.locator(".timeline > div > .bubble.agent").last().locator("> .text")).toHaveText("The builder is done.");

  // A reload replays the nesting.
  await page.reload();
  await expect(page.locator(".tool", { hasText: "delegate" }).getByRole("article", { name: "Sub-agent Builder" })).toBeVisible();
  await expect(page.locator(".runs small").first()).toContainText("team lead");
});
