"""CGBundle unpacking utilities for codegraph-hub.

This module provides the internal ``unpack`` function used to extract and
validate ``.cgbundle`` archives (gzip-compressed tar files) before they are
mounted as indexed packages.

See docs/cgbundle-spec.md for the full bundle specification.
"""

import hashlib
import json
import re
import sys
import tarfile
from pathlib import Path
from typing import Union

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

REQUIRED_TOP_LEVEL_FILES = {"manifest.json", "metadata.json", "config.json"}
REQUIRED_CONTENT_DIRS = {"graph", "embeddings"}

REQUIRED_MANIFEST_FIELDS = (
    "spec_version",
    "name",
    "version",
    "source_repo",
    "commit_hash",
    "tag",
    "created_at",
    "codegraph_version",
    "checksums",
)

REQUIRED_METADATA_FIELDS = (
    "name",
    "version",
    "source_repo",
    "commit_hash",
    "created_at",
)

# Cross-field consistency: these fields must match between manifest.json and metadata.json
CONSISTENCY_FIELDS = ("name", "version", "source_repo", "commit_hash", "created_at")

SUPPORTED_SPEC_MAJOR = 1


# ---------------------------------------------------------------------------
# Exceptions
# ---------------------------------------------------------------------------


class CGBundleError(Exception):
    """Raised when unpacking or validating a ``.cgbundle`` file fails.

    Attributes
    ----------
    code : str
        Machine-readable error code (e.g. ``"E_CHECKSUM_MISMATCH"``).
        See the CGBundle spec (section 11) for the full list.
    """

    def __init__(self, code: str, message: str) -> None:
        self.code = code
        super().__init__(f"[{code}] {message}")


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------


def unpack(
    bundle_path: Union[Path, str],
    target_dir: Union[Path, str, None] = None,
) -> Path:
    """Unpack a ``.cgbundle`` file into *target_dir*.

    All validation (archive structure, manifest fields, cross-field
    consistency, and SHA-256 checksums) is performed against the archive
    contents *before* any files are written to disk.

    Parameters
    ----------
    bundle_path:
        Path to the ``.cgbundle`` file.
    target_dir:
        Directory to extract into.  Defaults to the bundle's parent directory.

    Returns
    -------
    Path
        The extracted bundle root directory
        (``<target_dir>/<name>-<version>/``).

    Raises
    ------
    CGBundleError
        On any structural, manifest, checksum, or consistency error.
    FileNotFoundError
        If *bundle_path* does not exist.
    """
    bundle_path = Path(bundle_path)
    if target_dir is None:
        target_dir = bundle_path.parent
    target_dir = Path(target_dir)

    # Open archive -----------------------------------------------------------
    try:
        tf = tarfile.open(bundle_path, "r:gz")
    except (tarfile.TarError, OSError) as exc:
        raise CGBundleError(
            "E_NOT_ARCHIVE", f"Not a valid gzip tar archive: {exc}"
        ) from exc

    with tf:
        members = tf.getmembers()

        # Structural validation (no extraction yet) --------------------------
        root_prefix = _validate_archive_structure(members)

        # Read and validate manifest.json ------------------------------------
        manifest = _read_json_member(tf, root_prefix + "manifest.json", "manifest.json")
        _validate_manifest_fields(manifest)

        # Read and validate metadata.json ------------------------------------
        metadata = _read_json_member(tf, root_prefix + "metadata.json", "metadata.json")
        _validate_metadata_consistency(manifest, metadata)

        # Validate checksums against archive contents (pre-extraction) -------
        _validate_checksums_in_archive(tf, root_prefix, members, manifest["checksums"])

        # Extract ------------------------------------------------------------
        target_dir.mkdir(parents=True, exist_ok=True)
        if sys.version_info >= (3, 12):
            tf.extractall(target_dir, filter="data")
        else:
            tf.extractall(target_dir)  # noqa: S202 – all paths validated above

    return target_dir / root_prefix.rstrip("/")


# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------


def _validate_archive_structure(members: list) -> str:
    """Validate the tar archive layout and return the root prefix.

    Returns the root prefix string, e.g. ``"aws-sdk-1.11.210/"``.
    Raises :class:`CGBundleError` on any structural problem.
    """
    if not members:
        raise CGBundleError("E_INVALID_ROOT", "Archive is empty")

    # Derive the single root directory from all entry names.
    roots = {m.name.split("/")[0] for m in members if m.name}
    if not roots:
        raise CGBundleError("E_INVALID_ROOT", "Cannot determine archive root directory")
    if len(roots) > 1:
        raise CGBundleError(
            "E_INVALID_ROOT",
            f"Archive has multiple root-level entries: {sorted(roots)}",
        )

    root_name = next(iter(roots))
    if not root_name:
        raise CGBundleError("E_INVALID_ROOT", "Archive root directory name is empty")
    root_prefix = root_name + "/"

    names: set[str] = set()
    for m in members:
        # No symbolic links
        if m.issym() or m.islnk():
            raise CGBundleError(
                "E_SYMLINK", f"Archive contains a symbolic link: {m.name!r}"
            )

        # No absolute paths
        if m.name.startswith("/"):
            raise CGBundleError(
                "E_PATH_TRAVERSAL", f"Absolute path in archive: {m.name!r}"
            )

        # No path traversal sequences
        if ".." in m.name.split("/"):
            raise CGBundleError(
                "E_PATH_TRAVERSAL",
                f"Path traversal sequence in archive entry: {m.name!r}",
            )

        names.add(m.name)

    # Required top-level JSON files
    for fname in REQUIRED_TOP_LEVEL_FILES:
        if root_prefix + fname not in names:
            raise CGBundleError(
                "E_MISSING_FILE", f"Required file missing from archive: {fname}"
            )

    # Required content directories must be non-empty
    for dname in REQUIRED_CONTENT_DIRS:
        dir_prefix = root_prefix + dname + "/"
        has_files = any(
            n.startswith(dir_prefix) and not n.endswith("/") for n in names
        )
        if not has_files:
            raise CGBundleError(
                "E_MISSING_FILE",
                f"Required directory is absent or empty: {dname}/",
            )

    return root_prefix


