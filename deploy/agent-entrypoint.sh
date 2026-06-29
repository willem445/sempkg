#!/usr/bin/env sh
# Sync the curated bundles from the registry, then start the agent server.
# Sync is best-effort: if the registry is briefly unavailable we still serve
# whatever bundles are already installed in the workspace.
set -e

if [ "${SEMPKG_SYNC_ON_START:-0}" = "1" ]; then
  echo "[entrypoint] syncing workspace from registry…"
  sempkg sync || echo "[entrypoint] sync failed; serving already-installed bundles"
fi

mkdir -p /workspace/.state
echo "[entrypoint] starting: sempkg-agent $*"
exec sempkg-agent "$@"
