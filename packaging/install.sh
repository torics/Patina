#!/bin/sh
# Install the Patina binary for this machine from a GitHub release, after checking its SHA-256 sum.
#
#   curl -fsSL https://github.com/torics/Patina/releases/latest/download/install.sh | sh
#
# PATINA_VERSION picks a release (for example v0.2.0; the latest by default), PATINA_INSTALL_DIR the folder
# (~/.local/bin by default), PATINA_BASE_URL a mirror that holds the release files. For a private repository,
# download with `gh release download` instead.
set -eu

REPO="${PATINA_REPO:-torics/Patina}"
VERSION="${PATINA_VERSION:-latest}"
DIR="${PATINA_INSTALL_DIR:-$HOME/.local/bin}"

fail() { echo "patina install: $*" >&2; exit 1; }

case "$(uname -s)" in
  Linux) os=unknown-linux-musl ;;
  Darwin) os=apple-darwin ;;
  *) fail "this script supports Linux and macOS; on Windows, download the .zip from the releases page" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) fail "no release build for $(uname -m)" ;;
esac
target="$arch-$os"

if [ -n "${PATINA_BASE_URL:-}" ]; then
  base="$PATINA_BASE_URL"
elif [ "$VERSION" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi

if command -v curl >/dev/null 2>&1; then
  get() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  get() { wget -q "$1" -O "$2"; }
else
  fail "needs curl or wget"
fi
if command -v sha256sum >/dev/null 2>&1; then
  sum() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sum() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  fail "needs sha256sum or shasum to check the download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
archive="patina-$target.tar.gz"
echo "Downloading $archive ($VERSION)"
get "$base/$archive" "$tmp/$archive" || fail "cannot download $base/$archive"
get "$base/SHA256SUMS" "$tmp/SHA256SUMS" || fail "cannot download the checksums"
want="$(grep " $archive\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)"
[ -n "$want" ] || fail "$archive is not in SHA256SUMS"
got="$(sum "$tmp/$archive")"
[ "$want" = "$got" ] || fail "checksum mismatch for $archive: expected $want, got $got"

tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$DIR"
install -m 0755 "$tmp/patina-$target/patina" "$DIR/patina"
echo "Installed $("$DIR/patina" version | head -1) to $DIR/patina"
case ":$PATH:" in
  *":$DIR:"*) ;;
  *) echo "Add $DIR to your PATH to run it as \`patina\`." ;;
esac
echo "Next: patina doctor, then patina serve"
