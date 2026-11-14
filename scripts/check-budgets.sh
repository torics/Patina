#!/usr/bin/env bash
# Fails if the release binary or idle memory exceed the goals G1 and G3.
set -euo pipefail
BIN=${1:-target/release/patina}
MAX_BYTES=$((20 * 1024 * 1024))
MAX_RSS_KB=$((50 * 1024))
size=$(stat -c %s "$BIN")
echo "binary size: $size bytes (limit $MAX_BYTES)"
[ "$size" -le "$MAX_BYTES" ] || { echo "binary too large"; exit 1; }
T=$(mktemp -d); trap 'kill $PID 2>/dev/null || true; rm -rf "$T"' EXIT
"$BIN" serve --config-dir "$T/c" --data-dir "$T/d" --port 0 >"$T/log" 2>&1 & PID=$!
for _ in $(seq 20); do grep -q "running at" "$T/log" && break; sleep 0.1; done
grep -q "running at" "$T/log" || { echo "did not start within 2s"; cat "$T/log"; exit 1; }
sleep 1
rss=$(awk '/VmRSS/ {print $2}' /proc/$PID/status)
echo "idle RSS: ${rss} kB (limit $MAX_RSS_KB)"
[ "$rss" -le "$MAX_RSS_KB" ] || { echo "idle memory too high"; exit 1; }
