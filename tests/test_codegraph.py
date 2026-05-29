"""Unit tests for the codegraph wrapper module."""

import tempfile
from pathlib import Path
from unittest.mock import patch, MagicMock

import pytest

from codegraph_hub.codegraph import (
    SymbolLocation,
    find_symbols,
    read_symbol_source,
    _db_path,
)


@pytest.fixture
def temp_project_dir():
    """Provide a temporary project directory for testing."""
    with tempfile.TemporaryDirectory() as tmpdir:
        yield Path(tmpdir)


@pytest.fixture
def db_project_dir():
    """Provide a temporary project directory for database tests with proper cleanup."""
    import gc
    with tempfile.TemporaryDirectory() as tmpdir:
        yield Path(tmpdir)
        # Force garbage collection to release database file handles on Windows
        gc.collect()


class TestSymbolLocation:
    """Test the SymbolLocation dataclass."""

    def test_symbol_location_creation(self):
        """Test creating a SymbolLocation."""
        loc = SymbolLocation(
            name="my_function",
            qualified_name="module.my_function",
            kind="function",
            file_path="src/module.py",
            start_line=10,
            end_line=20,
            signature="def my_function(x, y) -> int:",
            docstring="A test function",
            language="python",
        )
        assert loc.name == "my_function"
        assert loc.qualified_name == "module.my_function"
        assert loc.kind == "function"
        assert loc.file_path == "src/module.py"
        assert loc.start_line == 10
        assert loc.end_line == 20
        assert loc.signature == "def my_function(x, y) -> int:"
        assert loc.docstring == "A test function"
        assert loc.language == "python"


class TestCodegraphFunctions:
    """Test codegraph wrapper functions."""

    def test_db_path(self, temp_project_dir):
        """Test _db_path returns correct path."""
        expected = temp_project_dir / ".codegraph" / "codegraph.db"
        assert _db_path(temp_project_dir) == expected

    def test_find_symbols_empty_when_no_db(self, temp_project_dir):
        """Test find_symbols returns empty list when no DB exists."""
        symbols = find_symbols(temp_project_dir, "test")
        assert symbols == []

    def test_read_symbol_source_with_existing_file(self, temp_project_dir):
        """Test read_symbol_source reads file contents."""
        # Create a test file
        src_dir = temp_project_dir / "src"
        src_dir.mkdir()
        test_file = src_dir / "module.py"
        test_file.write_text(
            "def function_a():\n"
            "    return 1\n"
            "\n"
            "def function_b():\n"
            "    return 2\n",
            encoding="utf-8",
        )

        symbol = SymbolLocation(
            name="function_a",
            qualified_name="module.function_a",
            kind="function",
            file_path=str(test_file),
            start_line=1,
            end_line=2,
            signature="def function_a():",
            docstring=None,
            language="python",
        )

        result = read_symbol_source(temp_project_dir, symbol)
        assert "function_a" in result
        assert "return 1" in result

    def test_read_symbol_source_with_context_lines(self, temp_project_dir):
        """Test read_symbol_source includes context lines."""
        # Create a test file
        src_dir = temp_project_dir / "src"
        src_dir.mkdir()
        test_file = src_dir / "module.py"
        test_file.write_text(
            "# Line 1\n"
            "# Line 2\n"
            "def target():\n"
            "    return 1\n"
            "# Line 5\n",
            encoding="utf-8",
        )

        symbol = SymbolLocation(
            name="target",
            qualified_name="module.target",
            kind="function",
            file_path=str(test_file),
            start_line=3,
            end_line=4,
            signature="def target():",
            docstring=None,
            language="python",
        )

        result = read_symbol_source(temp_project_dir, symbol, context_lines=1)
        # Should include line 2 (context before), lines 3-4 (symbol), line 5 (context after)
        assert "Line 2" in result
        assert "target" in result
        assert "Line 5" in result

    def test_read_symbol_source_missing_file(self, temp_project_dir):
        """Test read_symbol_source handles missing files gracefully."""
        symbol = SymbolLocation(
            name="test",
            qualified_name="module.test",
            kind="function",
            file_path="/nonexistent/file.py",
            start_line=1,
            end_line=1,
            signature=None,
            docstring=None,
            language="python",
        )

        result = read_symbol_source(temp_project_dir, symbol)
        assert "not found" in result
