"""Unit tests for the cgbundle unpacker (Task 3)."""

import hashlib
import io
import json
import tarfile
import tempfile
from pathlib import Path

import pytest

from codegraph_hub.cgbundle import CGBundleError, unpack

# ---------------------------------------------------------------------------
# Test-bundle builder helpers
# ---------------------------------------------------------------------------

_VALID_COMMIT_HASH = "a" * 40
_VALID_CREATED_AT = "2025-01-01T00:00:00Z"

_BASE_METADATA = {
    "name": "test-lib",
    "version": "1.2.3",
    "source_repo": "https://github.com/example/test-lib",
    "commit_hash": _VALID_COMMIT_HASH,
    "tag": "1.2.3",
    "language": "python",
    "indexed_paths": ["."],
    "created_at": _VALID_CREATED_AT,
}

_BASE_MANIFEST = {
    "spec_version": "1.0.0",
    "name": "test-lib",
    "version": "1.2.3",
    "source_repo": "https://github.com/example/test-lib",
    "commit_hash": _VALID_COMMIT_HASH,
    "tag": "1.2.3",
    "created_at": _VALID_CREATED_AT,
    "codegraph_version": "0.3.1",
    "checksums": {},  # filled in by _make_bundle
}


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _add_bytes(tf: tarfile.TarFile, name: str, data: bytes) -> None:
    """Add a bytes object as a regular file entry to an open TarFile."""
    info = tarfile.TarInfo(name=name)
    info.size = len(data)
    tf.addfile(info, io.BytesIO(data))


def _make_bundle(
    directory: Path,
    name: str = "test-lib",
    version: str = "1.2.3",
    manifest_override: dict | None = None,
    metadata_override: dict | None = None,
    manifest_delete_fields: set[str] | None = None,
    extra_archive_files: dict[str, bytes] | None = None,
    skip_files: set[str] | None = None,
    corrupt_checksum: str | None = None,
) -> Path:
    """Create a ``.cgbundle`` file for testing and return its path.

    Parameters
    ----------
    directory:
        Destination directory for the bundle file (created if absent).
    name / version:
        Bundle identity; used for the root directory and filename.
    manifest_override:
        Key-value pairs merged into the manifest after defaults are applied.
    metadata_override:
        Key-value pairs merged into metadata.json.
    manifest_delete_fields:
        Fields to delete from manifest.json entirely (for missing-field tests).
    extra_archive_files:
        Relative paths → bytes added to the archive but NOT to checksums.
        Triggers ``E_EXTRA_FILE`` validation failures.
    skip_files:
        Relative paths to omit from both the archive and the checksums map.
        Triggers ``E_MISSING_FILE`` validation failures.
    corrupt_checksum:
        Relative path whose checksum entry is zeroed out in the manifest.
        Triggers ``E_CHECKSUM_MISMATCH`` validation failures.
    """
    root = f"{name}-{version}"
    skip = skip_files or set()

    # Base file contents
    metadata = {**_BASE_METADATA, "name": name, "version": version}
    if metadata_override:
        metadata.update(metadata_override)

    base_files: dict[str, bytes] = {
        "metadata.json": json.dumps(metadata).encode(),
        "config.json": b"{}",
        "graph/nodes.bin": b"graph_data",
        "embeddings/vectors.bin": b"embeddings_data",
    }

    # Remove skipped files before computing checksums
    for s in skip:
        base_files.pop(s, None)

    # Build checksum map for manifest (covers all base files, not manifest.json itself)
    checksums = {path: _sha256(data) for path, data in base_files.items()}
    if corrupt_checksum:
        checksums[corrupt_checksum] = "0" * 64

    # Build manifest
    manifest = {**_BASE_MANIFEST, "name": name, "version": version, "checksums": checksums}
    if manifest_override:
        manifest.update(manifest_override)
    if manifest_delete_fields:
        for field in manifest_delete_fields:
            manifest.pop(field, None)
    manifest_bytes = json.dumps(manifest).encode()

    # Write archive
    directory.mkdir(parents=True, exist_ok=True)
    bundle_path = directory / f"{name}-{version}.cgbundle"

    with tarfile.open(bundle_path, "w:gz") as tf:
        # manifest.json is excluded from checksums per spec
        if "manifest.json" not in skip:
            _add_bytes(tf, f"{root}/manifest.json", manifest_bytes)
        for rel_path, data in base_files.items():
            _add_bytes(tf, f"{root}/{rel_path}", data)
        if extra_archive_files:
            for rel_path, data in extra_archive_files.items():
                _add_bytes(tf, f"{root}/{rel_path}", data)

    return bundle_path


# ---------------------------------------------------------------------------
# Successful unpack
# ---------------------------------------------------------------------------


