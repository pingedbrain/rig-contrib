"""The official MCP Python SDK checking a rig-mcp-server, keyless.

    python mcp_client.py stdio <path/to/mcp_echo_server>
    python mcp_client.py http <url> [token]

Exits non-zero on the first check that fails. An independent implementation
of the protocol, so it checks the server against the spec rather than against
rmcp's own client.
"""

import asyncio
import base64
import sys
from contextlib import asynccontextmanager

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client
from mcp.client.streamable_http import streamablehttp_client


def check(condition: bool, what: str) -> None:
    print(("ok   " if condition else "FAIL ") + what)
    if not condition:
        sys.exit(1)


@asynccontextmanager
async def connect(mode: str, target: str, token: str | None):
    if mode == "stdio":
        async with stdio_client(StdioServerParameters(command=target)) as (read, write):
            async with ClientSession(read, write) as session:
                yield session
    else:
        headers = {"Authorization": f"Bearer {token}"} if token else {}
        async with streamablehttp_client(target, headers=headers) as (read, write, _):
            async with ClientSession(read, write) as session:
                yield session


async def main(mode: str, target: str, token: str | None) -> None:
    async with connect(mode, target, token) as session:
        await session.initialize()
        tools = {tool.name: tool for tool in (await session.list_tools()).tools}
        check(set(tools) == {"echo", "add"}, f"{mode}: tools are echo and add ({sorted(tools)})")
        check(tools["add"].annotations.readOnlyHint is True, f"{mode}: add is read-only")
        check(tools["echo"].outputSchema is not None, f"{mode}: echo declares an output schema")

        added = await session.call_tool("add", {"a": 2, "b": 40})
        check(added.structuredContent == {"sum": 42.0}, f"{mode}: add answers 42 ({added.structuredContent})")

        chunks: list[str] = []

        async def progress(_progress, _total, message):
            chunks.append(message)

        first = await session.call_tool("echo", {"message": "ping"}, progress_callback=progress)
        check(first.structuredContent["answer"] == "pong", f"{mode}: echo answers pong")
        check(chunks == ["po", "ng"], f"{mode}: the answer streamed as progress ({chunks})")
        session_id = first.structuredContent["session_id"]
        again = await session.call_tool("echo", {"message": "again", "session_id": session_id})
        check(again.structuredContent["session_id"] == session_id, f"{mode}: the session continues")

        resources = {str(r.uri): r for r in (await session.list_resources()).resources}
        check(set(resources) == {"memo://intro", "blob://logo"}, f"{mode}: resources are intro and logo ({sorted(resources)})")
        check(resources["memo://intro"].mimeType == "text/markdown", f"{mode}: intro is text/markdown")
        check(resources["blob://logo"].mimeType == "application/octet-stream", f"{mode}: logo is octet-stream")

        intro = await session.read_resource("memo://intro")
        text = intro.contents[0]
        check(
            text.text == "# Hello" and text.mimeType == "text/markdown",
            f"{mode}: intro reads back as markdown ({text.text!r}, {text.mimeType})",
        )
        logo = await session.read_resource("blob://logo")
        blob = logo.contents[0]
        check(
            blob.blob == base64.b64encode(bytes([1, 2, 3])).decode() and blob.mimeType == "application/octet-stream",
            f"{mode}: logo is the base64 of [1, 2, 3] ({blob.blob}, {blob.mimeType})",
        )

        prompts = (await session.list_prompts()).prompts
        check(
            [p.name for p in prompts] == ["explain"]
            and [(a.name, a.required) for a in prompts[0].arguments] == [("topic", True)],
            f"{mode}: explain takes a required topic",
        )
        rendered = await session.get_prompt("explain", {"topic": "borrowing"})
        message = rendered.messages[0]
        check(
            message.content.text == "Explain borrowing in one paragraph.",
            f"{mode}: explain renders its message ({message.content.text!r})",
        )


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1], sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else None))
