"""Unit tests for the registry module."""

import json
import tempfile
from pathlib import Path
from unittest.mock import patch

import pytest

from codegraph_hub.registry import Package, Registry, CONFIG_DIR, CONFIG_FILE


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


class TestPackage:
    """Test the Package dataclass."""

    def test_package_creation(self):
        """Test Package can be created with basic attributes."""
        pkg = Package(name="test", path="/home/user/test", description="Test package")
        assert pkg.name == "test"
        assert pkg.path == "/home/user/test"
        assert pkg.description == "Test package"

    def test_package_abs_path_expands_user(self, temp_project_dir):
        """Test that abs_path expands ~ notation."""
        pkg = Package(name="test", path="~/test", description="")
        # abs_path should expand and resolve the path
        assert pkg.abs_path.is_absolute()

    def test_package_is_indexed_false_without_codegraph_dir(self, temp_project_dir):
        """Test is_indexed returns False when .codegraph directory doesn't exist."""
        pkg = Package(name="test", path=str(temp_project_dir), description="")
        assert pkg.is_indexed is False

    def test_package_is_indexed_true_with_codegraph_dir(self, temp_project_dir):
        """Test is_indexed returns True when .codegraph directory exists."""
        codegraph_dir = temp_project_dir / ".codegraph"
        codegraph_dir.mkdir()
        pkg = Package(name="test", path=str(temp_project_dir), description="")
        assert pkg.is_indexed is True


class TestRegistry:
    """Test the Registry class."""

    def test_registry_create_config_dir(self, temp_config_dir):
        """Test Registry creates config directory if it doesn't exist."""
        with patch("codegraph_hub.registry.CONFIG_DIR", temp_config_dir):
            registry = Registry()
            assert temp_config_dir.exists()

    def test_registry_add_package(self, temp_config_dir, temp_project_dir):
        """Test adding a package to the registry."""
        with patch("codegraph_hub.registry.CONFIG_DIR", temp_config_dir):
            with patch("codegraph_hub.registry.CONFIG_FILE", temp_config_dir / "packages.json"):
                registry = Registry()
                pkg = registry.add("mylib", str(temp_project_dir), "My library")
                assert pkg.name == "mylib"
                assert pkg.description == "My library"

    def test_registry_add_nonexistent_path_raises_error(self, temp_config_dir):
        """Test adding a package with non-existent path raises ValueError."""
        with patch("codegraph_hub.registry.CONFIG_DIR", temp_config_dir):
            with patch("codegraph_hub.registry.CONFIG_FILE", temp_config_dir / "packages.json"):
                registry = Registry()
                with pytest.raises(ValueError, match="Path does not exist"):
                    registry.add("mylib", "/nonexistent/path", "My library")

    def test_registry_get_package(self, temp_config_dir, temp_project_dir):
        """Test retrieving a package from the registry."""
        with patch("codegraph_hub.registry.CONFIG_DIR", temp_config_dir):
            with patch("codegraph_hub.registry.CONFIG_FILE", temp_config_dir / "packages.json"):
                registry = Registry()
                registry.add("mylib", str(temp_project_dir), "My library")
                pkg = registry.get("mylib")
                assert pkg is not None
                assert pkg.name == "mylib"

    def test_registry_get_nonexistent_package_returns_none(self, temp_config_dir):
        """Test getting a non-existent package returns None."""
        with patch("codegraph_hub.registry.CONFIG_DIR", temp_config_dir):
            with patch("codegraph_hub.registry.CONFIG_FILE", temp_config_dir / "packages.json"):
                registry = Registry()
                pkg = registry.get("nonexistent")
                assert pkg is None

    def test_registry_list_all(self, temp_config_dir, temp_project_dir):
        """Test listing all packages in the registry."""
        with patch("codegraph_hub.registry.CONFIG_DIR", temp_config_dir):
            with patch("codegraph_hub.registry.CONFIG_FILE", temp_config_dir / "packages.json"):
                registry = Registry()
                registry.add("lib1", str(temp_project_dir), "Library 1")
                registry.add("lib2", str(temp_project_dir), "Library 2")
                packages = registry.list_all()
                assert len(packages) == 2
                assert any(p.name == "lib1" for p in packages)
                assert any(p.name == "lib2" for p in packages)

    def test_registry_remove_package(self, temp_config_dir, temp_project_dir):
        """Test removing a package from the registry."""
        with patch("codegraph_hub.registry.CONFIG_DIR", temp_config_dir):
            with patch("codegraph_hub.registry.CONFIG_FILE", temp_config_dir / "packages.json"):
                registry = Registry()
                registry.add("mylib", str(temp_project_dir), "My library")
                result = registry.remove("mylib")
                assert result is True
                assert registry.get("mylib") is None

    def test_registry_remove_nonexistent_package_returns_false(self, temp_config_dir):
        """Test removing a non-existent package returns False."""
        with patch("codegraph_hub.registry.CONFIG_DIR", temp_config_dir):
            with patch("codegraph_hub.registry.CONFIG_FILE", temp_config_dir / "packages.json"):
                registry = Registry()
                result = registry.remove("nonexistent")
                assert result is False

    def test_registry_persistence(self, temp_config_dir, temp_project_dir):
        """Test that registry saves and loads from disk."""
        config_file = temp_config_dir / "packages.json"
        with patch("codegraph_hub.registry.CONFIG_DIR", temp_config_dir):
            with patch("codegraph_hub.registry.CONFIG_FILE", config_file):
                # Create and populate registry
                registry1 = Registry()
                registry1.add("mylib", str(temp_project_dir), "My library")

                # Create new registry instance and verify data persists
                registry2 = Registry()
                pkg = registry2.get("mylib")
                assert pkg is not None
                assert pkg.name == "mylib"
