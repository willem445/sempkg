#!/usr/bin/env sh
# install.sh — Install sembundle and/or sempkg from GitHub Releases
#
# Usage:
#   Install both (default):
#     curl -fsSL https://raw.githubusercontent.com/willem445/codegraph-hub/main/install.sh | sh
#
#   Install a specific binary only:
#     curl -fsSL https://raw.githubusercontent.com/willem445/codegraph-hub/main/install.sh | sh -s -- --only sembundle
#     curl -fsSL https://raw.githubusercontent.com/willem445/codegraph-hub/main/install.sh | sh -s -- --only sempkg
#
#   Install a specific version:
#     curl -fsSL https://raw.githubusercontent.com/willem445/codegraph-hub/main/install.sh | sh -s -- --version v1.2.0

set -eu

REPO="willem445/codegraph-hub"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${VERSION:-latest}"
ONLY=""  # empty = install both

# ── Argument parsing ──────────────────────────────────────────────────────────
while [ "$#" -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --only)    ONLY="$2";    shift 2 ;;
    --dir)     INSTALL_DIR="$2"; shift 2 ;;
    *) echo "Unknown option: $1" >&2; exit 1 ;;
  esac
done

# ── Detect OS and architecture ────────────────────────────────────────────────
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Linux)
    case "$arch" in
      x86_64)  target="x86_64-unknown-linux-gnu"  ;;
      aarch64) target="aarch64-unknown-linux-gnu"  ;;
      *) echo "Unsupported Linux architecture: $arch" >&2; exit 1 ;;
    esac
    ;;
  Darwin)
    case "$arch" in
      x86_64)  target="x86_64-apple-darwin"   ;;
      arm64)   target="aarch64-apple-darwin"   ;;
      *) echo "Unsupported macOS architecture: $arch" >&2; exit 1 ;;
    esac
    ;;
  *)
    echo "Unsupported OS: $os" >&2
    echo "On Windows, run install.ps1 instead." >&2
    exit 1
    ;;
esac

# ── Resolve version tag ───────────────────────────────────────────────────────
if [ "$VERSION" = "latest" ]; then
  if command -v curl > /dev/null 2>&1; then
    VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
      | grep '"tag_name"' | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')"
  elif command -v wget > /dev/null 2>&1; then
    VERSION="$(wget -qO- "https://api.github.com/repos/${REPO}/releases/latest" \
      | grep '"tag_name"' | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')"
  else
    echo "curl or wget is required." >&2; exit 1
  fi
fi

echo "Installing version ${VERSION} for ${target}"

# ── Download helper ───────────────────────────────────────────────────────────
download() {
  binary="$1"
  url="https://github.com/${REPO}/releases/download/${VERSION}/${binary}-${target}"
  dest="${INSTALL_DIR}/${binary}"

  echo "  Downloading ${binary} from ${url} ..."
  if command -v curl > /dev/null 2>&1; then
    curl -fsSL --output "${dest}" "${url}"
  else
    wget -qO "${dest}" "${url}"
  fi
  chmod +x "${dest}"
  echo "  Installed: ${dest}"
}

# ── Install ───────────────────────────────────────────────────────────────────
mkdir -p "${INSTALL_DIR}"

if [ -z "$ONLY" ] || [ "$ONLY" = "sembundle" ]; then
  download sembundle
fi

if [ -z "$ONLY" ] || [ "$ONLY" = "sempkg" ]; then
  download sempkg
fi

# ── PATH reminder ─────────────────────────────────────────────────────────────
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo ""
    echo "NOTE: ${INSTALL_DIR} is not on your PATH."
    echo "Add the following to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
    echo ""
    echo "  export PATH=\"\$PATH:${INSTALL_DIR}\""
    echo ""
    ;;
esac

echo "Done."
