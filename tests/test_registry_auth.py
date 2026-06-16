"""Tests for sempkg_registry TokenStore."""

from __future__ import annotations

import pytest
from pathlib import Path

from sempkg_registry.auth import TokenStore


@pytest.fixture
def store(tmp_path: Path) -> TokenStore:
    return TokenStore(config_dir=tmp_path / "config")


def test_add_token_returns_new_token(store: TokenStore) -> None:
    result = store.add_token(label="test")
    # token must be a non-empty URL-safe string with at least 32 chars
    assert isinstance(result.token, str)
    assert len(result.token) >= 32
    # created_at must be an ISO-8601 timestamp
    assert "T" in result.created_at


def test_is_valid_after_add(store: TokenStore) -> None:
    token = store.add_token(label="ci").token
    assert store.is_valid(token) is True


def test_is_valid_unknown_token(store: TokenStore) -> None:
    assert store.is_valid("not-a-real-token") is False


def test_plaintext_not_in_tokens_json(store: TokenStore, tmp_path: Path) -> None:
    """Confirm no plaintext token appears in the persisted JSON file."""
    import json
    store2 = TokenStore(config_dir=tmp_path / "sec")
    result = store2.add_token(label="security-check")
    raw = json.loads(store2._path.read_text(encoding="utf-8"))
    # The raw token must not appear as a key or in any value
    assert result.token not in raw
    assert result.token not in json.dumps(raw)


def test_revoke_existing_token(store: TokenStore) -> None:
    token = store.add_token(label="to-revoke").token
    assert store.revoke_token(token) is True
    assert store.is_valid(token) is False


def test_revoke_unknown_token(store: TokenStore) -> None:
    assert store.revoke_token("nonexistent") is False


def test_list_tokens_no_values(store: TokenStore) -> None:
    result = store.add_token(label="visible-label")
    listing = store.list_tokens()
    assert len(listing) == 1
    assert listing[0]["label"] == "visible-label"
    assert "created_at" in listing[0]
    # Plaintext token must NOT appear in the listing
    assert result.token not in str(listing)


def test_add_multiple_tokens(store: TokenStore) -> None:
    t1 = store.add_token(label="a").token
    t2 = store.add_token(label="b").token
    assert t1 != t2
    assert len(store.list_tokens()) == 2


def test_tokens_persisted(tmp_path: Path) -> None:
    """Token hash persists across TokenStore instances sharing the same config_dir."""
    config_dir = tmp_path / "shared"
    s1 = TokenStore(config_dir=config_dir)
    token = s1.add_token(label="persistent").token

    s2 = TokenStore(config_dir=config_dir)
    assert s2.is_valid(token) is True
