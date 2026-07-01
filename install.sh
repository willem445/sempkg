#!/usr/bin/env sh
# install.sh — Install sembundle and/or sempkg from GitHub Releases
#
# Usage:
#   Install both (default):
#     curl -fsSL https://raw.githubusercontent.com/willem445/sempkg/main/install.sh | sh
#
#   Install a specific binary only:
#     curl -fsSL https://raw.githubusercontent.com/willem445/sempkg/main/install.sh | sh -s -- --only sembundle
#     curl -fsSL https://raw.githubusercontent.com/willem445/sempkg/main/install.sh | sh -s -- --only sempkg
#
#   Install a specific version:
#     curl -fsSL https://raw.githubusercontent.com/willem445/sempkg/main/install.sh | sh -s -- --version v1.2.0
#
#   Force the CPU build (or force the GPU build) for sempkg:
#     curl -fsSL https://raw.githubusercontent.com/willem445/sempkg/main/install.sh | sh -s -- --gpu off
#     curl -fsSL https://raw.githubusercontent.com/willem445/sempkg/main/install.sh | sh -s -- --gpu on

set -eu

REPO="willem445/sempkg"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${VERSION:-latest}"
ONLY=""  # empty = install both
# GPU build selection for sempkg: auto (default) installs the CUDA/GPU build
# when a supported NVIDIA GPU + driver are detected, else the CPU build.
# 'on' forces the GPU build; 'off' forces the CPU build.
GPU="${GPU:-auto}"

# ── Argument parsing ──────────────────────────────────────────────────────────
while [ "$#" -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --only)    ONLY="$2";    shift 2 ;;
    --dir)     INSTALL_DIR="$2"; shift 2 ;;
    --gpu)     GPU="$2";     shift 2 ;;
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
# fetch URL DEST — returns non-zero on failure (e.g. a 404) without tripping
# `set -e`, so callers can fall back to another artifact.
fetch() {
  if command -v curl > /dev/null 2>&1; then
    curl -fsSL --output "$2" "$1"
  else
    wget -qO "$2" "$1"
  fi
}

download() {
  binary="$1"
  url="https://github.com/${REPO}/releases/download/${VERSION}/${binary}-${target}"
  dest="${INSTALL_DIR}/${binary}"

  echo "  Downloading ${binary} from ${url} ..."
  fetch "${url}" "${dest}"
  chmod +x "${dest}"
  echo "  Installed: ${dest}"
}

# ── GPU detection ─────────────────────────────────────────────────────────────
# Succeeds when the target has a CUDA build and an NVIDIA GPU with compute
# capability >= 7.5 (Turing) plus a driver new enough for CUDA 13 (>= 580) is
# present. nvidia-smi only exists when a driver is installed.
cuda_supported() {
  [ "$target" = "x86_64-unknown-linux-gnu" ] || return 1
  command -v nvidia-smi > /dev/null 2>&1 || return 1

  cap="$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader 2>/dev/null \
        | tr -d ' ' | LC_ALL=C sort -rn | head -n1)"
  drv="$(nvidia-smi --query-gpu=driver_version --format=csv,noheader 2>/dev/null \
        | head -n1 | tr -d ' ')"
  [ -n "$cap" ] || return 1

  cap_int="$(echo "$cap" | tr -d '.')"
  drv_major="$(echo "${drv:-0}" | cut -d. -f1)"

  if [ "${cap_int:-0}" -ge 75 ] 2>/dev/null && [ "${drv_major:-0}" -ge 580 ] 2>/dev/null; then
    return 0
  fi
  if [ "${cap_int:-0}" -ge 75 ] 2>/dev/null; then
    echo "  NVIDIA GPU (compute ${cap}) found, but driver ${drv} is older than 580 (required for the CUDA 13 build) — installing CPU build." >&2
  else
    echo "  NVIDIA GPU compute capability ${cap} is below 7.5 (Turing) — installing CPU build." >&2
  fi
  return 1
}

# Install sempkg, preferring the CUDA/GPU build when appropriate. On Linux the
# GPU build links the CUDA runtime statically, so it's a single self-contained
# binary (only the NVIDIA driver's libcuda.so is needed at runtime).
download_sempkg() {
  use_gpu=0
  case "$GPU" in
    on)   use_gpu=1 ;;
    off)  use_gpu=0 ;;
    auto) if cuda_supported; then use_gpu=1; fi ;;
    *)    echo "Unknown --gpu value: ${GPU} (expected auto|on|off)" >&2; exit 1 ;;
  esac

  if [ "$use_gpu" = "1" ] && [ "$target" != "x86_64-unknown-linux-gnu" ]; then
    echo "  No CUDA build available for ${target} — installing CPU build." >&2
    use_gpu=0
  fi

  if [ "$use_gpu" = "1" ]; then
    url="https://github.com/${REPO}/releases/download/${VERSION}/sempkg-${target}-cuda"
    dest="${INSTALL_DIR}/sempkg"
    echo "  Downloading sempkg (CUDA/GPU build) from ${url} ..."
    if fetch "${url}" "${dest}"; then
      chmod +x "${dest}"
      echo "  Installed GPU build: ${dest} (requires an NVIDIA driver >= 580)"
      return
    fi
    echo "  CUDA build not available for ${VERSION} — falling back to CPU build." >&2
  fi

  download sempkg
}

# ── Install ───────────────────────────────────────────────────────────────────
mkdir -p "${INSTALL_DIR}"

if [ -z "$ONLY" ] || [ "$ONLY" = "sembundle" ]; then
  download sembundle
fi

if [ -z "$ONLY" ] || [ "$ONLY" = "sempkg" ]; then
  download_sempkg
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
