#!/bin/sh
# Install imp (§9).
#
#   ./install.sh                 # download the release for this host
#   ./install.sh --from ./imp # install a binary you built yourself
#   ./install.sh --prefix /usr/local
#
# The download path is overridable so a mirror, a self-hosted Forgejo release,
# or a local file server can serve the same layout:
#
#   IMP_RELEASE_URL=https://git.albruiz.dev/albruiz/imp/releases/download \
#     ./install.sh
#
# Every downloaded file is checksum-verified when the release publishes a
# `.sha256` beside it, and the script refuses to continue if the digests differ.

set -eu

REPO="${IMP_REPO:-albruiz/imp}"
BASE="${IMP_RELEASE_URL:-https://git.albruiz.dev/${REPO}/releases/download}"
VERSION="${IMP_VERSION:-latest}"
PREFIX="${PREFIX:-$HOME/.local}"
FROM=""

usage() {
    cat <<'EOF'
usage: install.sh [--from PATH] [--prefix DIR] [--version V] [--help]

  --from PATH     install this binary instead of downloading one
  --prefix DIR    install under DIR/bin (default $HOME/.local)
  --version V     release tag to fetch (default: latest)
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --from) FROM="${2:?--from needs a path}"; shift 2 ;;
        --prefix) PREFIX="${2:?--prefix needs a directory}"; shift 2 ;;
        --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "install.sh: unknown argument $1" >&2; usage >&2; exit 2 ;;
    esac
done

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Linux) os_part="unknown-linux" ;;
    Darwin) os_part="apple-darwin" ;;
    *) echo "install.sh: unsupported OS: $os" >&2; exit 1 ;;
esac
case "$arch" in
    x86_64|amd64) arch_part="x86_64" ;;
    aarch64|arm64) arch_part="aarch64" ;;
    *) echo "install.sh: unsupported architecture: $arch" >&2; exit 1 ;;
esac
target="${arch_part}-${os_part}"

bindir="${PREFIX}/bin"
mkdir -p "$bindir"

if [ -n "$FROM" ]; then
    if [ ! -f "$FROM" ]; then
        echo "install.sh: $FROM is not a file" >&2
        exit 1
    fi
    install -m 0755 "$FROM" "$bindir/imp"
    echo "installed $(basename "$FROM") to $bindir/imp"
    exit 0
fi

asset="imp-${VERSION}-${target}.tar.gz"
url="${BASE}/${VERSION}/${asset}"
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT INT TERM

echo "downloading $url"
if ! curl -fsSL "$url" -o "$tmpdir/$asset"; then
    echo "install.sh: could not download $url" >&2
    echo "install.sh: build from source instead: cargo install --path crates/imp-cli" >&2
    exit 1
fi

if curl -fsSL "$url.sha256" -o "$tmpdir/$asset.sha256" 2>/dev/null; then
    expected="$(awk '{print $1}' "$tmpdir/$asset.sha256")"
    actual="$(sha256sum "$tmpdir/$asset" 2>/dev/null | awk '{print $1}')" \
        || actual="$(shasum -a 256 "$tmpdir/$asset" | awk '{print $1}')"
    if [ "$expected" != "$actual" ]; then
        echo "install.sh: checksum mismatch for $asset" >&2
        echo "  expected $expected" >&2
        echo "  got      $actual" >&2
        exit 1
    fi
    echo "checksum ok"
else
    echo "install.sh: no .sha256 published beside $asset; skipping verification" >&2
fi

tar -xzf "$tmpdir/$asset" -C "$tmpdir"
install -m 0755 "$tmpdir/imp" "$bindir/imp"

echo "installed imp to $bindir/imp"
case ":$PATH:" in
    *":$bindir:"*) ;;
    *) echo "note: add $bindir to your PATH" ;;
esac
"$bindir/imp" --version
