"""Unit tests for the CLI module."""

import tempfile
from pathlib import Path
from unittest.mock import patch, MagicMock
import argparse

import pytest

from codegraph_hub import cli
from codegraph_hub.registry import Registry, Package


@pytest.fixture
def temp_config_dir():
    """Provide a temporary config directory for testing."""
    with tempfile.TemporaryDirectory() as tmpdir:
        with patch("codegraph_hub.registry.CONFIG_DIR", Path(tmpdir)):
            with patch("codegraph_hub.registry.CONFIG_FILE", Path(tmpdir) / "packages.json"):
                yield Path(tmpdir)


@pytest.fixture
def temp_project_dir():
    """Provide a temporary project directory for testing."""
    with tempfile.TemporaryDirectory() as tmpdir:
        yield Path(tmpdir)


@pytest.fixture
def registry(temp_config_dir):
    """Provide a fresh Registry instance."""
    with patch("codegraph_hub.registry.CONFIG_DIR", temp_config_dir):
        with patch("codegraph_hub.registry.CONFIG_FILE", temp_config_dir / "packages.json"):
            return Registry()


class TestCmdList:
    """Test the list command."""

    def test_cmd_list_empty_registry(self, registry, capsys):
        """Test list command with no packages."""
        args = argparse.Namespace()
        result = cli.cmd_list(registry, args)
        assert result == 0
        captured = capsys.readouterr()
        assert "No packages registered" in captured.out

    def test_cmd_list_with_packages(self, registry, temp_project_dir, capsys):
        """Test list command with registered packages."""
        registry.add("mylib", str(temp_project_dir), "My library")
        args = argparse.Namespace()
        result = cli.cmd_list(registry, args)
        assert result == 0
        captured = capsys.readouterr()
        assert "mylib" in captured.out
        assert "NOT indexed" in captured.out


class TestCmdAdd:
    """Test the add command."""

    def test_cmd_add_nonexistent_path(self, registry, capsys):
        """Test add command with non-existent path."""
        args = argparse.Namespace(
            name="mylib",
            path="/nonexistent/path",
            description="My library",
        )
        result = cli.cmd_add(registry, args)
        assert result == 1
        captured = capsys.readouterr()
        assert "Error" in captured.err

    def test_cmd_add_existing_indexed_package(self, registry, temp_project_dir, capsys):
        """Test adding a package that's already indexed."""
        # Create .codegraph directory to simulate indexed package
        codegraph_dir = temp_project_dir / ".codegraph"
        codegraph_dir.mkdir()

        args = argparse.Namespace(
            name="mylib",
            path=str(temp_project_dir),
            description="My library",
        )

        with patch("codegraph_hub.cli.cg.init_and_index"):
            result = cli.cmd_add(registry, args)
            assert result == 0
            captured = capsys.readouterr()
            assert "already indexed" in captured.out


class TestCmdRemove:
    """Test the remove command."""

    def test_cmd_remove_existing_package(self, registry, temp_project_dir, capsys):
        """Test removing an existing package."""
        registry.add("mylib", str(temp_project_dir), "My library")
        args = argparse.Namespace(name="mylib")
        result = cli.cmd_remove(registry, args)
        assert result == 0
        captured = capsys.readouterr()
        assert "Removed" in captured.out
        assert registry.get("mylib") is None

    def test_cmd_remove_nonexistent_package(self, registry, capsys):
        """Test removing a non-existent package."""
        args = argparse.Namespace(name="nonexistent")
        result = cli.cmd_remove(registry, args)
        assert result == 1
        captured = capsys.readouterr()
        assert "not found" in captured.err


class TestCmdStatus:
    """Test the status command."""

    def test_cmd_status_nonexistent_package(self, registry, capsys):
        """Test status command for non-existent package."""
        args = argparse.Namespace(name="nonexistent")
        result = cli.cmd_status(registry, args)
        assert result == 1
        captured = capsys.readouterr()
        assert "not found" in captured.err

    def test_cmd_status_existing_package(self, registry, temp_project_dir, capsys):
        """Test status command for existing package."""
        registry.add("mylib", str(temp_project_dir), "My library")
        args = argparse.Namespace(name="mylib")

        with patch("codegraph_hub.cli.cg.status", return_value="Status OK"):
            result = cli.cmd_status(registry, args)
            assert result == 0
            captured = capsys.readouterr()
            assert "Status OK" in captured.out


class TestRequireIndexed:
    """Test the _require_indexed helper function."""

    def test_require_indexed_nonexistent_package(self, registry, capsys):
        """Test _require_indexed with non-existent package."""
        pkg, error_code = cli._require_indexed(registry, "nonexistent")
        assert pkg is None
        assert error_code == 1
        captured = capsys.readouterr()
        assert "not found" in captured.err

    def test_require_indexed_not_indexed_package(self, registry, temp_project_dir, capsys):
        """Test _require_indexed with non-indexed package."""
        registry.add("mylib", str(temp_project_dir), "My library")
        pkg, error_code = cli._require_indexed(registry, "mylib")
        assert pkg is None
        assert error_code == 1
        captured = capsys.readouterr()
        assert "not indexed" in captured.err

    def test_require_indexed_indexed_package(self, registry, temp_project_dir):
        """Test _require_indexed with indexed package."""
        # Create .codegraph directory
        codegraph_dir = temp_project_dir / ".codegraph"
        codegraph_dir.mkdir()

        registry.add("mylib", str(temp_project_dir), "My library")
        pkg, error_code = cli._require_indexed(registry, "mylib")
        assert pkg is not None
        assert error_code == 0
        assert pkg.name == "mylib"


class TestAllIndexed:
    """Test the _all_indexed helper function."""

    def test_all_indexed_empty_registry(self, registry):
        """Test _all_indexed with empty registry."""
        pkgs = cli._all_indexed(registry)
        assert pkgs == []

    def test_all_indexed_mixed_packages(self, registry, temp_project_dir):
        """Test _all_indexed returns only indexed packages."""
        # Create first temp dir and index it
        indexed_dir = temp_project_dir / "indexed"
        indexed_dir.mkdir()
        codegraph_dir = indexed_dir / ".codegraph"
        codegraph_dir.mkdir()

        # Create second temp dir without indexing
        unindexed_dir = temp_project_dir / "unindexed"
        unindexed_dir.mkdir()

        registry.add("indexed_lib", str(indexed_dir), "Indexed library")
        registry.add("unindexed_lib", str(unindexed_dir), "Unindexed library")

        pkgs = cli._all_indexed(registry)
        assert len(pkgs) == 1
        assert pkgs[0].name == "indexed_lib"
