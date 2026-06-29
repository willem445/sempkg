"""Functional test: publish a "latest" bundle into the agent's knowledge corpus.

End-to-end demonstration of the hosting flow from ``deploy/``:

    sembundle build  →  POST /publish (registry)  →  sempkg sync (workspace)

i.e. exactly what a scheduled CI job does when it bundles tip-of-main as a rolling
``latest`` build and uploads it, after which the agent's workspace installs it and
the bundle becomes part of the corpus the agent answers from.

Prerequisites (otherwise skipped):
- the ``sempkg`` and ``sembundle`` release binaries are built;
- the ``sempkg-registry`` service can start in this environment.

Run with:  pytest -m functional -k publish
"""

from __future__ import annotations

import subprocess
import textwrap
from pathlib import Path

import httpx
import pytest

pytestmark = pytest.mark.functional

PKG = "demolib"
VERSION = "latest"


def _make_source_tree(root: Path) -> tuple[Path, Path]:
    """Write a tiny source + docs tree to bundle."""
    src = root / "src"
    docs = root / "docs"
    src.mkdir(parents=True)
    docs.mkdir(parents=True)
    (src / "calc.py").write_text(
        textwrap.dedent(
            '''
            """A tiny demo library used by the sempkg-agent functional tests."""


            def add(a, b):
                """Return the sum of two numbers."""
                return a + b


            def fibonacci(n):
                """Return the nth Fibonacci number, iteratively."""
                a, b = 0, 1
                for _ in range(n):
                    a, b = b, a + b
                return a
            '''
        ).strip()
        + "\n",
        encoding="utf-8",
    )
    (docs / "overview.md").write_text(
        "# Demo Library\n\nProvides `add()` and `fibonacci()` helpers.\n",
        encoding="utf-8",
    )
    return src, docs


def _build_bundle(sembundle_bin: str, src: Path, docs: Path, out: Path) -> None:
    """Build a .sembundle, skipping the test if the indexer can't run offline."""
    result = subprocess.run(
        [
            sembundle_bin, "build",
            "--name", PKG,
            "--version", VERSION,
            "--source-repo", "https://example.com/demolib",
            "--commit-hash", "a" * 40,
            "--codegraph-version", "0.9.7",
            "--language", "python",
            "--source-dir", str(src),
            "--docs-dir", str(docs),
            "--output", str(out),
        ],
        capture_output=True, encoding="utf-8", errors="replace", timeout=900,
    )
    if result.returncode != 0:
        pytest.skip(
            "sembundle build failed — likely needs cached embedding models / network "
            f"in this environment:\n{result.stderr[-1500:]}"
        )
    assert out.is_file(), f"bundle not produced:\n{result.stdout}\n{result.stderr}"


def _mint_publish_token(registry) -> str:
    r = httpx.post(
        f"{registry.base_url}/admin/tokens",
        headers={"Authorization": f"Bearer {registry.admin}"},
        json={"label": "functional-test"},
        timeout=30,
    )
    assert r.status_code < 300, f"token mint failed: {r.status_code} {r.text}"
    token = r.json()["token"]
    assert token
    return token


def test_publish_latest_then_sync_into_corpus(
    sempkg_bin: str, sembundle_bin: str, registry, tmp_path: Path
) -> None:
    # 1. Build a tiny bundle as the rolling "latest" version.
    src, docs = _make_source_tree(tmp_path / "demolib")
    bundle = tmp_path / f"{PKG}-{VERSION}.sembundle"
    _build_bundle(sembundle_bin, src, docs, bundle)

    # 2. Mint a CI publish token and POST the bundle to the registry.
    token = _mint_publish_token(registry)
    with open(bundle, "rb") as fh:
        r = httpx.post(
            f"{registry.base_url}/publish",
            headers={"Authorization": f"Bearer {token}"},
            files={"file": (bundle.name, fh, "application/octet-stream")},
            timeout=120,
        )
    assert r.status_code < 300, f"publish failed: {r.status_code} {r.text}"
    published = r.json()
    assert published["name"] == PKG
    assert published["version"] == VERSION

    # 3. The registry index now advertises demolib@latest for install.
    index = httpx.get(f"{registry.base_url}/index.json", timeout=30).json()
    assert VERSION in index["packages"][PKG]["bundles"], index

    # 4. An agent workspace declares the bundle and syncs it from the registry.
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    (workspace / "sempkg.toml").write_text(
        textwrap.dedent(
            f"""
            [[registry]]
            name = "default"
            url  = "{registry.base_url}"

            [dependencies]
            {PKG} = {{ version = "{VERSION}" }}
            """
        ).strip()
        + "\n",
        encoding="utf-8",
    )
    sync = subprocess.run(
        [sempkg_bin, "sync"],
        cwd=str(workspace), capture_output=True, encoding="utf-8", errors="replace",
        timeout=600,
    )
    assert sync.returncode == 0, f"sempkg sync failed:\n{sync.stdout}\n{sync.stderr}"

    # 5. The bundle is now part of the agent's knowledge corpus.
    listed = subprocess.run(
        [sempkg_bin, "list"],
        cwd=str(workspace), capture_output=True, encoding="utf-8", errors="replace",
        timeout=30,
    )
    assert listed.returncode == 0, listed.stderr
    assert PKG in listed.stdout, f"{PKG} not installed:\n{listed.stdout}"

    # 6. The lock file records the version installed from the registry.
    lock = (workspace / "sempkg.lock").read_text(encoding="utf-8")
    assert PKG in lock and VERSION in lock, lock
