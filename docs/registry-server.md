# Bundle Registry Server Guide

This guide covers how to run a local or self-hosted bundle registry, create and manage publish tokens, publish bundles with SemBundle, and pull bundles with sempkg.

## Overview

The bundle registry server is provided by the SemBundle-registry CLI in this repository.

Core endpoints:
- GET /index.json: lists available packages/versions with SHA-256 hash and signing status per version
- GET /bundles/<package>/<version>/<package>-<version>.SemBundle: downloads bundle archive (includes X-Bundle-SHA256 header)
- GET /bundles/<package>/<version>/<package>-<version>.SemBundle.sig: downloads Ed25519 signature file (if published)
- POST /publish: uploads a bundle and optional signature (Bearer token required)
- POST /admin/tokens: creates publish token (admin password required)
- GET /admin/tokens: lists token metadata (admin password required)
- DELETE /admin/tokens/<token>: revokes token (admin password required)

### Trust model

Bundles are protected by two independent layers:

1. SHA-256 whole-bundle integrity — the registry stores and serves a SHA-256 hash of every bundle. Clients verify this hash before extraction, catching server-side tampering and in-transit corruption.

2. Ed25519 publisher signing — a maintainer generates a keypair (`SemBundle keygen`) and signs each bundle (`SemBundle sign`) before publishing. Consumers verify the signature against the public key (`bundle install --verify-key`) to confirm the bundle was produced by the expected party and has not been modified.

## Install Prerequisites

Use uv (recommended):

```powershell
uv pip install -e .[registry]
```

Or with pip:

```powershell
pip install -e .[registry]
```

This installs FastAPI, uvicorn, and multipart support used by the registry server.

## Self-Hosting (Local PC or LAN)

Set an admin password (required):

```powershell
$env:SemBundle_REGISTRY_ADMIN_PASSWORD = "change-me-now"
```

Start server on all interfaces so other LAN machines can reach it:

```powershell
SemBundle-registry serve --host 0.0.0.0 --port 8765 --storage-dir C:\registry\bundles --config-dir C:\registry\config
```

Notes:
- --storage-dir stores published bundles and generated index.json.
- --config-dir stores token metadata (tokens.json).
- For same-machine testing, use http://127.0.0.1:8765.
- For LAN testing, use your machine IP, for example http://192.168.1.25:8765.

## Docker Self-Hosting

Build image from repository root:

```powershell
docker build -f src/SemBundle_registry/Dockerfile -t SemBundle-registry .
```

Run container:

```powershell
docker run --rm -p 8765:8765 -e SemBundle_REGISTRY_ADMIN_PASSWORD="change-me-now" -v C:\registry-data:/data SemBundle-registry python -m SemBundle_registry serve --host 0.0.0.0 --port 8765 --storage-dir /data/bundles --config-dir /data/config
```

## Token Management

### Option A: Local CLI token management

Create token:

```powershell
SemBundle-registry token add --label "ci-publisher" --config-dir C:\registry\config
```

List tokens (metadata only):

```powershell
SemBundle-registry token list --config-dir C:\registry\config
```

Revoke token:

```powershell
SemBundle-registry token revoke <TOKEN_VALUE> --config-dir C:\registry\config
```

### Option B: Admin API token management

Create token:

```powershell
$admin = "change-me-now"
Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:8765/admin/tokens" -Headers @{ Authorization = "Bearer $admin" } -ContentType "application/json" -Body '{"label":"ci-publisher"}'
```

List tokens:

```powershell
$admin = "change-me-now"
Invoke-RestMethod -Method Get -Uri "http://127.0.0.1:8765/admin/tokens" -Headers @{ Authorization = "Bearer $admin" }
```

Revoke token:

```powershell
$admin = "change-me-now"
$token = "<TOKEN_VALUE>"
Invoke-RestMethod -Method Delete -Uri "http://127.0.0.1:8765/admin/tokens/$token" -Headers @{ Authorization = "Bearer $admin" }
```

## Bundle Integrity and Signing

### Keypair generation

Generate an Ed25519 keypair. Do this once per publisher (e.g., CI service account, maintainer):

```powershell
SemBundle keygen --output-dir C:\keys
```

Produces:
- `C:\keys\private.pem` — PKCS8 PEM private key. Keep secret, never commit.
- `C:\keys\public.pem` — SubjectPublicKeyInfo PEM public key. Distribute to consumers.

