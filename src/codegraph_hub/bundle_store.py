"""Manages installed .cgbundle archives in workspace or global scope."""

from __future__ import annotations

import hashlib
import json
import shutil
import tarfile
import tempfile
import urllib.request
from dataclasses import dataclass
from pathlib import Path


try:
    from cryptography.hazmat.primitives.serialization import load_pem_public_key
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
    from cryptography.exceptions import InvalidSignature
    _CRYPTOGRAPHY_AVAILABLE = True
except ImportError:
    _CRYPTOGRAPHY_AVAILABLE = False


# ---------------------------------------------------------------------------
# Exceptions
# ---------------------------------------------------------------------------

class BundleInstallError(Exception):
    """Raised for checksum mismatch, missing manifest, or malformed bundle."""


class VersionExistsError(BundleInstallError):
    """Raised when the requested version is already installed."""


# ---------------------------------------------------------------------------
# Data
# ---------------------------------------------------------------------------

@dataclass
class BundleInfo:
    name: str
    version: str
    store_dir: Path        # the extracted directory
    manifest: dict         # parsed manifest.json contents


# ---------------------------------------------------------------------------
# Store
# ---------------------------------------------------------------------------

class BundleStore:
    """Manages installed .cgbundle archives in workspace or global scope."""

    def __init__(self, store_dir: Path) -> None:
        self._store_dir = store_dir

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def install(self, bundle_path: Path) -> BundleInfo:
        """Install a .cgbundle file into the store.

        Validates checksums, extracts to store_dir/<name>/<version>/.
        Raises BundleInstallError on validation failure or VersionExistsError
        if already installed.
        """
        if not tarfile.is_tarfile(bundle_path):
            raise BundleInstallError(f"Not a valid tar archive: {bundle_path}")

        with tarfile.open(bundle_path, "r:gz") as tf:
            manifest = self._read_manifest(tf)
            name: str = manifest["name"]
            version: str = manifest["version"]

            dest = self.get_bundle_dir(name, version)
            if dest.exists():
                raise VersionExistsError(
                    f"{name}@{version} is already installed at {dest}"
                )

            self._validate_checksums(tf, manifest)
            self._extract(tf, name, version, dest)

        # Write manifest into extracted directory
        (dest / "manifest.json").write_text(
            json.dumps(manifest, indent=2), encoding="utf-8"
        )

        return BundleInfo(name=name, version=version, store_dir=dest, manifest=manifest)

    def install_from_registry(
        self, package: str, version: str, registry_url: str,
        verify_key_path: Path | None = None,
    ) -> BundleInfo:
        """Download and install a bundle from a registry server."""
        registry_url = registry_url.rstrip("/")

        # Step 1: Fetch index to get expected SHA-256
        expected_sha256: str | None = None
        index_url = f"{registry_url}/index.json"
        try:
            with urllib.request.urlopen(index_url) as response:  # noqa: S310
                index_data = json.loads(response.read().decode("utf-8"))
            expected_sha256 = (
                index_data
                .get("packages", {})
                .get(package, {})
                .get("bundles", {})
                .get(version, {})
                .get("sha256")
            )
        except Exception:  # noqa: BLE001
            expected_sha256 = None

        # Step 2: Download the bundle
        url = f"{registry_url}/bundles/{package}/{version}/{package}-{version}.cgbundle"
        with tempfile.NamedTemporaryFile(suffix=".cgbundle", delete=False) as tmp:
            tmp_path = Path(tmp.name)
        try:
            with urllib.request.urlopen(url) as response:  # noqa: S310
                bundle_bytes = response.read()
            tmp_path.write_bytes(bundle_bytes)

            # Step 3: Verify SHA-256
            if expected_sha256:
                actual_sha256 = hashlib.sha256(bundle_bytes).hexdigest()
                if actual_sha256 != expected_sha256:
                    raise BundleInstallError("SHA-256 mismatch: bundle may have been tampered with")

            # Step 4: Verify Ed25519 signature if requested
            if verify_key_path is not None:
                if not _CRYPTOGRAPHY_AVAILABLE:
                    raise BundleInstallError(
                        "Install 'cryptography' to use signature verification: pip install cryptography"
                    )
                bundle_sha256_hex = hashlib.sha256(bundle_bytes).hexdigest()
                sig_url = f"{registry_url}/bundles/{package}/{version}/{package}-{version}.cgbundle.sig"
                try:
                    with urllib.request.urlopen(sig_url) as resp:  # noqa: S310
                        sig_bytes = resp.read()
                except Exception as exc:  # noqa: BLE001
                    raise BundleInstallError(f"Failed to fetch signature: {exc}") from exc
                try:
                    key_data = verify_key_path.read_bytes()
                    public_key = load_pem_public_key(key_data)
                    if not isinstance(public_key, Ed25519PublicKey):
                        raise BundleInstallError("Key file does not contain an Ed25519 public key")
                    public_key.verify(sig_bytes, bundle_sha256_hex.encode("utf-8"))
                except InvalidSignature as exc:
                    raise BundleInstallError("Signature verification failed: invalid signature") from exc
                except BundleInstallError:
                    raise
                except Exception as exc:  # noqa: BLE001
                    raise BundleInstallError(f"Signature verification failed: {exc}") from exc

            # Step 5: Install
            return self.install(tmp_path)
        finally:
            tmp_path.unlink(missing_ok=True)

    def list_installed(self) -> list[BundleInfo]:
        """Return all installed bundles sorted by name then version."""
        results: list[BundleInfo] = []
        if not self._store_dir.exists():
            return results
        for manifest_path in sorted(self._store_dir.glob("*/*/manifest.json")):
            try:
                manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
                results.append(
                    BundleInfo(
                        name=manifest["name"],
                        version=manifest["version"],
                        store_dir=manifest_path.parent,
                        manifest=manifest,
                    )
                )
            except (json.JSONDecodeError, KeyError):
                continue
        return sorted(results, key=lambda b: (b.name, b.version))

    def remove(self, name: str, version: str) -> bool:
        """Remove an installed bundle. Returns True if removed, False if not found."""
        dest = self.get_bundle_dir(name, version)
        if not dest.exists():
            return False
        shutil.rmtree(dest)
        # Remove empty name directory
        name_dir = self._store_dir / name
        try:
            name_dir.rmdir()
        except OSError:
            pass
        return True

    def resolve(self, name: str, version: str) -> Path | None:
        """Return the extracted bundle directory path or None if not installed."""
        dest = self.get_bundle_dir(name, version)
        return dest if dest.exists() else None

    def verify_installed(
        self, name: str, version: str, verify_key_path: Path, registry_url: str
    ) -> None:
        """Verify an already-installed bundle's Ed25519 signature against the registry."""
        if not _CRYPTOGRAPHY_AVAILABLE:
            raise BundleInstallError(
                "Install 'cryptography' to use signature verification: pip install cryptography"
            )
        bundle_dir = self.get_bundle_dir(name, version)
        if not bundle_dir.exists():
            raise BundleInstallError(f"{name}@{version} is not installed")

        registry_url = registry_url.rstrip("/")
        index_url = f"{registry_url}/index.json"
        try:
            with urllib.request.urlopen(index_url) as response:  # noqa: S310
                index_data = json.loads(response.read().decode("utf-8"))
            bundle_sha256_hex: str = index_data["packages"][name]["bundles"][version]["sha256"]
        except (KeyError, TypeError) as exc:
            raise BundleInstallError(
                f"SHA-256 not available in registry index for {name}@{version}"
            ) from exc
        except Exception as exc:  # noqa: BLE001
            raise BundleInstallError(f"Failed to fetch registry index: {exc}") from exc

        sig_url = f"{registry_url}/bundles/{name}/{version}/{name}-{version}.cgbundle.sig"
        try:
            with urllib.request.urlopen(sig_url) as resp:  # noqa: S310
                sig_bytes = resp.read()
        except Exception as exc:  # noqa: BLE001
            raise BundleInstallError(f"Failed to fetch signature: {exc}") from exc

        try:
            key_data = verify_key_path.read_bytes()
            public_key = load_pem_public_key(key_data)
            if not isinstance(public_key, Ed25519PublicKey):
                raise BundleInstallError("Key file does not contain an Ed25519 public key")
            public_key.verify(sig_bytes, bundle_sha256_hex.encode("utf-8"))
        except InvalidSignature as exc:
            raise BundleInstallError("Signature verification failed: invalid signature") from exc
        except BundleInstallError:
            raise
        except Exception as exc:  # noqa: BLE001
            raise BundleInstallError(f"Signature verification failed: {exc}") from exc

    def get_bundle_dir(self, name: str, version: str) -> Path:
        """Return the path where a bundle would be stored (may not exist)."""
        return self._store_dir / name / version

    # ------------------------------------------------------------------
    # Private helpers
    # ------------------------------------------------------------------

    @staticmethod
    def _read_manifest(tf: tarfile.TarFile) -> dict:
        """Read and parse manifest.json from the tar archive."""
        members = tf.getmembers()
        for member in members:
            parts = Path(member.name).parts
            # manifest.json is at <name>-<version>/manifest.json
            if len(parts) == 2 and parts[1] == "manifest.json":
                f = tf.extractfile(member)
                if f is None:
                    raise BundleInstallError("manifest.json is not a regular file")
                try:
                    return json.loads(f.read().decode("utf-8"))
                except (json.JSONDecodeError, UnicodeDecodeError) as exc:
                    raise BundleInstallError(f"Failed to parse manifest.json: {exc}") from exc
        raise BundleInstallError("Bundle does not contain manifest.json")

    @staticmethod
    def _validate_checksums(tf: tarfile.TarFile, manifest: dict) -> None:
        """Validate SHA256 checksums for all files listed in manifest['checksums']."""
        checksums: dict[str, str] = manifest.get("checksums", {})
        if not checksums:
            return

        # Build a lookup: archive-relative path → member
        member_map: dict[str, tarfile.TarInfo] = {}
        for member in tf.getmembers():
            parts = Path(member.name).parts
            if len(parts) >= 2:
                # Strip leading <name>-<version>/ prefix
                rel_path = "/".join(parts[1:])
                member_map[rel_path] = member

        for rel_path, expected_hex in checksums.items():
            member = member_map.get(rel_path)
            if member is None:
                raise BundleInstallError(
                    f"Checksum entry '{rel_path}' not found in archive"
                )
            f = tf.extractfile(member)
            if f is None:
                raise BundleInstallError(f"Cannot read '{rel_path}' from archive")
            actual_hex = hashlib.sha256(f.read()).hexdigest()
            if actual_hex != expected_hex:
                raise BundleInstallError(
                    f"Checksum mismatch for '{rel_path}': "
                    f"expected {expected_hex}, got {actual_hex}"
                )

    @staticmethod
    def _extract(
        tf: tarfile.TarFile, name: str, version: str, dest: Path
    ) -> None:
        """Extract bundle contents (minus manifest.json) to dest, stripping prefix."""
        prefix = f"{name}-{version}/"
        dest.mkdir(parents=True, exist_ok=True)
        for member in tf.getmembers():
            if not member.name.startswith(prefix):
                continue
            rel = member.name[len(prefix):]
            if not rel or rel == "manifest.json":
                continue
            target = dest / rel
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
            elif member.isfile():
                target.parent.mkdir(parents=True, exist_ok=True)
                f = tf.extractfile(member)
                if f is not None:
                    target.write_bytes(f.read())


