// The workbench against the real binary: browse and edit files, attach one with @, read a file's diff, edit a
// stopped run's copy, preview the run's dev server, export the run; and, on a phone, start, steer, approve and
// merge a run.
import { expect, test, type Page } from "@playwright/test";
import { execFileSync, spawn, type ChildProcess } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { createServer, type Server } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";

const BIN = process.env.PATINA_BIN ?? join(import.meta.dirname, "../../target/debug/patina");
const PASSWORD = "a long enough password";
let daemon: ChildProcess;
let fake: Server;
let base = "";
let repo = "";
/** What the model was sent, request by request. */
const seen: string[] = [];

test.describe.configure({ mode: "serial" });

function chunk(delta: object, finish: string | null) {
  const usage = finish ? { prompt_tokens: 5, completion_tokens: 5, total_tokens: 10 } : undefined;
  return `data: ${JSON.stringify({ id: "1", object: "chat.completion.chunk", model: "m", choices: [{ index: 0, delta, finish_reason: finish }], usage })}\n\n`;
}

test.beforeAll(async () => {
  fake = createServer((req, res) => {
    if (req.url?.startsWith("/v1/models")) {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ object: "list", data: [{ id: "m" }] }));
      return;
    }
    let raw = "";
    req.on("data", (d) => (raw += d));
    req.on("end", () => {
      const body = JSON.parse(raw || "{}");
      seen.push(raw);
      const messages: any[] = body.messages ?? [];
      const all = JSON.stringify(messages);
      res.writeHead(200, { "content-type": "text/event-stream" });
      const say = (text: string) => res.end(chunk({ content: text }, null) + chunk({}, "stop") + "data: [DONE]\n\n");
      const call = (name: string, args: object) =>
        res.end(chunk({ tool_calls: [{ index: 0, id: "call_1", type: "function", function: { name, arguments: JSON.stringify(args) } }] }, null) + chunk({}, "tool_calls") + "data: [DONE]\n\n");
      const answered = messages.some((m) => m.role === "tool");
      if (!answered) return call("write_file", { path: "page.html", content: "<h1>Made by the agent</h1>\n" });
      if (all.includes("Use a heading")) return say("Got your note about the heading.");
      return say("The page is written.");
    });
  });
  await new Promise<void>((r) => fake.listen(0, "127.0.0.1", r));
  const addr = fake.address();
  const fakeUrl = `http://127.0.0.1:${typeof addr === "object" && addr ? addr.port : 0}/v1`;

  const dir = mkdtempSync(join(tmpdir(), "patina-wb-"));
  repo = join(dir, "site");
  mkdirSync(join(repo, "src"), { recursive: true });
  writeFileSync(join(repo, "README.md"), "# Site\n");
  writeFileSync(join(repo, "src", "app.ts"), "export const answer = 41;\n");
  const git = (...args: string[]) => execFileSync("git", ["-c", "user.name=e2e", "-c", "user.email=e2e@example.com", ...args], { cwd: repo });
  git("init", "-q", "-b", "main");
  git("add", ".");
  git("commit", "-q", "-m", "first");

  daemon = spawn(BIN, ["serve", "--port", "0", "--preview-port", "0", "--config-dir", join(dir, "c"), "--data-dir", join(dir, "d")]);
  let out = "";
  const token = await new Promise<string>((resolve, reject) => {
    const t = setTimeout(() => reject(new Error(`daemon did not start: ${out}`)), 10_000);
    daemon.stdout!.on("data", (d) => {
      out += d;
      const url = out.match(/running at (http:\/\/\S+)/);
      const tok = out.match(/setup token[^\n]*\n\s+([0-9a-f]{64})/);
      if (url && tok && /Preview is served at/.test(out)) {
        base = url[1];
        clearTimeout(t);
        resolve(tok[1]);
      }
    });
    daemon.on("exit", (c) => reject(new Error(`daemon exited with ${c}: ${out}`)));
  });

  // Set up through the API: password, provider, agent, and the project, which asks before every edit.
  const setup = await fetch(`${base}/api/v1/auth/setup`, { method: "POST", headers: { "content-type": "application/json", origin: base }, body: JSON.stringify({ token, password: PASSWORD }) });
  const cookie = setup.headers.getSetCookie()[0].split(";")[0];
  const { csrf } = await setup.json();
  const call = async (method: string, path: string, body: object) => {
    const r = await fetch(`${base}/api/v1${path}`, { method, headers: { "content-type": "application/json", origin: base, cookie, "x-patina-csrf": csrf }, body: JSON.stringify(body) });
    if (!r.ok) throw new Error(`${method} ${path}: ${r.status} ${await r.text()}`);
  };
  await call("POST", "/providers", { id: "local", kind: "openai_compatible", base_url: fakeUrl });
  await call("POST", "/agents", { id: "builder", name: "Builder", provider: "local", model: "m", tools: ["list_files", "read_file", "search", "edit_file", "write_file"] });
  await call("POST", "/projects", { path: repo, policy: "confirm_all" });
  // A dev server for Preview that serves the run's copy of the project.
  await call("PATCH", "/projects/site", { dev_command: process.platform === "win32" ? "python -m http.server {port} --bind 127.0.0.1" : "exec python3 -m http.server {port} --bind 127.0.0.1" });
});

