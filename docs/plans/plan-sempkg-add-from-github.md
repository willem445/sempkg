# Implementation Plan: `sempkg add <github-url>` — Build-on-Demand From GitHub

> **Status:** Proposed
> **Audience:** Implementation model / engineer
> **Goal:** Let users run a single command to pull a public GitHub project, build a
> `.sembundle` on demand, and install it into their workspace — with zero manual
> cloning, building, or file shuffling.

---

## 1. Objective

Make this work end-to-end:

```bash
sempkg add https://github.com/pandas-dev/pandas@v2.2.2
# and the shorthand / URL variants below
sempkg add pandas-dev/pandas
sempkg add pandas-dev/pandas@v2.2.2
sempkg add https://github.com/pandas-dev/pandas
sempkg add https://github.com/pandas-dev/pandas/tree/v2.2.2
sempkg add https://github.com/pandas-dev/pandas/releases/tag/v2.2.2
```

When given a GitHub source, `sempkg add` must:

1. **Resolve** the reference (`owner`, `repo`, `ref`) and the resolved **commit SHA**.
2. **Fast path:** check whether the release tag already ships a `.sembundle` asset.
   If so, download and install it directly (this becomes the norm once projects
   publish bundles).
3. **Build path (current reality):** download the GitHub auto-generated `tar.gz`
   archive, extract to a temp dir, run the existing build pipeline to produce a
   `.sembundle`.
4. **Install** the bundle into the workspace bundle store
   (`<workspace>/.sempkg/bundles/<name>/<version>/`).
5. **Record** the dependency in `sempkg.toml` (with the GitHub source) and update
   `sempkg.lock` (with the resolved SHA + bundle checksum) so `sempkg sync` can
   reproduce it on another machine.

All sembundle build arguments (`name`, `version`, `source_repo`, `commit_hash`,
`tag`, `codegraph_version`, `language`) must be **derived automatically** from the
GitHub reference — the user never passes them.

---

## 2. Current Architecture (context for the implementer)

Two **independent** Rust crates (no root Cargo workspace today):

- `src/sembundle/` — CLI that **packs**, **signs**, **publishes**, and (via the
  `build` subcommand) runs the full pipeline:
  - `src/sembundle/src/build.rs` → `build(BuildOptions)`:
    - `run_codegraph(...)` shells out to the external `codegraph` CLI
      (`codegraph init --index <dir>`), collects `.codegraph/` output.
    - `run_lance(...)` builds a LanceDB FTS docs index from `--docs-dir`.
    - `pack(...)` writes the final `<name>-<version>.sembundle`.
  - `BuildOptions` fields: `name, version, source_repo, commit_hash, tag,
    language, codegraph_version, output_path, source_dirs, docs_dirs, docs_glob`.
  - Requires a full 40-char lowercase commit SHA (validated in `pack`/`manifest`).
- `src/sempkg/` — the manager + MCP server + scoped queries:
  - `src/sempkg/src/cli.rs` — clap definitions. `Commands::Add` currently writes a
    dependency to `sempkg.toml` only (it does **not** install).
  - `src/sempkg/src/main.rs` — command handlers (`Add`, `Sync`, `Install`, ...).
  - `src/sempkg/src/registry.rs` — `download_from_url(url, sha)` already downloads
    arbitrary `.sembundle` URLs; `RegistryClient` handles registry installs.
  - `src/sempkg/src/store.rs` — `BundleStore` with `install_bytes(&[u8])` /
    `install(path)`, extraction, `.codegraph` view creation. Store layout:
    `<workspace>/.sempkg/bundles/<name>/<version>/` and `~/.sempkg/bundles/`.
  - `src/sempkg/src/manifest.rs` — `sempkg.toml` / `sempkg.lock` model.
    `DependencyEntry { version, registry, url }`. Written via `toml_edit`.
  - `src/sempkg/src/codegraph.rs` — `codegraph` CLI wrapper (uses `which`).
