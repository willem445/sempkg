"""Bundle file storage and index management for cgbundle_registry."""

from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path


@dataclass
class StoreResult:
    path: Path
    sha256: str


class VersionExistsError(Exception):
    """Raised when a bundle version already exists in storage."""


class BundleStorage:
    """Manages on-disk storage of .cgbundle files and index.json."""

    def __init__(self, storage_dir: Path | None = None) -> None:
        self.storage_dir = storage_dir or Path.home() / ".cgbundle-registry" / "bundles"
        self.storage_dir.mkdir(parents=True, exist_ok=True)

    # ------------------------------------------------------------------
    # Storage operations
    # ------------------------------------------------------------------

    def store(self, name: str, version: str, data: bytes, sig_bytes: bytes | None = None) -> StoreResult:
        """Write bundle bytes to disk. Raises VersionExistsError if already present."""
        dest_dir = self.storage_dir / name / version
        dest_file = dest_dir / f"{name}-{version}.cgbundle"
        if dest_file.exists():
            raise VersionExistsError(f"{name} {version} already exists")
        dest_dir.mkdir(parents=True, exist_ok=True)
        dest_file.write_bytes(data)
        sha256 = hashlib.sha256(data).hexdigest()
        (dest_dir / f"{name}-{version}.sha256").write_text(sha256, encoding="utf-8")
        if sig_bytes is not None:
            (dest_dir / f"{name}-{version}.sig").write_bytes(sig_bytes)
        return StoreResult(path=dest_file, sha256=sha256)

    def get_path(self, name: str, version: str) -> Path | None:
        """Return the Path to the bundle file, or None if not found."""
        path = self.storage_dir / name / version / f"{name}-{version}.cgbundle"
        return path if path.exists() else None

    def get_signature_path(self, name: str, version: str) -> Path | None:
        """Return the Path to the .sig file, or None if not found."""
        path = self.storage_dir / name / version / f"{name}-{version}.sig"
        return path if path.exists() else None

    # ------------------------------------------------------------------
    # Index management
    # ------------------------------------------------------------------

    def rebuild_index(self) -> dict:
        """Scan storage dir, build and persist index, then return it."""
        packages: dict[str, dict] = {}

        for name_dir in sorted(self.storage_dir.iterdir()):
            if not name_dir.is_dir():
                continue
            name = name_dir.name
            versions = sorted(
                v.name
                for v in name_dir.iterdir()
                if v.is_dir() and (v / f"{name}-{v.name}.cgbundle").exists()
            )
            if not versions:
                continue

            bundles: dict[str, dict] = {}
            for ver in versions:
                ver_dir = name_dir / ver
                sha256_file = ver_dir / f"{name}-{ver}.sha256"
                sha256 = sha256_file.read_text(encoding="utf-8") if sha256_file.exists() else ""
                signed = (ver_dir / f"{name}-{ver}.sig").exists()
                bundles[ver] = {"sha256": sha256, "signed": signed}

            packages[name] = {
                "versions": versions,
                "latest": versions[-1],
                "bundles": bundles,
            }

        index = {
            "packages": packages,
            "generated_at": datetime.now(tz=timezone.utc).isoformat(),
        }
        self.save_index(index)
        return index

    def load_index(self) -> dict:
        """Read index.json from storage root. Returns empty structure if absent."""
        index_path = self.storage_dir / "index.json"
        if not index_path.exists():
            return {
                "packages": {},
                "generated_at": datetime.now(tz=timezone.utc).isoformat(),
            }
        with index_path.open() as fh:
            return json.load(fh)

    def save_index(self, index: dict) -> None:
        """Write index.json to storage root."""
        index_path = self.storage_dir / "index.json"
        with index_path.open("w") as fh:
            json.dump(index, fh, indent=2)
