---
name: sempkg-add
description: >
  Use this skill when the user asks to add, install, index, or pull in a
  package/repo/dependency as a sempkg — e.g. "add this repo as a sempkg",
  "index our own source with sempkg", "pull in <library> so the agent can
  query it", or "set up semantic search over <github-url>". Triages a target
  (registry name, direct .sembundle URL, GitHub release/tag/repo URL, or a
  local path) into genuine source vs. docs vs. noise, composes the correct
  `sempkg add` command, runs it, and reports what it decided. This is the
  ingest path — for querying an already-installed bundle, use the `sempkg`
  skill instead.
---

# sempkg-add: Triage a Repo and Add It as a sempkg

This skill does the work a human currently does by hand before running
`sempkg add`: figure out what a target actually is, decide which directories
are real source vs. documentation vs. noise, compose the right flags, run the
command, and install it. It covers all four sources `sempkg add` supports —
no other CLI surface exists, and none should be invented.

**Autonomy: maximum.** There is no confirm-before-run gate. Scan, decide, run,
and install in one pass — adding a bundle is low-risk and reversible
(`sempkg remove <name>`, see Step 6). The one non-negotiable is Step 5: after
acting, tell the user exactly what you classified as source/docs/excluded and
the exact command you ran, so a misclassification is visible immediately
instead of surfacing later as bad query results.

After a successful add, the natural next step is querying the new bundle —
see the `sempkg` skill (`.claude/skills/sempkg/SKILL.md`) for that read path.
Do not reimplement query logic here.

---

## Step 1 — Identify which of the four sources this is

`sempkg add` accepts exactly one of these forms as its positional `spec` (or
via `--url`). Work out which one the user's target is before doing anything
else:

| Target looks like | Source | Example |
|---|---|---|
| `<name>@<version>`, optionally with `--registry <name>` / `--registry-url <url>` | Registry bundle | `sempkg add aws-sdk@1.11.210` |
| A URL ending in `.sembundle` (attached to a GitHub release asset) | Direct bundle URL | `sempkg add my-sdk@2.0.0 --url https://github.com/owner/repo/releases/download/v2.0.0/my-sdk-2.0.0.sembundle` |
| A GitHub repo/release/tag URL, or `owner/repo[@ref][#subdir]` shorthand, with no prebuilt `.sembundle` asset (or one the user wants bypassed) | GitHub source (build from source) | `sempkg add https://github.com/pandas-dev/pandas/releases/tag/v3.0.3 --full` |
| A local filesystem path — `.` for the current repo, or any other path | Local path | `sempkg add . --name mylib --include-source --source-dir src --docs-dir docs` |

Accepted GitHub spec forms (all equivalent inputs to the same source type):
`owner/repo`, `owner/repo@ref`, `owner/repo@ref#subdir`,
`https://github.com/owner/repo`, `https://github.com/owner/repo/tree/<ref>`,
`https://github.com/owner/repo/releases/tag/<tag>`. The `#subdir` suffix
selects a subdirectory of the checked-out repo as the build root — useful for
one specific package inside a monorepo without needing to fetch the whole
tree separately (works alongside, or instead of, multiple `--source-dir`
flags — see Step 3).

If the user gives you a bare name with no version and no path/URL, and you
cannot resolve it to a registry entry, say so rather than guessing a
registry or a GitHub URL.

## Step 2 — For a GitHub or local source, triage the tree yourself

Registry and direct-`.sembundle`-URL adds need no triage — the bundle is
already built. For a **GitHub source** or a **local path**, you are deciding
what goes into the index. Do this before composing the command:

1. **Find the build manifest(s) first** — `Cargo.toml`, `pyproject.toml`
   (or `setup.py`/`setup.cfg`), `package.json`, `go.mod`, `pom.xml`/
   `build.gradle`. Read what they declare as the package root, `src`/`lib`
   layout, or workspace members. **Prefer this over guessing from directory
   names** — a manifest telling you the real source root beats a `src/`
   folder that happens to exist but isn't what's actually shipped.
2. **Source** — the real implementation tree the manifest points at, or the
   language-conventional root (`src/`, `lib/`) if no manifest contradicts it.
3. **Docs** — `docs/`, `doc/`, `README*`, `guide/`, `book/` — prose that
   documents the API, not just any markdown file.
4. **Exclude** — tests and fixtures (`tests/`, `test/`, `spec/`,
   `__tests__/`, `testdata/`, `fixtures/`, `conftest.py`, `*_test.go`,
   `*.test.ts`), benchmarks (`bench/`, `benches/`), build output (`target/`,
   `dist/`, `build/`, `node_modules/`, `.venv/`, `__pycache__/`), vendored
   third-party code (`vendor/`, `third_party/`), generated code, and
   CI/tooling config (`.github/`, `.circleci/`).
5. **Judgement calls — get these right, they are the actual value of this
   skill:**
   - `examples/` is usually **worth indexing**, not excluding — it shows
     real API usage even though it isn't library source. Treat it as a
     second `--source-dir` (or `--docs-dir` if it's mostly narrative
     walkthroughs rather than runnable code) unless it's clearly stale or
     disconnected from the current API.
   - A **monorepo** needs *multiple* `--source-dir` flags (one per real
     package root), not one root that also drags in unrelated packages, tests,
     and tooling. Alternatively, if the user only wants one package from a
     GitHub monorepo, prefer the `owner/repo@ref#subdir` form from Step 1 so
     the whole repo isn't fetched.
   - If the repo's source sits at the top level with **no `src/`** (common in
     small Python/JS libraries and Go modules), do **not** fabricate a
     `--source-dir` that doesn't exist — omit `--source-dir` entirely and let
     it default to the whole root, then rely on `--exclude-dir` to cut the
     noise.
   - A repo with no meaningful prose docs doesn't need a forced `--docs-dir`
     — omitting it is correct, not an oversight to fix.

