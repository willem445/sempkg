#!/usr/bin/env sh
# tests/uninstall/smoke.sh — end-to-end check of uninstall.sh.
#
# Everything happens inside a throwaway sandbox: a fake $HOME (so `~/.sempkg` is
# the sandbox's, never the real user's), a fake install dir, a "victim"
# directory, a canary directory, and a fake user workspace. Nothing outside the
# sandbox is read or written.
#
# The pre-uninstall state is SEEDED DIRECTLY instead of by running install.sh.
# That is deliberate: install.ps1 has a known pre-existing PATH-append bug
# (issue #107), so a naive install -> uninstall round trip would fail on the
# installer's corruption rather than on anything uninstall does. Seeding the
# state an install *would* have produced isolates what uninstall actually owns.
# A full round trip is worth adding once #107 lands.
#
# Usage:
#   sh tests/uninstall/smoke.sh [--script path/to/uninstall.sh]
#
# `--script` exists so the harness can be pointed at an older revision of the
# script to prove it catches the bugs it is meant to catch.

set -eu

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
SCRIPT="${repo_root}/uninstall.sh"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --script) SCRIPT="$2"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

echo "Testing: ${SCRIPT}"

failures=0
ok()  { echo "  PASS: $1"; }
bad() { echo "  FAIL: $1" >&2; failures=$((failures + 1)); }

assert_exists() {
  if [ -e "$1" ]; then ok "$2"; else bad "$2 — missing: $1"; fi
}
assert_absent() {
  if [ -e "$1" ]; then bad "$2 — still present: $1"; else ok "$2"; fi
}

# ── Sandbox ───────────────────────────────────────────────────────────────────
setup() {
  SANDBOX="$(mktemp -d)"
  HOME_DIR="${SANDBOX}/home"
  BIN_DIR="${HOME_DIR}/.local/bin"
  DATA_DIR="${HOME_DIR}/.sempkg"
  VICTIM="${SANDBOX}/victim"
  CANARY="${SANDBOX}/canary"
  WORKSPACE="${SANDBOX}/workspace"

  mkdir -p "${BIN_DIR}" \
           "${DATA_DIR}/bundles/demo/1.0.0" \
           "${DATA_DIR}/models" \
           "${VICTIM}" "${CANARY}" "${WORKSPACE}/.sempkg/bundles"

  # What a completed install leaves behind.
  printf 'binary\n' > "${BIN_DIR}/sempkg"
  printf 'binary\n' > "${BIN_DIR}/sembundle"
  chmod +x "${BIN_DIR}/sempkg" "${BIN_DIR}/sembundle"
  # An unrelated tool sharing ~/.local/bin — must survive.
  printf 'binary\n' > "${BIN_DIR}/some-other-tool"

  # Runtime data the tool writes.
  printf 'gguf\n'  > "${DATA_DIR}/models/model.gguf"
  printf '{}\n'    > "${DATA_DIR}/packages.json"
  # Per-project state — belongs to the user's repo, never to the installer.
  printf 'bundle\n' > "${WORKSPACE}/.sempkg/bundles/installed"

  printf 'precious\n' > "${VICTIM}/precious.txt"
  printf 'keep\n'     > "${CANARY}/keep-me.txt"
}

teardown() {
  rm -rf "${SANDBOX}"
}

# Always runs the script with a *hostile* SEMPKG_HOME: the uninstaller must
# ignore it entirely and only ever touch the real ~/.sempkg (which, here, is the
# sandbox's). A script that honours it would delete the victim directory.
run_uninstall() {
  HOME="${HOME_DIR}" SEMPKG_HOME="${VICTIM}" sh "${SCRIPT}" --dir "${BIN_DIR}" "$@" > "${SANDBOX}/out.log" 2>&1
}

assert_untouched_bystanders() {
  assert_exists "${VICTIM}/precious.txt"                 "$1: victim dir untouched (SEMPKG_HOME ignored)"
  assert_exists "${CANARY}/keep-me.txt"                  "$1: canary dir untouched"
  assert_exists "${WORKSPACE}/.sempkg/bundles/installed" "$1: workspace .sempkg untouched"
  assert_exists "${BIN_DIR}/some-other-tool"             "$1: unrelated tool in bin dir untouched"
}

# ── Case 1: default run removes the binaries and keeps the data ───────────────
echo ""
echo "Case 1 — default run (binaries only)"
setup
if run_uninstall; then ok "Case 1: exit 0"; else bad "Case 1: non-zero exit"; fi
assert_absent "${BIN_DIR}/sempkg"            "Case 1: sempkg removed"
assert_absent "${BIN_DIR}/sembundle"         "Case 1: sembundle removed"
assert_exists "${DATA_DIR}/models/model.gguf" "Case 1: ~/.sempkg data KEPT (no --purge)"
assert_untouched_bystanders "Case 1"

# ── Case 2: re-running is a no-op ─────────────────────────────────────────────
echo ""
echo "Case 2 — idempotent re-run"
if run_uninstall; then ok "Case 2: exit 0 on re-run"; else bad "Case 2: non-zero exit on re-run"; fi
assert_exists "${DATA_DIR}/models/model.gguf" "Case 2: data still kept"
assert_untouched_bystanders "Case 2"
teardown

# ── Case 3: --purge removes exactly ~/.sempkg ─────────────────────────────────
echo ""
echo "Case 3 — --purge (with a hostile SEMPKG_HOME pointing at the victim dir)"
setup
if run_uninstall --purge; then ok "Case 3: exit 0"; else bad "Case 3: non-zero exit"; fi
assert_absent "${BIN_DIR}/sempkg"    "Case 3: sempkg removed"
assert_absent "${BIN_DIR}/sembundle" "Case 3: sembundle removed"
assert_absent "${DATA_DIR}"          "Case 3: ~/.sempkg purged"
assert_untouched_bystanders "Case 3"
teardown

# ── Case 4: --only removes just the one binary ────────────────────────────────
echo ""
echo "Case 4 — --only sempkg"
setup
if run_uninstall --only sempkg; then ok "Case 4: exit 0"; else bad "Case 4: non-zero exit"; fi
assert_absent "${BIN_DIR}/sempkg"    "Case 4: sempkg removed"
assert_exists "${BIN_DIR}/sembundle" "Case 4: sembundle KEPT"
assert_untouched_bystanders "Case 4"
teardown

# ── Result ────────────────────────────────────────────────────────────────────
echo ""
if [ "${failures}" -eq 0 ]; then
  echo "uninstall.sh: all checks passed"
  exit 0
fi
echo "uninstall.sh: ${failures} check(s) FAILED" >&2
exit 1