# ---------------------------------------------------------------------------
# Factory helpers
# ---------------------------------------------------------------------------

GLOBAL_BUNDLE_STORE_DIR = Path.home() / ".codegraph_hub" / "bundles"
WORKSPACE_BUNDLE_STORE_DIR_NAME = ".codegraph_hub"


def get_global_store() -> BundleStore:
    return BundleStore(GLOBAL_BUNDLE_STORE_DIR)


def get_workspace_store(workspace_dir: Path | None = None) -> BundleStore:
    cwd = workspace_dir or Path.cwd()
    return BundleStore(cwd / WORKSPACE_BUNDLE_STORE_DIR_NAME)


# ---------------------------------------------------------------------------
# Package-like adapter for installed bundles
# ---------------------------------------------------------------------------

@dataclass
class BundlePackage:
    """Thin wrapper around a BundleInfo that exposes the same interface as
    :class:`~codegraph_hub.registry.Package` so bundle store entries can be
    passed directly to codegraph query helpers.

    The ``abs_path`` points to the extracted bundle store directory, which
    contains ``graph/``, ``embeddings/``, ``config.json``, etc.
    """

    name: str
    path: str       # str form of the store_dir path
    version: str
    description: str = ""

    @property
    def abs_path(self) -> Path:
        return Path(self.path)

    @property
    def is_indexed(self) -> bool:
        """Bundle is queryable if its ``graph/`` directory exists."""
        return (Path(self.path) / "graph").exists()

    def has_qmd(self) -> bool:
        """Return True when the bundle contains a QMD documentation index."""
        return (Path(self.path) / "qmd" / "index" / "index.sqlite").exists()


def get_all_bundle_packages(workspace_dir: Path | None = None) -> list[BundlePackage]:
    """Return all installed bundles as :class:`BundlePackage` objects.

    Workspace bundles are listed first so that workspace-scoped versions take
    precedence when callers iterate and stop at the first name match.
    Global bundles that share a ``name@version`` with a workspace bundle are
    omitted (deduplicated).
    """
    seen: set[str] = set()
    result: list[BundlePackage] = []

    ws_store = get_workspace_store(workspace_dir)
    for info in ws_store.list_installed():
        key = f"{info.name}@{info.version}"
        result.append(
            BundlePackage(
                name=info.name,
                path=str(info.store_dir),
                version=info.version,
                description=info.manifest.get("source_repo", ""),
            )
        )
        seen.add(key)

    for info in get_global_store().list_installed():
        key = f"{info.name}@{info.version}"
        if key not in seen:
            result.append(
                BundlePackage(
                    name=info.name,
                    path=str(info.store_dir),
                    version=info.version,
                    description=info.manifest.get("source_repo", ""),
                )
            )

    return result
