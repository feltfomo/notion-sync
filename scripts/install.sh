#!/bin/sh
# notion-sync installer.
#
# Downloads the latest release binaries, checks they arrived intact, and installs
# them to ~/.local/bin (override with BINDIR=/some/dir).
#
#   curl -fsSL https://raw.githubusercontent.com/feltfomo/notion-sync/main/scripts/install.sh | sh
#
set -eu

repo="feltfomo/notion-sync"
target="x86_64-unknown-linux-musl"        # static build: runs on any x86_64 Linux
bindir="${BINDIR:-$HOME/.local/bin}"
base="https://github.com/$repo/releases/latest/download"

# Prebuilt binaries are x86_64 Linux only for now.
os="$(uname -s)"
arch="$(uname -m)"
if [ "$os" != "Linux" ] || [ "$arch" != "x86_64" ]; then
  echo "notion-sync ships prebuilt binaries for x86_64 Linux only (you have $os/$arch)." >&2
  echo "Build from source instead: cargo install --git https://github.com/$repo" >&2
  exit 1
fi

need() { command -v "$1" >/dev/null 2>&1 || { echo "missing required tool: $1" >&2; exit 1; }; }
need curl
need sha256sum

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"

echo "Downloading the latest notion-sync ($target)..."
for bin in notion-sync fidelity-probe; do
  curl -fSL -o "$bin" "$base/$bin-$target"
done
curl -fSL -o SHA256SUMS "$base/SHA256SUMS"

echo "Verifying checksums..."
for bin in notion-sync fidelity-probe; do
  want="$(awk -v f="$bin-$target" '$2 == f { print $1 }' SHA256SUMS)"
  got="$(sha256sum "$bin" | awk '{ print $1 }')"
  if [ -z "$want" ]; then echo "no checksum listed for $bin-$target" >&2; exit 1; fi
  if [ "$want" != "$got" ]; then echo "checksum mismatch for $bin (download corrupted?)" >&2; exit 1; fi
done

mkdir -p "$bindir"
for bin in notion-sync fidelity-probe; do
  install -m 0755 "$bin" "$bindir/$bin"
done

echo "Installed notion-sync and fidelity-probe to $bindir"
case ":$PATH:" in
  *":$bindir:"*) ;;
  *)
    echo
    echo "NOTE: $bindir is not on your PATH yet. Add it with:"
    echo "  echo 'export PATH=\"$bindir:\$PATH\"' >> ~/.bashrc && source ~/.bashrc"
    ;;
esac
echo
echo "Next: run 'notion-sync' once to create your config file, then edit it."
