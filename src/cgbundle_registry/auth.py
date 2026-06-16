"""Token management for cgbundle_registry.

Security design
---------------
* Tokens are generated with ``secrets.token_urlsafe(32)`` — 256 bits of
  cryptographically-secure entropy, URL-safe base64 encoded.
* Only the SHA-256 hash of each token is persisted to ``tokens.json``.
  The plaintext token is shown to the administrator exactly once (at
  creation time) and is never stored, so a compromised config file cannot
  be used to authenticate directly.
* ``is_valid()`` computes the hash of the submitted token and performs an
  O(1) dictionary lookup — there is no iteration and no timing oracle.
* ``tokens.json`` is written with mode ``0o600`` on POSIX so only the
  owning user can read it.
"""

from __future__ import annotations

import hashlib
import json
import os
import secrets
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import NamedTuple


class NewToken(NamedTuple):
    """Returned by :meth:`TokenStore.add_token`.

    ``token`` is the raw credential — the caller must show it to the user
    exactly once and then discard it; it is not stored anywhere.
    """

    token: str
    created_at: str


def _hash_token(token: str) -> str:
    """Return the hex-encoded SHA-256 digest of *token*."""
    return hashlib.sha256(token.encode("utf-8")).hexdigest()


class TokenStore:
    """Persistent token store backed by a JSON file.

    Token plaintext is **never** written to disk.  ``tokens.json`` maps
    ``SHA-256(token)`` → ``{label, created_at}``.
    """

    def __init__(self, config_dir: Path | None = None) -> None:
        self.config_dir = config_dir or Path.home() / ".cgbundle-registry"
        self.config_dir.mkdir(parents=True, exist_ok=True)
        self._path = self.config_dir / "tokens.json"

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def _load(self) -> dict[str, dict]:
        if not self._path.exists():
            return {}
        with self._path.open(encoding="utf-8") as fh:
            return json.load(fh)

    def _save(self, data: dict[str, dict]) -> None:
        with self._path.open("w", encoding="utf-8") as fh:
            json.dump(data, fh, indent=2)
        # Restrict to owner-read/write only on POSIX (no-op on Windows).
        if sys.platform != "win32":
            os.chmod(self._path, 0o600)

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def add_token(self, label: str = "") -> NewToken:
        """Generate a cryptographically-secure publish token.

        The SHA-256 hash is stored; the plaintext is returned inside a
        :class:`NewToken` and must be shown to the user exactly once.
        """
        token = secrets.token_urlsafe(32)  # 256-bit entropy, URL-safe base64
        created_at = datetime.now(tz=timezone.utc).isoformat()
        data = self._load()
        data[_hash_token(token)] = {"label": label, "created_at": created_at}
        self._save(data)
        return NewToken(token=token, created_at=created_at)

    def revoke_token(self, token: str) -> bool:
        """Remove a token by plaintext value. Returns True if it existed."""
        token_hash = _hash_token(token)
        data = self._load()
        if token_hash in data:
            del data[token_hash]
            self._save(data)
            return True
        return False

    def is_valid(self, token: str) -> bool:
        """Return True if the token hash exists in the store.

        O(1) dict lookup — no iteration, no timing oracle.
        """
        return _hash_token(token) in self._load()

    def list_tokens(self) -> list[dict]:
        """Return ``[{label, created_at}, …]`` — token values are never returned."""
        return [
            {"label": meta["label"], "created_at": meta["created_at"]}
            for meta in self._load().values()
        ]
