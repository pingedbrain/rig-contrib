"""Strands Agents calling an A2A agent; exits non-zero unless the answer contains EXPECT.

    python strands_client.py <url> <text> <expect> [token]
"""

import asyncio
import sys

import httpx
from a2a.client import ClientConfig
from strands.agent.a2a_agent import A2AAgent


async def main(url: str, text: str, expect: str, token: str | None) -> None:
    headers = {"Authorization": f"Bearer {token}"} if token else {}
    async with httpx.AsyncClient(headers=headers, timeout=60) as http:
        remote = A2AAgent(endpoint=url, client_config=ClientConfig(httpx_client=http))
        answer = str(await remote.invoke_async(text))
    print(f"strands <- {url}: {answer!r}")
    if expect not in answer:
        sys.exit(f"expected {expect!r} in {answer!r}")


asyncio.run(main(sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4] if len(sys.argv) > 4 else None))
