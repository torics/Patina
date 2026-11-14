import { useState } from "preact/hooks";
import { api } from "./api";
import { enter } from "./state";

export function Auth({ mode }: { mode: "setup" | "login" }) {
  const [token, setToken] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: Event) {
    e.preventDefault();
    setError(null);
    if (mode === "setup" && password !== confirm) return setError("The two passwords differ.");
    setBusy(true);
    try {
      if (mode === "setup") await api.setup(token, password);
      else await api.login(password);
      await enter();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form class="card narrow" onSubmit={submit}>
      <h1>Patina</h1>
      {mode === "setup" ? (
        <>
          <p>Welcome. Enter the one-time setup token that <code>patina serve</code> printed in its terminal, then choose your password.</p>
          <label>
            Setup token
            <input name="token" value={token} onInput={(e) => setToken(e.currentTarget.value)} autocomplete="off" required />
          </label>
        </>
      ) : (
        <p>Sign in to continue.</p>
      )}
      <label>
        Password
        <input name="password" type="password" value={password} onInput={(e) => setPassword(e.currentTarget.value)} autocomplete={mode === "setup" ? "new-password" : "current-password"} minLength={mode === "setup" ? 10 : undefined} required />
      </label>
      {mode === "setup" && (
        <label>
          Repeat password
          <input name="confirm" type="password" value={confirm} onInput={(e) => setConfirm(e.currentTarget.value)} autocomplete="new-password" required />
        </label>
      )}
      {error && (
        <p class="error" role="alert">
          {error}
        </p>
      )}
      <button type="submit" disabled={busy}>
        {mode === "setup" ? "Set password" : "Sign in"}
      </button>
    </form>
  );
}