- Both crates already depend on `tar`, `flate2`, `reqwest`, `sha2`, `tempfile`,
  `walkdir`, `glob`, `which`, `serde`.
- `install.ps1` / `install.sh` ship **both** binaries from GitHub Releases.

Key gaps to fill:
- sempkg has no GitHub URL parsing, no archive download/extract, and no in-process
  access to the build pipeline.
- sembundle's `build` lives in a binary crate, not a reusable library.
- `commit_hash`, `codegraph_version`, and `language` are not auto-derived.

---

## 3. High-Level Design

```
sempkg add <source>
        │
        ▼
 ┌─────────────────────┐   not GitHub
 │ parse source spec   ├────────────────► existing registry/url add flow (unchanged)
 └─────────┬───────────┘
           │ GitHub source
           ▼
 ┌─────────────────────┐
 │ resolve ref → SHA   │  (GitHub API; honors GITHUB_TOKEN)
 │ + repo metadata     │
 └─────────┬───────────┘
           ▼
 ┌─────────────────────────────────────┐
 │ release has *.sembundle asset?       │
 └───────┬──────────────────────┬──────┘
     yes │                   no │
         ▼                      ▼
 download asset        download tar.gz → extract → build pipeline
 (+ .sig if present)   (codegraph + lance + pack) → bytes
         │                      │
         └──────────┬───────────┘
                    ▼
        BundleStore.install_bytes(bytes)
                    ▼
   update sempkg.toml [dependencies] (git source)
   update sempkg.lock (sha + resolved rev)
```

---

## 4. Decision: Bundle the build pipeline into `sempkg` (recommended)

The user noted "we might have to bundle sembundle with sempkg." Choose **Option A**.

### Option A — Refactor `sembundle` into a library + binary (RECOMMENDED)

- Convert `src/sembundle` into a crate that exposes a **library target** in
  addition to its binary:
  - Add `src/sembundle/src/lib.rs` re-exporting `build`, `pack`, `BuildOptions`,
    `PackOptions`, and the error types. Keep `main.rs` as a thin CLI over the lib.
  - Add `[lib]` to `src/sembundle/Cargo.toml` (name `sembundle`).
- Make `sempkg` depend on it: `sembundle = { path = "../sembundle" }` in
  `src/sempkg/Cargo.toml`. (Both crates already share lance/arrow/tar versions, so
  no new heavy transitive cost.)
- sempkg calls `sembundle::build(BuildOptions { .. })` **in-process** — no PATH
  lookup of a `sembundle` binary, single self-contained tool.
- **Note:** the build pipeline still shells out to the external `codegraph` CLI.
  That remains a prerequisite (document it; bundling/auto-installing `codegraph`
  is a separate follow-up).

**Pros:** one binary, type-safe, no runtime discovery of sembundle; matches the
"bundle sembundle with sempkg" intent. **Cons:** slightly larger sempkg build.

### Option B — Shell out to the `sembundle` binary (fallback only)

- sempkg locates `sembundle` via `which` (and ships it next to sempkg).
- Simpler diff, but adds a runtime PATH dependency and arg-marshalling surface.

> **Recommendation:** Implement Option A. Mention Option B in code comments only as
> a fallback if the library refactor proves disruptive.

---

## 5. Detailed Work Items

### 5.1 New module: `src/sempkg/src/github.rs`

Responsible for parsing, resolving, and fetching from GitHub. Pure-ish module with
unit-testable parsing separated from network I/O.

**Types**

```rust
pub struct GitHubSource {
    pub owner: String,
    pub repo: String,
    /// Tag / branch / SHA as supplied (may be None → default branch).
    pub git_ref: Option<String>,
    /// Optional repo-relative subdirectory to scope the build (monorepos).
    pub subdir: Option<String>,
}

pub struct ResolvedSource {
    pub owner: String,
    pub repo: String,
    pub git_ref: String,        // concrete tag/branch/sha used for the archive
    pub commit_sha: String,     // full 40-char lowercase SHA (for commit_hash)
    pub is_tag: bool,           // archive path: refs/tags vs refs/heads vs raw sha
    pub package_name: String,   // sanitized repo name (sembundle name rules)
    pub version: String,        // ref with a leading `v` stripped, or short sha
    pub source_repo_url: String,// https://github.com/owner/repo
}
```

