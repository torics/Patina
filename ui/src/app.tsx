import { api } from "./api";
import { Auth } from "./auth";
import { Chat } from "./chat";
import { Settings } from "./settings";
import { agents, notice, report, screen, view } from "./state";

export function App() {
  if (screen.value === "loading") return <p class="center">Loading…</p>;
  if (screen.value === "setup" || screen.value === "login") return <Auth mode={screen.value} />;
  return (
    <div class="shell">
      <header>
        <strong>Patina</strong>
        <nav>
          <button class={view.value === "chat" ? "tab on" : "tab"} onClick={() => (view.value = "chat")} disabled={agents.value.length === 0}>
            Chat
          </button>
          <button class={view.value === "settings" ? "tab on" : "tab"} onClick={() => (view.value = "settings")}>
            Settings
          </button>
        </nav>
        <button
          class="link"
          onClick={() =>
            api
              .logout()
              .then(() => (screen.value = "login"))
              .catch(report)
          }
        >
          Sign out
        </button>
      </header>
      {notice.value && (
        <p class="notice" role="alert">
          {notice.value}{" "}
          <button class="link" onClick={() => (notice.value = null)}>
            Dismiss
          </button>
        </p>
      )}
      <main>{view.value === "chat" ? <Chat /> : <Settings />}</main>
    </div>
  );
}