On Linux/macOS `private.pem` is written with mode 0600 (owner read/write only).

### Signing a bundle before publish

```powershell
SemBundle sign my-lib-1.2.0.SemBundle --key C:\keys\private.pem
```

Produces `my-lib-1.2.0.SemBundle.sig` in the same directory. The signature covers the hex-encoded SHA-256 of the bundle file — it is interoperable with any Ed25519 verifier (Python `cryptography` library, `openssl`, etc.).

To write the `.sig` to a custom path:

```powershell
SemBundle sign my-lib-1.2.0.SemBundle --key C:\keys\private.pem --output releases\my-lib-1.2.0.sig
```

### Verifying a bundle locally

```powershell
SemBundle verify my-lib-1.2.0.SemBundle --sig my-lib-1.2.0.SemBundle.sig --key C:\keys\public.pem
```

Prints `Signature valid.` on success; exits with a non-zero code and an error message on failure.

## Publish Bundles into the Registry

### Publish without signature (integrity only)

1. Build or pack a .SemBundle archive.
2. Publish it with SemBundle.

```powershell
SemBundle publish .\my-lib-1.2.0.SemBundle --registry http://127.0.0.1:8765 --token <TOKEN_VALUE>
```

Environment variable equivalent:

```powershell
$env:SemBundle_REGISTRY_URL = "http://127.0.0.1:8765"
$env:SemBundle_TOKEN = "<TOKEN_VALUE>"
SemBundle publish .\my-lib-1.2.0.SemBundle
```

The server computes and stores the bundle's SHA-256 hash automatically. Clients verify this hash on download.

### Publish with a signature (integrity + provenance)

Sign the bundle first, then publish both files:

```powershell
# 1. Sign
SemBundle sign .\my-lib-1.2.0.SemBundle --key C:\keys\private.pem

# 2. Publish bundle + signature together
SemBundle publish .\my-lib-1.2.0.SemBundle --registry http://127.0.0.1:8765 --token <TOKEN_VALUE> --sig .\my-lib-1.2.0.SemBundle.sig
```

(The `--sig` flag is passed to `SemBundle publish` and attached in the multipart upload.)

The server will:
- validate token
- read manifest.json from the uploaded archive
- compute and store SHA-256 of the bundle
- store the `.sig` file if provided
- mark the version as `signed: true` in index.json
- regenerate index.json

## Workspace Manifest and Lock File

Like `package.json` + `package-lock.json`, `sempkg` supports a declarative workspace manifest and a lock file for reproducible, authenticated installs across machines.

**Commit both files to git.** Other developers clone the repo and run `sempkg bundle sync` to get the exact same bundles.

### `sempkg.toml` — manifest

Created automatically by `bundle add`, or write it by hand:

```toml
# sempkg workspace bundle manifest
# Run: sempkg bundle sync

[[registries]]
name = "default"
url  = "http://192.168.1.25:8765"

[[registries]]
name = "public"
url  = "https://registry.corp.example.com"

[workspace]
# Optional: path to Ed25519 public key for signature verification.
# verify_key = "keys/publisher.pem"

[dependencies]
my-lib  = { version = "1.2.0",   registry = "default" }
aws-sdk = { version = "1.11.210", registry = "public"  }
```

- Multiple `[[registries]]` entries are supported, each with a `name` and `url`. This mirrors PyPI's `[[tool.uv.index]]` or npm's registry config.
- Each dependency references a registry by `name`. If `registry` is omitted, the first entry is used.
- `verify_key` is optional; when set, `bundle sync` verifies Ed25519 signatures before installing.

### `sempkg.lock` — lock file

Auto-generated by `bundle lock` or `bundle sync`. Contains the SHA-256 of each bundle and the internal file checksums from the bundle's own `manifest.json`:

```toml
# Auto-generated by sempkg. DO NOT EDIT.
# Commit this file to ensure reproducible installs.

[[package]]
name         = "my-lib"
version      = "1.2.0"
registry_url = "http://192.168.1.25:8765"
sha256       = "a665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3"
signed       = true

[package.manifest_checksums]
"config.json"      = "abc123..."
"graph/nodes.bin"  = "def456..."
```

### Adding a new bundle dependency

```powershell
# With a named registry already in sempkg.toml:
sempkg bundle add my-lib@1.2.0 --registry default

# With an inline URL (creates/reuses registry entry automatically):
sempkg bundle add my-lib@1.2.0 --registry-url http://192.168.1.25:8765
```

