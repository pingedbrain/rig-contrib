"""A Strands Agents A2A server whose model echoes the last user message.

Deterministic and keyless, for interoperability tests: `echo: <text>` comes
back, streamed in two chunks. Strands 1.x speaks A2A v0.3.

    PORT=9000 [COMPLIANT=1] python strands_echo_server.py

COMPLIANT=1 turns on Strands' spec-compliant streaming (answer chunks as
artifact updates); without it Strands streams them as `working` status
messages, its default in 1.x. rig-a2a must understand both.
"""

import os
from collections.abc import AsyncIterator
from typing import Any

from strands import Agent
from strands.models.model import Model
from strands.multiagent.a2a import A2AServer


class EchoModel(Model):
    """Answers `echo: <last user text>`."""

    def update_config(self, **config: Any) -> None:
        pass

    def get_config(self) -> dict[str, Any]:
        return {}

    def structured_output(self, *args: Any, **kwargs: Any):  # noqa: ANN201
        raise NotImplementedError

    async def stream(self, messages, tool_specs=None, system_prompt=None, **kwargs: Any) -> AsyncIterator[dict]:
        last = next(m for m in reversed(messages) if m["role"] == "user")
        text = "".join(block.get("text", "") for block in last["content"])
        answer = f"echo: {text}"
        half = len(answer) // 2
        yield {"messageStart": {"role": "assistant"}}
        yield {"contentBlockStart": {"start": {}}}
        for chunk in (answer[:half], answer[half:]):
            yield {"contentBlockDelta": {"delta": {"text": chunk}}}
        yield {"contentBlockStop": {}}
        yield {"messageStop": {"stopReason": "end_turn"}}
        yield {"metadata": {"usage": {"inputTokens": 1, "outputTokens": 1, "totalTokens": 2}, "metrics": {"latencyMs": 1}}}


agent = Agent(
    name="strands-echo",
    description="Echoes every message back.",
    model=EchoModel(),
    callback_handler=None,
)
A2AServer(
    agent=agent,
    host="127.0.0.1",
    port=int(os.environ.get("PORT", "9000")),
    enable_a2a_compliant_streaming=os.environ.get("COMPLIANT") == "1",
).serve()
