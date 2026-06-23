"""Functional tests for the sempkg MCP server.

These tests drive ``sempkg mcp`` as a subprocess and speak JSON-RPC 2.0 over
its stdin/stdout — the same transport a real MCP client uses.  No agent or
intermediate framework is involved.

Prerequisites
-------------
- The sempkg release binary must be built::

    cargo build --release --manifest-path src/sempkg/Cargo.toml

- The ``codegraph`` bundle must be installed with ``--include-source`` in a
  workspace whose path is given by the ``SEMPKG_WORKSPACE`` env variable
  (default: the project root containing ``sempkg.toml``).

- The bundle must have been built with the codegraph-DB-aware sembundle so
  that ``read_symbol`` / ``read_code`` use aligned line ranges.  Rebuild with::

    sempkg refresh codegraph --workspace <workspace>
    # or, to install fresh in CI:
    sempkg add colbymchenry/codegraph@v0.9.7 \\
        --include-source --source-dir src/ --docs-dir docs/ \\
        --workspace <workspace> --reinstall

Ground truth
------------
All ``HANDLE_NODE_*`` constants are verified against a live codegraph@0.9.7
index.  If the constants go stale (upstream changes its source), update them
from the output of::

    sempkg search codegraph --query handleNode
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Generator

import pytest

# ---------------------------------------------------------------------------
# Ground-truth constants for codegraph@0.9.7
# ---------------------------------------------------------------------------

CODEGRAPH_PKG = "codegraph"

# The ToolHandler::handleNode method in mcp/tools.ts.
HANDLE_NODE_FILE = "mcp/tools.ts"
HANDLE_NODE_QUALIFIED = "ToolHandler::handleNode"
HANDLE_NODE_SHORT = "handleNode"
HANDLE_NODE_START = 2558
HANDLE_NODE_END = 2592
HANDLE_NODE_SPAN = HANDLE_NODE_END - HANDLE_NODE_START + 1  # 35 lines

# Marker strings that must appear inside the method body.
HANDLE_NODE_BODY_MARKERS = ["handleNode", "codegraph_node"]

# ---------------------------------------------------------------------------
# MCP client helper
# ---------------------------------------------------------------------------


class McpClient:
    """Minimal synchronous JSON-RPC 2.0 client over a subprocess's stdio.

    The server speaks newline-delimited JSON.  We write one request per line
    and read one response line back before sending the next, which is safe for
    the request-response tools exposed by ``sempkg mcp``.
    """

    def __init__(self, proc: subprocess.Popen, timeout: float = 30.0) -> None:
        self._proc = proc
        self._timeout = timeout
        self._id = 0

    # ------------------------------------------------------------------
    # Core transport
    # ------------------------------------------------------------------

    def _next_id(self) -> int:
        self._id += 1
        return self._id

    def send(self, method: str, params: dict | None = None) -> dict:
        """Send one JSON-RPC request and return the parsed response object."""
        msg = {
            "jsonrpc": "2.0",
            "id": self._next_id(),
            "method": method,
            "params": params or {},
        }
        assert self._proc.stdin is not None, "stdin closed"
        self._proc.stdin.write(json.dumps(msg) + "\n")
        self._proc.stdin.flush()

        assert self._proc.stdout is not None, "stdout closed"
        line = self._proc.stdout.readline()
        if not line:
            rc = self._proc.poll()
            raise RuntimeError(
                f"MCP server closed stdout unexpectedly (exit code {rc})"
            )
        return json.loads(line)

    # ------------------------------------------------------------------
    # Tool helpers
    # ------------------------------------------------------------------

    def call_tool(self, name: str, arguments: dict) -> dict:
        """Send a ``tools/call`` request and return the raw response dict."""
        return self.send("tools/call", {"name": name, "arguments": arguments})

    def tool_text(self, name: str, arguments: dict) -> str:
        """Call a tool and return the first text-content block as a string.

        Raises AssertionError if the call returns an error or empty content.
        """
        resp = self.call_tool(name, arguments)
        assert "error" not in resp, (
            f"tool '{name}' returned JSON-RPC error: {resp['error']}"
        )
        assert "result" in resp, f"unexpected response shape: {resp}"
        content = resp["result"].get("content", [])
        assert content, f"tool '{name}' returned empty content"
        return content[0]["text"]

    def tool_json(self, name: str, arguments: dict) -> object:
        """Like ``tool_text`` but JSON-decode the text payload."""
        return json.loads(self.tool_text(name, arguments))

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    def close(self) -> None:
        if self._proc.stdin:
            self._proc.stdin.close()
        try:
            self._proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self._proc.kill()


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


def _find_binary() -> str:
    """Return the path to the sempkg release (or debug) binary, or ''."""
    root = Path(__file__).parent.parent
    candidates = [
        root / "src" / "sempkg" / "target" / "release" / "sempkg",
        root / "src" / "sempkg" / "target" / "release" / "sempkg.exe",
        root / "src" / "sempkg" / "target" / "debug" / "sempkg",
        root / "src" / "sempkg" / "target" / "debug" / "sempkg.exe",
    ]
    for p in candidates:
        if p.is_file():
            return str(p)
    return shutil.which("sempkg") or ""


@pytest.fixture(scope="session")
def sempkg_bin() -> str:
    path = _find_binary()
    if not path:
        pytest.skip(
            "sempkg binary not found — run `cargo build --release "
            "--manifest-path src/sempkg/Cargo.toml`"
        )
    return path


@pytest.fixture(scope="session")
def workspace_dir() -> Path:
    """Workspace directory whose bundles will be queried.

    Controlled by the ``SEMPKG_WORKSPACE`` env variable; defaults to the
    project root (which contains ``sempkg.toml``).
    """
    env = os.environ.get("SEMPKG_WORKSPACE")
    if env:
        return Path(env)
    return Path(__file__).parent.parent


@pytest.fixture(scope="session")
def mcp_client(sempkg_bin: str, workspace_dir: Path) -> Generator[McpClient, None, None]:
    """Start a sempkg MCP server session and perform the JSON-RPC handshake.

    The session is shared across all tests in the session for speed.
    """
    proc = subprocess.Popen(
        [sempkg_bin, "mcp", "--workspace", str(workspace_dir)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        bufsize=1,
    )
    client = McpClient(proc)
    resp = client.send(
        "initialize",
        {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "pytest-functional", "version": "0"},
        },
    )
    assert "result" in resp, f"MCP initialize failed: {resp}"
    yield client
    client.close()


# ---------------------------------------------------------------------------
# CLI smoke tests
# ---------------------------------------------------------------------------


@pytest.mark.functional
class TestCliSmoke:
    """Basic CLI sanity checks — the binary runs and the workspace is populated."""

    def test_list_exits_zero(self, sempkg_bin: str, workspace_dir: Path) -> None:
        result = subprocess.run(
            [sempkg_bin, "list", "--workspace", str(workspace_dir)],
            capture_output=True,
            text=True,
            timeout=15,
        )
        assert result.returncode == 0, f"sempkg list failed:\n{result.stderr}"

    def test_list_contains_codegraph(self, sempkg_bin: str, workspace_dir: Path) -> None:
        result = subprocess.run(
            [sempkg_bin, "list", "--workspace", str(workspace_dir)],
            capture_output=True,
            text=True,
            timeout=15,
        )
        assert CODEGRAPH_PKG in result.stdout, (
            f"codegraph not listed — run 'sempkg sync' or 'sempkg add' first.\n"
            f"Output:\n{result.stdout}"
        )

    def test_codegraph_has_code_index(self, sempkg_bin: str, workspace_dir: Path) -> None:
        result = subprocess.run(
            [sempkg_bin, "list", "--workspace", str(workspace_dir)],
            capture_output=True,
            text=True,
            timeout=15,
        )
        cg_line = next(
            (l for l in result.stdout.splitlines() if CODEGRAPH_PKG in l), ""
        )
        assert "+code" in cg_line, (
            f"codegraph bundle missing +code index — rebuild with --include-source.\n"
            f"Line: {cg_line!r}"
        )

    def test_codegraph_is_indexed(self, sempkg_bin: str, workspace_dir: Path) -> None:
        result = subprocess.run(
            [sempkg_bin, "list", "--workspace", str(workspace_dir)],
            capture_output=True,
            text=True,
            timeout=15,
        )
        cg_line = next(
            (l for l in result.stdout.splitlines() if CODEGRAPH_PKG in l), ""
        )
        assert "[indexed]" in cg_line, (
            f"codegraph bundle not codegraph-indexed.\nLine: {cg_line!r}"
        )


# ---------------------------------------------------------------------------
# MCP — initialize
# ---------------------------------------------------------------------------


@pytest.mark.functional
class TestMcpHandshake:
    def test_server_info_present(self, mcp_client: McpClient) -> None:
        # The session fixture already called initialize; re-send with a new id
        # to verify the server keeps handling requests correctly.
        resp = mcp_client.send(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "pytest-recheck", "version": "0"},
            },
        )
        assert resp["result"]["serverInfo"]["name"] == "sempkg"

    def test_tools_list_includes_read_symbol(self, mcp_client: McpClient) -> None:
        resp = mcp_client.send("tools/list", {})
        tools = [t["name"] for t in resp["result"]["tools"]]
        assert "read_symbol" in tools
        assert "read_code" in tools
        assert "search_symbols" in tools


# ---------------------------------------------------------------------------
# MCP — search_symbols
# ---------------------------------------------------------------------------


@pytest.mark.functional
class TestSearchSymbols:
    """search_symbols returns accurate, codegraph-DB-sourced symbol locations."""

    def test_handleNode_found(self, mcp_client: McpClient) -> None:
        data = mcp_client.tool_json(
            "search_symbols",
            {"package": CODEGRAPH_PKG, "query": HANDLE_NODE_SHORT},
        )
        assert isinstance(data, list) and len(data) >= 1, (
            f"Expected ≥1 results for '{HANDLE_NODE_SHORT}', got: {data}"
        )
        assert data[0]["node"]["name"] == HANDLE_NODE_SHORT

    def test_handleNode_file_path(self, mcp_client: McpClient) -> None:
        data = mcp_client.tool_json(
            "search_symbols",
            {"package": CODEGRAPH_PKG, "query": HANDLE_NODE_SHORT},
        )
        assert data[0]["node"]["filePath"] == HANDLE_NODE_FILE, (
            f"filePath mismatch: {data[0]['node']['filePath']!r} != {HANDLE_NODE_FILE!r}"
        )

    def test_handleNode_start_line(self, mcp_client: McpClient) -> None:
        data = mcp_client.tool_json(
            "search_symbols",
            {"package": CODEGRAPH_PKG, "query": HANDLE_NODE_SHORT},
        )
        node = data[0]["node"]
        assert node["startLine"] == HANDLE_NODE_START, (
            f"startLine {node['startLine']} != expected {HANDLE_NODE_START}"
        )

    def test_handleNode_end_line(self, mcp_client: McpClient) -> None:
        data = mcp_client.tool_json(
            "search_symbols",
            {"package": CODEGRAPH_PKG, "query": HANDLE_NODE_SHORT},
        )
        node = data[0]["node"]
        assert node["endLine"] == HANDLE_NODE_END, (
            f"endLine {node['endLine']} != expected {HANDLE_NODE_END}"
        )

    def test_handleNode_qualified_name(self, mcp_client: McpClient) -> None:
        data = mcp_client.tool_json(
            "search_symbols",
            {"package": CODEGRAPH_PKG, "query": HANDLE_NODE_SHORT},
        )
        names = [n["node"].get("qualifiedName", "") for n in data]
        assert HANDLE_NODE_QUALIFIED in names, (
            f"'{HANDLE_NODE_QUALIFIED}' not in results: {names}"
        )


# ---------------------------------------------------------------------------
# MCP — read_symbol
# ---------------------------------------------------------------------------


def _extract_code_block(text: str) -> str:
    """Return the content between the first ``` fences in *text*."""
    parts = text.split("```")
    # parts[0] = header, parts[1] = code body, parts[2] = ''
    assert len(parts) >= 3, f"No fenced code block found in:\n{text[:300]}"
    return parts[1].strip()


