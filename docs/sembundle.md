# sembundle - User Guide

`sembundle` is the artifact builder and publisher for the sempkg ecosystem.

It creates portable `.sembundle` files (semantic index bundles), signs them with
Ed25519 keys, verifies signatures, and publishes bundles to a registry.

If `sempkg` is the consumer/install tool, `sembundle` is the producer/release
tool.

---

## Contents

1. [What a SemBundle is](#what-a-sembundle-is)
2. [Prerequisites](#prerequisites)
3. [Install sembundle](#install-sembundle)
4. [Quickstart: Build, Sign, Publish](#quickstart-build-sign-publish)
5. [CLI Reference](#cli-reference)
6. [Examples](#examples)
7. [Private/Public Key Security](#privatepublic-key-security)
8. [Publishing Artifacts for Your Project Users](#publishing-artifacts-for-your-project-users)

---

## What a SemBundle is

A `.sembundle` file is a portable semantic artifact for one package version.
It includes:

- CodeGraph outputs (`graph/`, `embeddings/`, `config.json`)
- bundle metadata (`manifest.json`, `metadata.json`)
- optional docs index (`lance/`)
- optional source-code index (`code/`, when built with `--include-source`)

For full archive/spec details, see [sembundle-spec.md](sembundle-spec.md).

---

## Prerequisites

| Requirement | Why |
|-------------|-----|
| Rust toolchain | Needed to install/build `sembundle` from source |
| CodeGraph on `PATH` | Required for `sembundle build` indexing pipeline |

Install CodeGraph:

```powershell
npm install -g @colbymchenry/codegraph
```

---

## Install sembundle

### Prebuilt binary (recommended)

Linux/macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/willem445/sempkg/main/install.sh | sh -- --only sembundle
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/willem445/sempkg/main/install.ps1 | iex
```

### Build from source

```powershell
cargo install --path src/sembundle
```

Check installation:

```powershell
sembundle --help
```

---

## Quickstart: Build, Sign, Publish

### 1) Build a bundle from your project source/docs

```powershell
sembundle build `
  --name my-sdk `
  --version 1.2.0 `
  --source-repo https://github.com/my-org/my-sdk `
  --commit-hash a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0 `
  --codegraph-version 0.3.1 `
  --source-dir .\src `
  --docs-dir .\docs `
  --docs-glob "**/*.md"
```

Output (default): `./my-sdk-1.2.0.sembundle`

### 2) Generate a signing keypair

```powershell
sembundle key-gen --output-dir .\keys
```

This writes:

- `keys/private.pem` (secret)
- `keys/public.pem` (share with consumers)

### 3) Sign the bundle

```powershell
sembundle sign .\my-sdk-1.2.0.sembundle --key .\keys\private.pem
```

Output: `./my-sdk-1.2.0.sembundle.sig`

### 4) Verify before release

```powershell
sembundle verify .\my-sdk-1.2.0.sembundle --sig .\my-sdk-1.2.0.sembundle.sig --key .\keys\public.pem
```

### 5) Publish to registry

```powershell
sembundle publish .\my-sdk-1.2.0.sembundle --registry http://127.0.0.1:8765 --token <TOKEN>
```

You can also use env vars instead of flags:

```powershell
$env:SemBundle_REGISTRY_URL = "http://127.0.0.1:8765"
$env:SemBundle_TOKEN = "<TOKEN>"
sembundle publish .\my-sdk-1.2.0.sembundle
```

---

## CLI Reference

## `sembundle pack`

Pack an existing CodeGraph output directory into a `.sembundle`.

Usage:

```text
sembundle pack <input_dir> --name <name> --version <version> --source-repo <url> --commit-hash <sha> --codegraph-version <ver> [options]
```

Required:

- `input_dir` (positional): directory containing `graph/`, `embeddings/`, `config.json`
- `--name, -n`: package name
- `--version, -r`: package version
- `--source-repo`: canonical source repository URL
- `--commit-hash`: full 40-char lowercase git SHA
- `--codegraph-version`: CodeGraph version used to produce index

Optional:

- `--tag`: release tag
- `--language` (default: `unknown`)
- `--indexed-paths` (comma-separated, default `.`)
- `--output, -o`: output file path
- `--lance-dir`: include prebuilt docs index extension (`lance/`)
- `--code-dir`: include prebuilt source-code index extension (`code/`)

## `sembundle build`

Run indexing + packaging in one command.

Usage:

```text
sembundle build --name <name> --version <version> --source-repo <url> --commit-hash <sha> --codegraph-version <ver> --source-dir <dir> [--source-dir <dir> ...] [options]
```

Required:

- `--name, -n`
- `--version, -r`
- `--source-repo`
- `--commit-hash`
- `--codegraph-version`
- `--source-dir, -s` (repeatable; at least one)

Optional:

- `--tag`
- `--language` (default: `unknown`)
- `--output, -o`
- `--docs-dir, -d` (repeatable)
- `--docs-glob`
- `--include-source` (embed `code/` source-code index extension)
- `--source-glob` (restrict files included by source-code index)

## `sembundle key-gen`

Generate Ed25519 keypair used for signing bundles.

Usage:

```text
sembundle key-gen [--output-dir <path>]
```

Options:

- `--output-dir, -o` (default: current directory)

Output files:

- `private.pem`
- `public.pem`

## `sembundle sign`

Sign a `.sembundle` using a private key.

Usage:

```text
sembundle sign <bundle_path> --key <private.pem> [--output <sig_path>]
```

Required:

- `bundle_path` (positional)
- `--key, -k` (private key PEM)

Optional:

- `--output, -o` (default: `<bundle_path>.sig`)

## `sembundle verify`

Verify a signature against a `.sembundle` using a public key.

Usage:

```text
sembundle verify <bundle_path> --sig <bundle.sig> --key <public.pem>
```

Required:

- `bundle_path` (positional)
- `--sig, -s`
- `--key, -k` (public key PEM)

## `sembundle publish`

Upload a `.sembundle` to a registry server.

Usage:

```text
sembundle publish <bundle_path> [--registry <url>] [--token <token>]
```

Required:

- `bundle_path` (positional)
- `--registry` or env `SemBundle_REGISTRY_URL`
- `--token` or env `SemBundle_TOKEN`

Notes:

- `bundle_path` must end in `.sembundle`
- publish currently uploads the bundle archive file

---

## Examples

### Build from pre-indexed directory (`pack`)

```powershell
sembundle pack .\codegraph-output `
  --name my-sdk `
  --version 1.2.0 `
  --source-repo https://github.com/my-org/my-sdk `
  --commit-hash a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0 `
  --codegraph-version 0.3.1 `
  --language rust
```

### Build including source-code index extension

```powershell
sembundle build `
  --name my-sdk `
  --version 1.2.0 `
  --source-repo https://github.com/my-org/my-sdk `
  --commit-hash a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0 `
  --codegraph-version 0.3.1 `
  --source-dir .\src `
  --include-source `
  --source-glob "**/*.{rs,py,js,ts,tsx}"
```

### Publish in CI with env vars

```sh
export SemBundle_REGISTRY_URL="https://registry.example.com"
export SemBundle_TOKEN="$SEMPKG_PUBLISH_TOKEN"
sembundle publish ./my-sdk-1.2.0.sembundle
```

### Validate signature in CI before publish

```sh
sembundle sign ./my-sdk-1.2.0.sembundle --key ./keys/private.pem
sembundle verify ./my-sdk-1.2.0.sembundle --sig ./my-sdk-1.2.0.sembundle.sig --key ./keys/public.pem
```

---

## Private/Public Key Security

`sembundle` uses Ed25519 keys for authenticity and provenance.

### How signing works

1. `sembundle sign` computes SHA-256 over the raw `.sembundle` bytes.
2. It hex-encodes that digest.
3. It signs the hex digest bytes with your Ed25519 private key.
4. It writes a 64-byte signature as hex in a `.sig` file.

`sembundle verify` performs the same digest process and verifies the signature
using the Ed25519 public key.

### What this protects

- Detects tampering after release
- Confirms the bundle was signed by the expected key owner
- Provides release provenance when consumers trust your public key

### Key management guidance

- Keep `private.pem` secret and out of git
- Store private keys in a secure secret manager or HSM-backed CI secret
- Share `public.pem` with consumers through a trusted channel
- Rotate keys on compromise and publish a clear key-rotation notice
- Use separate signing keys for test/staging/production release pipelines

### Recommended trust model for teams

- Publisher signs every release bundle
- Consumer projects set `verify_key` in `sempkg.toml`
- Consumers install via `sempkg sync` so signature checks are enforced for
  registry-based bundles

Example `sempkg.toml`:

```toml
[workspace]
verify_key = "keys/publisher.pem"

[[registry]]
name = "default"
url = "https://registry.example.com"

[dependencies]
my-sdk = { version = "1.2.0", registry = "default" }
```

---

## Publishing Artifacts for Your Project Users

There are two common distribution models.

### Model A: Private/Public registry (recommended for teams)

Publisher flow:

1. Build bundle (`sembundle build`)
2. Sign bundle (`sembundle sign`)
3. Publish bundle (`sembundle publish`)

Consumer flow:

```powershell
sempkg init --registry https://registry.example.com
sempkg add my-sdk@1.2.0
sempkg sync
```

If consumers configure `verify_key`, `sempkg` verifies signatures on
registry-sourced installs.

### Model B: GitHub Releases artifact

Publisher flow:

1. Build bundle artifact
2. Attach `.sembundle` to GitHub release
3. Optionally attach `.sembundle.sig` and publish `public.pem`

Consumer flow:

```powershell
sempkg add my-sdk@1.2.0 --url https://github.com/my-org/my-sdk/releases/download/v1.2.0/my-sdk-1.2.0.sembundle
sempkg sync
```

Use this model when you do not want to run a registry service.

### Publishing checklist

- Bundle name/version matches your release tag
- `source_repo` and `commit_hash` point to the exact shipped code
- Bundle verifies cleanly before upload
- Public key distribution path is documented for consumers
