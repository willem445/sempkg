# Plan: CI Workflow Improvements

Status: proposed
Date: 2026-07-01
Scope: `.github/workflows/tests.yml`, `.github/workflows/release.yml`

## Measured baseline

Data from recent runs (July 2026):

| Pipeline | Wall clock | Dominant cost |
|---|---|---|
| `tests.yml` (PR run 28516212740) | ~70 min | Functional MCP tests job: ~59 min |
| `tests.yml` Rust tests (Windows) | ~20 min | Full from-scratch compile of both crates every run |
| `tests.yml` Python tests | ~1.5 min × 6 matrix jobs | Matrix breadth, not per-job time |
| `release.yml` (run 28516011941) | ~2 h 22 min | Windows CUDA build: 2 h 3 min |

Step-level breakdown of the functional MCP job (~59 min):

- `apt-get install cmake clang libclang-dev llvm-dev` — **12 min 21 s**
- `cargo build --release --features reranker` — **23 min** (cold, no cache hit)
- pytest `test_mcp_functional.py` — **23 min**
- Everything else (model download, bundle installs) — under 1 min combined

## Root causes, ranked by impact

### 1. Rust build caching is silently broken in `tests.yml` (critical)

Both the `rust-test` and `functional-tests` jobs configure `Swatinem/rust-cache` with:

```yaml
workspaces: . -> target
```

The repo root has **no `Cargo.toml`** (the crates live at `src/sempkg` and
`src/sembundle`), so rust-cache cannot resolve a workspace and saves nothing.
Evidence: the `Post Cache Rust build` step completes in **0 seconds** on every
run. Every CI run compiles llama.cpp, aws-lc-rs, lance, and the full dependency
tree from scratch — this alone accounts for most of the 20-min Rust job and the
23-min build inside the functional job.

**Fix:** point rust-cache at the real crate roots and unify the target dir:

```yaml
env:
  CARGO_TARGET_DIR: ${{ github.workspace }}/target
...
- uses: Swatinem/rust-cache@v2
  with:
    workspaces: |
      src/sempkg
      src/sembundle
    key: test-rust-${{ matrix.os }}
```

Setting `CARGO_TARGET_DIR` on the functional job too (it currently isn't set
there) lets both jobs share one cacheable target layout. Note rust-cache's
default `cache-targets` expects the target dir it discovers via cargo metadata;
with an env-pinned `CARGO_TARGET_DIR` it caches that path. Verify after the
first run that `Post Cache Rust build` now takes tens of seconds and the
restore is >1 GB.

Expected effect: warm-cache Rust test jobs drop from ~13–20 min to ~3–6 min;
the functional job's build step drops from ~23 min to ~2–5 min (only the
sempkg crate itself recompiles).

### 2. Every PR commit runs the whole workflow twice (critical)

`tests.yml` triggers on bare `push:` **and** `pull_request:`. Any commit pushed
to a PR branch fires both events (confirmed: runs 28516208399 push and
28516212740 pull_request, same commit, 5 s apart — ~130 job-minutes duplicated).
There is also no concurrency group, so stacked pushes pile up runs.

**Fix:**

```yaml
on:
  push:
    branches: [main]
  pull_request:

concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: ${{ github.ref != 'refs/heads/main' }}
```

Expected effect: halves total CI minutes for PR development at zero cost.

### 3. Functional job installs build tools that are already on the runner (high)

The `apt-get update && apt-get install cmake clang libclang-dev llvm-dev` step
took **12 min 21 s**. `ubuntu-latest` images already ship cmake, clang, llvm,
and libclang. The same applies to the `rust-test` Linux job and the Linux
`release.yml` jobs (where the step is faster but still redundant).

**Fix:** delete the step, or reduce it to only what's genuinely missing with
`--no-install-recommends`. The existing `Resolve LIBCLANG_PATH` step already
handles locating libclang on the stock image. Verify with one CI run; if
bindgen fails, fall back to `apt-get install -y --no-install-recommends
libclang-dev` (seconds, not minutes — the 12 min came from `llvm-dev` +
recommended packages).

### 4. `--features reranker` forces the llama.cpp C++ build on all 3 OSes (high)

