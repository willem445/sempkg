#!/usr/bin/env sh
# uninstall.sh — Remove sembundle and/or sempkg installed by install.sh
#
# Usage:
#   Remove the binaries (default — leaves ~/.sempkg data untouched):
#     curl -fsSL https://raw.githubusercontent.com/willem445/sempkg/main/uninstall.sh | sh
#
#   Remove the binaries AND the global sempkg data (~/.sempkg: bundles, models):
#     curl -fsSL https://raw.githubusercontent.com/willem445/sempkg/main/uninstall.sh | sh -s -- --purge
#
#   Remove only one binary, or from a custom install directory:
#     ... | sh -s -- --only sempkg
#     ... | sh -s -- --dir /custom/path
#
# The script is safe to re-run: anything already gone is reported and skipped.
# It never deletes per-project `<workspace>/.sempkg/` directories — those belong
# to your projects, not to the installation (they are listed as manual cleanup).

set -eu

INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
DATA_DIR="${SEMPKG_HOME:-$HOME/.sempkg}"
PURGE=0
ONLY=""  # empty = both binaries

# ── Argument parsing ──────────────────────────────────────────────────────────
while [ "$#" -gt 0 ]; do
  case "$1" in
    --dir)    INSTALL_DIR="$2"; shift 2 ;;
    --only)   ONLY="$2";        shift 2 ;;
    --purge)  PURGE=1;          shift ;;
    -h|--help)
      sed -n '2,20p' "$0" 2>/dev/null || echo "See https://github.com/willem445/sempkg"
      exit 0 ;;
    *) echo "Unknown option: $1" >&2; exit 1 ;;
  esac
done

case "$ONLY" in
  ""|sembundle|sempkg) ;;
  *) echo "Unknown --only value: ${ONLY} (expected sembundle|sempkg)" >&2; exit 1 ;;
esac

# ── Remove binaries ───────────────────────────────────────────────────────────
removed=0

remove_binary() {
  path="${INSTALL_DIR}/$1"
  if [ -e "$path" ]; then
    rm -f "$path"
    echo "  Removed: ${path}"
    removed=$((removed + 1))
  else
    echo "  Not installed: ${path}"
  fi
}

echo "Uninstalling from ${INSTALL_DIR}"

if [ -z "$ONLY" ] || [ "$ONLY" = "sembundle" ]; then
  remove_binary sembundle
fi

if [ -z "$ONLY" ] || [ "$ONLY" = "sempkg" ]; then
  remove_binary sempkg
fi

# ── Global data (~/.sempkg) ───────────────────────────────────────────────────
data_size() {
  du -sh "$DATA_DIR" 2>/dev/null | cut -f1
}

if [ "$PURGE" = "1" ]; then
  if [ -d "$DATA_DIR" ]; then
    echo ""
    echo "Purging global data: ${DATA_DIR} ($(data_size))"
    rm -rf "$DATA_DIR"
    echo "  Removed: ${DATA_DIR}"
  else
    echo ""
    echo "No global data at ${DATA_DIR} — nothing to purge."
  fi
elif [ -d "$DATA_DIR" ]; then
  echo ""
  echo "Kept: ${DATA_DIR} ($(data_size)) — global bundles, downloaded GGUF models,"
  echo "      and the local-package registry. Re-run with --purge to delete it, or:"
  echo ""
  echo "  rm -rf ${DATA_DIR}"
fi

# ── What we deliberately do not touch ─────────────────────────────────────────
echo ""
echo "Not removed (delete these yourself if you want them gone):"
echo "  • <project>/.sempkg/, sempkg.toml, sempkg.lock — per-project workspace state"
echo "  • <project>/.codegraph/ — CodeGraph indexes of your own repositories"
echo "  • The CodeGraph CLI:  npm uninstall -g @colbymchenry/codegraph"
echo "  • MCP server entries pointing at sempkg (e.g. .vscode/mcp.json)"

# ── PATH note ─────────────────────────────────────────────────────────────────
# install.sh never edits shell profiles (it only prints a reminder), so there is
# nothing to undo automatically — but if the directory is now empty and only
# existed for sempkg, the PATH entry is dead weight.
if [ "$removed" -gt 0 ] && [ -d "$INSTALL_DIR" ] && [ -z "$(ls -A "$INSTALL_DIR" 2>/dev/null)" ]; then
  case ":${PATH}:" in
    *":${INSTALL_DIR}:"*)
      echo ""
      echo "NOTE: ${INSTALL_DIR} is now empty but still on your PATH."
      echo "If you added it for sempkg, remove this line from your shell profile:"
      echo ""
      echo "  export PATH=\"\$PATH:${INSTALL_DIR}\""
      ;;
  esac
fi

echo ""
echo "Done."
