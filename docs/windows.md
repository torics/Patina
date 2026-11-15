# Patina on Windows

Patina runs on Windows 10 and 11 (x64) as a single `patina.exe`. It works well as a local workbench for models
that run elsewhere: a provider on the internet, or a model server on another machine in your network.

## Run it

1. Install [Git for Windows](https://git-scm.com/download/win) if you do not have it. Projects need git; plain
   chat does not.
2. Put `patina.exe` in a folder of its own, for example `C:\Tools\Patina`.
3. Open PowerShell or Command Prompt in that folder and run:

   ```powershell
   .\patina.exe doctor
   .\patina.exe serve
   ```

   SmartScreen may warn the first time, because the exe is not signed. Choose **More info**, then **Run anyway**.
   Patina listens on `127.0.0.1` only. To reach it from another device, see [remote.md](remote.md).
4. `serve` prints the address, `http://127.0.0.1:7878`, and a one-time setup token. Open the address in your
   browser, paste the token and choose a password.
5. In Settings, add a provider. For a model on another machine in your network, use an OpenAI-compatible preset
   and its address, for example `http://192.168.1.20:11434/v1` for Ollama. Then add an agent and chat.

Keep the window open while you use Patina. Ctrl+C or closing the window stops it cleanly; running runs are
recorded as interrupted and can be resumed. `patina.exe reset-password` prints a new setup token if you forget
your password.

Patina keeps its settings in `%APPDATA%\patina\config` and its data (database, worktrees, logs) in
`%APPDATA%\patina\data`. `patina.exe doctor` prints both.

## What is different from Linux

- **No sandbox.** Landlock and cgroups are Linux features, so commands an agent runs are not contained: they run
  as you, with your files. Patina therefore asks before every edit and every command, and autopilot is not
  offered. `doctor`, Settings and each run say so. With Docker Desktop installed, a project with a
  `container_image` runs its commands in a Linux container instead.
- **No resource limits** on commands outside a container (no cgroups and no file size limit).
- **Commands run with `cmd.exe`.** Agents are told this, so they use `dir`, `type` and `&&` rather than `ls`,
  `cat` and `;`.
- Git's line-ending setting (`core.autocrlf`) applies as usual when a run's changes are merged into your checkout.