**Functions**

- `pub fn parse_source(spec: &str) -> Option<GitHubSource>`
  - Detect and parse all of:
    - `owner/repo`
    - `owner/repo@ref`
    - `https://github.com/owner/repo`
    - `https://github.com/owner/repo.git`
    - `https://github.com/owner/repo@ref`
    - `https://github.com/owner/repo/tree/<ref>` (ref may contain `/`)
    - `https://github.com/owner/repo/releases/tag/<tag>`
    - optional `#subdir` suffix for monorepo scoping (e.g. `owner/repo@v1#packages/core`)
  - Return `None` when the spec is **not** a GitHub source (so the caller falls
    back to the existing `name@version` registry path). Be careful: a bare
    `name@version` (no `/`) must **not** be treated as GitHub.
  - Validate `owner`/`repo` characters; reject path traversal.
- `pub fn resolve(src: &GitHubSource, token: Option<&str>) -> Result<ResolvedSource>`
  - Resolve `git_ref` → commit SHA via GitHub REST:
    - `GET /repos/{owner}/{repo}/commits/{ref}` (Accept:
      `application/vnd.github.sha`) returns the SHA directly; or parse JSON `sha`.
    - If `git_ref` is None, resolve the repo's default branch
      (`GET /repos/{owner}/{repo}` → `default_branch`) then its SHA.
  - Determine `is_tag` (affects archive URL). A practical approach: try
    `GET /repos/{o}/{r}/git/refs/tags/{ref}` first; on 404 treat as branch/sha.
  - Compute `package_name` (sanitize to sembundle name rules: lowercase, digits,
    hyphens, ≥2 chars — see `sembundle::validate_name`) and `version`
    (strip a single leading `v`; if ref is a raw SHA use the 12-char short SHA).
- `pub fn find_release_bundle_asset(resolved: &ResolvedSource, token: Option<&str>)
   -> Result<Option<ReleaseAsset>>`
  - `GET /repos/{o}/{r}/releases/tags/{tag}`; scan `assets[]` for a name ending in
    `.sembundle` (prefer one matching `{package_name}-{version}.sembundle`) and an
    optional companion `.sembundle.sig`. Return its `browser_download_url`(s).
  - Returns `Ok(None)` when there's no release or no matching asset (→ build path).
- `pub fn archive_tarball_url(resolved: &ResolvedSource) -> String`
  - `https://codeload.github.com/{o}/{r}/tar.gz/{ref}` **or** the canonical
    `https://github.com/{o}/{r}/archive/refs/tags/{tag}.tar.gz` /
    `.../refs/heads/{branch}.tar.gz` / `.../{sha}.tar.gz`. Pick one consistently.
- `pub fn download_and_extract_tarball(url: &str, token: Option<&str>, dest: &Path)
   -> Result<PathBuf>`
  - Stream-download (reuse `reqwest::blocking`), gunzip + untar into `dest`,
    **stripping the single top-level `repo-ref/` directory**, and return the
    extracted root path. Reuse the extraction pattern from
    `store.rs::install_bytes` (strip first path component, guard against
    `..`/absolute paths — Zip-Slip/Tar-Slip protection).

**Auth & networking**

- Read token from `GITHUB_TOKEN` (preferred) or `GH_TOKEN`. Add it as
  `Authorization: Bearer <token>` on API calls to avoid the 60 req/hr anonymous
  rate limit. Always set a `User-Agent` header (GitHub requires it).
- Handle 404 (bad ref/repo), 403 (rate limit — surface a clear message pointing to
  `GITHUB_TOKEN`), and network errors with actionable messages.

### 5.2 Auto-derive build inputs

In `src/sempkg/src/codegraph.rs` add:

