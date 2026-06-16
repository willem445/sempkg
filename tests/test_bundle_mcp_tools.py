"""Unit tests for bundle MCP tools in server.py."""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

# Imported lazily so we can patch bundle_store before server loads
import codegraph_hub.bundle_store as bundle_store_module
import codegraph_hub.server as server_module
from codegraph_hub.server import (
    get_bundle_info,
    list_bundle_callers,
    list_bundle_callees,
    list_bundle_packages,
    search_bundle_symbol,
)

_SERVER = "codegraph_hub.server"


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_bundle_info(name: str, version: str, store_dir: Path | None = None) -> bundle_store_module.BundleInfo:
    return bundle_store_module.BundleInfo(
        name=name,
        version=version,
        store_dir=store_dir or Path(f"/fake/{name}/{version}"),
        manifest={
            "name": name,
            "version": version,
            "source_repo": f"https://github.com/example/{name}",
            "commit_hash": "abc123",
            "created_at": "2026-01-01T00:00:00Z",
            "codegraph_version": "0.5.0",
            "extensions": [".py"],
        },
    )


def _empty_store() -> MagicMock:
    store = MagicMock()
    store.list_installed.return_value = []
    store.resolve.return_value = None
    return store


# ---------------------------------------------------------------------------
# list_bundle_packages
# ---------------------------------------------------------------------------

def test_list_bundle_packages_empty():
    with (
        patch(f"{_SERVER}.get_workspace_store", return_value=_empty_store()),
        patch(f"{_SERVER}.get_global_store", return_value=_empty_store()),
    ):
        result = list_bundle_packages()
    assert "no installed bundles" in result.lower() or "no bundle" in result.lower()


def test_list_bundle_packages_with_bundles():
    ws_store = MagicMock()
    ws_store.list_installed.return_value = [_make_bundle_info("mylib", "1.0.0")]

    global_store = MagicMock()
    global_store.list_installed.return_value = [_make_bundle_info("shared-lib", "2.3.1")]

    with (
        patch(f"{_SERVER}.get_workspace_store", return_value=ws_store),
        patch(f"{_SERVER}.get_global_store", return_value=global_store),
    ):
        result = list_bundle_packages()

    assert "mylib" in result
    assert "1.0.0" in result
    assert "shared-lib" in result
    assert "2.3.1" in result
    assert "workspace" in result.lower()
    assert "global" in result.lower()


# ---------------------------------------------------------------------------
# get_bundle_info
# ---------------------------------------------------------------------------

def test_get_bundle_info_not_found():
    with (
        patch(f"{_SERVER}.get_workspace_store", return_value=_empty_store()),
        patch(f"{_SERVER}.get_global_store", return_value=_empty_store()),
    ):
        result = get_bundle_info("missing-lib", "9.9.9")

    assert "not found" in result.lower()
    assert "missing-lib" in result


def test_get_bundle_info_found():
    ws_store = MagicMock()
    ws_store.list_installed.return_value = [_make_bundle_info("mylib", "1.0.0")]

    with (
        patch(f"{_SERVER}.get_workspace_store", return_value=ws_store),
        patch(f"{_SERVER}.get_global_store", return_value=_empty_store()),
    ):
        result = get_bundle_info("mylib", "1.0.0")

    assert "mylib" in result
    assert "1.0.0" in result
    assert "https://github.com/example/mylib" in result
    assert "abc123" in result


# ---------------------------------------------------------------------------
# search_bundle_symbol
# ---------------------------------------------------------------------------

def test_search_bundle_symbol_not_found():
    with (
        patch(f"{_SERVER}.get_workspace_store", return_value=_empty_store()),
        patch(f"{_SERVER}.get_global_store", return_value=_empty_store()),
    ):
        result = search_bundle_symbol("missing-lib", "1.0.0", "some_symbol")

    assert "not found" in result.lower()
    assert "missing-lib" in result


def test_search_bundle_symbol_found():
    bundle_dir = Path("/fake/mylib/1.0.0")
    ws_store = MagicMock()
    ws_store.resolve.return_value = bundle_dir

    with (
        patch(f"{_SERVER}.get_workspace_store", return_value=ws_store),
        patch(f"{_SERVER}.get_global_store", return_value=_empty_store()),
        patch(f"{_SERVER}.query", return_value="MyClass  mylib/core.py:10") as mock_query,
    ):
        result = search_bundle_symbol("mylib", "1.0.0", "MyClass")

    mock_query.assert_called_once_with(bundle_dir, "MyClass", kind=None, limit=10)
    assert "MyClass" in result


# ---------------------------------------------------------------------------
# list_bundle_callers / list_bundle_callees
# ---------------------------------------------------------------------------

def test_list_bundle_callers_not_found():
    with (
        patch(f"{_SERVER}.get_workspace_store", return_value=_empty_store()),
        patch(f"{_SERVER}.get_global_store", return_value=_empty_store()),
    ):
        result = list_bundle_callers("missing-lib", "1.0.0", "my_func")

    assert "not found" in result.lower()


def test_list_bundle_callees_not_found():
    with (
        patch(f"{_SERVER}.get_workspace_store", return_value=_empty_store()),
        patch(f"{_SERVER}.get_global_store", return_value=_empty_store()),
    ):
        result = list_bundle_callees("missing-lib", "1.0.0", "my_func")

    assert "not found" in result.lower()