@pytest.mark.functional
class TestReadSymbol:
    """read_symbol fetches the precise body of a named symbol from the code index."""

    def test_qualified_name_returns_content(self, mcp_client: McpClient) -> None:
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": HANDLE_NODE_QUALIFIED},
        )
        assert "not found in the code index" not in text.lower(), (
            f"read_symbol reported symbol not found (bundle may need rebuild):\n{text}"
        )

    def test_result_header_contains_symbol(self, mcp_client: McpClient) -> None:
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": HANDLE_NODE_QUALIFIED},
        )
        # Header format: **{symbol}** ({kind}) @ {path}:{start}-{end}
        assert f"**{HANDLE_NODE_QUALIFIED}**" in text or f"**{HANDLE_NODE_SHORT}**" in text, (
            f"Symbol name missing from header:\n{text[:200]}"
        )

    def test_result_header_contains_location(self, mcp_client: McpClient) -> None:
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": HANDLE_NODE_QUALIFIED},
        )
        assert HANDLE_NODE_FILE in text, (
            f"File path '{HANDLE_NODE_FILE}' missing from result header:\n{text[:200]}"
        )
        assert str(HANDLE_NODE_START) in text, (
            f"start_line {HANDLE_NODE_START} missing from result header:\n{text[:200]}"
        )

    def test_body_contains_expected_markers(self, mcp_client: McpClient) -> None:
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": HANDLE_NODE_QUALIFIED},
        )
        body = _extract_code_block(text)
        for marker in HANDLE_NODE_BODY_MARKERS:
            assert marker in body, (
                f"Expected marker '{marker}' not in code body:\n{body[:400]}"
            )

    def test_body_not_whole_file(self, mcp_client: McpClient) -> None:
        """The returned body must be scoped to the method, not the entire file."""
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": HANDLE_NODE_QUALIFIED},
        )
        body = _extract_code_block(text)
        # Allow generous headroom (3× span) to accommodate sub-chunk headers,
        # but reject whole-file blobs which would be thousands of lines.
        max_lines = HANDLE_NODE_SPAN * 3
        actual_lines = len(body.splitlines())
        assert actual_lines <= max_lines, (
            f"read_symbol returned {actual_lines} lines — expected ≤{max_lines}. "
            f"Possible whole-file bleed (bundle built with old sembundle?)."
        )

    def test_short_name_resolves(self, mcp_client: McpClient) -> None:
        """Short name 'handleNode' should fall back to a match."""
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": HANDLE_NODE_SHORT},
        )
        assert "not found in the code index" not in text.lower(), (
            f"Short-name lookup failed:\n{text}"
        )
        assert HANDLE_NODE_SHORT in text


