# MCP servers

nudge is an [MCP](https://modelcontextprotocol.io) client: it can connect to external
Model Context Protocol servers and merge their tools into the model's tool list,
indistinguishable from the built-ins (namespaced `server__tool`). Permission follows
each tool's `readOnlyHint` annotation — read-only tools auto-allow, the rest prompt.
Both local **stdio** subprocesses and remote **Streamable HTTP** servers are supported,
the latter with static-token, OAuth (dynamic registration), or OAuth (pre-registered
client) auth.

## The three tool layers

Tools come in three layers, by how they're loaded:

| Layer | Source | Loaded |
|---|---|---|
| **Foundational** | the built-in native tools (`Bash`, `Read`, `Edit`, …) | always — part of the cached prompt prefix |
| **User-specified** | [MCP](https://modelcontextprotocol.io) servers you declare in a project-local `.mcp.json` | at startup, every session — also cached |
| **Dormant** | a built-in catalog of servers that ship with the agent | on demand, mid-session, via `/mcp load <name>` |

Foundational and user-specified tools are the stable set; loading a dormant server is the
only thing that changes the tool surface after startup. The tool array is ordered so the
stable set sits behind a cache breakpoint, so a load/unload re-processes only the dormant
tail — the always-on definitions stay a cache hit.

## Loading dormant servers

Dormant servers are defined in code (`src/coding/mcp/catalog.rs`), so they need no config
file and ship inside the binary. Manage them from the prompt:

```
/mcp                  list loaded servers + dormant ones available to load
/mcp load <name>      connect a dormant server (tools appear next turn)
/mcp unload <name>    disconnect it (kills the subprocess / closes the transport)
```

Only dormant servers can be unloaded; user-specified and foundational tools stay for the
session. To add to the catalog, edit `dormant_catalog()` and rebuild.

## User-specified servers (`.mcp.json`)

Drop a `.mcp.json` in the directory you run the agent from to give it extra tools from
[MCP](https://modelcontextprotocol.io) servers, loaded at startup. Copy the template to
get started:

```bash
cp .mcp.example.json .mcp.json
```

The MCP *protocol* is standardized, but **how a client is configured is not** — there is
no spec for the config file, so each client (Claude Desktop, Cursor, this one) has its own
schema. The top-level shape follows the de-facto `mcpServers` convention, so server
identity and transport fields paste in from other clients directly; the **auth fields are
nudge–specific** and documented below. Unknown keys are ignored, so a config carrying
another client's extra fields still loads.

## Config reference

Each entry under `mcpServers` is one server. The transport is inferred: a `url` makes it
**remote HTTP**, otherwise a `command` makes it a local **stdio** subprocess.

| Field | Applies to | Meaning |
|---|---|---|
| `command` | stdio | Executable to launch (e.g. `npx`). |
| `args` | stdio | Argument list. |
| `env` | stdio | Extra environment variables for the subprocess. |
| `url` | http | Streamable HTTP endpoint. Presence of this field selects the HTTP transport. |
| `auth` | http | `"oauth"` runs the OAuth flow; absent (or anything else) uses the static token below. |
| `token` | http | Literal bearer token. Convenient, but keep it out of committed files. |
| `token_env` | http | Name of an env var holding the bearer token — preferred over `token` for secrets. |
| `scopes` | http (oauth) | OAuth scopes to request. Optional; empty lets the server choose. |
| `client_id` | http (oauth) | Pre-registered OAuth client id. **Present ⇒ pre-registered flow** (e.g. Google, which doesn't support dynamic registration); **absent ⇒ dynamic client registration (DCR)**. |
| `client_secret_env` | http (oauth) | Name of an env var holding the pre-registered client secret (kept out of the file). |

**Secrets** go through env-var indirection (`token_env`, `client_secret_env`), not literals
in the file. **OAuth tokens** are obtained interactively once (a browser opens for
consent), then cached at `~/.nudge/mcp-auth/<server>.json` (mode `0600`) and reused on
later runs. The OAuth redirect is a fixed loopback `http://127.0.0.1:8765/callback` — for a
pre-registered client, register that URI verbatim with the provider.

## Examples

```json
{
  "mcpServers": {
    "everything": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-everything"]
    },
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "ghp_..." }
    },
    "static-remote": {
      "url": "https://example.com/mcp",
      "token_env": "EXAMPLE_MCP_TOKEN"
    },
    "gitlab": {
      "url": "https://gitlab.com/api/v4/mcp",
      "auth": "oauth"
    },
    "gmail": {
      "url": "https://gmailmcp.googleapis.com/mcp/v1",
      "auth": "oauth",
      "client_id": "<...>.apps.googleusercontent.com",
      "client_secret_env": "GMAIL_MCP_SECRET",
      "scopes": [
        "https://www.googleapis.com/auth/gmail.readonly",
        "https://www.googleapis.com/auth/gmail.compose"
      ]
    }
  }
}
```

`gitlab` uses OAuth with dynamic client registration (no `client_id` — the agent registers
itself at runtime). `gmail` uses OAuth with a pre-registered client, which requires a
Google Cloud OAuth client (client id + secret, with `http://127.0.0.1:8765/callback` as a
registered redirect URI).

Servers are launched/connected at startup; their tools appear as `server__tool`. A failed
server is logged and skipped, and a missing or malformed `.mcp.json` is non-fatal — the
agent just runs with its built-in tools. Keep `.mcp.json` out of version control if it
holds credentials.