- `pub fn version() -> Result<String>` — run `codegraph --version`, parse the
  version string. Fall back to a constant like `"unknown"` (or a configurable
  default) if it can't be determined, since `BuildOptions.codegraph_version` is
  required but should not block the build.

Language detection helper (new small fn, e.g. in `github.rs` or a `detect.rs`):

- `fn detect_language(root: &Path) -> String` — heuristic by counting source file
  extensions in the extracted tree (`.py`→python, `.rs`→rust, `.cpp/.cc/.h`→cpp,
  `.ts/.js`→typescript/javascript, ...). Default `"unknown"`. Keep it simple;
  this only populates manifest metadata.

Source/docs directory selection for the build:

- `source_dirs = [extracted_root]` (or `extracted_root/subdir` when `#subdir`
  given). `docs_dirs = [extracted_root]` with the default docs glob
  (`**/*.{md,rst,txt}`) so READMEs/docs get indexed into LanceDB. If no docs match,
  build still succeeds with code-only (verify `build.rs` tolerates empty docs — it
  currently skips lance when `docs_dirs` is empty; if docs_dirs is non-empty but
  nothing matches it errors. **Action:** make docs indexing best-effort — see 5.6).

### 5.3 New orchestration in `src/sempkg/src/main.rs`

Add a function `fn add_from_github(src: GitHubSource, dir: &Path, opts: AddOpts)
-> Result<()>` that performs steps 1–5 from §1:

1. `let token = env GITHUB_TOKEN/GH_TOKEN`.
2. `let resolved = github::resolve(&src, token)?`.
3. Fast path: `if let Some(asset) = github::find_release_bundle_asset(&resolved,
   token)? { bytes = download_from_url(asset.url, None)?; /* optional sig verify */ }`
   - Skip the fast path if `--build`/`--no-prefer-release` flag is set.
4. Build path otherwise:
   - `tmp = tempfile::TempDir::new()?`
   - `root = github::download_and_extract_tarball(archive_url, token, tmp.path())?`
   - Assemble `BuildOptions`:
     - `name = resolved.package_name`
     - `version = resolved.version`
     - `source_repo = resolved.source_repo_url`
     - `commit_hash = resolved.commit_sha`
     - `tag = src.git_ref.clone()` (when it was a tag)
     - `language = detect_language(&root)`
     - `codegraph_version = codegraph::version().unwrap_or_default()`
     - `source_dirs = [root (+subdir)]`, `docs_dirs = [root (+subdir)]`
     - `output_path = Some(tmp/<name>-<version>.sembundle)`
   - `let bundle_path = sembundle::build(opts)?;`
   - `bytes = std::fs::read(bundle_path)?`
5. Install: `let store = BundleStore::workspace(dir); let info =
   store.install_bytes(&bytes)?;` (handle `AlreadyInstalled` gracefully —
   treat as success / `--reinstall` to overwrite; see 5.5).
6. Update manifest + lock (see 5.4). Print a friendly summary
   (`Installed pandas@2.2.2 from github:pandas-dev/pandas@v2.2.2`).

Cache (optional, nice-to-have): write built bundles to
`~/.sempkg/cache/github/<owner>-<repo>-<sha>.sembundle` and reuse on repeat installs
to skip rebuilds. Gate behind the resolved commit SHA for correctness.

### 5.4 Manifest & lock schema changes (`src/sempkg/src/manifest.rs`)

Extend `DependencyEntry` to represent a GitHub source while staying
backward-compatible (all new fields optional):

```rust
pub struct DependencyEntry {
    pub version: String,
    pub registry: Option<String>,
    pub url: Option<String>,
    /// GitHub source shorthand, e.g. "github:pandas-dev/pandas".
    pub git: Option<String>,
    /// The git ref originally requested (tag/branch/sha).
    pub git_ref: Option<String>,
    /// Optional monorepo subdir.
    pub subdir: Option<String>,
}
```

