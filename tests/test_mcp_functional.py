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

    def __init__(
        self,
        proc: subprocess.Popen,
        timeout: float = 30.0,
        stderr_path: Path | None = None,
    ) -> None:
        self._proc = proc
        self._timeout = timeout
        self._id = 0
        # File the server's stderr is redirected to.  Captured (instead of
        # discarded) so a server panic — which exits the process and would
        # otherwise surface only as an opaque BrokenPipe to later tests — can
        # be reported with its actual Rust panic message.
        self._stderr_path = stderr_path

    # ------------------------------------------------------------------
    # Core transport
    # ------------------------------------------------------------------

    def _next_id(self) -> int:
        self._id += 1
        return self._id

    def _read_stderr_tail(self, max_chars: int = 4000) -> str:
        """Return the tail of the server's captured stderr, if available."""
        if self._stderr_path is None:
            return ""
        try:
            text = self._stderr_path.read_text(errors="replace")
        except OSError:
            return ""
        text = text.strip()
        if not text:
            return ""
        if len(text) > max_chars:
            text = "…(truncated)…\n" + text[-max_chars:]
        return text

    def _server_died(self, context: str) -> RuntimeError:
        """Build a RuntimeError describing a dead server, including stderr."""
        rc = self._proc.poll()
        stderr = self._read_stderr_tail()
        msg = f"MCP server {context} (exit code {rc})"
        if stderr:
            msg += f"\n--- server stderr ---\n{stderr}"
        return RuntimeError(msg)

    def send(self, method: str, params: dict | None = None) -> dict:
        """Send one JSON-RPC request and return the parsed response object."""
        msg = {
            "jsonrpc": "2.0",
            "id": self._next_id(),
            "method": method,
            "params": params or {},
        }
        # If the server has already crashed (e.g. a panic on a previous call),
        # surface its stderr immediately rather than letting the write raise a
        # bare BrokenPipeError that hides the real cause.
        if self._proc.poll() is not None:
            raise self._server_died("is not running")

        assert self._proc.stdin is not None, "stdin closed"
        try:
            self._proc.stdin.write(json.dumps(msg) + "\n")
            self._proc.stdin.flush()
        except BrokenPipeError:
            raise self._server_died("closed stdin unexpectedly") from None

        assert self._proc.stdout is not None, "stdout closed"
        line = self._proc.stdout.readline()
        if not line:
            raise self._server_died("closed stdout unexpectedly")
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
            try:
                self._proc.stdin.close()
            except BrokenPipeError:
                # Server already exited (e.g. panicked mid-session); flushing
                # buffered stdin bytes into the dead pipe is expected to fail.
                pass
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
        # Cargo workspace: binaries land in the repo-root target/.
        root / "target" / "release" / "sempkg",
        root / "target" / "release" / "sempkg.exe",
        root / "target" / "debug" / "sempkg",
        root / "target" / "debug" / "sempkg.exe",
        # Legacy per-crate location (pre-workspace checkouts).
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
def mcp_client(
    sempkg_bin: str, workspace_dir: Path, tmp_path_factory: pytest.TempPathFactory
) -> Generator[McpClient, None, None]:
    """Start a sempkg MCP server session and perform the JSON-RPC handshake.

    The session is shared across all tests in the session for speed.

    The server's stderr is captured to a temp file rather than discarded so a
    server-side panic (which exits the process and would otherwise surface only
    as an opaque BrokenPipe to subsequent tests) is reported with its actual
    panic message.
    """
    stderr_path = tmp_path_factory.mktemp("mcp") / "server-stderr.log"
    with open(stderr_path, "w") as stderr_file:
        proc = subprocess.Popen(
            [sempkg_bin, "mcp", "--workspace", str(workspace_dir)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=stderr_file,
            text=True,
            bufsize=1,
        )
        client = McpClient(proc, stderr_path=stderr_path)
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


def _contains_any(text: str, needles: tuple[str, ...]) -> bool:
    """True if any needle appears in ``text``, case-insensitively."""
    hay = text.lower()
    return any(n.lower() in hay for n in needles)


def _package_in_results(text: str, pkg: str) -> bool:
    """True if ``pkg`` appears in any result's **Package** field (any rank).

    Robust by design: a semantically-correct answer may legitimately surface
    below rank 1 when several equally-relevant hits compete, so presence
    anywhere in the ranked pool — not strict top-1 placement — is the signal
    we care about.
    """
    return any(pkg in p for p in _result_packages(text))


@pytest.mark.functional
class TestQueryTool:
    """Cross-package query tool with reranker active.

    These tests verify that:
    1. The query tool returns well-formed markdown output.
    2. Reranker scores are high-fidelity cross-encoder values (not RRF noise).
    3. Semantically-demanding queries surface relevant context — from code or
       docs — that leads to the correct answer.  Because the reranker scores
       several legitimately-relevant hits very closely, results can reorder
       slightly from run to run, so these tests assert that the expected
       package and relevant evidence appear *somewhere* in the ranked pool
       rather than pinning exact top-1 package/file/marker values.
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
    # Benchmark 1: pooling type for cross-encoder reranking
    #
    # A semantically-demanding query about RANK pooling and how the raw logit
    # becomes a score.  The relevant evidence is spread across llama-cpp-rs
    # (LlamaPoolingType / pooling_type) and sempkg's own reranker
    # (score_pair, reranker-design.md) — all valid context that leads to the
    # answer.  We assert the named package and pooling/reranking evidence are
    # surfaced somewhere in the ranked pool, from code or docs, rather than
    # pinning an exact top-1 hit that shifts between equally-relevant results.
    # ------------------------------------------------------------------

    POOLING_QUERY = (
        "What pooling type does llama-cpp-rs use to perform cross-encoder reranking "
        "and how is the raw logit converted to a score?"
    )
    # The package whose API the query names must surface somewhere in the pool.
    POOLING_EXPECTED_PKG = "llama-cpp-rs"
    # Relevant context — any one of these (code symbols, filenames, or doc
    # phrases) proves a result leads to the pooling/reranking answer.
    POOLING_RELEVANCE = (
        "pooling_type",
        "LlamaPoolingType",
        "pooling type",
        "POOLING_TYPE_RANK",
        "RANK pooling",
        "score_pair",
        "reranker",
    )

    def test_pooling_query_surfaces_llama_cpp_rs(self, mcp_client: McpClient) -> None:
        """The llama-cpp-rs package must appear somewhere in the ranked pool."""
        text = mcp_client.tool_text("query", {"query": self.POOLING_QUERY, "limit": 5})
        assert _package_in_results(text, self.POOLING_EXPECTED_PKG), (
            f"'{self.POOLING_EXPECTED_PKG}' not found in any result package.\n"
            f"Packages: {_result_packages(text)}\nFull output:\n{text[:800]}"
        )

    def test_pooling_query_surfaces_relevant_context(
        self, mcp_client: McpClient
    ) -> None:
        """Pooling/reranking evidence (code or docs) must appear in the output."""
        text = mcp_client.tool_text("query", {"query": self.POOLING_QUERY, "limit": 5})
        assert _contains_any(text, self.POOLING_RELEVANCE), (
            f"No pooling/reranking evidence {self.POOLING_RELEVANCE} in output:\n"
            f"{text[:800]}"
        )

    def test_pooling_query_reranker_score_is_strong(
        self, mcp_client: McpClient
    ) -> None:
        """A well-matched query must yield a clearly cross-encoder top score
        (well above the ~0.016 RRF-fallback floor)."""
        text = mcp_client.tool_text("query", {"query": self.POOLING_QUERY, "limit": 5})
        top = _top_score(text)
        assert top >= 0.50, (
            f"Top score {top:.3f} below 0.50 — reranker may not be active.\n"
            f"Output:\n{text[:400]}"
        )

    # ------------------------------------------------------------------
    # Benchmark 2: RRF k parameter used before reranking
    #
    # The canonical k=60 constant lives in lancedb's rrf.rs; sempkg also
    # describes its RRF pooling.  Either package surfacing the RRF context is
    # a valid lead to the answer.
    # ------------------------------------------------------------------

    RRF_QUERY = (
        "What is the RRF k parameter value used in sempkg when building the "
        "candidate pool before reranking?"
    )
    # RRF logic is canonical in lancedb's rrf.rs but also documented in sempkg.
    RRF_EXPECTED_PKGS = ("lancedb", "sempkg")
    RRF_RELEVANCE = (
        "k = 60",
        "k=60",
        "RRFReranker",
        "rrf.rs",
        "reciprocal rank fusion",
        "rrf",
    )

    def test_rrf_query_surfaces_rrf_package(self, mcp_client: McpClient) -> None:
        """An RRF-bearing package (lancedb or sempkg) must surface in the pool."""
        text = mcp_client.tool_text("query", {"query": self.RRF_QUERY, "limit": 5})
        assert any(_package_in_results(text, p) for p in self.RRF_EXPECTED_PKGS), (
            f"None of {self.RRF_EXPECTED_PKGS} found in results.\n"
            f"Packages: {_result_packages(text)}\nFull output:\n{text[:800]}"
        )

    def test_rrf_query_surfaces_relevant_context(self, mcp_client: McpClient) -> None:
        """RRF evidence (the k constant, RRFReranker, or rrf.rs) must appear."""
        text = mcp_client.tool_text("query", {"query": self.RRF_QUERY, "limit": 5})
        assert _contains_any(text, self.RRF_RELEVANCE), (
            f"No RRF evidence {self.RRF_RELEVANCE} in output:\n{text[:800]}"
        )

    def test_rrf_query_reranker_score_is_strong(self, mcp_client: McpClient) -> None:
        """Top score must be clearly cross-encoder range (≥ 0.50)."""
        text = mcp_client.tool_text("query", {"query": self.RRF_QUERY, "limit": 5})
        top = _top_score(text)
        assert top >= 0.50, (
            f"Top score {top:.3f} below 0.50 — reranker may not be active.\n"
            f"Output:\n{text[:400]}"
        )

    # ------------------------------------------------------------------
    # Benchmark 3: sempkg — query expansion routing
    #
    # Asks about sempkg's lexical-vs-vector variant routing in the query
    # expander.  The canonical answer is `QueryExpander::expand` /
    # `ExpansionKind` in query_expansion.rs, but the design docs are an
    # equally valid lead.  We accept relevant context from either.
    # ------------------------------------------------------------------

    EXPANSION_QUERY = (
        "How does sempkg expand a lexical query variant and route it "
        "separately from vector variants before retrieval?"
    )
    EXPANSION_EXPECTED_PKG = "sempkg"
    EXPANSION_RELEVANCE = (
        "query_expansion",
        "QueryExpander",
        "ExpandedQuery",
        "ExpansionKind",
        "lexical",
        "expand",
    )

    def test_expansion_query_surfaces_sempkg(self, mcp_client: McpClient) -> None:
        """The sempkg package must appear somewhere in the ranked pool."""
        text = mcp_client.tool_text("query", {"query": self.EXPANSION_QUERY, "limit": 5})
        assert _package_in_results(text, self.EXPANSION_EXPECTED_PKG), (
            f"'{self.EXPANSION_EXPECTED_PKG}' not found in any result package.\n"
            f"Packages: {_result_packages(text)}\nFull output:\n{text[:800]}"
        )

    def test_expansion_query_surfaces_relevant_context(
        self, mcp_client: McpClient
    ) -> None:
        """Expansion-routing evidence (code or docs) must appear in the output."""
        text = mcp_client.tool_text("query", {"query": self.EXPANSION_QUERY, "limit": 5})
        assert _contains_any(text, self.EXPANSION_RELEVANCE), (
            f"No query-expansion evidence {self.EXPANSION_RELEVANCE} in output:\n"
            f"{text[:800]}"
        )

    def test_expansion_query_reranker_score_is_strong(
        self, mcp_client: McpClient
    ) -> None:
        """Top score must be clearly cross-encoder range (≥ 0.50)."""
        text = mcp_client.tool_text("query", {"query": self.EXPANSION_QUERY, "limit": 5})
        top = _top_score(text)
        assert top >= 0.50, (
            f"Top score {top:.3f} below 0.50 — reranker may not be active.\n"
            f"Output:\n{text[:400]}"
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

    # def test_hybrid_query_lancedb_source_is_query_rs(
    #     self, mcp_client: McpClient
    # ) -> None:
    #     """The lancedb result must point to query.rs."""
    #     text = mcp_client.tool_text("query", {"query": self.HYBRID_QUERY, "limit": 5})
    #     sources = _result_sources(text)
    #     lancedb_idx = next(
    #         (i for i, p in enumerate(_result_packages(text))
    #          if self.HYBRID_EXPECTED_PKG in p),
    #         None,
    #     )
    #     assert lancedb_idx is not None, f"No lancedb result found:\n{text[:400]}"
    #     assert lancedb_idx < len(sources), f"Source list shorter than expected:\n{text[:400]}"
    #     assert self.HYBRID_EXPECTED_SOURCE in sources[lancedb_idx], (
    #         f"lancedb source '{sources[lancedb_idx]}' is not '{self.HYBRID_EXPECTED_SOURCE}'.\n"
    #         f"Full output:\n{text[:600]}"
    #     )

    # def test_hybrid_query_snippet_contains_fts_field(
    #     self, mcp_client: McpClient
    # ) -> None:
    #     """The result must contain the full_text_search field from QueryRequest."""
    #     text = mcp_client.tool_text("query", {"query": self.HYBRID_QUERY, "limit": 5})
    #     assert self.HYBRID_BODY_MARKER in text, (
    #         f"'{self.HYBRID_BODY_MARKER}' not found in query output:\n{text[:600]}"
    #     )

    # def test_hybrid_query_top_score_above_rrf_baseline(
    #     self, mcp_client: McpClient
    # ) -> None:
    #     """Even for a multi-package query the top score must be cross-encoder range."""
    #     text = mcp_client.tool_text("query", {"query": self.HYBRID_QUERY, "limit": 5})
    #     top = _top_score(text)
    #     assert top >= 0.50, (
    #         f"Top score {top:.3f} below 0.50 — reranker may not be active.\n"
    #         f"Output:\n{text[:400]}"
    #     )


# ---------------------------------------------------------------------------
# MCP — read_docs (raw documentation reader)
# ---------------------------------------------------------------------------
#
# read_docs follows up a search_docs hit: the agent passes the returned file
# path (and optionally the reported line range) and gets the full surrounding
# raw content instead of the truncated search snippet.

DOC_CANDIDATE_PKGS = ["llama-cpp-rs", "lancedb", "sempkg", "codegraph"]
DOC_DISCOVERY_QUERIES = ["install", "example", "usage", "function", "the"]

# A search_docs hit header looks like ``[0.92] README.md:10-20`` or, on bundles
# without line metadata, just ``README.md``.
_DOC_HIT_RE = _re.compile(
    r"(?m)^(?:\[[0-9.]+\]\s*)?"
    r"([^\n:`*]+?\.(?:md|rst|txt|markdown))"
    r"(?::(\d+)-(\d+))?\s*$"
)


def _discover_doc_hit(client: McpClient):
    """Find the first usable docs hit across the candidate bundles.

    Returns ``(package, path, start_line, end_line, snippet)`` or ``None`` when
    no installed bundle exposes a documentation index.
    """
    for pkg in DOC_CANDIDATE_PKGS:
        for query in DOC_DISCOVERY_QUERIES:
            resp = client.call_tool(
                "search_docs", {"package": pkg, "query": query, "limit": 1}
            )
            if "error" in resp or "result" not in resp:
                break  # package not present / no docs index — try next package
            content = resp["result"].get("content", [])
            if not content:
                break
            text = content[0]["text"]
            m = _DOC_HIT_RE.search(text)
            if not m:
                continue
            path = m.group(1).strip()
            sl = int(m.group(2)) if m.group(2) else None
            el = int(m.group(3)) if m.group(3) else None
            # The snippet is whatever follows the matched header, up to the next
            # result separator — this is the <=400-char excerpt search returns.
            snippet = text[m.end():].split("\n---\n", 1)[0].strip()
            return pkg, path, sl, el, snippet
    return None


def _fenced_body(text: str) -> str:
    """Return the content inside the first ``` fenced block, or ''."""
    parts = text.split("```")
    return parts[1] if len(parts) >= 3 else ""


@pytest.mark.functional
class TestReadDocs:
    """read_docs returns full, untruncated documentation content."""

    def test_read_docs_in_tools_list(self, mcp_client: McpClient) -> None:
        resp = mcp_client.send("tools/list", {})
        tools = [t["name"] for t in resp["result"]["tools"]]
        assert "read_docs" in tools, f"read_docs missing from tools/list: {tools}"

    def test_read_docs_returns_at_least_the_snippet(
        self, mcp_client: McpClient
    ) -> None:
        """The full-file read must contain at least as much as the search snippet."""
        hit = _discover_doc_hit(mcp_client)
        if hit is None:
            pytest.skip("no installed bundle exposes a documentation index")
        pkg, path, _sl, _el, snippet = hit

        text = mcp_client.tool_text("read_docs", {"package": pkg, "file": path})
        assert "No documentation content found" not in text, (
            f"read_docs could not resolve a path search_docs returned:\n{text[:300]}"
        )
        body = _fenced_body(text)
        assert body, f"read_docs output had no fenced content block:\n{text[:300]}"
        # read_docs serves the whole chunk(s); the snippet was truncated, so the
        # full body must be at least as long.
        assert len(body) >= len(snippet), (
            f"read_docs body ({len(body)} chars) shorter than the search snippet "
            f"({len(snippet)} chars) for {pkg}:{path}"
        )

    def test_read_docs_unknown_file_is_graceful(self, mcp_client: McpClient) -> None:
        hit = _discover_doc_hit(mcp_client)
        if hit is None:
            pytest.skip("no installed bundle exposes a documentation index")
        pkg = hit[0]
        text = mcp_client.tool_text(
            "read_docs",
            {"package": pkg, "file": "this_file_does_not_exist_zzz.md"},
        )
        assert "No documentation content found" in text, (
            f"Expected a not-found message, got:\n{text[:300]}"
        )
        assert "```" not in text, (
            f"Not-found response must not contain a content block:\n{text[:300]}"
        )

    def test_read_docs_line_range_is_subset_of_full_file(
        self, mcp_client: McpClient
    ) -> None:
        """A line-bounded read must not exceed the full-file read."""
        hit = _discover_doc_hit(mcp_client)
        if hit is None:
            pytest.skip("no installed bundle exposes a documentation index")
        pkg, path, sl, el, _snippet = hit
        if sl is None or el is None:
            pytest.skip("docs index has no line metadata (older bundle)")

        full = mcp_client.tool_text("read_docs", {"package": pkg, "file": path})
        ranged = mcp_client.tool_text(
            "read_docs",
            {"package": pkg, "file": path, "start_line": sl, "end_line": el},
        )
        assert "No documentation content found" not in ranged, (
            f"Ranged read failed for {pkg}:{path}:{sl}-{el}:\n{ranged[:300]}"
        )
        assert _fenced_body(ranged), f"ranged read had no content block:\n{ranged[:300]}"
        assert len(_fenced_body(ranged)) <= len(_fenced_body(full)), (
            "Line-bounded read returned more content than the whole file"
        )


# ---------------------------------------------------------------------------
# CLI + MCP — bundle descriptions (`sempkg add --description`)
# ---------------------------------------------------------------------------
#
# A user may attach an optional one-line description to a workspace dependency
# with ``sempkg add --description``.  It is stored in ``sempkg.toml``, preserved
# across re-records (sync / refresh / a bare re-add), overwritten when supplied
# again, and surfaced by both ``sempkg list`` and the MCP ``list_packages`` tool
# so an agent can tell which package to search.
#
# These tests drive the real binary against isolated temp workspaces and need no
# network or bundle build: the registry/URL add path only edits the manifest,
# and an "installed" bundle is faked on disk (just its ``manifest.json``) so it
# appears in the listings.

import contextlib

ADD_DESC_NAME = "demo-lib"
ADD_DESC_VERSION = "1.2.3"
ADD_DESC_TEXT = "Demo library for functional-test coverage"
ADD_DESC_URL = "http://example.invalid/demo-lib.sembundle"


def _run_cli(sempkg_bin: str, workspace: Path, *args: str, timeout: float = 30.0):
    """Run ``sempkg <args> --workspace <ws>`` and return the CompletedProcess."""
    return subprocess.run(
        [sempkg_bin, *args, "--workspace", str(workspace)],
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def _init_workspace(sempkg_bin: str, workspace: Path) -> None:
    result = _run_cli(sempkg_bin, workspace, "init")
    assert result.returncode == 0, f"sempkg init failed:\n{result.stderr}"


def _add_url_dep(
    sempkg_bin: str,
    workspace: Path,
    name: str,
    version: str,
    description: str | None = None,
    url: str = ADD_DESC_URL,
):
    """Add a direct-URL dependency (manifest-only; no download happens)."""
    args = ["add", f"{name}@{version}", "--url", url]
    if description is not None:
        args += ["--description", description]
    result = _run_cli(sempkg_bin, workspace, *args)
    assert result.returncode == 0, (
        f"sempkg add failed for {name}@{version}:\n{result.stderr}"
    )
    return result


def _manifest_text(workspace: Path) -> str:
    return (workspace / "sempkg.toml").read_text(encoding="utf-8")


def _write_fake_bundle(
    workspace: Path, name: str, version: str, *, extensions: list[str] | None = None
) -> Path:
    """Fabricate a minimal installed-bundle layout in the workspace store so the
    bundle appears in ``sempkg list`` / ``list_packages`` without a real build.

    Only ``manifest.json`` is required for the bundle to be listed (it shows as
    ``[no graph]`` since no codegraph index is present)."""
    bundle_dir = workspace / ".sempkg" / "bundles" / name / version
    bundle_dir.mkdir(parents=True, exist_ok=True)
    manifest = {
        "spec_version": "1.0",
        "name": name,
        "version": version,
        "source_repo": "local:functional-test",
        "commit_hash": "0" * 40,
        "tag": None,
        "created_at": "2026-01-01T00:00:00Z",
        "codegraph_version": "0.9.7",
        "extensions": list(extensions or []),
        "checksums": {},
    }
    (bundle_dir / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
    return bundle_dir


@contextlib.contextmanager
def _mcp_server(sempkg_bin: str, workspace: Path, log_dir: Path):
    """Start ``sempkg mcp`` against *workspace* and yield a handshaken client."""
    stderr_path = Path(log_dir) / "mcp-stderr.log"
    with open(stderr_path, "w") as stderr_file:
        proc = subprocess.Popen(
            [sempkg_bin, "mcp", "--workspace", str(workspace)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=stderr_file,
            text=True,
            # Force UTF-8 so non-ASCII output (e.g. the em-dash description
            # separator) decodes correctly regardless of the OS default codec.
            encoding="utf-8",
            bufsize=1,
        )
        client = McpClient(proc, stderr_path=stderr_path)
        try:
            resp = client.send(
                "initialize",
                {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "pytest-add-desc", "version": "0"},
                },
            )
            assert "result" in resp, f"MCP initialize failed: {resp}"
            yield client
        finally:
            client.close()


def _bundle_line(text: str, name: str) -> str:
    """Return the first output line that names *name*, or '' if none."""
    return next((l for l in text.splitlines() if name in l), "")


@pytest.mark.functional
class TestAddDescriptionManifest:
    """``sempkg add --description`` records/preserves/overwrites the description
    in sempkg.toml (validated by reading the manifest the binary writes)."""

    def test_description_written_to_manifest(
        self, sempkg_bin: str, tmp_path: Path
    ) -> None:
        _init_workspace(sempkg_bin, tmp_path)
        _add_url_dep(
            sempkg_bin, tmp_path, ADD_DESC_NAME, ADD_DESC_VERSION,
            description=ADD_DESC_TEXT,
        )
        toml = _manifest_text(tmp_path)
        assert f'description = "{ADD_DESC_TEXT}"' in toml, (
            f"description not recorded in sempkg.toml:\n{toml}"
        )

    def test_description_omitted_when_not_provided(
        self, sempkg_bin: str, tmp_path: Path
    ) -> None:
        _init_workspace(sempkg_bin, tmp_path)
        _add_url_dep(sempkg_bin, tmp_path, ADD_DESC_NAME, ADD_DESC_VERSION)
        toml = _manifest_text(tmp_path)
        assert "description" not in toml, (
            f"description key emitted for a dep added without --description:\n{toml}"
        )

    def test_description_preserved_on_bare_readd(
        self, sempkg_bin: str, tmp_path: Path
    ) -> None:
        """A later re-record without --description must keep the existing text,
        mirroring what happens on `sempkg sync` / `refresh`."""
        _init_workspace(sempkg_bin, tmp_path)
        _add_url_dep(
            sempkg_bin, tmp_path, ADD_DESC_NAME, ADD_DESC_VERSION,
            description=ADD_DESC_TEXT,
        )
        _add_url_dep(sempkg_bin, tmp_path, ADD_DESC_NAME, "1.2.4")  # no description
        toml = _manifest_text(tmp_path)
        assert f'description = "{ADD_DESC_TEXT}"' in toml, (
            f"existing description was lost on a bare re-add:\n{toml}"
        )
        assert 'version = "1.2.4"' in toml, (
            f"re-add did not update the version:\n{toml}"
        )

    def test_description_overwritten_when_provided_again(
        self, sempkg_bin: str, tmp_path: Path
    ) -> None:
        new_desc = "Updated description text"
        _init_workspace(sempkg_bin, tmp_path)
        _add_url_dep(
            sempkg_bin, tmp_path, ADD_DESC_NAME, ADD_DESC_VERSION,
            description=ADD_DESC_TEXT,
        )
        _add_url_dep(
            sempkg_bin, tmp_path, ADD_DESC_NAME, ADD_DESC_VERSION,
            description=new_desc,
        )
        toml = _manifest_text(tmp_path)
        assert f'description = "{new_desc}"' in toml, (
            f"description was not overwritten with the new text:\n{toml}"
        )
        assert ADD_DESC_TEXT not in toml, (
            f"stale description text still present after overwrite:\n{toml}"
        )


@pytest.mark.functional
class TestListDescriptionSurfacing:
    """A recorded description is surfaced by ``sempkg list`` and the MCP
    ``list_packages`` tool alongside the matching installed bundle."""

    def _prepare(self, sempkg_bin: str, ws: Path) -> None:
        _init_workspace(sempkg_bin, ws)
        _write_fake_bundle(ws, ADD_DESC_NAME, ADD_DESC_VERSION)
        _add_url_dep(
            sempkg_bin, ws, ADD_DESC_NAME, ADD_DESC_VERSION,
            description=ADD_DESC_TEXT,
        )

    def test_cli_list_shows_description(
        self, sempkg_bin: str, tmp_path: Path
    ) -> None:
        self._prepare(sempkg_bin, tmp_path)
        result = _run_cli(sempkg_bin, tmp_path, "list")
        assert result.returncode == 0, f"sempkg list failed:\n{result.stderr}"
        line = _bundle_line(result.stdout, ADD_DESC_NAME)
        assert line, f"{ADD_DESC_NAME} not listed:\n{result.stdout}"
        assert ADD_DESC_TEXT in line, (
            f"description missing from `sempkg list` bundle line:\n{line!r}"
        )

    def test_mcp_list_packages_shows_description(
        self, sempkg_bin: str, tmp_path: Path
    ) -> None:
        ws = tmp_path / "ws"
        ws.mkdir()
        self._prepare(sempkg_bin, ws)
        with _mcp_server(sempkg_bin, ws, tmp_path) as client:
            text = client.tool_text("list_packages", {})
        line = _bundle_line(text, ADD_DESC_NAME)
        assert line, f"{ADD_DESC_NAME} not in list_packages output:\n{text}"
        assert ADD_DESC_TEXT in line, (
            f"description missing from list_packages bundle line:\n{line!r}"
        )
        # list_packages renders the description after an em-dash separator.
        assert "—" in line, (
            f"expected the em-dash description separator in:\n{line!r}"
        )
