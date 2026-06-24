# nudge

A Claude-Code-style coding agent for your terminal, written in Rust **from scratch** — no agent SDK, no framework, just the Anthropic Messages API over HTTP.

Built as a learning project: the goal is to understand what actually lives inside an agentic harness — the loop, the tool-use protocol, prompt-cache economics, session persistence, permission gating, and a TUI that makes agent activity legible. The code favors explicitness over abstraction so each mechanism is readable on its own.

## Features

- **Agentic loop** — the model plans, calls tools, observes results, and iterates until the task is done, bounded by an iteration budget that ends gracefully (the agent explains it hit the cap and asks how to proceed).
- **Tool surface** — `Bash`, `Read`, `Edit` (modify + append modes), `CreateNew`, `Grep` (structured ripgrep), `Glob`, `TodoWrite`. Tool names and field shapes follow Claude Code's wire format where the same tool exists in both.
- **TUI** (ratatui) — collapsed-by-default action display: each tool call is one compact group with a live status bullet (spinner → ok/error) and a one-line result row; `Ctrl-O` expands everything, including the model's thinking. Title bar shows session id, cwd, git branch, model, and platform.
- **Permission gating** — shell-executing and file-mutating tools prompt before running; read-only tools auto-allow. For `Bash`, the model must state an *intent* ("count lines in all Rust files") shown as the action label, while the permission prompt always shows the raw command — you approve what runs, not the label.
- **MCP client** — connect to external [Model Context Protocol](https://modelcontextprotocol.io) servers declared in a project-local `.mcp.json`. Their tools are discovered at startup and merged into the model's tool list (namespaced `server__tool`), indistinguishable from built-ins. Permission follows each tool's `readOnlyHint` annotation — read-only tools auto-allow, the rest prompt. Both local **stdio** subprocesses and remote **Streamable HTTP** servers are supported, the latter with static-token, OAuth (dynamic registration), or OAuth (pre-registered client) auth. Servers load in three layers — always-on foundational tools, always-on user servers from `.mcp.json`, and a built-in **dormant** catalog the user loads/unloads mid-session (`/mcp load <name>`) to keep the default context lean.
- **Sessions** — every conversation is appended to a JSONL log under `~/.nudge/projects/<flattened-cwd>/<uuid>.jsonl`; `--resume <id>` restores it, with strict truncation of any incomplete trailing turn.
- **Detachable, multi-process sessions** — the agent loop is decoupled from the UI, so a session outlives its front-end. `/background` detaches the TUI while the agent keeps working; reattach later and the full history replays. A session can also run headless behind a local socket (`--daemon`) and be driven from another terminal (`--connect`).
- **Prompt caching** — layered `cache_control` breakpoints on the system prompt plus a floating breakpoint that walks forward along the chat history. At ~100-message depth this cuts billed input ~7× and shortens time-to-first-token accordingly.
- **Adaptive thinking** — the model decides when to reason; `--thinking omitted` hides the reasoning text for faster first tokens at the same cost.

## Quick start

Requires a recent Rust toolchain (edition 2024) and an Anthropic API key.

```bash
git clone <this-repo> && cd nudge
echo 'ANTHROPIC_API_KEY=sk-ant-...' > .env   # .env is gitignored
cargo run
```

### Install the binary

`cargo install` builds an optimized `nudge` binary into `~/.cargo/bin` so you can run it from any directory:

```bash
cargo install --path .                 # from a local checkout
cargo install --git <this-repo>        # straight from git
```

The installed binary reads `ANTHROPIC_API_KEY` from the environment (a `.env` in the current directory still works), so export it in your shell profile:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
nudge
```


The agent operates in whatever directory you launch it from:

```bash
cd /path/to/your/project
cargo run --manifest-path /path/to/nudge/Cargo.toml
```

## MCP servers

Tools come in three layers, by how they're loaded:

| Layer | Source | Loaded |
|---|---|---|
| **Foundational** | the built-in native tools (`Bash`, `Read`, `Edit`, …) | always — part of the cached prompt prefix |
| **User-specified** | [MCP](https://modelcontextprotocol.io) servers you declare in a project-local `.mcp.json` | at startup, every session — also cached |
| **Dormant** | a built-in catalog of servers that ship with the agent | on demand, mid-session, via `/mcp load <name>` |

Foundational and user-specified tools are the stable set; loading a dormant server is the only thing that changes the tool surface after startup. The tool array is ordered so the stable set sits behind a cache breakpoint, so a load/unload re-processes only the dormant tail — the always-on definitions stay a cache hit.

### Loading dormant servers

Dormant servers are defined in code (`src/coding/mcp/catalog.rs`), so they need no config file and ship inside the binary. Manage them from the prompt:

```
/mcp                  list loaded servers + dormant ones available to load
/mcp load <name>      connect a dormant server (tools appear next turn)
/mcp unload <name>    disconnect it (kills the subprocess / closes the transport)
```

Only dormant servers can be unloaded; user-specified and foundational tools stay for the session. To add to the catalog, edit `dormant_catalog()` and rebuild.

### User-specified servers (`.mcp.json`)

Drop a `.mcp.json` in the directory you run the agent from to give it extra tools from [MCP](https://modelcontextprotocol.io) servers, loaded at startup. Copy the template to get started:

```bash
cp .mcp.example.json .mcp.json
```

The MCP *protocol* is standardized, but **how a client is configured is not** — there is no spec for the config file, so each client (Claude Desktop, Cursor, this one) has its own schema. The top-level shape follows the de-facto `mcpServers` convention, so server identity and transport fields paste in from other clients directly; the **auth fields are nudge–specific** and documented below. Unknown keys are ignored, so a config carrying another client's extra fields still loads.

### Config reference

Each entry under `mcpServers` is one server. The transport is inferred: a `url` makes it **remote HTTP**, otherwise a `command` makes it a local **stdio** subprocess.

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

**Secrets** go through env-var indirection (`token_env`, `client_secret_env`), not literals in the file. **OAuth tokens** are obtained interactively once (a browser opens for consent), then cached at `~/.nudge/mcp-auth/<server>.json` (mode `0600`) and reused on later runs. The OAuth redirect is a fixed loopback `http://127.0.0.1:8765/callback` — for a pre-registered client, register that URI verbatim with the provider.

### Examples

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

`gitlab` uses OAuth with dynamic client registration (no `client_id` — the agent registers itself at runtime). `gmail` uses OAuth with a pre-registered client, which requires a Google Cloud OAuth client (client id + secret, with `http://127.0.0.1:8765/callback` as a registered redirect URI).

Servers are launched/connected at startup; their tools appear as `server__tool`. A failed server is logged and skipped, and a missing or malformed `.mcp.json` is non-fatal — the agent just runs with its built-in tools. Keep `.mcp.json` out of version control if it holds credentials.

## Usage

```
nudge [OPTIONS]

OPTIONS:
    --resume <id>        Resume a previous session from ~/.nudge/projects/<cwd>/<id>.jsonl
                         (the id is shown in the TUI title bar)
    --thinking <mode>    Thinking display: summarized (default) or omitted
    --daemon             Run the session headless behind a local Unix socket (no
                         TUI); attach to it from another terminal with --connect
    --connect            Attach a TUI to a running --daemon (or a backgrounded
                         session) over its socket
    --socket <path>      Socket path for --daemon / --connect
                         (default: ~/.nudge/daemon.sock)
    -h, --help           Show help
```

### Detaching and multi-process

The session outlives its front-end. Inside the TUI, `/background` detaches it — the agent keeps running and buffering output — and pressing `Enter` reattaches, replaying the full history. While a session is backgrounded, another terminal can take over the *live* session with `nudge --connect`.

You can also start a session with no TUI at all: `nudge --daemon` hosts it headless behind a local socket, and `nudge --connect` attaches a TUI to it from elsewhere on the same machine. Both ends meet over a Unix socket (default `~/.nudge/daemon.sock`; override with `--socket`). For now a backgrounded or daemon session lives only as long as its launching process — surviving a closed terminal is planned.

### TUI controls

| Key | Action |
|---|---|
| `Enter` | send message |
| `Alt+Enter` / `Ctrl+Enter` / trailing `\` + `Enter` | insert newline (paste keeps newlines) |
| `Ctrl-O` | expand / collapse tool results and thinking |
| mouse wheel / `PgUp` `PgDn` / `Home` `End` | scroll / jump / resume tail-follow |
| `y` / `n` / `Esc` | answer permission prompt |
| `Ctrl-C` (or `Ctrl-D` on empty input) | quit |

## How it works

Three long-lived tokio tasks connected by mpsc channels: the **agent task** runs the loop and emits typed events (`AssistantText`, `ToolUseStart`, `ToolResult`, `PermissionRequest`, …); a **broker task** sits between the loop and the front-end — it buffers the event stream, admits one front-end at a time, and keeps the loop alive while front-ends attach and detach; the **front-end task** (the TUI) renders events and sends user messages back. Because the loop talks only to the broker, the session outlives any one front-end — you can detach and reattach, locally or from another process over a socket — and the agent never touches stdin/stdout directly, so the UI is swappable.

The code is layered into five modules with a downward dependency direction — `coding → core → llm`, plus `transport → core` and `tui` on top — so each layer can be understood (and swapped) on its own:

| Module | Role |
|---|---|
| `src/main.rs` | CLI parsing, session create/resume, run-mode wiring (in-process / daemon / connect) |
| `src/llm/` | provider-agnostic LLM API: a neutral message model + `Provider` trait, with `AnthropicProvider` owning all Anthropic wire shaping (cache-breakpoint placement, the floating breakpoint, the HTTP calls) |
| `src/core/` | the generic harness: the loop (build request → call provider → dispatch tools → repeat) + a `Backend` trait, the agent↔UI event contract, the broker that decouples the loop from the front-end, and the session mechanism (JSONL persistence, resume with strict truncation) — knows nothing about concrete tools or prompts |
| `src/coding/` | the coding agent: implements `Backend` — system prompt, project/env context, the tool registry + dispatch + permission classification, the MCP client, file-state tracking, and the cwd-keyed session path policy |
| `src/transport/` | lets a front-end drive a session from another process: a small framed wire protocol over a local socket, with daemon (host) and client ends behind a `SessionHandle` the TUI is generic over — so the same TUI code runs in-process or attached to a remote host |
| `src/tui/` | ratatui app: semantic log entries, collapsed/expanded rendering, permission modal, session replay |

The `core` loop is generic over both the `Provider` and the `Backend`, so a different model backend or a non-coding agent could reuse it untouched.

## Selected design decisions

- **Permission prompts await a typed reply.** The `PermissionRequest` event embeds a `oneshot::Sender<bool>`; the agent literally `.await`s your decision. A denial cancels the rest of the tool batch and pauses for your next instruction, which rides back in the same turn — denial means "I want something different", not "retry".
- **`Edit`/`CreateNew` instead of a `Write` tool.** Tools partition by file-state precondition: `Edit` requires an existing file, `CreateNew` requires a non-existing path. There is deliberately no create-or-overwrite primitive — wholesale-overwriting a file the model hasn't read would let it silently destroy content.
- **Crash-safe conversation state.** On API failure mid-turn, the in-memory message vec rolls back to the last completed turn so the conversation always stays on a valid alternating-role boundary. The JSONL log is independent and append-only — it's the audit trail, not the API payload.
- **Floating cache breakpoint.** Model output never changes once emitted, so each request moves a `cache_control` marker to the latest assistant message; the entire stable history is then a cache read (~0.1× input price) and only the newest messages pay full price.
- **TUI stores meaning, not pixels.** The log holds semantic entries (`ToolCall { id, name, summary, result }`), re-rendered per frame — that's what makes expand/collapse instant and lets session replay render identically to live. All foreign text (tool output, summaries, model text) is sanitized before it reaches a terminal cell, because cell-grid renderers don't interpret control characters.
- **A broker decouples the loop from the front-end.** The loop's event/command channels terminate at a long-lived broker rather than the UI, so attaching or detaching a front-end never ends the session — only an explicit quit does. The broker buffers the event stream (a reattaching front-end replays the whole transcript) and admits one controller at a time. The TUI also renders user turns and allow/deny lines *from that stream*, not optimistically on the keypress, so one code path serves both live input and replay — and `/background` plus the `--daemon`/`--connect` split fall out of it without the loop knowing which front-end, if any, is watching.

## Status

A learning project under active development — interfaces and on-disk formats may change without notice. Driving a running session remotely (e.g. from a phone) is in progress. Issues and discussion welcome.