test.afterAll(() => {
  daemon?.kill("SIGTERM");
  fake?.close();
});

/** With PATINA_SHOTS set to a folder, save a screenshot there, for looking at the layout. */
async function shot(page: Page, name: string) {
  if (process.env.PATINA_SHOTS) await page.screenshot({ path: join(process.env.PATINA_SHOTS, `${name}.png`) });
}

async function signIn(page: Page) {
  await page.goto(base);
  await page.getByLabel("Password", { exact: true }).fill(PASSWORD);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("button", { name: "Workbench" })).toBeVisible();
}

test("browse and edit files, attach one with @, review a file's diff, edit the run's copy, preview and export", async ({ page }) => {
  const violations: string[] = [];
  page.on("console", (m) => /Content.Security.Policy|Refused to/i.test(m.text()) && violations.push(`${m.text().slice(0, 60)} at ${JSON.stringify(m.location())}`));
  await signIn(page);
  await page.getByLabel("Project").selectOption("site");

  // The tree shows the checkout; a file opens in the editor and saves to the checkout.
  const tree = page.getByRole("navigation", { name: "Files" });
  await tree.getByRole("button", { name: "src" }).click();
  await tree.getByRole("button", { name: "app.ts" }).click();
  const editor = page.getByRole("region", { name: "Editor" });
  const content = editor.getByLabel("Content of src/app.ts");
  await expect(content).toContainText("export const answer = 41;");
  await content.click();
  await page.keyboard.press("Control+Home");
  await page.keyboard.press("End");
  await page.keyboard.type(" // checked");
  await expect(editor.getByText("Unsaved changes")).toBeVisible();
  await page.keyboard.press("Control+s");
  await expect(editor.getByText("Saved to your checkout.")).toBeVisible();
  expect(readFileSync(join(repo, "src/app.ts"), "utf8")).toBe("export const answer = 41; // checked\n");
  execFileSync("git", ["-c", "user.name=e2e", "-c", "user.email=e2e@example.com", "commit", "-qam", "checked"], { cwd: repo });

  // @ suggests files from the project, and a chosen one goes with the prompt.
  const prompt = page.getByLabel("Prompt", { exact: true });
  await prompt.fill("Make a page like @READ");
  await prompt.press("End");
  await expect(page.getByRole("listbox", { name: "Files to attach" }).getByRole("option")).toHaveText(["README.md"]);
  await prompt.press("Enter");
  await expect(prompt).toHaveValue("Make a page like @README.md ");
  await expect(page.locator(".attached")).toContainText("Attached: README.md");
  await page.getByRole("button", { name: "Send" }).click();
  await expect(page.locator(".bubble.user")).toHaveText("Make a page like @README.md ");

  // Approve the edit; the run's file shows as a diff in the editor's diff view.
  const approval = page.getByRole("group", { name: "Approval for write_file" });
  await expect(approval).toBeVisible();
  expect(seen.at(-1)).toContain("<attached_file path=\\\"README.md\\\">\\n# Site\\n</attached_file>");
  await approval.getByRole("button", { name: "Approve", exact: true }).click();
  await expect(page.locator(".status")).toHaveText("completed");
  await page.getByRole("button", { name: "Review the changes" }).click();
  const changes = page.getByRole("region", { name: "Changes" });
  await changes.getByRole("button", { name: "Show the diff of page.html" }).click();
  await expect(changes.getByLabel("Diff of page.html").locator(".cm-content")).toContainText("Made by the agent");
  await shot(page, "desktop-changes");

  // The stopped run's copy can be edited too: the save is a commit on the run's branch.
  await page.getByRole("button", { name: "This run's copy" }).click();
  await tree.getByRole("button", { name: "page.html" }).click();
  const runFile = editor.getByLabel("Content of page.html");
  await expect(runFile).toContainText("Made by the agent");
  await runFile.click();
  await page.keyboard.press("Control+End");
  await page.keyboard.type("<p>Edited by the owner</p>");
  await editor.getByRole("button", { name: "Save" }).click();
  await expect(editor.getByText(/committed to the run's branch/)).toBeVisible();
  await expect(page.getByText("You edited page.html.")).toBeVisible();
  await shot(page, "desktop-editor");
  await page.getByRole("tab", { name: "Changes" }).click();
  await expect(changes.locator("pre.diff")).toContainText("+<p>Edited by the owner</p>");

  // Preview: the project's dev server runs in the run's copy, on its own origin, in a sandboxed frame.
  await page.getByRole("tab", { name: "Preview" }).click();
  const preview = page.getByRole("region", { name: "Preview" });
  await preview.getByRole("button", { name: "Preview this run" }).click();
  await preview.getByRole("button", { name: "Start the dev server" }).click();
  const frame = preview.locator("iframe");
  await expect(frame).toHaveAttribute("sandbox", "allow-scripts allow-forms allow-popups");
  expect(new URL((await frame.getAttribute("src"))!).port).not.toBe(new URL(base).port);
  await expect(async () => {
    await preview.getByRole("button", { name: "Reload" }).click();
    await expect(page.frameLocator(".preview-frame").getByRole("link", { name: "page.html" })).toBeVisible({ timeout: 1000 });
  }).toPass({ timeout: 15_000 });
  await shot(page, "desktop-preview");
  await preview.getByRole("button", { name: "Stop the dev server" }).click();
  await expect(preview.getByRole("button", { name: "Start the dev server" })).toBeVisible();

  // The run exports as Markdown and as JSON.
  await page.getByText("Export").click();
  const md = page.getByRole("link", { name: "As Markdown" });
  const download = page.waitForEvent("download");
  await md.click();
  const file = readFileSync((await (await download).path())!, "utf8");
  expect(file).toContain("## Prompt\n\nMake a page like @README.md");
  expect(file).toContain("The page is written.");

  // The light theme is one click away, and stays after a reload.
  await page.getByRole("button", { name: "Use the light theme" }).click();
  await page.reload();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  await shot(page, "desktop-light");
  expect(violations).toEqual([]);
});

test.describe("on a phone", () => {
  test.use({ viewport: { width: 360, height: 740 }, isMobile: true, hasTouch: true });

  test("the owner starts, steers, approves and merges a run", async ({ page }) => {
    await signIn(page);
    // Nothing is wider than the screen.
    const fits = () => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth);
    await page.getByLabel("Project").selectOption("site");
    await page.getByLabel("Prompt", { exact: true }).fill("Write a page");
    await page.getByRole("button", { name: "Send" }).tap();

    // The run and its approval come first, and every action fits on the screen.
    const approval = page.getByRole("group", { name: "Approval for write_file" });
    await expect(approval).toBeInViewport({ ratio: 0.3 });
    await expect(page.locator(".status")).toHaveText("waiting approval");
    await expect(page.getByRole("button", { name: "1 waiting for approval" })).toBeVisible();
    for (const name of ["Approve", "Deny"]) {
      const box = (await approval.getByRole("button", { name, exact: true }).boundingBox())!;
      expect(box.x >= 0 && box.x + box.width <= 360 && box.height >= 36, `${name}: ${JSON.stringify(box)}`).toBe(true);
    }
    expect(await fits()).toBe(true);
    await shot(page, "phone-approval");

    // Steer while it waits, then approve.
    await page.getByLabel(/^Message the agent/).fill("Use a heading, please.");
    await page.getByRole("button", { name: "Send", exact: true }).tap();
    await expect(page.getByText("Sent. It appears in the timeline when the agent reads it.")).toBeVisible();
    await approval.getByRole("button", { name: "Approve", exact: true }).tap();
    await expect(page.locator(".status")).toHaveText("completed");
    await expect(page.locator(".bubble.user.steer")).toContainText("Use a heading, please.");
    await expect(page.locator(".bubble.agent .text").last()).toHaveText("Got your note about the heading.");
    await expect(page.getByRole("button", { name: "1 waiting for approval" })).toHaveCount(0);

    // Review and merge from the Changes pane.
    await page.getByRole("button", { name: "Review the changes" }).tap();
    const changes = page.getByRole("region", { name: "Changes" });
    await expect(changes.locator(".files")).toHaveText("added page.html");
    expect(await fits()).toBe(true);
    await shot(page, "phone-changes");
    await changes.getByRole("button", { name: "Merge into your branch" }).tap();
    await expect(changes.getByText("Merged into main.")).toBeVisible();
    expect(readFileSync(join(repo, "page.html"), "utf8")).toBe("<h1>Made by the agent</h1>\n");

    // The panes are one tap away.
    await page.getByRole("navigation", { name: "Panes" }).getByRole("button", { name: "Run" }).tap();
    await expect(page.locator(".bubble.agent .text").last()).toBeVisible();
  });
});
