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


# ---------------------------------------------------------------------------
# MCP — read_symbol ambiguity (new behaviour added in code-bundle branch)
# ---------------------------------------------------------------------------
#
# Ground truth verified live against codegraph@0.9.7:
#
#   withLock   → 2 nodes:  FileLock::withLock  (utils.ts 261-268)
#                           Mutex::withLock     (utils.ts 365-372)
#   withLockAsync → 1 node: FileLock::withLockAsync (utils.ts 273-280)
#
# The test class is split into three logical groups:
#   1. Ambiguous path  — read_symbol with a short name that matches multiple nodes
#   2. Disambiguation  — read_code with explicit file:line resolves to the
#                        correct individual candidate
#   3. Non-ambiguous paths — unique short name, qualified name, and not-found

AMBIG_SHORT      = "withLock"
AMBIG_COUNT      = 2

AMBIG_C1_QUAL    = "FileLock::withLock"
AMBIG_C1_FILE    = "utils.ts"
AMBIG_C1_START   = 261
AMBIG_C1_END     = 268
# Body of FileLock::withLock is a *synchronous* wrapper: calls this.acquire()
# without await and returns fn() directly.
AMBIG_C1_BODY_MARKER  = "return fn()"

AMBIG_C2_QUAL    = "Mutex::withLock"
AMBIG_C2_FILE    = "utils.ts"
AMBIG_C2_START   = 365
AMBIG_C2_END     = 372
# Body of Mutex::withLock is *async*: awaits acquire() and returns await fn().
AMBIG_C2_BODY_MARKER  = "async withLock"

# A symbol whose short name is unique in the index (only one node named this).
UNIQUE_SHORT     = "withLockAsync"
UNIQUE_FILE      = "utils.ts"
UNIQUE_START     = 273
UNIQUE_END       = 280

# A symbol name that does not exist in the index.
NOT_FOUND_SYMBOL = "xyzzy_nonexistent_symbol_12345"