- `sempkg.toml` entry shape (rendered via `toml_edit` inline table):
  ```toml
  [dependencies]
  pandas = { version = "2.2.2", git = "github:pandas-dev/pandas", git_ref = "v2.2.2" }
  ```
- Update `build_document` / inline-table serialization in `manifest.rs` to emit the
  new keys only when present. Update `insert_dep` accordingly.
- `sempkg.lock` (`LockEntry`): record the **resolved commit SHA** and the built
  bundle's `sha256` so installs are reproducible/auditable. Add `commit_sha:
  Option<String>` and reuse existing `sha256`. `registry_url` field can store the
  source label (`github:owner/repo@ref`).

### 5.5 CLI changes (`src/sempkg/src/cli.rs`)

`Commands::Add`:
- Keep the positional `spec` but document that it now also accepts GitHub
  sources/URLs/shorthand.
- Add flags:
  - `--build` / `--no-prefer-release`: force the build path even if a release
    asset exists.
  - `--reinstall`: rebuild/reinstall even if already present in the store.
  - `--subdir <path>`: monorepo scoping (alternative to `#subdir`).
  - (optional) `--name <override>` / `--version <override>` escape hatches.
- **Behavioral note:** for **GitHub** sources, `add` performs the full
  fetch+build+install immediately (there is no registry to defer to). For existing
  **registry/url** specs, `add` keeps today's behavior (writes manifest; user runs
  `sync`). Make this explicit in `--help` and in the success message.

Routing in `main.rs::run` under `Commands::Add`:
```rust
if let Some(src) = github::parse_source(&spec) {
    return add_from_github(src, dir, add_opts);
}
// else: existing registry / --url / --registry-url logic (unchanged)
```

### 5.6 Make `sempkg sync` reproduce GitHub dependencies

In `Commands::Sync` (`main.rs`), when a `DependencyEntry` has `git.is_some()`:
- Re-run the same resolve → (release asset or build) → install flow.
- Prefer the lock's `commit_sha` when present (reproducible builds); otherwise
  resolve fresh and write it back to the lock.
- Respect `--reinstall`.

This keeps `sempkg.toml` portable: a teammate runs `sempkg sync` and gets the same
bundles built locally.

### 5.7 Build pipeline robustness (`src/sembundle/src/build.rs`)

- **Docs best-effort:** today `run_lance` errors if `docs_dirs` is non-empty but no
  files match the glob. For the GitHub flow we always pass the repo as a docs dir.
  Change behavior so "no docs matched" logs a warning and proceeds **code-only**
  instead of failing the whole build. (Either add a `docs_optional: bool` to
  `BuildOptions`, or have sempkg pre-scan for doc files and only pass `docs_dirs`
  when matches exist.)
- Surface a clear, actionable error when the external `codegraph` CLI is missing
  (reuse `SempkgError::CodegraphNotFound` style messaging) — this is the most
  likely first-run failure.

---

## 6. Implementation Phases & Acceptance Criteria

### Phase 1 — sembundle library refactor (Option A)
- [ ] `src/sembundle/src/lib.rs` exposes `build`, `BuildOptions`, `pack`,
      `PackOptions`, error types, and `validate_name`.
- [ ] `src/sembundle/Cargo.toml` declares both `[lib]` and `[[bin]]`; binary still
      builds and behaves identically.
- [ ] `cargo build -p sembundle` (or in `src/sembundle`) passes.

### Phase 2 — GitHub source parsing (pure, unit-tested)
- [ ] `github::parse_source` handles every form in §1 and returns `None` for bare
      `name@version`.
- [ ] Unit tests cover shorthand, full URL, `/tree/<ref>`, `/releases/tag/<tag>`,
      refs containing `/`, `.git` suffix, and `#subdir`.

### Phase 3 — Resolution & fetch (network)
- [ ] `resolve` returns a full 40-char SHA for tags, branches, and short/long SHAs.
- [ ] `find_release_bundle_asset` detects an existing `.sembundle` asset (+ `.sig`).
- [ ] `download_and_extract_tarball` strips the top dir and is Tar-Slip-safe.
- [ ] `GITHUB_TOKEN`/`GH_TOKEN` honored; clear 403 rate-limit message.

