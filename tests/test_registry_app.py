"""Tests for cgbundle_registry FastAPI application."""

from __future__ import annotations

import io
import json
import tarfile
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from cgbundle_registry.app import create_app
from cgbundle_registry.auth import TokenStore
from cgbundle_registry.storage import BundleStorage


ADMIN_PASSWORD = "test-admin-secret"


def make_bundle(name: str, version: str) -> bytes:
    """Create a minimal valid .cgbundle (tar.gz) in memory."""
    manifest = {
        "name": name,
        "version": version,
        "source_repo": "https://example.com/repo",
        "commit_hash": "deadbeef",
        "created_at": "2024-01-01T00:00:00+00:00",
        "codegraph_version": "0.1.0",
        "checksums": {},
        "extensions": {},
    }
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tf:
        manifest_bytes = json.dumps(manifest).encode()
        info = tarfile.TarInfo(name=f"{name}-{version}/manifest.json")
        info.size = len(manifest_bytes)
        tf.addfile(info, io.BytesIO(manifest_bytes))
    return buf.getvalue()


@pytest.fixture
def client(tmp_path: Path) -> TestClient:
    storage = BundleStorage(storage_dir=tmp_path / "bundles")
    token_store = TokenStore(config_dir=tmp_path / "config")
    app = create_app(storage=storage, token_store=token_store, admin_password=ADMIN_PASSWORD)
    return TestClient(app)


@pytest.fixture
def client_with_token(tmp_path: Path):
    """Returns (TestClient, token, storage)."""
    storage = BundleStorage(storage_dir=tmp_path / "bundles")
    token_store = TokenStore(config_dir=tmp_path / "config")
    token = token_store.add_token(label="test-token").token
    app = create_app(storage=storage, token_store=token_store, admin_password=ADMIN_PASSWORD)
    return TestClient(app), token, storage


# ------------------------------------------------------------------
# GET /index.json
# ------------------------------------------------------------------


def test_index_empty(client: TestClient) -> None:
    resp = client.get("/index.json")
    assert resp.status_code == 200
    data = resp.json()
    assert data["packages"] == {}
    assert "generated_at" in data


# ------------------------------------------------------------------
# POST /publish
# ------------------------------------------------------------------


def test_publish_valid_bundle(client_with_token) -> None:
    tc, token, _ = client_with_token
    bundle_data = make_bundle("mylib", "1.0.0")
    resp = tc.post(
        "/publish",
        headers={"Authorization": f"Bearer {token}"},
        files={"file": ("mylib-1.0.0.cgbundle", bundle_data, "application/octet-stream")},
    )
    assert resp.status_code == 200
    body = resp.json()
    assert body["status"] == "ok"
    assert body["name"] == "mylib"
    assert body["version"] == "1.0.0"


def test_publish_updates_index(client_with_token) -> None:
    tc, token, _ = client_with_token
    bundle_data = make_bundle("mylib", "2.0.0")
    tc.post(
        "/publish",
        headers={"Authorization": f"Bearer {token}"},
        files={"file": ("mylib-2.0.0.cgbundle", bundle_data, "application/octet-stream")},
    )
    resp = tc.get("/index.json")
    assert resp.status_code == 200
    assert "mylib" in resp.json()["packages"]


def test_publish_invalid_token(client: TestClient) -> None:
    bundle_data = make_bundle("mylib", "1.0.0")
    resp = client.post(
        "/publish",
        headers={"Authorization": "Bearer invalid-token"},
        files={"file": ("mylib-1.0.0.cgbundle", bundle_data, "application/octet-stream")},
    )
    assert resp.status_code == 401


def test_publish_no_token(client: TestClient) -> None:
    bundle_data = make_bundle("mylib", "1.0.0")
    resp = client.post(
        "/publish",
        files={"file": ("mylib-1.0.0.cgbundle", bundle_data, "application/octet-stream")},
    )
    assert resp.status_code == 401


def test_publish_duplicate_version(client_with_token) -> None:
    tc, token, _ = client_with_token
    bundle_data = make_bundle("mylib", "1.0.0")
    headers = {"Authorization": f"Bearer {token}"}
    files = {"file": ("mylib-1.0.0.cgbundle", bundle_data, "application/octet-stream")}
    tc.post("/publish", headers=headers, files=files)

    # Upload again
    files2 = {"file": ("mylib-1.0.0.cgbundle", bundle_data, "application/octet-stream")}
    resp = tc.post("/publish", headers=headers, files=files2)
    assert resp.status_code == 409


def test_publish_malformed_bundle(client_with_token) -> None:
    tc, token, _ = client_with_token
    resp = tc.post(
        "/publish",
        headers={"Authorization": f"Bearer {token}"},
        files={"file": ("bad.cgbundle", b"not a tar file", "application/octet-stream")},
    )
    assert resp.status_code == 400


# ------------------------------------------------------------------
# GET /bundles/{package}/{version}/{filename}
# ------------------------------------------------------------------


def test_download_bundle(client_with_token) -> None:
    tc, token, _ = client_with_token
    bundle_data = make_bundle("mylib", "1.0.0")
    tc.post(
        "/publish",
        headers={"Authorization": f"Bearer {token}"},
        files={"file": ("mylib-1.0.0.cgbundle", bundle_data, "application/octet-stream")},
    )
    resp = tc.get("/bundles/mylib/1.0.0/mylib-1.0.0.cgbundle")
    assert resp.status_code == 200
    assert resp.content == bundle_data


