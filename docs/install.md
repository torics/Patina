# Install and first run

From download to a first answer takes a few minutes. You need git, and an API key for a model provider (or a local model server such as Ollama).

## 1. Get the binary

Linux and macOS, with the install script. It picks the build for your machine, checks it against the release's `SHA256SUMS` and installs `patina` to `~/.local/bin`:

```sh
curl -fsSL https://github.com/torics/Patina/releases/latest/download/install.sh | sh
```

Or download the archive for your machine from the releases page and check it yourself:

| Machine | Archive |
| --- | --- |
| Linux, x86_64 | `patina-x86_64-unknown-linux-musl.tar.gz` |
| Linux, ARM64 | `patina-aarch64-unknown-linux-musl.tar.gz` |
| macOS, Apple silicon | `patina-aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `patina-x86_64-apple-darwin.tar.gz` |
| Windows, x86_64 | `patina-x86_64-pc-windows-msvc.zip` |

```sh
sha256sum -c SHA256SUMS --ignore-missing    # macOS: shasum -a 256 -c SHA256SUMS --ignore-missing
tar -xzf patina-x86_64-unknown-linux-musl.tar.gz
install -m 0755 patina-x86_64-unknown-linux-musl/patina ~/.local/bin/
```

The Linux builds are static and run on any distribution. While the repository is private, the script and the release links need a signed-in download: use `gh release download --repo torics/Patina --pattern 'patina-<target>*' --pattern SHA256SUMS`, then the steps above.

## 2. Check the machine

```sh
patina doctor
```

It reports git, the sandbox this machine offers (Landlock on Linux 5.13 and later, or Docker or Podman), which resource limits hold, whether the ports are free, and any problem in the config files. Nothing it reports stops you from starting; without a sandbox every edit and command asks for your approval.

## 3. Start it and sign in

```sh
patina serve
```

It prints the address, `http://127.0.0.1:7878`, and a one-time setup token. Open the address, enter the token and choose a password.

## 4. Add a provider and an agent

In Settings, **Add provider**, start from a preset and paste your key (see [providers.md](providers.md)). Choose **Test connection**. Then make an agent: give it a name, pick the provider and a model, and tick the tools it may use. Or choose **Team from a template** for a ready-made team.

## 5. First run

Back on the Workbench, pick the agent and send a prompt. To let it work on code, add a project in Settings (a folder that is a git repository) and pick it at the top. Each run works on its own branch in its own worktree; nothing reaches your branch until you merge it from the Changes tab.

## Keep it running

- Windows: run `patina.exe serve` in a terminal window. What differs from Linux (no sandbox, `cmd.exe`) is in
  [windows.md](windows.md).
- Linux: a systemd user unit, which also turns on per-command resource limits. See [systemd.md](systemd.md).
- From your phone or another computer: [remote.md](remote.md).
- What to back up: [backup.md](backup.md).

## The container image

Each release also publishes `ghcr.io/torics/patina:<version>` for amd64 and arm64: the static binary on Alpine with git. Patina binds to loopback, so run it on the host's network and keep its folders in volumes:

```sh
docker run -d --name patina --network host \
  -v patina-config:/home/patina/.config/patina -v patina-data:/home/patina/.local/share/patina \
  -v "$HOME/code:/home/patina/code" \
  ghcr.io/torics/patina:latest
docker logs patina    # the setup token
```

Projects must be inside a mounted folder. Inside the container, Landlock works when the host kernel has it and the container runtime's seccomp profile allows its system calls; `docker exec patina patina doctor` says. The container sandbox backend is not available inside the image.

## Build from source

Building needs Rust (stable) and Node 22.

```sh
cargo xtask ui             # export the API types and build ui/dist
cargo build --release      # target/release/patina, with the UI embedded
```

## Verifying a download

Every release has a `SHA256SUMS` file covering each archive and the install script, and the install script checks the archive against it. The checksums come from the same release, so they prove the download is intact, not who built it; the release page and the workflow run that produced it (`.github/workflows/release.yml`) are the record of where it came from.

The binaries are not code-signed yet:

- **macOS** marks a downloaded file as quarantined, and Gatekeeper refuses an unsigned binary started from Finder. The install script uses `curl`, which does not set the mark. For a browser download, run `xattr -d com.apple.quarantine ./patina` after checking the sum.
- **Windows** SmartScreen may warn the first time you run `patina.exe`. Check the sum with `Get-FileHash patina-x86_64-pc-windows-msvc.zip -Algorithm SHA256` against `SHA256SUMS`, then choose "Run anyway".

`patina version --check` compares your version with the latest release. It is the only call Patina makes to an address you did not configure, and only when you run it.
