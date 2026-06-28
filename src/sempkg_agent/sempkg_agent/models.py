"""Curated catalog of selectable OpenRouter models.

The chat UI exposes this as a dropdown. The list is deliberately small — a few
cheap options, a couple of mid-tier, and one or two strong reasoning models — to
keep cost predictable. Per-request model selection is validated against this
catalog so callers can't route arbitrary (potentially expensive) models.

Override the whole list with ``SEMPKG_AGENT_MODEL_CATALOG`` (a JSON array of
``{id,label,tier,note}`` objects). Slugs must match what your OpenRouter account
serves — adjust them as OpenRouter's offerings change.
"""

from __future__ import annotations

import logging
import os

from pydantic import BaseModel, TypeAdapter, ValidationError

logger = logging.getLogger(__name__)

Tier = str  # "cheap" | "medium" | "high"


class ModelOption(BaseModel):
    id: str  # OpenRouter slug, e.g. "anthropic/claude-3.5-sonnet"
    label: str
    tier: Tier
    note: str = ""


# Cheap → fast & inexpensive; Medium → balanced; High → strongest reasoning.
DEFAULT_CATALOG: list[ModelOption] = [
    ModelOption(
        id="openai/gpt-4o-mini",
        label="GPT-4o mini",
        tier="cheap",
        note="Fast & very cheap. Good for simple, well-scoped lookups.",
    ),
    ModelOption(
        id="google/gemini-2.0-flash-001",
        label="Gemini 2.0 Flash",
        tier="cheap",
        note="Cheap, fast, large context. Solid tool use.",
    ),
    ModelOption(
        id="deepseek/deepseek-v4-flash",
        label="DeepSeek V4 Flash",
        tier="cheap",
        note="Lowest cost. Capable but can over-search on hard tasks.",
    ),
    ModelOption(
        id="openai/gpt-4o",
        label="GPT-4o",
        tier="medium",
        note="Balanced quality and cost for most retrieval work.",
    ),
    ModelOption(
        id="anthropic/claude-3.5-haiku",
        label="Claude 3.5 Haiku",
        tier="medium",
        note="Strong instruction-following at moderate cost.",
    ),
    ModelOption(
        id="anthropic/claude-3.5-sonnet",
        label="Claude 3.5 Sonnet",
        tier="high",
        note="Excellent agentic reasoning; converges cleanly. Higher cost.",
    ),
    ModelOption(
        id="anthropic/claude-3.7-sonnet",
        label="Claude 3.7 Sonnet",
        tier="high",
        note="Top-tier reasoning for the hardest multi-step retrievals.",
    ),
]


def load_catalog(default_model: str | None = None) -> list[ModelOption]:
    """Return the model catalog, honouring the env override.

    Guarantees the configured ``default_model`` is selectable by appending it if
    it is not already present.
    """
    catalog = DEFAULT_CATALOG
    raw = os.environ.get("SEMPKG_AGENT_MODEL_CATALOG")
    if raw:
        try:
            catalog = TypeAdapter(list[ModelOption]).validate_json(raw)
        except ValidationError as exc:
            logger.warning("Invalid SEMPKG_AGENT_MODEL_CATALOG, using defaults: %s", exc)
            catalog = DEFAULT_CATALOG

    if default_model and not any(m.id == default_model for m in catalog):
        catalog = [
            *catalog,
            ModelOption(id=default_model, label=default_model, tier="default",
                        note="Server default model."),
        ]
    return catalog


def catalog_ids(default_model: str | None = None) -> set[str]:
    return {m.id for m in load_catalog(default_model)}


def is_allowed(model_id: str, default_model: str | None = None) -> bool:
    return model_id in catalog_ids(default_model)


# Convenience for serializing the catalog to JSON-able dicts (API responses).
def catalog_as_dicts(default_model: str | None = None) -> list[dict]:
    return [m.model_dump() for m in load_catalog(default_model)]