# ---------------------------------------------------------------------------
# MCP — read_code
# ---------------------------------------------------------------------------


@pytest.mark.functional
class TestReadCode:
    """read_code fetches the symbol that encloses a known file:line pair."""

    def test_exact_start_line(self, mcp_client: McpClient) -> None:
        text = mcp_client.tool_text(
            "read_code",
            {
                "package": CODEGRAPH_PKG,
                "file": HANDLE_NODE_FILE,
                "line": HANDLE_NODE_START,
            },
        )
        assert "no symbol found covering" not in text.lower(), (
            f"read_code at start line reported not found:\n{text}"
        )
        assert HANDLE_NODE_SHORT in text

    def test_mid_body_line_resolves_same_method(self, mcp_client: McpClient) -> None:
        """Any line inside the method body must resolve to handleNode."""
        mid = (HANDLE_NODE_START + HANDLE_NODE_END) // 2
        text = mcp_client.tool_text(
            "read_code",
            {"package": CODEGRAPH_PKG, "file": HANDLE_NODE_FILE, "line": mid},
        )
        assert "no symbol found covering" not in text.lower(), (
            f"read_code at mid-body line {mid} returned not-found:\n{text}"
        )
        assert HANDLE_NODE_SHORT in text

    def test_result_scoped_to_method(self, mcp_client: McpClient) -> None:
        text = mcp_client.tool_text(
            "read_code",
            {
                "package": CODEGRAPH_PKG,
                "file": HANDLE_NODE_FILE,
                "line": HANDLE_NODE_START,
            },
        )
        body = _extract_code_block(text)
        max_lines = HANDLE_NODE_SPAN * 3
        actual_lines = len(body.splitlines())
        assert actual_lines <= max_lines, (
            f"read_code returned {actual_lines} lines — expected ≤{max_lines}. "
            f"Possible whole-file bleed."
        )

    def test_read_code_matches_read_symbol(self, mcp_client: McpClient) -> None:
        """read_code and read_symbol must return the same source body."""
        by_name = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": HANDLE_NODE_QUALIFIED},
        )
        by_loc = mcp_client.tool_text(
            "read_code",
            {
                "package": CODEGRAPH_PKG,
                "file": HANDLE_NODE_FILE,
                "line": HANDLE_NODE_START,
            },
        )

        def _code_body(s: str) -> str:
            """Strip the markdown header and return just the fenced code."""
            return _extract_code_block(s)

        assert _code_body(by_name) == _code_body(by_loc), (
            "read_symbol and read_code returned different code bodies.\n"
            f"by_name body:\n{_code_body(by_name)[:300]}\n"
            f"by_loc  body:\n{_code_body(by_loc)[:300]}"
        )