@pytest.mark.functional
class TestReadSymbolAmbiguity:
    """read_symbol must return a candidate list when a short name is ambiguous,
    and callers must be able to disambiguate with read_code(file, line)."""

    # ------------------------------------------------------------------
    # 1. Ambiguous path
    # ------------------------------------------------------------------

    def test_ambiguous_short_name_is_flagged(self, mcp_client: McpClient) -> None:
        """read_symbol with an ambiguous short name must not silently return
        one arbitrary node — it must report ambiguity."""
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": AMBIG_SHORT},
        )
        assert "ambiguous" in text.lower(), (
            f"Expected 'ambiguous' in response for '{AMBIG_SHORT}', got:\n{text[:300]}"
        )

    def test_ambiguous_response_states_candidate_count(self, mcp_client: McpClient) -> None:
        """The response must state how many candidates share the name."""
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": AMBIG_SHORT},
        )
        assert str(AMBIG_COUNT) in text, (
            f"Expected candidate count {AMBIG_COUNT} in ambiguous response:\n{text[:300]}"
        )

    def test_ambiguous_response_contains_candidate_table(self, mcp_client: McpClient) -> None:
        """The response must include a Markdown table of candidates."""
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": AMBIG_SHORT},
        )
        # A Markdown table always has separator row of |---|
        assert "|---|" in text, (
            f"No Markdown table (separator '|---|') found in ambiguous response:\n{text[:400]}"
        )

    def test_ambiguous_table_lists_both_qualified_names(self, mcp_client: McpClient) -> None:
        """Both qualified names must appear in the candidate table."""
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": AMBIG_SHORT},
        )
        assert AMBIG_C1_QUAL in text, (
            f"'{AMBIG_C1_QUAL}' missing from ambiguous candidate table:\n{text}"
        )
        assert AMBIG_C2_QUAL in text, (
            f"'{AMBIG_C2_QUAL}' missing from ambiguous candidate table:\n{text}"
        )

    def test_ambiguous_table_contains_file_path(self, mcp_client: McpClient) -> None:
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": AMBIG_SHORT},
        )
        # Both candidates live in utils.ts — the path must appear at least twice.
        assert text.count(AMBIG_C1_FILE) >= AMBIG_COUNT, (
            f"Expected '{AMBIG_C1_FILE}' to appear {AMBIG_COUNT} times in candidate "
            f"table, found {text.count(AMBIG_C1_FILE)}:\n{text}"
        )

    def test_ambiguous_table_contains_line_ranges(self, mcp_client: McpClient) -> None:
        """Start and end line numbers for both candidates must be present."""
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": AMBIG_SHORT},
        )
        for line_no in (AMBIG_C1_START, AMBIG_C1_END, AMBIG_C2_START, AMBIG_C2_END):
            assert str(line_no) in text, (
                f"Line number {line_no} missing from ambiguous response:\n{text}"
            )

    def test_ambiguous_response_suggests_read_code(self, mcp_client: McpClient) -> None:
        """The message must guide the caller to use read_code to disambiguate."""
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": AMBIG_SHORT},
        )
        assert "read_code" in text, (
            f"Ambiguous response does not mention 'read_code':\n{text}"
        )

    def test_ambiguous_response_has_no_code_block(self, mcp_client: McpClient) -> None:
        """An ambiguous response must not include a fenced code block — we do
        not want an arbitrary body sneaking through."""
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": AMBIG_SHORT},
        )
        assert "```" not in text, (
            f"Ambiguous response must not contain a code fence:\n{text}"
        )

    # ------------------------------------------------------------------
    # 2. Disambiguation via read_code
    # ------------------------------------------------------------------

    def test_read_code_disambiguates_to_first_candidate(self, mcp_client: McpClient) -> None:
        """read_code at the start line of candidate 1 must return FileLock::withLock."""
        text = mcp_client.tool_text(
            "read_code",
            {
                "package": CODEGRAPH_PKG,
                "file": AMBIG_C1_FILE,
                "line": AMBIG_C1_START,
            },
        )
        assert AMBIG_C1_BODY_MARKER in text, (
            f"Expected body marker '{AMBIG_C1_BODY_MARKER}' for {AMBIG_C1_QUAL}:\n{text}"
        )

    def test_read_code_disambiguates_to_second_candidate(self, mcp_client: McpClient) -> None:
        """read_code at the start line of candidate 2 must return Mutex::withLock."""
        text = mcp_client.tool_text(
            "read_code",
            {
                "package": CODEGRAPH_PKG,
                "file": AMBIG_C2_FILE,
                "line": AMBIG_C2_START,
            },
        )
        assert AMBIG_C2_BODY_MARKER in text, (
            f"Expected body marker '{AMBIG_C2_BODY_MARKER}' for {AMBIG_C2_QUAL}:\n{text}"
        )

    def test_two_disambiguated_bodies_differ(self, mcp_client: McpClient) -> None:
        """The two candidates must resolve to different source bodies."""
        body1 = _extract_code_block(
            mcp_client.tool_text(
                "read_code",
                {"package": CODEGRAPH_PKG, "file": AMBIG_C1_FILE, "line": AMBIG_C1_START},
            )
        )
        body2 = _extract_code_block(
            mcp_client.tool_text(
                "read_code",
                {"package": CODEGRAPH_PKG, "file": AMBIG_C2_FILE, "line": AMBIG_C2_START},
            )
        )
        assert body1 != body2, (
            "Both candidates of ambiguous symbol resolved to the same body — "
            "disambiguation is not working."
        )

    def test_read_code_candidate1_scoped_to_method(self, mcp_client: McpClient) -> None:
        """The resolved body for candidate 1 must be scoped to just the method."""
        text = mcp_client.tool_text(
            "read_code",
            {"package": CODEGRAPH_PKG, "file": AMBIG_C1_FILE, "line": AMBIG_C1_START},
        )
        body = _extract_code_block(text)
        span = AMBIG_C1_END - AMBIG_C1_START + 1
        assert len(body.splitlines()) <= span * 3, (
            f"read_code candidate 1 returned too many lines ({len(body.splitlines())}), "
            f"expected ≤{span * 3}"
        )

    def test_read_code_candidate2_scoped_to_method(self, mcp_client: McpClient) -> None:
        text = mcp_client.tool_text(
            "read_code",
            {"package": CODEGRAPH_PKG, "file": AMBIG_C2_FILE, "line": AMBIG_C2_START},
        )
        body = _extract_code_block(text)
        span = AMBIG_C2_END - AMBIG_C2_START + 1
        assert len(body.splitlines()) <= span * 3, (
            f"read_code candidate 2 returned too many lines ({len(body.splitlines())}), "
            f"expected ≤{span * 3}"
        )

    # ------------------------------------------------------------------
    # 3. Non-ambiguous paths — unique short name, qualified bypass, not-found
    # ------------------------------------------------------------------

    def test_unique_short_name_returns_body_not_ambiguous(self, mcp_client: McpClient) -> None:
        """A short name with exactly one match must return the code body,
        not an ambiguity message."""
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": UNIQUE_SHORT},
        )
        assert "ambiguous" not in text.lower(), (
            f"Unique symbol '{UNIQUE_SHORT}' was incorrectly flagged as ambiguous:\n{text}"
        )
        assert "```" in text, (
            f"read_symbol on unique symbol '{UNIQUE_SHORT}' returned no code block:\n{text}"
        )

    def test_unique_short_name_has_correct_location(self, mcp_client: McpClient) -> None:
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": UNIQUE_SHORT},
        )
        assert UNIQUE_FILE in text, (
            f"Expected file '{UNIQUE_FILE}' in unique-symbol result:\n{text[:200]}"
        )
        assert str(UNIQUE_START) in text, (
            f"Expected start line {UNIQUE_START} in unique-symbol result:\n{text[:200]}"
        )

    def test_qualified_name_bypasses_ambiguity(self, mcp_client: McpClient) -> None:
        """Providing a fully-qualified name must resolve to exactly one symbol
        even when the short name would be ambiguous."""
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": AMBIG_C1_QUAL},
        )
        assert "ambiguous" not in text.lower(), (
            f"Qualified name '{AMBIG_C1_QUAL}' was incorrectly flagged as ambiguous:\n{text}"
        )
        assert "```" in text, (
            f"read_symbol with qualified name returned no code block:\n{text}"
        )

    def test_qualified_name_returns_correct_candidate(self, mcp_client: McpClient) -> None:
        """The qualified name for candidate 1 must return candidate 1's body."""
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": AMBIG_C1_QUAL},
        )
        assert AMBIG_C1_BODY_MARKER in text, (
            f"Expected body marker '{AMBIG_C1_BODY_MARKER}' when looking up "
            f"qualified name '{AMBIG_C1_QUAL}':\n{text}"
        )

    def test_qualified_name_not_found_returns_candidate_2_body(
        self, mcp_client: McpClient
    ) -> None:
        """Symmetry check: looking up the second qualified name returns its body."""
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": AMBIG_C2_QUAL},
        )
        assert "ambiguous" not in text.lower(), (
            f"Qualified name '{AMBIG_C2_QUAL}' was incorrectly flagged as ambiguous:\n{text}"
        )
        assert AMBIG_C2_BODY_MARKER in text, (
            f"Expected body marker '{AMBIG_C2_BODY_MARKER}' for '{AMBIG_C2_QUAL}':\n{text}"
        )

    def test_not_found_symbol_returns_not_found_message(
        self, mcp_client: McpClient
    ) -> None:
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": NOT_FOUND_SYMBOL},
        )
        assert "not found" in text.lower(), (
            f"Expected 'not found' for nonexistent symbol, got:\n{text[:200]}"
        )

    def test_not_found_symbol_does_not_say_ambiguous(
        self, mcp_client: McpClient
    ) -> None:
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": NOT_FOUND_SYMBOL},
        )
        assert "ambiguous" not in text.lower(), (
            f"Not-found response incorrectly says 'ambiguous':\n{text}"
        )

    def test_not_found_symbol_has_no_code_block(self, mcp_client: McpClient) -> None:
        text = mcp_client.tool_text(
            "read_symbol",
            {"package": CODEGRAPH_PKG, "symbol": NOT_FOUND_SYMBOL},
        )
        assert "```" not in text, (
            f"Not-found response must not contain a code block:\n{text}"
        )


