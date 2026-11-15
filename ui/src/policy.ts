// The command allowlist as the daemon applies it (`patina-core/src/policy.rs`), so the approval card can show
// whether a pattern covers the command in front of the owner. The daemon decides; this only previews.

const SHELL_OPERATORS = [";", "&", "|", "`", "$(", ">", "<", "\n", "\r"];

function glob(pattern: string, text: string): boolean {
  const parts = pattern.split("*");
  if (parts.length === 1) return pattern === text;
  const [first, last] = [parts[0], parts[parts.length - 1]];
  if (!text.startsWith(first) || text.length < first.length + last.length || !text.endsWith(last)) return false;
  let rest = text.slice(first.length, text.length - last.length);
  for (const part of parts.slice(1, -1)) {
    const i = rest.indexOf(part);
    if (i < 0) return false;
    rest = rest.slice(i + part.length);
  }
  return true;
}

/** Whether a pattern covers a command. `*` matches any run of characters; a command with shell operators
 * is covered only by a pattern that equals it. */
export function patternMatches(pattern: string, command: string): boolean {
  const [p, c] = [pattern.trim(), command.trim()];
  if (!p) return false;
  const plain = !SHELL_OPERATORS.some((op) => c.includes(op));
  return p === c || (plain && p.includes("*") && glob(p, c));
}

/** A pattern to offer for "always allow": the command's first two words and anything after them. */
export function suggestPattern(command: string): string {
  const c = command.trim();
  if (SHELL_OPERATORS.some((op) => c.includes(op))) return c;
  const words = c.split(/\s+/);
  return words.length <= 2 ? c : `${words[0]} ${words[1]} *`;
}

/** File names that usually hold credentials, as `looks_like_credentials` in the daemon. Copying one into a
 * worktree needs the owner's explicit yes. */
export function looksLikeCredentials(path: string): boolean {
  const name = (path.split("/").pop() ?? path).toLowerCase();
  return (
    name === ".env" ||
    name.startsWith(".env.") ||
    name.endsWith(".env") ||
    [".pem", ".key", ".p12", ".pfx", ".keystore", ".jks"].some((ext) => name.endsWith(ext)) ||
    [".npmrc", ".netrc", ".pypirc", ".git-credentials", "id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"].includes(name) ||
    name.includes("secret") ||
    name.includes("credential")
  );
}

/** The non-empty lines of a text box, trimmed. */
export function lines(text: string): string[] {
  return text
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l.length > 0);
}

/** `NAME=value` lines as a map. Returns the first line that is not in that form as `bad`. */
export function parseEnv(text: string): { env: Record<string, string>; bad: string | null } {
  const env: Record<string, string> = {};
  for (const line of lines(text)) {
    const i = line.indexOf("=");
    if (i <= 0) return { env, bad: line };
    env[line.slice(0, i).trim()] = line.slice(i + 1);
  }
  return { env, bad: null };
}