# ---------------------------------------------------------------------------
# MCP — list_files
# ---------------------------------------------------------------------------

# Ground-truth constants for list_files against codegraph@0.9.7.
#
# The index contains 118 distinct tracked source files, all TypeScript.
# The mcp/ subdirectory holds 10 files, all named *.ts.
LIST_FILES_TOTAL = 118
LIST_FILES_MCP_COUNT = 10          # files whose path contains "mcp"
LIST_FILES_MCP_PREFIX = "mcp/"     # each matching file starts with this
LIST_FILES_KNOWN_FILE = "mcp/tools.ts"   # a well-known file in the index


@pytest.mark.functional
class TestListFiles:
    """list_files returns accurate file lists with substring/glob filtering and limit."""

    # ------------------------------------------------------------------
    # No filter — full listing
    # ------------------------------------------------------------------

    def test_no_filter_returns_files(self, mcp_client: McpClient) -> None:
        """Unfiltered call must return at least the known total."""
        text = mcp_client.tool_text(
            "list_files", {"package": CODEGRAPH_PKG}
        )
        lines = [l for l in text.splitlines() if l.strip()]
        assert len(lines) >= LIST_FILES_TOTAL, (
            f"Expected ≥{LIST_FILES_TOTAL} files unfiltered, got {len(lines)}:\n{text[:300]}"
        )

    def test_no_filter_contains_known_file(self, mcp_client: McpClient) -> None:
        text = mcp_client.tool_text(
            "list_files", {"package": CODEGRAPH_PKG}
        )
        assert LIST_FILES_KNOWN_FILE in text, (
            f"'{LIST_FILES_KNOWN_FILE}' missing from unfiltered listing:\n{text[:300]}"
        )

    def test_no_filter_does_not_start_with_no_files(self, mcp_client: McpClient) -> None:
        """Full listing must not be mistaken for a 'no matches' sentinel."""
        text = mcp_client.tool_text(
            "list_files", {"package": CODEGRAPH_PKG}
        )
        assert not text.startswith("No files"), (
            f"list_files (no filter) returned a 'no matches' sentinel:\n{text[:200]}"
        )

    # ------------------------------------------------------------------
    # Substring filter
    # ------------------------------------------------------------------

    def test_substring_filter_narrows_results(self, mcp_client: McpClient) -> None:
        """A substring filter should return only files whose path contains it."""
        text = mcp_client.tool_text(
            "list_files", {"package": CODEGRAPH_PKG, "filter": "mcp"}
        )
        lines = [l for l in text.splitlines() if l.strip()]
        assert len(lines) == LIST_FILES_MCP_COUNT, (
            f"Expected {LIST_FILES_MCP_COUNT} 'mcp' files, got {len(lines)}:\n{text}"
        )

    def test_substring_filter_all_lines_match(self, mcp_client: McpClient) -> None:
        text = mcp_client.tool_text(
            "list_files", {"package": CODEGRAPH_PKG, "filter": "mcp"}
        )
        for line in text.splitlines():
            if not line.strip():
                continue
            assert "mcp" in line.lower(), (
                f"Non-matching line in substring results: {line!r}"
            )

    def test_substring_filter_case_insensitive(self, mcp_client: McpClient) -> None:
        """Substring match should be case-insensitive."""
        lower = mcp_client.tool_text(
            "list_files", {"package": CODEGRAPH_PKG, "filter": "mcp"}
        )
        upper = mcp_client.tool_text(
            "list_files", {"package": CODEGRAPH_PKG, "filter": "MCP"}
        )
        assert lower == upper, (
            "Substring filter is case-sensitive — 'mcp' and 'MCP' returned different results"
        )

    def test_substring_filter_contains_known_file(self, mcp_client: McpClient) -> None:
        text = mcp_client.tool_text(
            "list_files", {"package": CODEGRAPH_PKG, "filter": "mcp"}
        )
        assert LIST_FILES_KNOWN_FILE in text, (
            f"Expected '{LIST_FILES_KNOWN_FILE}' in substring-filtered results:\n{text}"
        )

    # ------------------------------------------------------------------
    # Glob filter
    # ------------------------------------------------------------------

    def test_glob_filter_directory_wildcard(self, mcp_client: McpClient) -> None:
        """mcp/*.ts should match exactly the same files as the 'mcp' substring."""
        text = mcp_client.tool_text(
            "list_files", {"package": CODEGRAPH_PKG, "filter": "mcp/*.ts"}
        )
        lines = [l for l in text.splitlines() if l.strip()]
        assert len(lines) == LIST_FILES_MCP_COUNT, (
            f"Expected {LIST_FILES_MCP_COUNT} files for glob 'mcp/*.ts', got {len(lines)}:\n{text}"
        )

    def test_glob_filter_all_ts_files(self, mcp_client: McpClient) -> None:
        """**/*.ts should return all tracked files (all are TypeScript)."""
        text = mcp_client.tool_text(
            "list_files", {"package": CODEGRAPH_PKG, "filter": "**/*.ts"}
        )
        lines = [l for l in text.splitlines() if l.strip()]
        assert len(lines) >= LIST_FILES_TOTAL, (
            f"**/*.ts expected ≥{LIST_FILES_TOTAL} files, got {len(lines)}:\n{text[:300]}"
        )

    def test_glob_filter_extension_exclusion(self, mcp_client: McpClient) -> None:
        """**/*.rs should return zero results (no Rust files in codegraph)."""
        text = mcp_client.tool_text(
            "list_files", {"package": CODEGRAPH_PKG, "filter": "**/*.rs"}
        )
        assert text.startswith("No files matched"), (
            f"Expected 'No files matched' for **/*.rs glob, got:\n{text[:200]}"
        )

    def test_glob_filter_known_file_exact(self, mcp_client: McpClient) -> None:
        """An exact filename glob should return exactly one result."""
        text = mcp_client.tool_text(
            "list_files", {"package": CODEGRAPH_PKG, "filter": "mcp/tools.ts"}
        )
        lines = [l.strip() for l in text.splitlines() if l.strip()]
        assert lines == [LIST_FILES_KNOWN_FILE], (
            f"Expected exactly ['{LIST_FILES_KNOWN_FILE}'], got: {lines}"
        )

    # ------------------------------------------------------------------
    # Limit parameter
    # ------------------------------------------------------------------

    def test_limit_caps_output(self, mcp_client: McpClient) -> None:
        """limit=5 must return exactly 5 file lines plus a truncation notice."""
        text = mcp_client.tool_text(
            "list_files", {"package": CODEGRAPH_PKG, "limit": 5}
        )
        # Split off the truncation notice (contains "more file(s) not shown")
        content_lines = [
            l for l in text.splitlines()
            if l.strip() and "more file(s) not shown" not in l
        ]
        assert len(content_lines) == 5, (
            f"Expected 5 file lines with limit=5, got {len(content_lines)}:\n{text}"
        )

    def test_limit_truncation_notice_present(self, mcp_client: McpClient) -> None:
        text = mcp_client.tool_text(
            "list_files", {"package": CODEGRAPH_PKG, "limit": 5}
        )
        assert "more file(s) not shown" in text, (
            f"Truncation notice missing from limit=5 output:\n{text}"
        )

    def test_limit_truncation_notice_count_is_correct(self, mcp_client: McpClient) -> None:
        limit = 5
        text = mcp_client.tool_text(
            "list_files", {"package": CODEGRAPH_PKG, "limit": limit}
        )
        notice_line = next(
            (l for l in text.splitlines() if "more file(s) not shown" in l), ""
        )
        # Extract the leading number from "… N more file(s) not shown"
        import re
        m = re.search(r"(\d+) more file\(s\) not shown", notice_line)
        assert m, f"Could not parse truncation count from: {notice_line!r}"
        remaining = int(m.group(1))
        assert remaining == LIST_FILES_TOTAL - limit, (
            f"Truncation count {remaining} != expected {LIST_FILES_TOTAL - limit}"
        )

    def test_limit_larger_than_total_no_notice(self, mcp_client: McpClient) -> None:
        """A limit larger than the total file count must not add a truncation notice."""
        text = mcp_client.tool_text(
            "list_files", {"package": CODEGRAPH_PKG, "limit": 10_000}
        )
        assert "more file(s) not shown" not in text, (
            f"Unexpected truncation notice when limit exceeds total:\n{text[:300]}"
        )

    # ------------------------------------------------------------------
    # No-match sentinel — clearly distinguishable from filter errors
    # ------------------------------------------------------------------

    def test_no_match_returns_sentinel_message(self, mcp_client: McpClient) -> None:
        text = mcp_client.tool_text(
            "list_files",
            {"package": CODEGRAPH_PKG, "filter": "nonexistent_xyzzy_404"},
        )
        assert text.startswith("No files matched"), (
            f"Expected 'No files matched' sentinel, got:\n{text[:200]}"
        )

    def test_no_match_sentinel_includes_total_count(self, mcp_client: McpClient) -> None:
        """The no-match message must tell the agent how many files exist in total."""
        text = mcp_client.tool_text(
            "list_files",
            {"package": CODEGRAPH_PKG, "filter": "nonexistent_xyzzy_404"},
        )
        assert str(LIST_FILES_TOTAL) in text, (
            f"Total file count ({LIST_FILES_TOTAL}) missing from no-match sentinel:\n{text}"
        )

    def test_no_match_sentinel_includes_filter_value(self, mcp_client: McpClient) -> None:
        pat = "nonexistent_xyzzy_404"
        text = mcp_client.tool_text(
            "list_files", {"package": CODEGRAPH_PKG, "filter": pat}
        )
        assert pat in text, (
            f"Filter value '{pat}' missing from no-match message:\n{text}"
        )

    def test_no_match_does_not_say_filter_error(self, mcp_client: McpClient) -> None:
        """A valid filter with zero matches must NOT be reported as a filter error."""
        text = mcp_client.tool_text(
            "list_files",
            {"package": CODEGRAPH_PKG, "filter": "nonexistent_xyzzy_404"},
        )
        assert not text.startswith("Filter error"), (
            f"Valid filter reported as syntax error:\n{text[:200]}"
        )

    # ------------------------------------------------------------------
    # Invalid glob sentinel — clearly distinguishable from no matches
    # ------------------------------------------------------------------

    def test_invalid_glob_returns_filter_error_sentinel(self, mcp_client: McpClient) -> None:
        """A syntactically broken glob (contains * but has unclosed bracket) must
        return 'Filter error: …', not 'No files matched'."""
        text = mcp_client.tool_text(
            "list_files",
            {"package": CODEGRAPH_PKG, "filter": "**/[unclosed"},
        )
        assert text.startswith("Filter error"), (
            f"Expected 'Filter error' sentinel for invalid glob, got:\n{text[:200]}"
        )

    def test_invalid_glob_does_not_say_no_files_matched(self, mcp_client: McpClient) -> None:
        text = mcp_client.tool_text(
            "list_files",
            {"package": CODEGRAPH_PKG, "filter": "**/[unclosed"},
        )
        assert not text.startswith("No files matched"), (
            f"Invalid glob sentinel must not look like a no-match result:\n{text[:200]}"
        )
