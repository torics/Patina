// Writes the license texts of every package the built UI bundles: the production dependencies in
// ui/package-lock.json, read from ui/node_modules. Run after `npm ci` in ui/.
import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const ui = new URL("../ui/", import.meta.url).pathname;
const lock = JSON.parse(readFileSync(join(ui, "package-lock.json"), "utf8"));
const out = ["Third-party notices for Patina's web interface", ""];
const entries = Object.entries(lock.packages)
  .filter(([path, p]) => path.startsWith("node_modules/") && !p.dev)
  .sort(([a], [b]) => a.localeCompare(b));
for (const [path, p] of entries) {
  const dir = join(ui, path);
  const name = path.slice(path.lastIndexOf("node_modules/") + "node_modules/".length);
  const file = readdirSync(dir).find((f) => /^(license|licence|copying)/i.test(f));
  if (!file) throw new Error(`${name} ${p.version} has no license file`);
  out.push("=".repeat(72), `${name} ${p.version} (${p.license})`, "=".repeat(72), readFileSync(join(dir, file), "utf8").trim(), "");
}
writeFileSync(process.argv[2] ?? "THIRD_PARTY_NOTICES_UI.txt", out.join("\n") + "\n");
console.log(`${entries.length} packages`);