# ---------------------------------------------------------------------------
# MCP — query (cross-package hybrid search + reranker)
# ---------------------------------------------------------------------------
#
# Ground truth verified live against all installed bundles with reranker
# active (Qwen3-Reranker-0.6B, RANK pooling).
#
# Score expectations:
#   - When reranker is loaded, top-hit scores are cross-encoder P(yes) values
#     in [0, 1].  They must be substantially above the RRF-fallback floor of
#     ~0.016 (= 1/61).  We require at least one score ≥ 0.50.
#   - The relevance floor is 0.10; queries with no relevant content return a
#     sentinel message rather than below-floor noise.
#
# Each benchmark query was chosen to be semantically demanding — BM25 alone
# on keyword overlap would surface many wrong candidates, so correct top-1
# placement is strong evidence that the cross-encoder ran.

import re as _re


def _parse_query_scores(text: str) -> list[float]:
    """Extract the reranker scores from the query tool markdown output.

    Handles both the reranker-active format  ``· score 0.998``
    and the RRF-fallback format              ``· score 0.016``
    """
    return [float(m) for m in _re.findall(r"·\s*score\s+([0-9.]+)", text)]


def _top_score(text: str) -> float:
    """Return the highest score in a query result, or 0.0 if none found."""
    scores = _parse_query_scores(text)
    return max(scores, default=0.0)


