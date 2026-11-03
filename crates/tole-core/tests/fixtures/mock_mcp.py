#!/usr/bin/env python3
"""Minimal stdio MCP server for the prompts/resources client tests (#213).

argv[1] = "full" advertises prompts + resources; "bare" advertises neither
and answers prompts/* and resources/* with "method not found" so a client
that fails to probe capabilities first is caught.
"""
import json
import sys

mode = sys.argv[1] if len(sys.argv) > 1 else "full"


def reply(i, result=None, error=None):
    msg = {"jsonrpc": "2.0", "id": i}
    if error:
        msg["error"] = error
    else:
        msg["result"] = result
    print(json.dumps(msg), flush=True)


for line in sys.stdin:
    req = json.loads(line)
    m, i, p = req.get("method"), req.get("id"), req.get("params") or {}
    if i is None:
        continue  # notification
    if m == "initialize":
        caps = {"tools": {}}
        if mode == "full":
            caps["prompts"] = {}
            caps["resources"] = {}
        reply(i, {
            "protocolVersion": p.get("protocolVersion", "2025-03-26"),
            "capabilities": caps,
            "serverInfo": {"name": "mock", "version": "0"},
        })
    elif m == "tools/list":
        reply(i, {"tools": []})
    elif mode == "bare":
        reply(i, error={"code": -32601, "message": "method not found"})
    elif m == "prompts/list":
        reply(i, {"prompts": [{
            "name": "review",
            "description": "Review a topic",
            "arguments": [{"name": "topic", "required": True}],
        }]})
    elif m == "prompts/get":
        topic = (p.get("arguments") or {}).get("topic", "?")
        reply(i, {"messages": [{
            "role": "user",
            "content": {"type": "text", "text": f"Review: {topic}"},
        }]})
    elif m == "resources/read":
        reply(i, {"contents": [{
            "uri": p["uri"], "mimeType": "text/plain", "text": "demo body",
        }]})
    else:
        reply(i, error={"code": -32601, "message": "method not found"})
