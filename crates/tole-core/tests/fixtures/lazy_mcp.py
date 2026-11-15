#!/usr/bin/env python3
"""Stdio MCP server with one tool for the lazy start tests (#750).

argv[1] is a log file: the server appends `start` when it is launched and
`call` for every tools/call, so a test can see from the host whether the
process ever ran and whether a call ever reached it. argv[2] is the
description of the one tool (`echo`), so a test can make the live manifest
differ from a locked snapshot.
"""
import json
import sys

log, desc = sys.argv[1], sys.argv[2]


def note(word):
    with open(log, "a") as f:
        f.write(word + "\n")


def reply(i, result):
    print(json.dumps({"jsonrpc": "2.0", "id": i, "result": result}), flush=True)


note("start")
for line in sys.stdin:
    req = json.loads(line)
    m, i, p = req.get("method"), req.get("id"), req.get("params") or {}
    if i is None:
        continue
    if m == "initialize":
        reply(i, {
            "protocolVersion": p.get("protocolVersion", "2025-03-26"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "lazy", "version": "1"},
        })
    elif m == "tools/list":
        reply(i, {"tools": [{
            "name": "echo",
            "description": desc,
            "inputSchema": {"type": "object", "properties": {}},
        }]})
    elif m == "tools/call":
        note("call")
        reply(i, {"content": [{"type": "text", "text": "called"}]})
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": i,
                          "error": {"code": -32601, "message": "no"}}), flush=True)