### Phase 4 — Orchestration + install
- [ ] `add_from_github` wires fast path + build path + `BundleStore.install_bytes`.
- [ ] `codegraph::version()` and `detect_language()` implemented with safe defaults.
- [ ] `sempkg add pandas-dev/pandas@v2.2.2` (with `codegraph` on PATH) installs a
      queryable bundle; `sempkg list` shows it; `sempkg status pandas` reports
      `Queryable: true`.

### Phase 5 — Manifest/lock + sync reproducibility
- [ ] `DependencyEntry` gains optional `git`/`git_ref`/`subdir`; old manifests still
      parse.
- [ ] `add` writes the git dependency; `sempkg.lock` records resolved SHA + sha256.
- [ ] `sempkg sync` rebuilds/reinstalls GitHub dependencies from a clean store.

### Phase 6 — Docs & polish
- [ ] README: new "Install a semantic bundle directly from GitHub" section.
- [ ] `docs/sempkg.md` updated with the new `add` behavior and flags.
- [ ] Helpful errors for: missing `codegraph`, bad ref, private/404 repo, rate limit.

---

## 7. Testing Strategy

- **Unit:** `parse_source`, `version` string parsing, `detect_language`,
  archive-URL construction, manifest (de)serialization round-trip with the new
  fields, tar extraction strip/slip-guard.
- **Integration (gated/`#[ignore]` or behind a `network`/`codegraph` feature):**
  - Small public repo end-to-end build+install (choose a tiny repo to keep CI fast).
  - Fast-path test using a repo/tag known to ship a `.sembundle` asset (or a mock).
- **Manual smoke:** the five `sempkg add` invocations from §1.

---

## 8. Security & Robustness Checklist

- Tar/Zip-Slip protection on extraction (reject `..` and absolute paths; this also
  applies to the existing `store.rs` extractor — mirror the same guard).
- Validate `owner`/`repo`/`ref` to avoid URL/path injection into API + archive URLs.
- Treat downloaded archives as untrusted; never execute repo scripts — only run
  `codegraph` and the lance indexer over file contents.
- Don't log tokens. Read tokens only from env, never persist to `sempkg.toml`.
- Enforce request timeouts (already 120s in existing clients) and a max archive
  size guard to avoid runaway downloads.
- Verify release-asset signatures when a `.sembundle.sig` and a configured
  `verify_key` are present (reuse `verify::verify_bundle_signature`).

---

## 9. Out of Scope / Follow-ups

- Auto-installing/bundling the external `codegraph` CLI (still a prerequisite).
- Non-GitHub forges (GitLab, Bitbucket) — design `parse_source` so a future
  `GitForge` enum can slot in.
- Hosted public registry / index (explicitly deferred by the user).
- Build caching across machines / shared cache server.

---

## 10. Quick Reference — Files to Touch

| File | Change |
|------|--------|
| `src/sembundle/src/lib.rs` | **new** — expose build pipeline as a library |
| `src/sembundle/Cargo.toml` | add `[lib]` target |
| `src/sembundle/src/build.rs` | make docs indexing best-effort (no hard fail) |
| `src/sempkg/Cargo.toml` | add `sembundle = { path = "../sembundle" }` dep |
| `src/sempkg/src/github.rs` | **new** — parse / resolve / fetch from GitHub |
| `src/sempkg/src/codegraph.rs` | add `version()` helper |
| `src/sempkg/src/cli.rs` | extend `Add` (flags + docs) |
| `src/sempkg/src/main.rs` | add `add_from_github`, route `Add`, extend `Sync` |
| `src/sempkg/src/manifest.rs` | extend `DependencyEntry` + lock + serialization |
| `src/sempkg/src/store.rs` | reuse/centralize slip-safe extraction (optional) |
| `README.md`, `docs/sempkg.md` | document the new workflow |
