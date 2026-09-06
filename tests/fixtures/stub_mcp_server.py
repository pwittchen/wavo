#!/usr/bin/env python3
"""A minimal MCP server over stdio, standing in for `demix-mcp` in tests.

It implements only what wavo uses — `initialize`, `tools/list` and `tools/call`
— plus one tool that makes the process exit mid-call, so the restart policy of
§8.3 can be exercised without killing a real spleeter run.
"""

import json
import sys

TOOLS = [
    {
        "name": "process_audio",
        "description": "Process an audio source with demix.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": {"anyOf": [{"type": "string"}, {"type": "null"}], "default": None},
                "search": {"anyOf": [{"type": "string"}, {"type": "null"}], "default": None},
                "mode": {
                    "enum": ["nosplit", "2stems", "4stems", "5stems"],
                    "default": "nosplit",
                    "type": "string",
                },
                "output_dir": {"type": "string", "default": "output"},
                "cwd": {"anyOf": [{"type": "string"}, {"type": "null"}], "default": None},
            },
        },
    },
    {
        "name": "die",
        "description": "Exit without answering, the way a crashing server does.",
        "inputSchema": {"type": "object", "properties": {}},
    },
]


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def result(request_id, payload):
    send({"jsonrpc": "2.0", "id": request_id, "result": payload})


def call_tool(request_id, params):
    name = params.get("name")
    arguments = params.get("arguments") or {}

    if name == "die":
        sys.exit(1)

    if name == "process_audio":
        output_dir = arguments.get("output_dir", "")
        payload = {
            "ok": True,
            "command": ["demix", "-s", arguments.get("search", "")],
            "exit_code": 0,
            "stdout": "Detected key: C major (confidence: 87%)\n" + ("x" * 5000),
            "stderr": "",
            "output_dir": output_dir,
            "files": {
                "music/mp3/song_vocals.mp3": output_dir + "/music/mp3/song_vocals.mp3",
                "music/mp3/song_accompaniment.mp3": output_dir
                + "/music/mp3/song_accompaniment.mp3",
                "video/song.mkv": output_dir + "/video/song.mkv",
            },
            "cwd_seen": arguments.get("cwd", ""),
        }
        # Echoed back so a test can see the link arrived exactly as the model sent
        # it; `url` is one of the keys wavo keeps in a reduced result.
        if arguments.get("url"):
            payload["url"] = arguments["url"]
    else:
        payload = {"ok": False, "error": "unknown tool: %s" % name}

    result(
        request_id,
        {
            "content": [{"type": "text", "text": json.dumps(payload)}],
            "structuredContent": payload,
            "isError": not payload["ok"],
        },
    )


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except ValueError:
            continue

        method = message.get("method")
        request_id = message.get("id")

        # Notifications carry no id and get no answer.
        if request_id is None:
            continue

        if method == "initialize":
            version = (message.get("params") or {}).get("protocolVersion", "2025-06-18")
            result(
                request_id,
                {
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "stub-demix", "version": "0.0.1"},
                },
            )
        elif method == "tools/list":
            result(request_id, {"tools": TOOLS})
        elif method == "tools/call":
            call_tool(request_id, message.get("params") or {})
        elif method == "ping":
            result(request_id, {})
        else:
            send(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "error": {"code": -32601, "message": "method not found: %s" % method},
                }
            )


if __name__ == "__main__":
    main()
