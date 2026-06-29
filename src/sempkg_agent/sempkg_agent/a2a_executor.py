"""A2A AgentExecutor bridging the A2A task lifecycle to the KnowledgeAgent.

A2A is the primary inbound protocol because it natively models the conversational
back-and-forth the user asked for: when the agent needs more information it puts
the task into the ``input-required`` state and the calling agent replies on the
same ``contextId`` to continue the task. We map the A2A ``contextId`` onto our
agent ``session_id`` so the LangGraph checkpointer replays the full history.
"""

from __future__ import annotations

import logging

from a2a.server.agent_execution import AgentExecutor, RequestContext
from a2a.server.events import EventQueue
from a2a.server.tasks import TaskUpdater
from a2a.types import DataPart, Part, TaskState, TextPart
from a2a.utils import new_agent_text_message, new_task

from .a2a_compat import context_id as _ctx_id
from .agent import KnowledgeAgent
from .render import render_clarification_markdown, render_result_markdown
from .schemas import ContextRequest

logger = logging.getLogger(__name__)


def _extract_data_parts(message) -> dict:
    """Collect any structured DataPart payloads from an inbound message."""
    out: dict = {}
    parts = getattr(message, "parts", None) or []
    for part in parts:
        root = getattr(part, "root", part)
        data = getattr(root, "data", None)
        if isinstance(data, dict):
            out.update(data)
    return out


class KnowledgeAgentExecutor(AgentExecutor):
    """Drives one A2A task through the KnowledgeAgent."""

    def __init__(self, agent: KnowledgeAgent) -> None:
        self._agent = agent

    async def execute(self, context: RequestContext, event_queue: EventQueue) -> None:
        task = context.current_task
        if task is None:
            task = new_task(context.message)
            await event_queue.enqueue_event(task)

        ctx_id = _ctx_id(task)
        updater = TaskUpdater(event_queue, task.id, ctx_id)
        await updater.update_status(TaskState.working)

        prompt = context.get_user_input()
        data = _extract_data_parts(context.message)
        package = data.get("package")
        max_findings = data.get("max_findings")

        if not prompt and not package:
            await updater.update_status(
                TaskState.input_required,
                new_agent_text_message(
                    "Please describe the context you need.", ctx_id, task.id
                ),
                final=True,
            )
            return

        request = ContextRequest(
            prompt=prompt or "",
            package=package,
            # The A2A contextId is the durable conversation key across turns.
            session_id=ctx_id,
            max_findings=max_findings,
        )

        try:
            answer = await self._agent.ask(request)
        except Exception as exc:  # noqa: BLE001 - surface as a failed task, don't crash the server
            logger.exception("Agent run failed")
            await updater.update_status(
                TaskState.failed,
                new_agent_text_message(f"Retrieval failed: {exc}", ctx_id, task.id),
                final=True,
            )
            return

        if answer.is_clarification():
            clar = answer.as_clarification()
            # input-required: the calling agent answers on the same contextId.
            await updater.update_status(
                TaskState.input_required,
                new_agent_text_message(clar.question, ctx_id, task.id),
                final=True,
            )
            # Also attach the structured clarification for machine consumers.
            await updater.add_artifact(
                [Part(root=DataPart(data=clar.model_dump()))],
                name="clarification",
            )
            return

        result = answer.as_result()
        await updater.add_artifact(
            [
                Part(root=DataPart(data=result.model_dump())),
                Part(root=TextPart(text=render_result_markdown(result))),
            ],
            name="context_result",
        )
        await updater.complete()

    async def cancel(self, context: RequestContext, event_queue: EventQueue) -> None:
        # The retrieval loop is short-lived; nothing external to tear down.
        task = context.current_task
        if task is not None:
            updater = TaskUpdater(event_queue, task.id, _ctx_id(task))
            await updater.update_status(TaskState.canceled, final=True)


# Exported for callers that want the markdown of a clarification too.
__all__ = ["KnowledgeAgentExecutor", "render_clarification_markdown"]