## Step 3 — Compose the command

Use exactly the flags below — every one of them exists on `sempkg add`
today; do not invent new ones.

**Registry:**
```
sempkg add <name>@<version> [--registry <name>] [--registry-url <url>] [--description "..."]
```

**Direct bundle URL:**
```
sempkg add <name>@<version> --url <...releases/download/.../*.sembundle> [--description "..."]
```

**GitHub source (build from source):**
```
sempkg add <owner/repo-url-or-shorthand-or-tag-url> --full [--include-source]
  [--name <override>] [--version <override>]
  [--source-dir <dir>]... [--docs-dir <dir>]... [--exclude-dir <dir>]...
  [--description "..."]
```
- `--full` forces a shallow git clone instead of a tarball download — needed
  whenever the GitHub archive is export-ignore-filtered and would be missing
  docs (this affects real projects, e.g. pandas, CPython) or whenever a
  release has no prebuilt `.sembundle` asset to install instead. Requires
  `git` on `PATH`.
- `--build` (distinct from `--full`) forces the full build path even when a
  release `.sembundle` asset *does* exist — use it if the user wants a fresh
  build rather than the published bundle.
- Repeat `--source-dir`/`--docs-dir`/`--exclude-dir` for each directory (see
  Step 2's monorepo judgement call).

**Local path (including the current repo):**
```
sempkg add <path-or-.> --name <name> --include-source
  [--source-dir <dir>]... [--docs-dir <dir>]... [--exclude-dir <dir>]...
  [--description "..."]
```
- `--name` is required for local paths that aren't obviously named (always
  safe to pass explicitly).
- `--include-source` builds a source-code index (enables `search_code` /
  `read_symbol` and augments `get_callers`/`get_callees` with bodies) — pass
  it for local/GitHub-source adds unless the user only wants doc search.
- These flags are **persisted** in `sempkg.toml`; a later `sempkg refresh`
  reuses them without needing to repeat the classification.

All four forms accept an optional `--description "..."` — a one-line summary
stored in `sempkg.toml` and surfaced by `sempkg list` / the MCP
`list_packages` tool. Add one when you can state in a sentence what the
package is for; it helps future queries pick the right bundle by name.

## Step 4 — Run it, then sync

- Registry and direct-URL adds only register the dependency in
  `sempkg.toml`; run `sempkg sync` afterward to install.
- GitHub-source and local-path adds fetch/build **and install immediately** —
  no separate `sync` is required, but run `sempkg sync` anyway if you also
  just added other dependencies in the same pass.

## Step 5 — Report the classification (required, not a gate)

After the command finishes (success or failure), tell the user, in the same
turn:

1. The **exact command** you ran (copy-pasteable).
2. What you classified as **source**, **docs**, and **excluded** — the
   directories/globs, and briefly why (manifest-driven vs. convention vs. a
   judgement call from Step 2).
3. How to correct it if wrong: re-run `sempkg add <same target>` with
   explicit `--source-dir`/`--docs-dir`/`--exclude-dir` (it overwrites the
   stored settings), or `sempkg remove <name>` then re-add from scratch.

This is not a confirmation prompt — you've already run it — but skipping it
turns a wrong filter into bad query results discovered weeks later instead of
now.

## Step 6 — Verify and correct

```powershell
sempkg status <name>          # confirm it installed and what indexes it has (+lance / +code)
sempkg files <name>           # spot-check that indexed files look like source, not fixtures/build output
sempkg files <name> -f "*.rs" # narrow by extension if the file list is long
```

If `sempkg files` shows test fixtures, vendored code, or build output that
slipped through, re-add with a tighter `--exclude-dir` (or a narrower
`--source-dir`). If it's missing files you expected, loosen the filter or add
another `--source-dir`/`--docs-dir`. To undo entirely: `sempkg remove <name>`.

## Private / enterprise GitHub authentication

If a GitHub source or release asset add hits a 404/403, it's almost always a
missing token for a private repo or an enterprise host. For host
`github.company.com`, sempkg checks environment variables in this order —
tell the user which one to set:

1. `GITHUB_TOKEN_GITHUB_COMPANY_COM`
2. `GH_TOKEN_GITHUB_COMPANY_COM`
3. `GITHUB_ENTERPRISE_TOKEN`
4. `GH_ENTERPRISE_TOKEN`
5. `GITHUB_TOKEN`
6. `GH_TOKEN`

Host-specific variables (1–2) are preferred so public-GitHub and enterprise
credentials don't collide when both are set. For plain `github.com`, only
5–6 apply. Example:

```powershell
$env:GITHUB_TOKEN_GITHUB_COMPANY_COM = "<your-enterprise-pat>"
sempkg add https://github.company.com/org/repo/releases/tag/v3.0.3 --full
```

## After adding

Once installed, query the bundle with the `sempkg` skill
(`.claude/skills/sempkg/SKILL.md`) — `list_packages`/`sempkg list` to confirm
it's visible, then `search_symbols`/`get_context`/`search_docs` as needed.
Do not duplicate that query logic here.