class TestUnpackValid:
    def test_returns_root_directory_path(self, tmp_path):
        bundle = _make_bundle(tmp_path / "src")
        out = unpack(bundle, tmp_path / "out")
        assert out.name == "test-lib-1.2.3"

    def test_all_required_files_extracted(self, tmp_path):
        bundle = _make_bundle(tmp_path / "src")
        out = unpack(bundle, tmp_path / "out")
        assert (out / "manifest.json").is_file()
        assert (out / "metadata.json").is_file()
        assert (out / "config.json").is_file()
        assert (out / "graph" / "nodes.bin").is_file()
        assert (out / "embeddings" / "vectors.bin").is_file()

    def test_default_target_dir_is_bundle_parent(self, tmp_path):
        src_dir = tmp_path / "src"
        bundle = _make_bundle(src_dir)
        out = unpack(bundle)
        assert out.parent == src_dir
        assert out.is_dir()

    def test_target_dir_created_if_absent(self, tmp_path):
        bundle = _make_bundle(tmp_path / "src")
        target = tmp_path / "deep" / "nested" / "out"
        assert not target.exists()
        out = unpack(bundle, target)
        assert out.is_dir()

    def test_custom_name_and_version(self, tmp_path):
        bundle = _make_bundle(tmp_path / "src", name="aws-sdk", version="1.11.210")
        out = unpack(bundle, tmp_path / "out")
        assert out.name == "aws-sdk-1.11.210"
        assert out.is_dir()


# ---------------------------------------------------------------------------
# Corrupted / invalid archive
# ---------------------------------------------------------------------------


class TestCorruptedBundle:
    def test_not_a_tar_file(self, tmp_path):
        bad = tmp_path / "bad.cgbundle"
        bad.write_bytes(b"this is not a tar file at all")
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bad, tmp_path / "out")
        assert exc_info.value.code == "E_NOT_ARCHIVE"

    def test_truncated_gzip(self, tmp_path):
        # Write a valid gzip header but truncated body
        bad = tmp_path / "truncated.cgbundle"
        bad.write_bytes(b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x03" + b"\x00" * 10)
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bad, tmp_path / "out")
        assert exc_info.value.code == "E_NOT_ARCHIVE"

    def test_empty_archive(self, tmp_path):
        bundle = tmp_path / "empty.cgbundle"
        with tarfile.open(bundle, "w:gz"):
            pass
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_INVALID_ROOT"

    def test_multiple_root_directories(self, tmp_path):
        bundle = tmp_path / "multi-root.cgbundle"
        with tarfile.open(bundle, "w:gz") as tf:
            _add_bytes(tf, "root-a/file.txt", b"a")
            _add_bytes(tf, "root-b/file.txt", b"b")
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_INVALID_ROOT"

    def test_symlink_in_archive(self, tmp_path):
        bundle = tmp_path / "symlink.cgbundle"
        with tarfile.open(bundle, "w:gz") as tf:
            _add_bytes(tf, "test-lib-1.2.3/manifest.json", b"{}")
            sym = tarfile.TarInfo("test-lib-1.2.3/link")
            sym.type = tarfile.SYMTYPE
            sym.linkname = "/etc/passwd"
            tf.addfile(sym)
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_SYMLINK"

    def test_path_traversal_in_archive(self, tmp_path):
        bundle = tmp_path / "traversal.cgbundle"
        with tarfile.open(bundle, "w:gz") as tf:
            _add_bytes(tf, "test-lib-1.2.3/../../../evil.txt", b"evil")
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_PATH_TRAVERSAL"


# ---------------------------------------------------------------------------
# Missing required files
# ---------------------------------------------------------------------------


class TestMissingFiles:
    def test_missing_manifest_json(self, tmp_path):
        bundle = _make_bundle(tmp_path / "src", skip_files={"manifest.json"})
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_MISSING_FILE"

    def test_missing_metadata_json(self, tmp_path):
        bundle = _make_bundle(tmp_path / "src", skip_files={"metadata.json"})
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_MISSING_FILE"

    def test_missing_config_json(self, tmp_path):
        bundle = _make_bundle(tmp_path / "src", skip_files={"config.json"})
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_MISSING_FILE"

    def test_empty_graph_directory(self, tmp_path):
        bundle = _make_bundle(tmp_path / "src", skip_files={"graph/nodes.bin"})
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_MISSING_FILE"

    def test_empty_embeddings_directory(self, tmp_path):
        bundle = _make_bundle(tmp_path / "src", skip_files={"embeddings/vectors.bin"})
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_MISSING_FILE"


# ---------------------------------------------------------------------------
# Checksum validation
# ---------------------------------------------------------------------------


class TestChecksumValidation:
    def test_checksum_mismatch(self, tmp_path):
        bundle = _make_bundle(tmp_path / "src", corrupt_checksum="config.json")
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_CHECKSUM_MISMATCH"

    def test_checksum_mismatch_graph_file(self, tmp_path):
        bundle = _make_bundle(tmp_path / "src", corrupt_checksum="graph/nodes.bin")
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_CHECKSUM_MISMATCH"

    def test_extra_file_not_in_checksums(self, tmp_path):
        bundle = _make_bundle(
            tmp_path / "src",
            extra_archive_files={"unlisted_extra.bin": b"extra data"},
        )
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_EXTRA_FILE"