def test_download_bundle_not_found(client: TestClient) -> None:
    resp = client.get("/bundles/nolib/9.9.9/nolib-9.9.9.cgbundle")
    assert resp.status_code == 404


def test_download_bundle_wrong_filename(client_with_token) -> None:
    tc, token, _ = client_with_token
    bundle_data = make_bundle("mylib", "1.0.0")
    tc.post(
        "/publish",
        headers={"Authorization": f"Bearer {token}"},
        files={"file": ("mylib-1.0.0.cgbundle", bundle_data, "application/octet-stream")},
    )
    resp = tc.get("/bundles/mylib/1.0.0/wrong-name.cgbundle")
    assert resp.status_code == 400


# ------------------------------------------------------------------
# Admin token endpoints
# ------------------------------------------------------------------


def test_create_token_via_api(client: TestClient) -> None:
    resp = client.post(
        "/admin/tokens",
        headers={"Authorization": f"Bearer {ADMIN_PASSWORD}"},
        json={"label": "api-token"},
    )
    assert resp.status_code == 201
    body = resp.json()
    assert "token" in body
    assert body["label"] == "api-token"
    assert "created_at" in body


# ------------------------------------------------------------------
# SHA-256 and signature tests
# ------------------------------------------------------------------


def test_publish_creates_sha256_sidecar(client_with_token) -> None:
    import hashlib
    tc, token, storage = client_with_token
    bundle_data = make_bundle("mylib", "1.0.0")
    tc.post(
        "/publish",
        headers={"Authorization": f"Bearer {token}"},
        files={"file": ("mylib-1.0.0.cgbundle", bundle_data, "application/octet-stream")},
    )
    sha256_file = storage.storage_dir / "mylib" / "1.0.0" / "mylib-1.0.0.sha256"
    assert sha256_file.exists()
    assert sha256_file.read_text(encoding="utf-8") == hashlib.sha256(bundle_data).hexdigest()


def test_index_includes_sha256_after_publish(client_with_token) -> None:
    tc, token, _ = client_with_token
    bundle_data = make_bundle("mylib", "1.0.0")
    tc.post(
        "/publish",
        headers={"Authorization": f"Bearer {token}"},
        files={"file": ("mylib-1.0.0.cgbundle", bundle_data, "application/octet-stream")},
    )
    resp = tc.get("/index.json")
    assert resp.status_code == 200
    data = resp.json()
    assert "sha256" in data["packages"]["mylib"]["bundles"]["1.0.0"]


def test_publish_with_signature(client_with_token) -> None:
    tc, token, _ = client_with_token
    bundle_data = make_bundle("mylib", "1.0.0")
    sig_bytes = b"\xab" * 64
    resp = tc.post(
        "/publish",
        headers={"Authorization": f"Bearer {token}"},
        files={
            "file": ("mylib-1.0.0.cgbundle", bundle_data, "application/octet-stream"),
            "signature": ("mylib-1.0.0.cgbundle.sig", sig_bytes, "application/octet-stream"),
        },
    )
    assert resp.status_code == 200
    resp2 = tc.get("/bundles/mylib/1.0.0/mylib-1.0.0.cgbundle.sig")
    assert resp2.status_code == 200
    assert resp2.content == sig_bytes


def test_download_bundle_includes_sha256_header(client_with_token) -> None:
    import hashlib
    tc, token, _ = client_with_token
    bundle_data = make_bundle("mylib", "1.0.0")
    tc.post(
        "/publish",
        headers={"Authorization": f"Bearer {token}"},
        files={"file": ("mylib-1.0.0.cgbundle", bundle_data, "application/octet-stream")},
    )
    resp = tc.get("/bundles/mylib/1.0.0/mylib-1.0.0.cgbundle")
    assert resp.status_code == 200
    assert "x-bundle-sha256" in resp.headers
    assert resp.headers["x-bundle-sha256"] == hashlib.sha256(bundle_data).hexdigest()


def test_create_token_wrong_password(client: TestClient) -> None:
    resp = client.post(
        "/admin/tokens",
        headers={"Authorization": "Bearer wrong"},
        json={"label": "x"},
    )
    assert resp.status_code == 401


def test_list_tokens_via_api(client: TestClient) -> None:
    client.post(
        "/admin/tokens",
        headers={"Authorization": f"Bearer {ADMIN_PASSWORD}"},
        json={"label": "t1"},
    )
    resp = client.get("/admin/tokens", headers={"Authorization": f"Bearer {ADMIN_PASSWORD}"})
    assert resp.status_code == 200
    tokens = resp.json()["tokens"]
    assert len(tokens) == 1
    assert tokens[0]["label"] == "t1"
    # token values must not appear
    for t in tokens:
        assert "token" not in t


def test_revoke_token_via_api(client: TestClient) -> None:
    create_resp = client.post(
        "/admin/tokens",
        headers={"Authorization": f"Bearer {ADMIN_PASSWORD}"},
        json={"label": "to-revoke"},
    )
    token_value = create_resp.json()["token"]

    del_resp = client.delete(
        f"/admin/tokens/{token_value}",
        headers={"Authorization": f"Bearer {ADMIN_PASSWORD}"},
    )
    assert del_resp.status_code == 200

    # Verify publish is now rejected
    bundle_data = make_bundle("mylib", "1.0.0")
    resp = client.post(
        "/publish",
        headers={"Authorization": f"Bearer {token_value}"},
        files={"file": ("mylib-1.0.0.cgbundle", bundle_data, "application/octet-stream")},
    )
    assert resp.status_code == 401