def _read_json_member(
    tf: tarfile.TarFile, member_name: str, display_name: str
) -> dict:
    """Extract and JSON-parse a single archive member by name."""
    try:
        fobj = tf.extractfile(member_name)
    except KeyError:
        raise CGBundleError(
            "E_MISSING_FILE", f"{display_name} not found in archive"
        )

    if fobj is None:
        raise CGBundleError(
            "E_MISSING_FILE", f"{display_name} is not a regular file"
        )

    try:
        return json.loads(fobj.read().decode("utf-8"))
    except (json.JSONDecodeError, UnicodeDecodeError) as exc:
        raise CGBundleError(
            "E_INVALID_JSON", f"{display_name} is not valid JSON: {exc}"
        ) from exc


def _validate_manifest_fields(manifest: dict) -> None:
    """Validate all required fields in ``manifest.json``."""
    # Presence check
    for field in REQUIRED_MANIFEST_FIELDS:
        if field not in manifest:
            raise CGBundleError(
                "E_MISSING_FIELD",
                f"manifest.json is missing required field: '{field}'",
            )

    # Non-empty string fields
    for field in ("name", "version", "source_repo", "codegraph_version", "spec_version"):
        if not manifest.get(field):
            raise CGBundleError(
                "E_MISSING_FIELD",
                f"manifest.json field '{field}' must not be empty",
            )

    # spec_version: must be semver; major component must be supported
    spec_ver = manifest["spec_version"]
    try:
        major = int(str(spec_ver).split(".")[0])
    except (ValueError, IndexError, AttributeError):
        raise CGBundleError(
            "E_INVALID_FIELD",
            f"spec_version is not a valid semantic version: {spec_ver!r}",
        )
    if major > SUPPORTED_SPEC_MAJOR:
        raise CGBundleError(
            "E_SPEC_VERSION",
            f"Unsupported spec_version {spec_ver!r} (this consumer supports major version "
            f"{SUPPORTED_SPEC_MAJOR})",
        )

    # commit_hash: exactly 40 lowercase hex characters
    if not re.fullmatch(r"[0-9a-f]{40}", manifest.get("commit_hash", "")):
        raise CGBundleError(
            "E_INVALID_FIELD",
            "commit_hash must be exactly 40 lowercase hexadecimal characters",
        )

    # created_at: ISO 8601 UTC datetime (basic pattern)
    if not re.match(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}", manifest.get("created_at", "")):
        raise CGBundleError(
            "E_INVALID_FIELD",
            f"created_at is not a valid ISO 8601 datetime: {manifest.get('created_at')!r}",
        )

    # checksums must be a JSON object
    if not isinstance(manifest.get("checksums"), dict):
        raise CGBundleError(
            "E_INVALID_FIELD", "checksums must be a JSON object"
        )


def _validate_metadata_consistency(manifest: dict, metadata: dict) -> None:
    """Check that ``metadata.json`` fields are consistent with ``manifest.json``."""
    for field in REQUIRED_METADATA_FIELDS:
        if field not in metadata:
            raise CGBundleError(
                "E_MISSING_FIELD",
                f"metadata.json is missing required field: '{field}'",
            )

    for field in CONSISTENCY_FIELDS:
        if manifest.get(field) != metadata.get(field):
            raise CGBundleError(
                "E_CONSISTENCY_MISMATCH",
                f"metadata.json field '{field}' ({metadata.get(field)!r}) does not match "
                f"manifest.json field '{field}' ({manifest.get(field)!r})",
            )


def _validate_checksums_in_archive(
    tf: tarfile.TarFile,
    root_prefix: str,
    members: list,
    expected: dict,
) -> None:
    """Verify SHA-256 checksums for all non-manifest files in the archive.

    Reads each file's content from the open archive and compares it against
    the corresponding entry in the manifest's ``checksums`` map.
    """
    # Collect all regular files except manifest.json
    archive_files: dict[str, tarfile.TarInfo] = {}
    for m in members:
        if not m.isfile():
            continue
        rel = m.name[len(root_prefix):]  # e.g. "graph/nodes.bin"
        if rel == "manifest.json":
            continue
        archive_files[rel] = m

    # Extra files present in archive but absent from checksums
    extra = set(archive_files) - set(expected)
    if extra:
        raise CGBundleError(
            "E_EXTRA_FILE",
            f"Files present in archive but not listed in manifest checksums: "
            f"{sorted(extra)}",
        )

    # Files listed in checksums but absent from archive
    missing = set(expected) - set(archive_files)
    if missing:
        raise CGBundleError(
            "E_MISSING_FILE",
            f"Files listed in manifest checksums but not found in archive: "
            f"{sorted(missing)}",
        )

    # Verify each checksum
    for rel_path, expected_hash in expected.items():
        member = archive_files[rel_path]
        fobj = tf.extractfile(member)
        actual_hash = hashlib.sha256(fobj.read()).hexdigest()
        if actual_hash != expected_hash:
            raise CGBundleError(
                "E_CHECKSUM_MISMATCH",
                f"SHA-256 mismatch for '{rel_path}': "
                f"expected {expected_hash!r}, got {actual_hash!r}",
            )
