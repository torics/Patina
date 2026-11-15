"""A minimal MCP server over stdio, written without any MCP library, for Patina's tests.

It speaks protocol version 2025-06-18, as many servers in use do, and offers tools that report on its own
process: its environment, its process id, and a child it starts.
"""
import json
import os
import subprocess
import sys

TOOLS = [
    {
        "name": "echo",
        "description": "Return the text it is given.",
        "inputSchema": {
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    },
    {
        "name": "env",
        "description": "Return the value of an environment variable, or <unset>.",
        "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]},
    },
    {"name": "pid", "description": "Return this server's process id.", "inputSchema": {"type": "object"}},
    {
        "name": "spawn_child",
        "description": "Start a long sleep in a child process and return its process id.",
        "inputSchema": {"type": "object"},
    },
    {"name": "fail", "description": "Report an error.", "inputSchema": {"type": "object"}},
    {"name": "exit", "description": "Stop the server at once.", "inputSchema": {"type": "object"}},
    {"name": "dotted.name", "description": "A name a model API would refuse.", "inputSchema": {"type": "object"}},
]

children = []


def reply(msg_id, result=None, error=None):
    msg = {"jsonrpc": "2.0", "id": msg_id}
    if error is not None:
        msg["error"] = error
    else:
        msg["result"] = result
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def text(t, is_error=False):
    return {"content": [{"type": "text", "text": t}], "isError": is_error}


def call(name, args):
    if name == "echo":
        return text(args.get("text", ""))
    if name == "env":
        return text(os.environ.get(args.get("name", ""), "<unset>"))
    if name == "pid":
        return text(str(os.getpid()))
    if name == "spawn_child":
        p = subprocess.Popen(["sleep", "300"])
        children.append(p)
        return text(str(p.pid))
    if name == "fail":
        return text("it went wrong", True)
    if name == "exit":
        os._exit(0)
    if name == "dotted.name":
        return text("dotted")
    return None


sys.stderr.write("test server started\n")
sys.stderr.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    msg_id = msg.get("id")
    if msg_id is None:
        continue  # a notification
    if method == "initialize":
        reply(msg_id, {
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "patina-test-stdio", "version": "1.0.0"},
        })
    elif method == "ping":
        reply(msg_id, {})
    elif method == "tools/list":
        reply(msg_id, {"tools": TOOLS})
    elif method == "tools/call":
        params = msg.get("params", {})
        result = call(params.get("name"), params.get("arguments") or {})
        if result is None:
            reply(msg_id, error={"code": -32602, "message": "unknown tool"})
        else:
            reply(msg_id, result)
    else:
        reply(msg_id, error={"code": -32601, "message": "method not found"})
