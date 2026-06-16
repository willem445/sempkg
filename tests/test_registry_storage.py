"""Tests for sempkg_registry BundleStorage."""

from __future__ import annotations

import io
import json
import tarfile
from pathlib import Path

import pytest

from sempkg_registry.storage import BundleStorage, StoreResult, VersionExistsError


def make_bundle(name: str, version: str) -> bytes:
    """Create a minimal valid .sembundle (tar.gz) in memory."""
    manifest = {
        "name": name,
        "version": version,
        "source_repo": "https://example.com/repo",
        "commit_hash": "abc123",
        "created_at": "2024-01-01T00:00:00+00:00",
        "codegraph_version": "0.1.0",
        "checksums": {},
        "extensions": {},
    }
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tf:
        manifest_bytes = json.dumps(manifest).encode()
        info = tarfile.TarInfo(name=f"{name}-{version}/manifest.json")
        info.size = len(manifest_bytes)
        tf.addfile(info, io.BytesIO(manifest_bytes))
    return buf.getvalue()


@pytest.fixture
def storage(tmp_path: Path) -> BundleStorage:
    return BundleStorage(storage_dir=tmp_path / "bundles")


def test_store_creates_file(storage: BundleStorage) -> None:
    data = make_bundle("mylib", "1.0.0")
    result = storage.store("mylib", "1.0.0", data)
    assert isinstance(result, StoreResult)
    assert result.path.exists()
    assert result.path.name == "mylib-1.0.0.sembundle"


def test_store_raises_on_duplicate(storage: BundleStorage) -> None:
    data = make_bundle("mylib", "1.0.0")
    storage.store("mylib", "1.0.0", data)
    with pytest.raises(VersionExistsError):
        storage.store("mylib", "1.0.0", data)


def test_get_path_existing(storage: BundleStorage) -> None:
    data = make_bundle("mylib", "2.0.0")
    storage.store("mylib", "2.0.0", data)
    path = storage.get_path("mylib", "2.0.0")
    assert path is not None
    assert path.exists()


def test_get_path_missing(storage: BundleStorage) -> None:
    assert storage.get_path("nolib", "9.9.9") is None


def test_rebuild_index_empty(storage: BundleStorage) -> None:
    index = storage.rebuild_index()
    assert index["packages"] == {}


def test_rebuild_index_with_bundles(storage: BundleStorage) -> None:
    storage.store("mylib", "1.0.0", make_bundle("mylib", "1.0.0"))
    storage.store("mylib", "1.1.0", make_bundle("mylib", "1.1.0"))
    storage.store("otherlib", "0.1.0", make_bundle("otherlib", "0.1.0"))

    index = storage.rebuild_index()
    assert set(index["packages"].keys()) == {"mylib", "otherlib"}
    assert index["packages"]["mylib"]["latest"] == "1.1.0"
    assert "1.0.0" in index["packages"]["mylib"]["versions"]
    assert index["packages"]["otherlib"]["latest"] == "0.1.0"


def test_load_index_returns_empty_when_missing(storage: BundleStorage) -> None:
    index = storage.load_index()
    assert "packages" in index
    assert index["packages"] == {}


def test_save_and_load_index(storage: BundleStorage) -> None:
    index = {"packages": {"lib": {"versions": ["1.0.0"], "latest": "1.0.0"}}, "generated_at": "now"}
    storage.save_index(index)
    loaded = storage.load_index()
    assert loaded["packages"]["lib"]["latest"] == "1.0.0"


def test_store_includes_sha256(storage: BundleStorage) -> None:
    import hashlib
    data = make_bundle("mylib", "1.0.0")
    result = storage.store("mylib", "1.0.0", data)
    sha256_file = storage.storage_dir / "mylib" / "1.0.0" / "mylib-1.0.0.sha256"
    expected = hashlib.sha256(data).hexdigest()
    assert sha256_file.exists()
    assert sha256_file.read_text(encoding="utf-8") == expected
    assert result.sha256 == expected


def test_rebuild_index_includes_sha256(storage: BundleStorage) -> None:
    storage.store("mylib", "1.0.0", make_bundle("mylib", "1.0.0"))
    index = storage.rebuild_index()
    bundle_entry = index["packages"]["mylib"]["bundles"]["1.0.0"]
    assert "sha256" in bundle_entry
    assert len(bundle_entry["sha256"]) == 64


def test_rebuild_index_signed_flag(storage: BundleStorage) -> None:
    storage.store("mylib", "1.0.0", make_bundle("mylib", "1.0.0"))
    storage.store("otherlib", "1.0.0", make_bundle("otherlib", "1.0.0"), sig_bytes=b"\x00" * 64)
    index = storage.rebuild_index()
    assert index["packages"]["mylib"]["bundles"]["1.0.0"]["signed"] is False
    assert index["packages"]["otherlib"]["bundles"]["1.0.0"]["signed"] is True
