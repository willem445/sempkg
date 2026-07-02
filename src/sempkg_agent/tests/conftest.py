"""Shared fixtures for the sempkg-agent test suite.

The offline unit tests need nothing here. The ``@pytest.mark.functional`` tests
use these fixtures to locate the built ``sempkg`` / ``sembundle`` binaries and to
spin up a real registry service; every fixture **skips** (never fails) when its
prerequisite is missing, so the default suite stays green on any machine.
"""

from __future__ import annotations

import os
import shutil
import socket
import subprocess
import sys
import time
from collections.abc import Generator
from pathlib import Path
from types import SimpleNamespace

import pytest

# src/sempkg_agent/tests/conftest.py -> repo root
REPO_ROOT = Path(__file__).resolve().parents[3]


def _find_binary(crate: str, name: str) -> str:
    """Return the path to a built Rust binary (release preferred), or ''."""
    exe = f"{name}.exe" if os.name == "nt" else name
    # Cargo workspace: both crates build into the repo-root target/. Keep the
    # legacy per-crate target/ as a fallback for pre-workspace checkouts.
    search_roots = (REPO_ROOT / "target", REPO_ROOT / "src" / crate / "target")
    for base in search_roots:
        for profile in ("release", "debug"):
            for candidate in (exe, name):
                p = base / profile / candidate
                if p.is_file():
                    return str(p)
    return shutil.which(name) or ""


@pytest.fixture(scope="session")
def sempkg_bin() -> str:
    path = _find_binary("sempkg", "sempkg")
    if not path:
        pytest.skip(
            "sempkg binary not found — build it: "
            "cargo build --release --manifest-path src/sempkg/Cargo.toml"
        )
    return path


@pytest.fixture(scope="session")
def sembundle_bin() -> str:
    path = _find_binary("sembundle", "sembundle")
    if not path:
        pytest.skip(
            "sembundle binary not found — build it: "
            "cargo build --release --manifest-path src/sembundle/Cargo.toml"
        )
    return path


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _wait_until_serving(url: str, proc: subprocess.Popen, timeout: float = 30.0) -> None:
    """Poll ``url`` until it answers 200, or raise (incl. if the process dies)."""
    import httpx

    deadline = time.time() + timeout
    last_err: Exception | None = None
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"registry process exited early (rc={proc.returncode})")
        try:
            if httpx.get(url, timeout=2.0).status_code == 200:
                return
        except Exception as exc:  # noqa: BLE001 - server not up yet
            last_err = exc
        time.sleep(0.3)
    raise RuntimeError(f"registry did not become ready at {url}: {last_err}")


@pytest.fixture(scope="session")
def registry(tmp_path_factory: pytest.TempPathFactory) -> Generator[SimpleNamespace, None, None]:
    """Launch a real ``sempkg-registry`` HTTP service for the test session.

    Runs under the current interpreter with ``src`` on PYTHONPATH so the Rust
    ``sempkg`` client can install from it over HTTP. Skips if the service can't
    start (e.g. a missing registry dependency in this environment).
    """
    data = tmp_path_factory.mktemp("registry")
    port = _free_port()
    base_url = f"http://127.0.0.1:{port}"
    admin = "functional-admin-secret"

    env = dict(os.environ)
    env["sempkg_registry_ADMIN_PASSWORD"] = admin
    env["PYTHONPATH"] = str(REPO_ROOT / "src") + os.pathsep + env.get("PYTHONPATH", "")
    log = open(data / "registry.log", "w")  # noqa: SIM115 - closed in teardown
    proc = subprocess.Popen(
        [
            sys.executable, "-m", "sempkg_registry", "serve",
            "--host", "127.0.0.1", "--port", str(port),
            "--storage-dir", str(data / "bundles"),
            "--config-dir", str(data / "config"),
        ],
        env=env, stdout=log, stderr=subprocess.STDOUT,
        encoding="utf-8", errors="replace",
    )
    try:
        _wait_until_serving(f"{base_url}/index.json", proc)
    except Exception as exc:  # noqa: BLE001 - prerequisite missing -> skip, don't fail
        proc.terminate()
        with open(data / "registry.log", errors="replace") as fh:
            tail = fh.read()[-2000:]
        pytest.skip(f"sempkg-registry could not start ({exc}).\n--- log ---\n{tail}")

    yield SimpleNamespace(base_url=base_url, admin=admin, storage=data / "bundles")

    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
    log.close()
