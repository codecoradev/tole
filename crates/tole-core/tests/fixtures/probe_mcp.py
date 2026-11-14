#!/usr/bin/env python3
"""Stdio MCP server for the probe lifecycle tests (#600).

argv: MODE DIR. It records its environment (DIR/env), its pid (DIR/server)
and the pid of a grandchild `sleep` it starts (DIR/grandchild), so a test
can check what the probe passed and that the whole process group is gone.

MODE ok: a normal server with one tool. hang: never answers. die: exits at
once, leaving the grandchild behind.
"""
import json
import os
import subprocess
import sys
import time

mode, d = sys.argv[1], sys.argv[2]
with open(os.path.join(d, "env"), "w") as f:
    f.write("\n".join(f"{k}={v}" for k, v in os.environ.items()))
gc = subprocess.Popen(["sleep", "300"])
with open(os.path.join(d, "grandchild"), "w") as f:
    f.write(str(gc.pid))
with open(os.path.join(d, "server"), "w") as f:
    f.write(str(os.getpid()))
if mode == "die":
    sys.exit(1)
if mode == "hang":
    time.sleep(300)


def reply(i, result):
    print(json.dumps({"jsonrpc": "2.0", "id": i, "result": result}), flush=True)


for line in sys.stdin:
    req = json.loads(line)
    m, i, p = req.get("method"), req.get("id"), req.get("params") or {}
    if i is None:
        continue
    if m == "initialize":
        reply(i, {
            "protocolVersion": p.get("protocolVersion", "2025-03-26"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "probe", "version": "1.2.3"},
        })
    elif m == "tools/list":
        reply(i, {"tools": [{
            "name": "add",
            "description": "Add two numbers.",
            "inputSchema": {"type": "object"},
        }]})
    else:
        reply(i, {})
