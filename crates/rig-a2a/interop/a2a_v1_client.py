"""The official A2A Python SDK (v1.x) calling an agent over A2A v1.0.

Streams one message and exits non-zero unless the task completes with an
answer containing EXPECT. An independent implementation of the protocol, so
it checks the server against the spec rather than against rig-a2a's own client.

    python a2a_v1_client.py <url> <text> <expect> [token]
"""

import asyncio
import sys
import uuid

import httpx
from a2a.client import ClientConfig, create_client
from a2a.types import Message, Part, Role, SendMessageRequest, TaskState


async def main(url: str, text: str, expect: str, token: str | None) -> None:
    headers = {"Authorization": f"Bearer {token}"} if token else {}
    async with httpx.AsyncClient(headers=headers, timeout=60) as http:
        client = await create_client(url, ClientConfig(httpx_client=http, streaming=True))
        message = Message(message_id=str(uuid.uuid4()), role=Role.ROLE_USER, parts=[Part(text=text)])
        answer, state = "", None
        async for event in client.send_message(SendMessageRequest(message=message)):
            kind = event.WhichOneof("payload")
            if kind == "artifact_update":
                chunk = "".join(part.text for part in event.artifact_update.artifact.parts)
                answer = answer + chunk if event.artifact_update.append else chunk
            elif kind == "status_update":
                state = event.status_update.status.state
            elif kind == "task":
                state = event.task.status.state
    print(f"a2a-sdk v1 <- {url}: {answer!r} ({TaskState.Name(state) if state is not None else None})")
    if state != TaskState.TASK_STATE_COMPLETED or expect not in answer:
        sys.exit(f"expected a completed task containing {expect!r}")


asyncio.run(main(sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4] if len(sys.argv) > 4 else None))
