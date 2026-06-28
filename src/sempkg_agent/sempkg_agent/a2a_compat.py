"""Small compatibility shims for ``a2a-sdk`` field-naming differences.

The generated A2A types have used both ``contextId`` and ``context_id`` across
releases. These helpers read whichever is present so the executor is resilient to
the pinned SDK version.
"""

from __future__ import annotations


def context_id(obj) -> str | None:
    """Return the conversation/context id of a Task or Message, spelling-agnostic."""
    return getattr(obj, "context_id", None) or getattr(obj, "contextId", None)


def task_id(obj) -> str | None:
    """Return the task id of a Task or Message, spelling-agnostic."""
    return getattr(obj, "task_id", None) or getattr(obj, "taskId", None) or getattr(obj, "id", None)
