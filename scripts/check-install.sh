#!/usr/bin/env bash
# The install script against a local mirror of a release: it installs a good archive and refuses a tampered one.
set -euo pipefail
BIN=${1:-target/release/patina}
W=$(mktemp -d)
trap 'kill $SERVER 2>/dev/null || true; rm -rf "$W"' EXIT
target=x86_64-unknown-linux-musl
[ "$(uname -m)" = aarch64 ] && target=aarch64-unknown-linux-musl
mkdir -p "$W/mirror/patina-$target"
cp "$BIN" "$W/mirror/patina-$target/"
(cd "$W/mirror" && tar -czf "patina-$target.tar.gz" "patina-$target" && rm -r "patina-$target" && sha256sum ./*.tar.gz | sed 's# \./# #' > SHA256SUMS)
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$W/mirror" >/dev/null 2>&1 & SERVER=$!
for _ in $(seq 50); do curl -fs "http://127.0.0.1:$port/SHA256SUMS" >/dev/null && break; sleep 0.1; done
PATINA_BASE_URL="http://127.0.0.1:$port" PATINA_INSTALL_DIR="$W/good" sh packaging/install.sh
"$W/good/patina" version
echo tampered >> "$W/mirror/patina-$target.tar.gz"
if PATINA_BASE_URL="http://127.0.0.1:$port" PATINA_INSTALL_DIR="$W/bad" sh packaging/install.sh 2>"$W/err"; then
  echo "a tampered archive was installed"; exit 1
fi
grep -q "checksum mismatch" "$W/err" && [ ! -e "$W/bad/patina" ] && echo "a tampered archive is refused"
