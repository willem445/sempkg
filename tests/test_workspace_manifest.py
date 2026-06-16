"""Tests for workspace_manifest module."""

from __future__ import annotations

import hashlib
import io
import json
import tarfile
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from codegraph_hub.workspace_manifest import (
    BundleDep,
    LockEntry,
    LockFile,
    LOCK_FILENAME,
    MANIFEST_FILENAME,
    RegistryConfig,
    WorkspaceManifest,
    load_lock,
    load_manifest,
    resolve_lock,
    save_lock,
    save_manifest,
    sync_workspace,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _make_bundle(name: str = "mylib", version: str = "1.0.0") -> bytes:
    """Build a minimal .cgbundle (gzipped tar) in memory."""
    prefix = f"{name}-{version}"
    file_content = b"print('hello')\n"
    checksums = {"src/foo.py": _sha256(file_content)}
    manifest = {
        "name": name,
        "version": version,
        "source_repo": "https://github.com/example/mylib",
        "commit_hash": "abc123",
        "created_at": "2024-01-01T00:00:00Z",
        "codegraph_version": "0.1.0",
        "checksums": checksums,
        "extensions": {},
    }
    manifest_bytes = json.dumps(manifest, indent=2).encode("utf-8")

    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tf:
        ti = tarfile.TarInfo(name=f"{prefix}/manifest.json")
        ti.size = len(manifest_bytes)
        tf.addfile(ti, io.BytesIO(manifest_bytes))

        ti2 = tarfile.TarInfo(name=f"{prefix}/src/foo.py")
        ti2.size = len(file_content)
        tf.addfile(ti2, io.BytesIO(file_content))

    return buf.getvalue()


class _MockResponse:
    """Minimal context-manager response mock for urllib.request.urlopen."""

    def __init__(self, data: bytes) -> None:
        self._data = data

    def read(self) -> bytes:
        return self._data

    def __enter__(self) -> "_MockResponse":
        return self

    def __exit__(self, *args: object) -> bool:
        return False


# ---------------------------------------------------------------------------
# Parsing tests
# ---------------------------------------------------------------------------

def test_load_manifest_basic(tmp_path: Path) -> None:
    (tmp_path / MANIFEST_FILENAME).write_text(
        """
[[registries]]
name = "default"
url = "http://127.0.0.1:8765"

[dependencies]
mylib = {version = "1.2.0", registry = "default"}
""",
        encoding="utf-8",
    )
    manifest = load_manifest(tmp_path)
    assert len(manifest.registries) == 1
    assert manifest.registries[0].name == "default"
    assert manifest.registries[0].url == "http://127.0.0.1:8765"
    assert "mylib" in manifest.dependencies
    dep = manifest.dependencies["mylib"]
    assert dep.version == "1.2.0"
    assert dep.registry == "default"
    assert manifest.verify_key is None


def test_load_manifest_default_registry(tmp_path: Path) -> None:
    (tmp_path / MANIFEST_FILENAME).write_text(
        """
[[registries]]
name = "default"
url = "http://127.0.0.1:8765"

[dependencies]
mylib = {version = "1.0.0"}
""",
        encoding="utf-8",
    )
    manifest = load_manifest(tmp_path)
    assert manifest.dependencies["mylib"].registry == "default"


def test_load_manifest_missing_registry_raises(tmp_path: Path) -> None:
    (tmp_path / MANIFEST_FILENAME).write_text(
        """
[[registries]]
name = "default"
url = "http://127.0.0.1:8765"

[dependencies]
mylib = {version = "1.0.0", registry = "nonexistent"}
""",
        encoding="utf-8",
    )
    with pytest.raises(ValueError, match="nonexistent"):
        load_manifest(tmp_path)


def test_load_manifest_file_not_found(tmp_path: Path) -> None:
    with pytest.raises(FileNotFoundError):
        load_manifest(tmp_path)


# ---------------------------------------------------------------------------
# Lock read/write round-trip
# ---------------------------------------------------------------------------

def test_lock_roundtrip(tmp_path: Path) -> None:
    lock = LockFile(
        packages={
            "mylib": LockEntry(
                name="mylib",
                version="1.2.0",
                registry_url="http://192.168.1.25:8765",
                sha256="a665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3",
                signed=True,
                manifest_checksums={"config.json": "abc123", "graph/nodes.bin": "def456"},
            ),
            "otherlib": LockEntry(
                name="otherlib",
                version="2.0.0",
                registry_url="http://192.168.1.25:8765",
                sha256="b" * 64,
                signed=False,
                manifest_checksums={},
            ),
        }
    )
    save_lock(lock, tmp_path)
    lock2 = load_lock(tmp_path)

    assert set(lock2.packages.keys()) == {"mylib", "otherlib"}

    e = lock2.packages["mylib"]
    assert e.version == "1.2.0"
    assert e.sha256 == "a665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3"
    assert e.signed is True
    assert e.manifest_checksums == {"config.json": "abc123", "graph/nodes.bin": "def456"}

    e2 = lock2.packages["otherlib"]
    assert e2.signed is False
    assert e2.manifest_checksums == {}


def test_load_lock_missing_returns_empty(tmp_path: Path) -> None:
    lock = load_lock(tmp_path)
    assert lock.packages == {}


# ---------------------------------------------------------------------------
# resolve_lock (mock HTTP)
# ---------------------------------------------------------------------------

def test_resolve_lock_fetches_sha256(tmp_path: Path) -> None:
    manifest = WorkspaceManifest(
        registries=[RegistryConfig(name="default", url="http://test.local")],
        dependencies={"mylib": BundleDep(name="mylib", version="1.0.0", registry="default")},
        verify_key=None,
    )

    bundle_bytes = _make_bundle("mylib", "1.0.0")
    expected_sha256 = _sha256(bundle_bytes)

    index_data = {
        "packages": {
            "mylib": {
                "bundles": {
                    "1.0.0": {"sha256": expected_sha256, "signed": False}
                }
            }
        }
    }

    def _urlopen(url: str) -> _MockResponse:
        if "index.json" in url:
            return _MockResponse(json.dumps(index_data).encode())
        return _MockResponse(bundle_bytes)

    with patch("urllib.request.urlopen", side_effect=_urlopen):
        lock = resolve_lock(manifest, tmp_path)

    assert "mylib" in lock.packages
    entry = lock.packages["mylib"]
    assert entry.sha256 == expected_sha256
    assert entry.registry_url == "http://test.local"
    assert entry.signed is False
    # manifest_checksums come from the bundle's inner manifest.json checksums
    assert "src/foo.py" in entry.manifest_checksums


# ---------------------------------------------------------------------------
# sync_workspace (mock HTTP)
# ---------------------------------------------------------------------------

def _write_manifest(workspace_dir: Path, registry_url: str, pkg: str, version: str) -> None:
    (workspace_dir / MANIFEST_FILENAME).write_text(
        f"""
[[registries]]
name = "default"
url = "{registry_url}"

[dependencies]
{pkg} = {{version = "{version}", registry = "default"}}
""",
        encoding="utf-8",
    )


def test_sync_installs_bundles(tmp_path: Path) -> None:
    registry_url = "http://test.local"
    _write_manifest(tmp_path, registry_url, "mylib", "1.0.0")

    bundle_bytes = _make_bundle("mylib", "1.0.0")
    expected_sha256 = _sha256(bundle_bytes)

    index_data = {
        "packages": {
            "mylib": {
                "bundles": {
                    "1.0.0": {"sha256": expected_sha256, "signed": False}
                }
            }
        }
    }

    def _urlopen(url: str) -> _MockResponse:
        if "index.json" in url:
            return _MockResponse(json.dumps(index_data).encode())
        return _MockResponse(bundle_bytes)

    with patch("urllib.request.urlopen", side_effect=_urlopen):
        installed = sync_workspace(tmp_path)

    assert "mylib@1.0.0" in installed
    bundle_dir = tmp_path / ".codegraph_hub" / "mylib" / "1.0.0"
    assert bundle_dir.exists()
    assert (tmp_path / LOCK_FILENAME).exists()


def test_sync_skips_already_installed(tmp_path: Path) -> None:
    registry_url = "http://test.local"
    _write_manifest(tmp_path, registry_url, "mylib", "1.0.0")

    # Pre-create the bundle directory so store.resolve() returns non-None
    bundle_dir = tmp_path / ".codegraph_hub" / "mylib" / "1.0.0"
    bundle_dir.mkdir(parents=True)
    (bundle_dir / "manifest.json").write_text(
        json.dumps({"name": "mylib", "version": "1.0.0", "checksums": {}}),
        encoding="utf-8",
    )

    # Write a lock file so resolve_lock is not called
    lock = LockFile(
        packages={
            "mylib": LockEntry(
                name="mylib",
                version="1.0.0",
                registry_url=registry_url,
                sha256="abc123",
                signed=False,
                manifest_checksums={},
            )
        }
    )
    save_lock(lock, tmp_path)

    mock_urlopen = MagicMock()
    with patch("urllib.request.urlopen", mock_urlopen):
        installed = sync_workspace(tmp_path)

    assert installed == []
    mock_urlopen.assert_not_called()
