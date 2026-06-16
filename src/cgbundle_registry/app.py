"""FastAPI application factory for cgbundle_registry."""

from __future__ import annotations

import io
import json
import secrets
import tarfile
from pathlib import Path

from fastapi import FastAPI, File, Header, HTTPException, Request, UploadFile
from fastapi.responses import FileResponse, JSONResponse

from .auth import TokenStore
from .storage import BundleStorage, VersionExistsError

# Maximum accepted upload for POST /publish (bytes).
# Requests larger than this are rejected with HTTP 413 before the full body
# is read, protecting the server from memory-exhaustion attacks.
_MAX_UPLOAD_BYTES = 500 * 1024 * 1024  # 500 MB


def _require_admin(
    authorization: str | None,
    admin_password: str,
) -> None:
    """Raise 401 if the Authorization header does not carry the admin password."""
    if not authorization or not authorization.startswith("Bearer "):
        raise HTTPException(status_code=401, detail="Missing or malformed Authorization header")
    provided = authorization.removeprefix("Bearer ").strip()
    if not secrets.compare_digest(provided, admin_password):
        raise HTTPException(status_code=401, detail="Invalid admin password")


def _safe_path_component(value: str, label: str = "value") -> str:
    """Reject any path component that contains directory traversal characters."""
    if "/" in value or "\\" in value or ".." in value or value.startswith("."):
        raise HTTPException(status_code=400, detail=f"Invalid {label}: {value!r}")
    return value


def _extract_manifest(data: bytes) -> dict:
    """Extract and parse manifest.json from a .cgbundle (tar.gz) byte blob.

    Raises HTTPException 400 if the archive is invalid or manifest is missing.
    """
    buf = io.BytesIO(data)
    if not tarfile.is_tarfile(buf):
        raise HTTPException(status_code=400, detail="Uploaded file is not a valid tar archive")
    buf.seek(0)
    with tarfile.open(fileobj=buf, mode="r:gz") as tf:
        # manifest.json may be at the root or one directory deep
        manifest_member = None
        for member in tf.getmembers():
            parts = Path(member.name).parts
            if parts and parts[-1] == "manifest.json":
                manifest_member = member
                break
        if manifest_member is None:
            raise HTTPException(status_code=400, detail="Bundle is missing manifest.json")
        fh = tf.extractfile(manifest_member)
        if fh is None:
            raise HTTPException(status_code=400, detail="Cannot read manifest.json from bundle")
        return json.loads(fh.read())


def create_app(
    storage: BundleStorage,
    token_store: TokenStore,
    admin_password: str,
) -> FastAPI:
    """Return a configured FastAPI application instance."""
    if not admin_password:
        raise ValueError("admin_password must not be empty")

    app = FastAPI(title="cgbundle Registry", version="0.1.0")

    # ------------------------------------------------------------------
    # GET /index.json
    # ------------------------------------------------------------------

    @app.get("/index.json")
    def get_index() -> JSONResponse:
        return JSONResponse(content=storage.load_index())

    # ------------------------------------------------------------------
    # GET /bundles/{package}/{version}/{filename}
    # ------------------------------------------------------------------

    @app.get("/bundles/{package}/{version}/{filename}")
    def download_bundle(package: str, version: str, filename: str) -> FileResponse:
        _safe_path_component(package, "package")
        _safe_path_component(version, "version")
        _safe_path_component(filename, "filename")

        expected_bundle = f"{package}-{version}.cgbundle"
        expected_sig = f"{package}-{version}.cgbundle.sig"

        if filename == expected_sig:
            sig_path = storage.get_signature_path(package, version)
            if sig_path is None:
                raise HTTPException(status_code=404, detail="Signature not found")
            return FileResponse(path=str(sig_path), media_type="application/octet-stream", filename=filename)

        if filename != expected_bundle:
            raise HTTPException(
                status_code=400,
                detail=f"Filename must be {expected_bundle!r}",
            )

        path = storage.get_path(package, version)
        if path is None:
            raise HTTPException(status_code=404, detail="Bundle not found")

        headers: dict[str, str] = {}
        sha256_sidecar = path.parent / f"{package}-{version}.sha256"
        if sha256_sidecar.exists():
            headers["X-Bundle-SHA256"] = sha256_sidecar.read_text(encoding="utf-8").strip()

        return FileResponse(
            path=str(path),
            media_type="application/octet-stream",
            filename=filename,
            headers=headers,
        )

    # ------------------------------------------------------------------
    # POST /publish
    # ------------------------------------------------------------------

    @app.post("/publish")
    async def publish_bundle(
        file: UploadFile = File(...),
        signature: UploadFile | None = File(default=None),
        authorization: str | None = Header(default=None),
    ) -> JSONResponse:
        # Validate token
        if not authorization or not authorization.startswith("Bearer "):
            raise HTTPException(status_code=401, detail="Missing or malformed Authorization header")
        token = authorization.removeprefix("Bearer ").strip()
        if not token_store.is_valid(token):
            raise HTTPException(status_code=401, detail="Invalid token")

        # Read upload — enforce size limit before full-body processing.
        data = await file.read(_MAX_UPLOAD_BYTES + 1)
        if len(data) > _MAX_UPLOAD_BYTES:
            raise HTTPException(status_code=413, detail="Bundle exceeds maximum upload size (500 MB)")

        sig_bytes = await signature.read() if signature else None

        # Validate and extract manifest
        manifest = _extract_manifest(data)

        name = manifest.get("name")
        version = manifest.get("version")
        if not name or not version:
            raise HTTPException(status_code=400, detail="manifest.json missing 'name' or 'version'")

        _safe_path_component(str(name), "name")
        _safe_path_component(str(version), "version")

        # Store
        try:
            storage.store(str(name), str(version), data, sig_bytes=sig_bytes)
        except VersionExistsError:
            raise HTTPException(status_code=409, detail=f"Version {name} {version} already exists")

        # Rebuild index
        storage.rebuild_index()

        return JSONResponse(content={"status": "ok", "name": name, "version": version})

    # ------------------------------------------------------------------
    # POST /admin/tokens
    # ------------------------------------------------------------------

    @app.post("/admin/tokens")
    async def create_token(
        request: Request,
        authorization: str | None = Header(default=None),
    ) -> JSONResponse:
        _require_admin(authorization, admin_password)
        try:
            body = await request.json()
        except Exception:
            body = {}
        label = body.get("label", "") if isinstance(body, dict) else ""
        new_token = token_store.add_token(label=label)
        return JSONResponse(
            content={"token": new_token.token, "label": label, "created_at": new_token.created_at},
            status_code=201,
        )

    # ------------------------------------------------------------------
    # DELETE /admin/tokens/{token}
    # ------------------------------------------------------------------

    @app.delete("/admin/tokens/{token}")
    def revoke_token(
        token: str,
        authorization: str | None = Header(default=None),
    ) -> JSONResponse:
        _require_admin(authorization, admin_password)
        if not token_store.revoke_token(token):
            raise HTTPException(status_code=404, detail="Token not found")
        return JSONResponse(content={"status": "revoked"})

    # ------------------------------------------------------------------
    # GET /admin/tokens
    # ------------------------------------------------------------------

    @app.get("/admin/tokens")
    def list_tokens(
        authorization: str | None = Header(default=None),
    ) -> JSONResponse:
        _require_admin(authorization, admin_password)
        return JSONResponse(content={"tokens": token_store.list_tokens()})

    return app
