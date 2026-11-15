# Adding an MCP server

MCP (Model Context Protocol) servers give agents tools that Patina does not build in, such as web search, fetching pages or working with an issue tracker. Patina is an MCP client. It speaks to a server in one of two ways:

- **stdio**: Patina starts a program on this machine and talks to it over its standard input and output. Most servers published as npm or Python packages work this way.
- **Streamable HTTP**: Patina connects to a server at a URL.

## Add one

1. Open Settings, MCP servers, and choose **Add MCP server**.
2. Give it a short name. Its tools appear to models as `mcp__<name>__<tool>`.
3. For a program, enter the command and its arguments, one per line. For a server at a URL, enter the URL.
4. Add the environment variables (stdio) or headers (HTTP) the server needs, such as an API token. What you type is stored encrypted, like a provider key, and is never shown again. To take a variable from the daemon's own environment instead, type `env:NAME`.
5. Save, then choose **Show tools**. Patina starts the server and lists its tools. If the server fails to start, its own error output is shown.

The same settings live in `mcp.toml` in the config folder (see `docs/config.md`). Hand edits apply on the server's next use.

## Give an agent its tools

Edit the agent in Settings. Under "MCP tools the agent may use", tick **Every tool of <server>**, or choose single tools. In the agent's file this is `mcp__<server>` or `mcp__<server>__<tool>` in its `tools` list. MCP tools work in a plain chat as well as in a project.

When a run starts, Patina starts each server its agents use, unless it is already running. If a server cannot be reached, the run does not start and the error names the server. A disabled server gives its tools to no agent.

## Trust

An MCP tool asks for your approval before every call until you trust it. This holds in every approval mode, even "Never ask", because the server runs outside the sandbox and Patina cannot see what it does.

- On the approval card, **Approve and trust this tool** approves the call and trusts the tool from then on, in the running run and later ones.
- In Settings, **Show tools** lists the server's tools with a checkbox for each trusted one. Untick it to make the tool ask again. "Trust every tool" also covers tools the server adds later.

Trust a tool only when you are comfortable with any call a model could make to it. A search tool is a reasonable candidate; a tool that sends email or deletes records is not.

## What to keep in mind

- **A stdio server runs as you, outside the sandbox.** It is a program you chose to install; Patina does not contain it. It gets a scrubbed environment: only `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_ALL`, `TZ` and `TMPDIR` from the daemon, plus the variables you set for it. Your provider keys and Patina's master key are not passed.
- **Results are data, not instructions.** Each result reaches the model marked as coming from outside Patina, and Patina's system preamble tells the model to treat such text as data. A web page fetched through MCP can still try to steer the model. Approvals on the actions that matter are what protect you.
- **Images and binary content** in results are described to the model, not passed on.
- **One server process serves every run.** Patina keeps a connection open while it is used, starts the server again if it exits, and stops it when its settings change, when it is removed or disabled, and when Patina stops. Stopping a stdio server also stops any process it started.
- **Time limit.** Each call may take `timeout_s` seconds, 120 by default and at most 600.
- **No OAuth sign-in yet.** A remote server that needs OAuth cannot be used unless it also accepts a token in a header.

## Example: a web search server

```toml
[[server]]
id = "search"
command = "npx"
args = ["-y", "<the search server's npm package>"]
env = { API_KEY = "secret:mcp/search/API_KEY" }
```

Set `API_KEY` in Settings so it is stored encrypted, give an agent `mcp__search`, and trust the search tool once you have seen a few calls.