`cargo test --features reranker` compiles llama.cpp natively on ubuntu,
windows, and macOS in every test run. Most of the crate's test surface doesn't
need it, and the functional job already exercises the reranker end-to-end on
Linux.

**Fix:** test default features on all OSes; add the `reranker` feature only on
ubuntu (where the cache is warmest and compile is fastest):

```yaml
- name: Test sempkg
  run: cargo test --manifest-path src/sempkg/Cargo.toml

- name: Test sempkg (reranker feature)
  if: runner.os == 'Linux'
  run: cargo test --manifest-path src/sempkg/Cargo.toml --features reranker
```

This also removes the need for the LLVM/libclang/NASM setup steps on
Windows/macOS test jobs entirely (they exist only for bindgen via
llama-cpp-sys). Keep them in `release.yml`, which genuinely builds the feature
everywhere.

### 5. Functional job runs on every PR with no path awareness (medium)

The `if:` guard limits it to PRs targeting main — but nearly all PRs target
main, so in practice it runs (and costs ~25–60 min) on every PR, including
Python-registry-only or docs-only changes.

**Fix:** add path filtering. Simplest form — a `paths-filter` gate job (or
`dorny/paths-filter` step) so `functional-tests` runs only when
`src/sempkg/**`, `src/sembundle/**`, `tests/test_mcp_functional.py`, or the
workflow file change. Same idea for the `rust-test` job (skip on Python-only
changes) and the Python `test` job (skip on Rust-only changes). Keep
unconditional runs on `main` pushes and `workflow_dispatch` as the safety net.

### 6. Python test matrix is wider than the code under test (medium)

6 jobs (3 OS × 2 Python) for a FastAPI service that runs server-side, each job
paying ~40 s of `npm install -g codegraph` and Node setup. Per-job time is
small but it multiplies the duplicate-run problem and queue pressure.

**Fix (pragmatic):**
- Matrix: `ubuntu × {3.11, 3.12}` + `windows × 3.11` (drop macOS and
  windows/3.12 — the registry targets Linux deployment; one Windows leg covers
  path/portability regressions). 6 jobs → 3.
- Run `--cov` only on the job that uploads to codecov; plain `pytest` elsewhere
  (coverage instrumentation slows the suite and the other 5 reports are
  discarded today).
- Verify whether the registry tests actually shell out to `codegraph`; if not,
  drop the Node + npm steps from this job entirely.
- Use `astral-sh/setup-uv@v5` with `enable-cache: true` and `uv sync` per the
  repo's documented workflow, instead of `uv pip install --system`.

### 7. Release: Windows CUDA job is the 2-hour long pole (high, release-only)

`Jimver/cuda-toolkit` with `method: local` downloads and runs the full ~3 GB
CUDA installer every run (`use-github-cache: false`, deliberately, to protect
the 10 GB cache budget). The job then compiles llama.cpp for 5 CUDA
architectures. Total: 2 h 3 min, and `release` publishes nothing until it
finishes.

**Fixes, in order of effort:**
1. Switch to `method: network` with an explicit sub-package list
   (`["nvcc", "cudart", "cublas", "cublas_dev", "thrust"]` — trim to what
   llama.cpp actually links). The network installer fetches ~600 MB instead of
   ~3 GB and skips full-installer extraction; on Windows this typically saves
   15–30 min.
