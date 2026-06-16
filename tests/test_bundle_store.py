"""Tests for bundle_store.BundleStore."""

from __future__ import annotations

import gzip
import hashlib
import io
import json
import tarfile
from pathlib import Path

import pytest

from codegraph_hub.bundle_store import (
    BundleInfo,
    BundleInstallError,
    BundleStore,
    VersionExistsError,
)


# ---------------------------------------------------------------------------
# Helpers to build in-memory .cgbundle archives
# ---------------------------------------------------------------------------

def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _make_bundle(
    name: str = "mylib",
    version: str = "1.0.0",
    extra_files: dict[str, bytes] | None = None,
    bad_checksums: bool = False,
    omit_manifest: bool = False,
) -> bytes:
    """Return bytes of a minimal .cgbundle (gzipped tar).

    extra_files: mapping of relative path -> content (no leading prefix).
    """
    extra_files = extra_files or {"src/foo.py": b"print('hello')\n"}
    prefix = f"{name}-{version}"

    checksums: dict[str, str] = {}
    for rel_path, content in extra_files.items():
        checksums[rel_path] = _sha256(b"wrong" if bad_checksums else content)

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
        if not omit_manifest:
            _add_bytes(tf, f"{prefix}/manifest.json", manifest_bytes)
        for rel_path, content in extra_files.items():
            _add_bytes(tf, f"{prefix}/{rel_path}", content)
    return buf.getvalue()


def _add_bytes(tf: tarfile.TarFile, arcname: str, data: bytes) -> None:
    info = tarfile.TarInfo(name=arcname)
    info.size = len(data)
    tf.addfile(info, io.BytesIO(data))


def _write_bundle(tmp_path: Path, name: str = "mylib", version: str = "1.0.0", **kwargs) -> Path:
    bundle_bytes = _make_bundle(name=name, version=version, **kwargs)
    tmp_path.mkdir(parents=True, exist_ok=True)
    bundle_file = tmp_path / f"{name}-{version}.cgbundle"
    bundle_file.write_bytes(bundle_bytes)
    return bundle_file


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

class TestInstallHappyPath:
    def test_returns_bundle_info(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        bundle = _write_bundle(tmp_path)
        info = store.install(bundle)

        assert isinstance(info, BundleInfo)
        assert info.name == "mylib"
        assert info.version == "1.0.0"

    def test_extracted_files_present(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        bundle = _write_bundle(tmp_path, extra_files={"src/foo.py": b"x = 1\n"})
        info = store.install(bundle)

        assert (info.store_dir / "src" / "foo.py").read_bytes() == b"x = 1\n"

    def test_manifest_written(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        bundle = _write_bundle(tmp_path)
        info = store.install(bundle)

        manifest_path = info.store_dir / "manifest.json"
        assert manifest_path.exists()
        loaded = json.loads(manifest_path.read_text())
        assert loaded["name"] == "mylib"
        assert loaded["version"] == "1.0.0"

    def test_store_layout(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        bundle = _write_bundle(tmp_path, name="pkg", version="2.3.4")
        info = store.install(bundle)

        assert info.store_dir == tmp_path / "store" / "pkg" / "2.3.4"


class TestInstallErrors:
    def test_checksum_mismatch_raises(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        bundle = _write_bundle(tmp_path, bad_checksums=True)
        with pytest.raises(BundleInstallError, match="[Cc]hecksum"):
            store.install(bundle)

    def test_missing_manifest_raises(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        bundle = _write_bundle(tmp_path, omit_manifest=True)
        with pytest.raises(BundleInstallError, match="manifest"):
            store.install(bundle)

    def test_duplicate_version_raises_version_exists(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        bundle = _write_bundle(tmp_path)
        store.install(bundle)

        bundle2 = _write_bundle(tmp_path / "second", name="mylib", version="1.0.0")
        with pytest.raises(VersionExistsError):
            store.install(bundle2)

    def test_version_exists_is_bundle_install_error(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        bundle = _write_bundle(tmp_path)
        store.install(bundle)
        bundle2 = _write_bundle(tmp_path / "b2")
        with pytest.raises(BundleInstallError):
            store.install(bundle2)


class TestListInstalled:
    def test_empty_store(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        assert store.list_installed() == []

    def test_shows_installed_bundles(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        store.install(_write_bundle(tmp_path / "a", name="alpha", version="1.0.0"))
        store.install(_write_bundle(tmp_path / "b", name="beta", version="2.0.0"))

        items = store.list_installed()
        assert len(items) == 2
        names = [i.name for i in items]
        assert "alpha" in names
        assert "beta" in names

    def test_sorted_by_name_then_version(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        store.install(_write_bundle(tmp_path / "b", name="beta", version="1.0.0"))
        store.install(_write_bundle(tmp_path / "a", name="alpha", version="1.0.0"))

        items = store.list_installed()
        assert [i.name for i in items] == ["alpha", "beta"]


class TestRemove:
    def test_remove_returns_true(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        store.install(_write_bundle(tmp_path))
        assert store.remove("mylib", "1.0.0") is True

    def test_remove_deletes_directory(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        info = store.install(_write_bundle(tmp_path))
        store.remove("mylib", "1.0.0")
        assert not info.store_dir.exists()

    def test_remove_not_found_returns_false(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        assert store.remove("nonexistent", "9.9.9") is False

    def test_remove_cleans_up_empty_name_dir(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        store.install(_write_bundle(tmp_path))
        store.remove("mylib", "1.0.0")
        assert not (tmp_path / "store" / "mylib").exists()


class TestResolve:
    def test_resolve_installed(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        info = store.install(_write_bundle(tmp_path))
        resolved = store.resolve("mylib", "1.0.0")
        assert resolved == info.store_dir

    def test_resolve_not_installed_returns_none(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        assert store.resolve("absent", "1.0.0") is None

    def test_resolve_after_remove_returns_none(self, tmp_path):
        store = BundleStore(tmp_path / "store")
        store.install(_write_bundle(tmp_path))
        store.remove("mylib", "1.0.0")
        assert store.resolve("mylib", "1.0.0") is None


class TestInstallFromRegistry:
    def test_install_from_registry_sha256_mismatch(self, tmp_path):
        """Mock urlopen so the index returns a wrong SHA-256 → raises BundleInstallError."""
        from unittest.mock import MagicMock, patch

        store = BundleStore(tmp_path / "store")
        bundle_bytes = _make_bundle()
        wrong_sha256 = "a" * 64

        index_data = json.dumps({
            "packages": {
                "mylib": {
                    "versions": ["1.0.0"],
                    "latest": "1.0.0",
                    "bundles": {
                        "1.0.0": {
                            "sha256": wrong_sha256,
                            "signed": False,
                        }
                    },
                }
            }
        }).encode("utf-8")

        def mock_urlopen(url):
            mock_resp = MagicMock()
            mock_resp.__enter__ = lambda s: s
            mock_resp.__exit__ = MagicMock(return_value=False)
            if str(url).endswith("/index.json"):
                mock_resp.read.return_value = index_data
            else:
                mock_resp.read.return_value = bundle_bytes
            return mock_resp

        with patch("urllib.request.urlopen", side_effect=mock_urlopen):
            with pytest.raises(BundleInstallError, match="SHA-256 mismatch"):
                store.install_from_registry("mylib", "1.0.0", "http://localhost:8765")
