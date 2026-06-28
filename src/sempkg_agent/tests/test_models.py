"""Model catalog tests (no network)."""

from __future__ import annotations

from sempkg_agent.models import catalog_ids, is_allowed, load_catalog


def test_default_catalog_has_tiers() -> None:
    cat = load_catalog()
    tiers = {m.tier for m in cat}
    assert "cheap" in tiers and "high" in tiers
    # A few cheap, some medium, 1-2 high — keep the list small and curated.
    assert 4 <= len(cat) <= 12


def test_default_model_is_appended_when_absent() -> None:
    cat = load_catalog("some/custom-model")
    assert any(m.id == "some/custom-model" for m in cat)
    assert is_allowed("some/custom-model", "some/custom-model")


def test_is_allowed_rejects_unknown() -> None:
    assert not is_allowed("evil/expensive-model")
    assert "openai/gpt-4o-mini" in catalog_ids()
