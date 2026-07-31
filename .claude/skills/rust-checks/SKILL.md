---
name: rust-checks
description: >
  Validate a Rust change in sempkg cheaply — the right cargo command, the right
  feature flags, the shared target dir, and what to leave to CI. Use this skill
  BEFORE running any cargo command (build, test, clippy, check, run) in this
  repository, and when deciding whether a change is ready to hand to review.
---

# sempkg Rust checks

Compiling this workspace is expensive (Lance + DataFusion + Arrow + llama-cpp).
Two disk emergencies have already come from agents compiling carelessly. This
skill is the cheap path. See `CLAUDE.md`'s "Build & test policy" for the full
rationale — this is the day-to-day cheat sheet version.

---

## First: are you allowed to compile at all?

**If you are reviewing a PR — no.** Read the diff and read CI:

```bash
gh pr checks <pr>
gh run view <run-id> --log-failed
```

CI runs three platforms, the real feature combinations and the functional MCP
suite. A green local run on one machine tells you *less* than that. A red or
missing CI run is the worker's defect to fix — hand it back as a finding.

**If you are a worker — yes, and it is your job.** The reviewer is not your
test run. Everything below is for you.

---

## The feature flags (not optional)

`sempkg` **cannot** build with `--all-features` — it enables `cuda`, `vulkan`,
`rocm` and `metal` at once, each needing a different vendor SDK. It will fail,
and it will fail slowly.

```bash
# sempkg — always these two features, never --all-features
cargo clippy -p sempkg --features reranker,embeddings -- -D warnings
cargo test  -p sempkg --features reranker,embeddings

# sembundle — --all-features is fine here (no optional GPU backends)
cargo clippy -p sembundle --all-features -- -D warnings
cargo test  -p sembundle --all-features
```

CI's lint job runs with `-D warnings`, so a warning is a failure. Check it
before you push, not after.

---

## Pick the cheapest command that proves your point

| Goal | Command |
| --- | --- |
| Does it type-check? | `cargo check -p <crate>` |
| Did I break the crate I touched? | `cargo test -p <crate>` |
| Is it lint-clean? | `cargo clippy -p <crate> <features> -- -D warnings` |
| Is it formatted? | `cargo fmt --all --check` |
| Does it work on 3 OSes / all features? | **CI.** Do not do this locally. |

Scope by crate (`-p sempkg` or `-p sembundle` — the two Rust workspace
members). `cargo test --workspace` rebuilds everything and is almost never
what you actually need.

---

## The target dir is shared — it is not yours

All agents in a working group share one `CARGO_TARGET_DIR` (`.loomux-target/`,
gitignored). That sharing is the only reason N worktrees don't each pay for a
cold build.

- **Never** set your own `CARGO_TARGET_DIR`.
- **Never** build into your scratchpad or create a `target-<name>/` directory.
  That is a from-scratch build of the whole dependency graph — it cost 8.3 GB
  the one time an agent did it.
- **Never** `cargo clean` the shared dir. You would wipe every other agent's
  cache, including agents mid-build. Cleaning is the orchestrator's call, made
  when no agents are live.

Check for room before a long build — under ~10 GB free, stop and say so:

```powershell
Get-PSDrive C | Select-Object @{n='FreeGB';e={[math]::Round($_.Free/1GB,1)}}
```

---

## Before you open the PR

- [ ] `cargo fmt --all --check` clean
- [ ] `cargo clippy` clean with the right features (`-D warnings`)
- [ ] tests for what you touched, passing
- [ ] **red-before-green evidence**: your new test run against the *base* branch,
      failing — the command and the failure line go in the PR description. A
      test nobody has seen fail is decoration, not a safety net.
- [ ] then let CI run the matrix, and fix what it finds

Handing an unvalidated branch to a reviewer just moves the compile cost onto
someone who is explicitly forbidden from paying it.
