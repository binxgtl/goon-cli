# MCP tools

Goon has its own MCP client (protocol revisions 2024-10-07 through 2025-11-25,
negotiated at connect). It supports stdio processes and Streamable HTTP
endpoints. Servers are configured only in `$XDG_CONFIG_HOME/goon/mcp.json`
(default `~/.config/goon/mcp.json`). Provider `goon init` does not change this
file. Project-local MCP config is not loaded.

## Example

Create the file with permissions 0600. Replace paths/URLs and enable only the
servers you intend to use. Commands/args are argv, not shell commands: `~`, `$HOME`
and pipes are not expanded. Stdio starts in the selected workspace.

```json
{
  "servers": {
    "local": {
      "transport": "stdio",
      "command": "node",
      "args": ["/absolute/path/to/server.mjs"],
      "envFrom": { "SERVICE_API_KEY": "MY_SERVICE_KEY" },
      "readOnlyTools": [],
      "net": false,
      "allowNet": false,
      "timeoutMs": 30000,
      "disabled": true
    },
    "remote": {
      "transport": "http",
      "url": "https://YOUR_MCP_HOST/mcp",
      "bearerEnv": "MY_MCP_TOKEN",
      "readOnlyTools": [],
      "net": false,
      "allowNet": false,
      "timeoutMs": 30000,
      "disabled": true
    }
  }
}
```

`envFrom` maps child variable names to names in the current shell, never literal
secret values. The SDK's normal safe environment (PATH, HOME, etc.) is retained;
provider keys are not automatically forwarded. On Termux, add mappings for
PREFIX/TMPDIR if a particular server needs them. A missing referenced variable
fails startup. An HTTP server without authentication can omit bearerEnv. No
account login or OAuth flow is performed; configure a token usable by the server.
HTTP redirects carrying credentials are rejected.

`goon mcp list` lists names, transport and read-only allowlists without connecting.
`goon mcp check` starts/connects enabled servers, enumerates tools (including
paginated lists), prints original and stable namespaced names, then closes them.
Neither command requires a model key. `check` does run configured stdio commands.
Normal `goon` startup connects enabled servers before the first model request.
Servers start concurrently (startup time is the slowest server, not the sum), and
running servers are reused when the harness is rebuilt for a new effort, model or
session; `/mcp reload`, `/mcp check` and `/cd` restart them. The MCP SDK is only
loaded when at least one server is enabled.
`--no-mcp` skips configuration loading and connection for that invocation.

## Permissions and lifetime

All tools require approval unless their **original** names appear in that
server's `readOnlyTools`. Inspect a server and its actual behavior before adding
entries. Server-provided annotations are untrusted metadata and may only
escalate: `destructiveHint`/`openWorldHint` raise a tool to high risk, and
`readOnlyHint` still leaves it at the medium floor. `trust: true` auto-approves
only medium/low-risk calls; it never lowers risk and never makes a tool
read-only. Only the user-controlled `readOnlyTools` list lowers a tool to low and
exposes it in `--read-only`. Read-only mode exposes only built-in read tools,
research delegation and MCP allowlisted tools. This is tool policy, not an OS
sandbox: MCP processes run as this user, and starting a server can itself perform
I/O. Remote tools execute on the remote service. Goon cannot enforce remote
filesystem containment.

Network policy (v0.5): a tool is marked `net` when the server sets `net: true`
or the tool declares `openWorldHint: true`. Such tools are denied by the
execution policy before approval unless network access is permitted globally
(`--allow-net` / `allowNet: true`) or for that server (`allowNet: true`). HTTP
transport alone does not mark every tool as network. A server allowlist in
`--allow-net-hosts` does not exempt MCP tools because their destination hosts are
not knowable from metadata.

JSON Schema arguments are validated locally before dispatch. Results are bounded
to 32 KiB (UTF-8 bytes, head+tail) via the execution policy; non-text blocks are
represented by markers. A tool result with
isError becomes an error result for the model. External tool descriptions and
output remain external data, not permission to change the user's task.

Each server can be disabled independently. A startup error closes connected
clients, unregisters their tools and releases the main session lock. Cancelled
calls propagate AbortSignal through the SDK. Shutdown attempts HTTP session
termination (bounded to two seconds per server), then closes all transports;
stdio shutdown uses the SDK's child-process cleanup. Server stderr is discarded
to keep terminal/NDJSON output clean. Inspect server logs separately for debugging.

Limits: 16 servers, 128 total tools, 16 continuation pages, 16k characters per
input schema, 2 MiB stdio message buffer, 100–600000 ms configurable request
timeout (default 30000). Startup fails on duplicates or invalid/excessive schemas
rather than silently dropping tools. In interactive TUI sessions, `/mcp` reloads
server connections in place; MCP resources (`list_mcp_resources`,
`list_mcp_resource_templates`, `read_mcp_resource`) are also supported. Prompt
templates, sampling, elicitation, OAuth, legacy SSE and experimental MCP tasks
are not implemented.