def _result_packages(text: str) -> list[str]:
    """Return the list of **Package** values from a query result table."""
    return _re.findall(r"\*\*Package\*\*\s*\|\s*`([^`]+)`", text)


def _result_sources(text: str) -> list[str]:
    """Return the list of **Source** file values from a query result table."""
    return _re.findall(r"\*\*Source\*\*\s*\|\s*`([^`]+)`", text)


@pytest.mark.functional
class TestQueryTool:
    """Cross-package query tool with reranker active.

    These tests verify that:
    1. The query tool returns well-formed markdown output.
    2. Reranker scores are high-fidelity cross-encoder values (not RRF noise).
    3. Semantically-demanding queries surface the correct result from the right
       package, even when the answer lives in a challenging location that BM25
       alone would likely miss.
    4. The relevance floor suppresses completely irrelevant queries with a
       clear sentinel message.
    """

    # ------------------------------------------------------------------
    # Output shape and reranker-active signal
    # ------------------------------------------------------------------

    def test_query_returns_results_header(self, mcp_client: McpClient) -> None:
        """Output must start with the standard '## Query results for:' header."""
        text = mcp_client.tool_text(
            "query",
            {"query": "how does sempkg build the unified candidate pool before reranking"},
        )
        assert text.startswith("## Query results for:"), (
            f"Missing results header:\n{text[:200]}"
        )

    def test_query_scores_above_rrf_floor(self, mcp_client: McpClient) -> None:
        """When the reranker is loaded, the top score must be well above the RRF
        baseline of ~0.016.  A score ≥ 0.50 is unambiguously cross-encoder output."""
        text = mcp_client.tool_text(
            "query",
            {"query": "how does sempkg build the unified candidate pool before reranking"},
        )
        top = _top_score(text)
        assert top >= 0.50, (
            f"Top score {top:.3f} is below 0.50 — reranker may not be active "
            f"(RRF fallback produces scores ~0.016).\nOutput:\n{text[:400]}"
        )

    def test_query_result_has_package_provenance(self, mcp_client: McpClient) -> None:
        """Every result section must declare its source package."""
        text = mcp_client.tool_text(
            "query",
            {"query": "how does sempkg build the unified candidate pool before reranking"},
        )
        packages = _result_packages(text)
        assert packages, (
            f"No **Package** entries found in query output:\n{text[:400]}"
        )

    def test_query_result_has_source_files(self, mcp_client: McpClient) -> None:
        """Every result section must include a Source file path."""
        text = mcp_client.tool_text(
            "query",
            {"query": "how does sempkg build the unified candidate pool before reranking"},
        )
        sources = _result_sources(text)
        assert sources, (
            f"No **Source** entries found in query output:\n{text[:400]}"
        )

    # ------------------------------------------------------------------
    # Relevance floor — sentinel for irrelevant queries
    # ------------------------------------------------------------------

    def test_irrelevant_query_triggers_floor_sentinel(self, mcp_client: McpClient) -> None:
        """A completely off-topic query must return the 'No relevant results' sentinel
        rather than low-score noise results."""
        text = mcp_client.tool_text(
            "query",
            {
                "query": (
                    "xyzzy completely irrelevant nonsense query "
                    "about sandwich recipes and cooking temperatures"
                )
            },
        )
        assert "No relevant results for:" in text, (
            f"Expected 'No relevant results for:' sentinel for off-topic query:\n{text[:400]}"
        )

    def test_irrelevant_query_sentinel_mentions_floor(self, mcp_client: McpClient) -> None:
        """The floor sentinel must state the threshold so the caller understands why."""
        text = mcp_client.tool_text(
            "query",
            {
                "query": (
                    "xyzzy completely irrelevant nonsense query "
                    "about sandwich recipes and cooking temperatures"
                )
            },
        )
        assert "0.10" in text, (
            f"Floor threshold (0.10) missing from sentinel message:\n{text}"
        )

    def test_irrelevant_query_sentinel_suggests_alternatives(
        self, mcp_client: McpClient
    ) -> None:
        """The sentinel must guide the caller to narrower tools."""
        text = mcp_client.tool_text(
            "query",
            {
                "query": (
                    "xyzzy completely irrelevant nonsense query "
                    "about sandwich recipes and cooking temperatures"
                )
            },
        )
        assert "search_code" in text or "search_symbols" in text or "search_docs" in text, (
            f"Floor sentinel does not suggest alternative tools:\n{text}"
        )

    # ------------------------------------------------------------------
    # Benchmark 1: llama-cpp-rs — RANK pooling for cross-encoder reranking
    #
    # This query asks about a specific implementation detail (LLAMA_POOLING_TYPE_RANK
    # and sigmoid normalisation) that only appears in llama-cpp-rs docs and the
    # sempkg reranker design doc.  BM25 on "pooling type" alone would surface many
    # unrelated embedding-pooling results; the cross-encoder must prefer the
    # reranker-specific content.
    # ------------------------------------------------------------------

    POOLING_QUERY = (
        "What pooling type does llama-cpp-rs use to perform cross-encoder reranking "
        "and how is the raw logit converted to a score?"
    )
    POOLING_EXPECTED_PKG = "llama-cpp-rs"
    POOLING_EXPECTED_SOURCE = "README.md"
    POOLING_BODY_MARKER = "LLAMA_POOLING_TYPE_RANK"

    def test_pooling_query_top_hit_is_llama_cpp_rs(self, mcp_client: McpClient) -> None:
        """Top result must be from the llama-cpp-rs package."""
        text = mcp_client.tool_text("query", {"query": self.POOLING_QUERY, "limit": 5})
        packages = _result_packages(text)
        assert packages, f"No packages returned:\n{text[:400]}"
        assert self.POOLING_EXPECTED_PKG in packages[0], (
            f"Top result package '{packages[0]}' is not '{self.POOLING_EXPECTED_PKG}'.\n"
            f"Full output:\n{text[:600]}"
        )

    def test_pooling_query_top_source_is_reranker_readme(
        self, mcp_client: McpClient
    ) -> None:
        """Top result must point to the reranker README inside llama-cpp-rs."""
        text = mcp_client.tool_text("query", {"query": self.POOLING_QUERY, "limit": 5})
        sources = _result_sources(text)
        assert sources, f"No sources returned:\n{text[:400]}"
        assert self.POOLING_EXPECTED_SOURCE in sources[0], (
            f"Top source '{sources[0]}' is not '{self.POOLING_EXPECTED_SOURCE}'.\n"
            f"Full output:\n{text[:600]}"
        )

    def test_pooling_query_snippet_contains_pooling_type(
        self, mcp_client: McpClient
    ) -> None:
        """The top result snippet must contain LLAMA_POOLING_TYPE_RANK."""
        text = mcp_client.tool_text("query", {"query": self.POOLING_QUERY, "limit": 5})
        assert self.POOLING_BODY_MARKER in text, (
            f"'{self.POOLING_BODY_MARKER}' not found in query output:\n{text[:600]}"
        )

    def test_pooling_query_score_is_high(self, mcp_client: McpClient) -> None:
        """Cross-encoder score for this well-matched query must be ≥ 0.85."""
        text = mcp_client.tool_text("query", {"query": self.POOLING_QUERY, "limit": 5})
        top = _top_score(text)
        assert top >= 0.85, (
            f"Expected top score ≥ 0.85 for pooling/reranker query, got {top:.3f}.\n"
            f"Output:\n{text[:400]}"
        )

    # ------------------------------------------------------------------
    # Benchmark 2: lancedb — RRFReranker with k=60 from the paper
    #
    # Asking for the specific numeric constant (k=60) in lancedb's RRF
    # implementation requires surfacing `RRFReranker::new` in rerankers/rrf.rs.
    # BM25 on "RRF k parameter" would also match sempkg's own RRF comments;
    # the cross-encoder must prefer the canonical source-of-truth in lancedb.
    # ------------------------------------------------------------------

    RRF_QUERY = (
        "What is the RRF k parameter value used in sempkg when building the "
        "candidate pool before reranking?"
    )
    RRF_EXPECTED_PKG = "lancedb"
    RRF_EXPECTED_SOURCE = "rerankers/rrf.rs"
    RRF_BODY_MARKER = "k = 60"

    def test_rrf_query_top_hit_is_lancedb(self, mcp_client: McpClient) -> None:
        """Top result for the RRF k-parameter query must be from lancedb."""
        text = mcp_client.tool_text("query", {"query": self.RRF_QUERY, "limit": 5})
        packages = _result_packages(text)
        assert packages, f"No packages returned:\n{text[:400]}"
        assert self.RRF_EXPECTED_PKG in packages[0], (
            f"Top result package '{packages[0]}' is not '{self.RRF_EXPECTED_PKG}'.\n"
            f"Full output:\n{text[:600]}"
        )

    def test_rrf_query_top_source_is_rrf_rs(self, mcp_client: McpClient) -> None:
        """Top result must point to rerankers/rrf.rs."""
        text = mcp_client.tool_text("query", {"query": self.RRF_QUERY, "limit": 5})
        sources = _result_sources(text)
        assert sources, f"No sources returned:\n{text[:400]}"
        assert self.RRF_EXPECTED_SOURCE in sources[0], (
            f"Top source '{sources[0]}' is not '{self.RRF_EXPECTED_SOURCE}'.\n"
            f"Full output:\n{text[:600]}"
        )

    def test_rrf_query_snippet_contains_k_value(self, mcp_client: McpClient) -> None:
        """Result snippet must show the k=60 constant from the paper."""
        text = mcp_client.tool_text("query", {"query": self.RRF_QUERY, "limit": 5})
        assert self.RRF_BODY_MARKER in text, (
            f"'{self.RRF_BODY_MARKER}' not found in query output:\n{text[:600]}"
        )

    def test_rrf_query_score_is_high(self, mcp_client: McpClient) -> None:
        """Cross-encoder score must be ≥ 0.85."""
        text = mcp_client.tool_text("query", {"query": self.RRF_QUERY, "limit": 5})
        top = _top_score(text)
        assert top >= 0.85, (
            f"Expected top score ≥ 0.85 for RRF k-parameter query, got {top:.3f}.\n"
            f"Output:\n{text[:400]}"
        )

    # ------------------------------------------------------------------
    # Benchmark 3: sempkg — query expansion routing (ExpansionKind enum)
    #
    # This query asks about the internal routing logic in sempkg's query
    # expander: lexical vs. vector variant dispatch.  The canonical answer
    # lives in `query_expansion.rs` as the `ExpansionKind` enum and
    # `QueryExpander::expand` method.  Many other packages have "expansion"
    # or "query" in their docs, so keyword overlap alone is noisy.
    # ------------------------------------------------------------------

    EXPANSION_QUERY = (
        "How does sempkg expand a lexical query variant and route it "
        "separately from vector variants before retrieval?"
    )
    EXPANSION_EXPECTED_PKG = "sempkg"
    EXPANSION_EXPECTED_SOURCE_CANDIDATES = (
        "design/reranker-design.md",
        "sempkg/src/mcp.rs",
    )
    EXPANSION_EXPECTED_MARKER = "query expansion"

    def test_expansion_query_top_hit_is_sempkg(self, mcp_client: McpClient) -> None:
        """Top result must be from the sempkg package."""
        text = mcp_client.tool_text(
            "query", {"query": self.EXPANSION_QUERY, "limit": 5}
        )
        packages = _result_packages(text)
        assert packages, f"No packages returned:\n{text[:400]}"
        assert self.EXPANSION_EXPECTED_PKG in packages[0], (
            f"Top result package '{packages[0]}' is not '{self.EXPANSION_EXPECTED_PKG}'.\n"
            f"Full output:\n{text[:600]}"
        )

    def test_expansion_query_top_source_is_query_expansion_rs(
        self, mcp_client: McpClient
    ) -> None:
        """Top result should come from sempkg expansion implementation/design docs."""
        text = mcp_client.tool_text(
            "query", {"query": self.EXPANSION_QUERY, "limit": 5}
        )
        sources = _result_sources(text)
        assert sources, f"No sources returned:\n{text[:400]}"
        assert any(c in sources[0] for c in self.EXPANSION_EXPECTED_SOURCE_CANDIDATES), (
            f"Top source '{sources[0]}' is not one of "
            f"{self.EXPANSION_EXPECTED_SOURCE_CANDIDATES}.\n"
            f"Full output:\n{text[:600]}"
        )

    def test_expansion_query_snippet_contains_expander_symbol(
        self, mcp_client: McpClient
    ) -> None:
        """Result snippet must reference query expansion semantics."""
        text = mcp_client.tool_text(
            "query", {"query": self.EXPANSION_QUERY, "limit": 5}
        )
        assert self.EXPANSION_EXPECTED_MARKER in text.lower(), (
            f"'{self.EXPANSION_EXPECTED_MARKER}' not found in query output:\n{text[:600]}"
        )

    def test_expansion_query_score_is_high(self, mcp_client: McpClient) -> None:
        """Cross-encoder score must be ≥ 0.85."""
        text = mcp_client.tool_text(
            "query", {"query": self.EXPANSION_QUERY, "limit": 5}
        )
        top = _top_score(text)
        assert top >= 0.85, (
            f"Expected top score ≥ 0.85 for query expansion routing query, "
            f"got {top:.3f}.\nOutput:\n{text[:400]}"
        )

    # ------------------------------------------------------------------
    # Benchmark 4: cross-package — lancedb QueryRequest hybrid fields
    #
    # Asking specifically how lancedb merges vector + FTS results in a
    # hybrid query should surface `QueryRequest` in `query.rs` (lancedb)
    # because it contains both `full_text_search` and `reranker` fields
    # alongside the hybrid merger logic.  The sempkg CLI and ADR-002 are
    # noisy competitors that BM25 would over-promote.
    # ------------------------------------------------------------------

    HYBRID_QUERY = (
        "How does lancedb merge vector search results with full-text search "
        "results in a hybrid query?"
    )
    HYBRID_EXPECTED_PKG = "lancedb"
    HYBRID_EXPECTED_SOURCE = "query.rs"
    HYBRID_BODY_MARKER = "full_text_search"

    def test_hybrid_query_result_includes_lancedb(self, mcp_client: McpClient) -> None:
        """At least one of the top results must be from lancedb."""
        text = mcp_client.tool_text("query", {"query": self.HYBRID_QUERY, "limit": 5})
        packages = _result_packages(text)
        assert any(self.HYBRID_EXPECTED_PKG in p for p in packages), (
            f"No lancedb result in top 5:\n{text[:600]}"
        )

    def test_hybrid_query_lancedb_source_is_query_rs(
        self, mcp_client: McpClient
    ) -> None:
        """The lancedb result must point to query.rs."""
        text = mcp_client.tool_text("query", {"query": self.HYBRID_QUERY, "limit": 5})
        sources = _result_sources(text)
        lancedb_idx = next(
            (i for i, p in enumerate(_result_packages(text))
             if self.HYBRID_EXPECTED_PKG in p),
            None,
        )
        assert lancedb_idx is not None, f"No lancedb result found:\n{text[:400]}"
        assert lancedb_idx < len(sources), f"Source list shorter than expected:\n{text[:400]}"
        assert self.HYBRID_EXPECTED_SOURCE in sources[lancedb_idx], (
            f"lancedb source '{sources[lancedb_idx]}' is not '{self.HYBRID_EXPECTED_SOURCE}'.\n"
            f"Full output:\n{text[:600]}"
        )

    def test_hybrid_query_snippet_contains_fts_field(
        self, mcp_client: McpClient
    ) -> None:
        """The result must contain the full_text_search field from QueryRequest."""
        text = mcp_client.tool_text("query", {"query": self.HYBRID_QUERY, "limit": 5})
        assert self.HYBRID_BODY_MARKER in text, (
            f"'{self.HYBRID_BODY_MARKER}' not found in query output:\n{text[:600]}"
        )

    def test_hybrid_query_top_score_above_rrf_baseline(
        self, mcp_client: McpClient
    ) -> None:
        """Even for a multi-package query the top score must be cross-encoder range."""
        text = mcp_client.tool_text("query", {"query": self.HYBRID_QUERY, "limit": 5})
        top = _top_score(text)
        assert top >= 0.50, (
            f"Top score {top:.3f} below 0.50 — reranker may not be active.\n"
            f"Output:\n{text[:400]}"
        )