# ---------------------------------------------------------------------------
# Manifest field validation
# ---------------------------------------------------------------------------


class TestManifestValidation:
    def test_invalid_json_manifest(self, tmp_path):
        bundle = tmp_path / "test-lib-1.2.3.cgbundle"
        root = "test-lib-1.2.3"
        meta = json.dumps(_BASE_METADATA).encode()
        with tarfile.open(bundle, "w:gz") as tf:
            _add_bytes(tf, f"{root}/manifest.json", b"{ not valid json {{")
            _add_bytes(tf, f"{root}/metadata.json", meta)
            _add_bytes(tf, f"{root}/config.json", b"{}")
            _add_bytes(tf, f"{root}/graph/nodes.bin", b"graph")
            _add_bytes(tf, f"{root}/embeddings/vectors.bin", b"embed")
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_INVALID_JSON"

    def test_missing_required_manifest_field(self, tmp_path):
        bundle = _make_bundle(
            tmp_path / "src", manifest_delete_fields={"commit_hash"}
        )
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_MISSING_FIELD"

    def test_empty_name_field(self, tmp_path):
        bundle = _make_bundle(tmp_path / "src", manifest_override={"name": ""})
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_MISSING_FIELD"

    def test_unsupported_spec_version(self, tmp_path):
        bundle = _make_bundle(
            tmp_path / "src", manifest_override={"spec_version": "99.0.0"}
        )
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_SPEC_VERSION"

    def test_invalid_commit_hash_length(self, tmp_path):
        bundle = _make_bundle(
            tmp_path / "src", manifest_override={"commit_hash": "abc123"}
        )
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_INVALID_FIELD"

    def test_invalid_commit_hash_uppercase(self, tmp_path):
        # Spec requires lowercase hex
        bundle = _make_bundle(
            tmp_path / "src",
            manifest_override={"commit_hash": "A" * 40},
        )
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_INVALID_FIELD"

    def test_invalid_created_at(self, tmp_path):
        bundle = _make_bundle(
            tmp_path / "src", manifest_override={"created_at": "not-a-date"}
        )
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_INVALID_FIELD"

    def test_checksums_not_a_dict(self, tmp_path):
        bundle = _make_bundle(
            tmp_path / "src", manifest_override={"checksums": ["a", "b"]}
        )
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_INVALID_FIELD"

    def test_spec_version_not_semver(self, tmp_path):
        bundle = _make_bundle(
            tmp_path / "src", manifest_override={"spec_version": "not-a-version"}
        )
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_INVALID_FIELD"


# ---------------------------------------------------------------------------
# Cross-field consistency (manifest vs metadata)
# ---------------------------------------------------------------------------


class TestConsistencyValidation:
    def test_metadata_name_mismatch(self, tmp_path):
        bundle = _make_bundle(
            tmp_path / "src", metadata_override={"name": "wrong-name"}
        )
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_CONSISTENCY_MISMATCH"

    def test_metadata_version_mismatch(self, tmp_path):
        bundle = _make_bundle(
            tmp_path / "src", metadata_override={"version": "9.9.9"}
        )
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_CONSISTENCY_MISMATCH"

    def test_metadata_commit_hash_mismatch(self, tmp_path):
        bundle = _make_bundle(
            tmp_path / "src", metadata_override={"commit_hash": "b" * 40}
        )
        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_CONSISTENCY_MISMATCH"

    def test_metadata_missing_required_field(self, tmp_path):
        # Build a bundle where metadata.json is missing 'source_repo'
        meta = {k: v for k, v in _BASE_METADATA.items() if k != "source_repo"}
        meta_bytes = json.dumps(meta).encode()

        # We need the checksums to be correct, so rebuild manually
        root = "test-lib-1.2.3"
        config_bytes = b"{}"
        graph_bytes = b"graph_data"
        embed_bytes = b"embeddings_data"
        checksums = {
            "metadata.json": _sha256(meta_bytes),
            "config.json": _sha256(config_bytes),
            "graph/nodes.bin": _sha256(graph_bytes),
            "embeddings/vectors.bin": _sha256(embed_bytes),
        }
        manifest = {**_BASE_MANIFEST, "checksums": checksums}
        manifest_bytes = json.dumps(manifest).encode()

        bundle = tmp_path / "test-lib-1.2.3.cgbundle"
        with tarfile.open(bundle, "w:gz") as tf:
            _add_bytes(tf, f"{root}/manifest.json", manifest_bytes)
            _add_bytes(tf, f"{root}/metadata.json", meta_bytes)
            _add_bytes(tf, f"{root}/config.json", config_bytes)
            _add_bytes(tf, f"{root}/graph/nodes.bin", graph_bytes)
            _add_bytes(tf, f"{root}/embeddings/vectors.bin", embed_bytes)

        with pytest.raises(CGBundleError) as exc_info:
            unpack(bundle, tmp_path / "out")
        assert exc_info.value.code == "E_MISSING_FIELD"