This updates `sempkg.toml`, fetches the SHA-256 from the registry to refresh `sempkg.lock`, and installs the bundle into the workspace store.

### Syncing a workspace (reproducible install)

```powershell
# Install all deps from the manifest, using lock file for hashes:
sempkg bundle sync

# With signature verification:
sempkg bundle sync --verify-key keys/publisher.pem

# Force reinstall even if already present:
sempkg bundle sync --reinstall
```

Already-installed bundles are skipped unless `--reinstall` is passed. The lock file is refreshed automatically if any dep is missing from it.

### Refreshing the lock file without installing

```powershell
sempkg bundle lock
```

Contacts each registry, fetches current SHA-256 and checksums for every pinned version, and writes `sempkg.lock`. Does not install anything. Run this to update hashes after republishing a bundle (not recommended — prefer bumping the version).

## Pull Bundles with sempkg (ad-hoc)

### Inspect registry index

```powershell
sempkg bundle search-registry http://127.0.0.1:8765
```

### Install into workspace scope (integrity check only)

```powershell
sempkg bundle install my-lib@1.2.0 --registry http://127.0.0.1:8765
```

The client automatically fetches `index.json`, checks the bundle's SHA-256 against the server-recorded hash, and rejects the download if they differ.

### Install with signature verification (integrity + provenance)

Obtain the publisher's `public.pem` out-of-band (e.g., from the project's GitHub repository), then:

```powershell
sempkg bundle install my-lib@1.2.0 --registry http://127.0.0.1:8765 --verify-key C:\keys\public.pem
```

The client will:
1. Fetch index.json and read the expected SHA-256
2. Download the bundle and verify SHA-256
3. Download `my-lib-1.2.0.SemBundle.sig` from the registry
4. Verify the Ed25519 signature against the bundle hash using the supplied public key
5. Reject installation if either check fails

Requires the `cryptography` package: `uv pip install cryptography`

### Install into global scope

```powershell
sempkg bundle install my-lib@1.2.0 --registry http://127.0.0.1:8765 --global
sempkg bundle install my-lib@1.2.0 --registry http://127.0.0.1:8765 --global --verify-key C:\keys\public.pem
```

### List and remove installed bundles

List:

```powershell
sempkg bundle list
sempkg bundle list --workspace
sempkg bundle list --global
```

Remove:

```powershell
sempkg bundle remove my-lib@1.2.0
sempkg bundle remove my-lib@1.2.0 --global
```

## Registry Index Format

The server-generated index.json has this structure:

```json
{
  "packages": {
    "my-lib": {
      "versions": ["1.1.0", "1.2.0"],
      "latest": "1.2.0",
      "bundles": {
        "1.1.0": {
          "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
          "signed": false
        },
        "1.2.0": {
          "sha256": "a665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3",
          "signed": true
        }
      }
    }
  },
  "generated_at": "2026-06-15T12:34:56Z"
}
```

- `sha256`: SHA-256 of the raw `.SemBundle` file bytes. Clients verify this before extracting.
- `signed`: `true` if a `.sig` file was published alongside the bundle.

## Troubleshooting

- Error: SemBundle_REGISTRY_ADMIN_PASSWORD environment variable is not set
  - Set the variable before running SemBundle-registry serve.

- Publish returns 401
  - Token missing/invalid. Confirm Bearer token and correct config-dir.

- Publish returns 409
  - That package version already exists. Publish a new version.

- sempkg bundle install fails with "SHA-256 mismatch: bundle may have been tampered with"
  - The downloaded bundle does not match the hash stored in the registry's index. The bundle on disk may be corrupted, replaced, or the registry may have been modified. Contact the registry administrator.

- sempkg bundle install fails with "Signature verification failed"
  - The Ed25519 signature does not match the bundle. Either the bundle has been tampered with after signing, or you are using the wrong public key. Do not install this bundle.

- sempkg bundle install fails with "Install 'cryptography' to use signature verification"
  - Run: `uv pip install cryptography` (or `pip install cryptography`)

- Registry looks empty after publish
  - Confirm server uses expected storage-dir and inspect <storage-dir>/index.json.

- SemBundle verify prints "Signature verification FAILED"
  - The bundle has been modified since signing, or the wrong public key was used. Do not distribute this bundle.