2. Check whether the `${{ matrix.target }}-cuda` rust-cache key is actually
   restoring across release runs (same 0-second-post-step check as #1; here
   `workspaces: src/sempkg -> target` is plausibly correct, but the nvcc
   outputs live in OUT_DIR under target and are multi-GB — confirm they fit
   the cache budget after item #9 frees space).
3. If release latency matters more than atomicity: publish the release from
   the non-CUDA artifacts as soon as `build` + `bundle-sempkg` finish, and
   append CUDA artifacts in a follow-up job (`softprops/action-gh-release`
   appends to an existing release). Optional — only if 1–2 aren't enough.

### 8. Release: `bundle-sempkg` waits on the whole build matrix (low)

`needs: build` blocks on all 6 matrix legs, but the job only consumes the
`sembundle-x86_64-unknown-linux-gnu` artifact. Either give the Linux sembundle
build its own named job that `bundle-sempkg` depends on, or just build
sembundle inside `bundle-sempkg` (with the fixed cache it's a few minutes).
Minor today because CUDA is the long pole, but it matters once #7 lands.

### 9. Cache budget hygiene (medium, enables #1 and #7)

The repo's 10 GB Actions cache budget currently hosts up to 9 distinct Rust
cache keys (3 test OSes + functional + 6 release targets + 2 CUDA). Post-fix,
each working Rust cache will be 1–3 GB, guaranteeing eviction thrash.

**Fix:**
- Share one cache key between `rust-test (ubuntu)` and `functional-tests`
  (same OS, same deps; rust-cache keys on profile automatically — if the
  debug/release split doubles size, accept it or have the functional job
  reuse the release cache key only).
- Let `release.yml` caches be distinct but drop `retention` pressure: releases
  are infrequent; if eviction makes their caches useless in practice, consider
  `sccache` with the GHA backend (`mozilla-actions/sccache-action`) for the
  release jobs instead of caching whole target dirs — it caches per-compilation
  and degrades gracefully under eviction.

### 10. Small hygiene items (low)

- `actions/checkout` is pinned inconsistently (`v7` in some jobs, `v4` in
  others) — standardize on v7.
- `codecov/codecov-action@v3` is deprecated — bump to v5 (needs
  `CODECOV_TOKEN` for v4+ on public repos, or keep `fail_ci_if_error: false`).
- macOS jobs run `brew update` before `brew install llvm` — set
  `HOMEBREW_NO_AUTO_UPDATE: 1` and drop the explicit update (saves 1–3 min).
  (Moot for test jobs if #4 removes LLVM setup from macOS.)
- The LLVM/libclang/NASM/protoc setup block is copy-pasted across 4 jobs in 2
  workflows — extract a composite action (`.github/actions/setup-rust-native/`)
  so fixes like #3 land once.
- Consider `cargo-nextest` (`taiki-e/install-action@nextest`) for the Rust test
  jobs — typically 30–60 % faster test execution, better per-test timeouts.
- Longer-term: make `src/sempkg` + `src/sembundle` a single Cargo workspace
  with a root `Cargo.toml`. They share most heavy dependencies; a workspace
  dedupes compilation, gives one lockfile/one cache, and simplifies every
  `--manifest-path` invocation. Bigger change — do it separately from the CI
  fixes.

## Suggested implementation order

| Phase | Items | Effort | Expected effect |
|---|---|---|---|
| 1 | #1 cache fix, #2 dedup + concurrency, #3 drop apt step, #10 checkout/codecov bumps | ~1 hour, one PR | PR CI: ~70 min → ~25–30 min cold, ~12–18 min warm; total CI minutes roughly halved again by dedup |
| 2 | #4 reranker-on-Linux-only, #6 matrix trim, #9 cache budget | small PR | Rust jobs on Win/mac: ~20 min → ~5–8 min; 3 fewer Python jobs |
| 3 | #5 path filters | small PR, needs care with required-checks config | Docs/Python-only PRs skip the 25–60 min functional job |
| 4 | #7 CUDA network install, #8 bundle dependency, #10 composite action | separate PR, validate via `workflow_dispatch` | Release: ~2 h 20 min → plausibly ~1 h–1 h 15 min |

## Verification

- After Phase 1, confirm on a real run: `Post Cache Rust build` saves (>0 s,
  reports cache size), second run restores and build steps shrink accordingly.
- `workflow_dispatch` the release pipeline once after Phase 4 (it already
  supports artifact-only smoke runs) before cutting a tag.
- Watch the repo's Actions cache page (Settings → Actions → Caches) for
  eviction after the caches start actually saving.

## Non-goals

- Shrinking the 23-min functional pytest suite itself (model inference bound;
  `pytest-xdist` is risky with a shared MCP server process) — revisit only if
  it becomes the long pole after Phase 1.
- Larger/paid runners — worth pricing only if warm-cache times are still
  unsatisfactory after Phase 2.
