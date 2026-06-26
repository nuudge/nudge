[[_TOC_]]

# nudge

Yet another coding agent — except this one doesn't sleep, doesn't quit, and follows you home. Written in Rust from scratch: no agent SDK, no framework, no abstraction tax, just the raw LLM API over HTTP. The loop is decoupled from the UI, so a session outlives any front-end — detach it, reattach from another terminal, or **drive it live from your phone** over an end-to-end-encrypted link.

Every moving part is out in the open — the loop, the tool-use protocol, prompt-cache economics, session persistence, permission gating.

No framework, no excessive abstraction, no 50-layer call stack to trace at 2am — just readable code, easy to see when and where it decides to `rm -rf` your weekend.

## Components

nudge is three parts over one session protocol — three different ways for it to reach you. The terminal agent is the whole product on its own; the rest just removes your remaining excuses.

- **Terminal agent** — the core Rust binary: the agentic loop, the built-in tool surface, an MCP client, and a [ratatui](https://ratatui.rs) TUI.
- **Remote control** — a session can run headless behind a daemon and be reached from elsewhere over an **end-to-end-encrypted, ciphertext-blind relay**; pairing is a single QR scan.
- **Mobile app (Android)** — a native Kotlin + Jetpack Compose client that turns your phone into a live front-end for a running session (below).

### Mobile app (Android)

Work anywhere. Any time. Never stop. Scan the QR code the agent prints and your phone becomes a live controller for the running session — the office is now wherever you are, and it never closes:

- **Pair in one scan** — point the camera at the agent's QR code (or paste it). No accounts, no setup, no escape.
- **Watch it work, live** — streamed replies and thinking, every tool call and its result, in markdown; a glance line shows the model and git branch. Approve a refactor from the bus, review a stack trace at dinner, touch grass while it touches your codebase.
- **Approve from anywhere** — when the agent wants to run a command or edit a file, the prompt finds you wherever you are; **allow or deny remotely**, the same gate as the terminal. It will wait. It is infinitely patient.
- **Send and steer** — redirect it mid-task from your pocket.
- **Come and go** — detach and reattach to the same session; the transcript replays on reconnect, and the agent keeps grinding while you're "away."
- **Private by design** — end-to-end encrypted through a ciphertext-blind relay that only ever sees ciphertext. The rendezvous id is a 128-bit secret carried inside the QR code, so an unpaired device can neither find nor decrypt your session — and neither can the relay.

The app speaks a small framed protocol shared — as a pure-JVM kit (`android/protocol/`) — with the agent and the relay, so the wire format has a single source of truth.

## Quick start

nudge is three components: the **terminal agent** (the whole product on its own), an optional **relay** (the public meeting point that enables phone handoff and cross-machine attach), and the optional **Android app**. Build whichever you need — the agent stands alone.

### The agent

Requires Rust (edition 2024, install via [rustup](https://rustup.rs)) and an Anthropic API key. CI builds on Rust 1.96.0; recent stable usually works too.

```bash
git clone https://gitlab.com/hongtao1207/nudge.git && cd nudge
echo 'ANTHROPIC_API_KEY=sk-ant-...' > .env   # .env is gitignored
cargo run
```

**Install the binary.** `cargo install` builds an optimized `nudge` binary into `~/.cargo/bin` so you can run it from any directory:

```bash
cargo install --path .                 # from a local checkout
cargo install --git https://gitlab.com/hongtao1207/nudge   # straight from git
```

The installed binary reads `ANTHROPIC_API_KEY` from the environment. To avoid setting it per project, put it in a global config at `~/.nudge/config.env`:

```bash
mkdir -p ~/.nudge
echo 'ANTHROPIC_API_KEY=sk-ant-...' > ~/.nudge/config.env
nudge
```

A `.env` in the current directory still works and takes precedence over the global config, and an exported shell variable overrides both:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
nudge
```

The agent operates in whatever directory you launch it from:

```bash
cd /path/to/your/project
cargo run --manifest-path /path/to/nudge/Cargo.toml
```

### The relay (optional — enables phone handoff)

Phone handoff and cross-machine attach route through a **relay**: a publicly reachable box both devices dial out to, so they meet even when both sit behind NAT. It's ciphertext-blind — every frame is end-to-end encrypted before it leaves your device, so the relay only ever forwards opaque bytes and could not read your session if it tried. You have two ways to get one:

- **Use mine.** If you trust my relay box, point nudge at the relay I run — set it in `~/.nudge/config.env`, a project `.env`, or your shell:
  ```bash
  NUDGE_RELAY=wss://35.244.115.57.sslip.io
  ```
  The relay can't read your traffic, it's end-to-end encrypted. But you're still trusting my machine to be online and that I'm true to my words. For truly sensitive workloads, run your own.

- **Run your own.** The relay is a separate workspace crate, so it builds without dragging in the agent's dependency tree:
  ```bash
  cargo build --release -p relay      # → target/release/relay
  ```
  That binary is a plain `ws://` loopback pipe. To expose it on the public internet you front it with TLS (a domain + reverse proxy); the full walk-through — Caddy for automatic HTTPS, a hardened systemd unit, and an optional `Makefile` that stands up a GCP box end-to-end — is in [`deploy/README.md`](deploy/README.md). Then point nudge at it:
  ```bash
  NUDGE_RELAY=wss://your-relay.example.com
  ```

Without a relay, `/background` still detaches the session locally and `--socket` attaches another terminal on the same machine — you just don't get phone or off-box handoff.

### The Android app (optional)

Minimum device API is 26 (Android 8.0). Install the prebuilt APK if you trust me, or build your own from source — then launch and pair as described at the bottom.

- **Install the prebuilt APK (optional).** I publish a signed release APK to the project's package registry, so you can skip the Android toolchain entirely. This is purely a matter of trust: the APK is signed with my release key, and installing it means trusting that key and that I built it honestly:

   ```bash
   # browse available versions at https://gitlab.com/hongtao1207/nudge/-/packages
   curl -fL -o nudge.apk \
     "https://gitlab.com/api/v4/projects/83699725/packages/generic/nudge-android/0.1/nudge.apk"
   adb install nudge.apk    # or copy nudge.apk to your phone and tap it (enable "install unknown apps")
   ```

- **Do not trust me? Build your own.** Requires the **Android SDK** and a **JDK** (the build targets JDK 21). The easiest path is to open the `android/` directory in **Android Studio**, which provisions the SDK and runs the app on a device or emulator for you.

   From the command line, point Gradle at your SDK and assemble a debug APK:

   ```bash
   cd android
   echo "sdk.dir=$ANDROID_HOME" > local.properties   # or export ANDROID_HOME / ANDROID_SDK_ROOT
   ./gradlew :app:assembleDebug                       # first run downloads Gradle + deps
   ```

   The APK lands at `android/app/build/outputs/apk/debug/app-debug.apk`; install it on a connected device with:
   
   ```bash
   adb install android/app/build/outputs/apk/debug/app-debug.apk
   ```

   Launch the app and scan the QR the agent shows on `/background` (or `--daemon`) to drive the live session. QR scanning uses Google Play Services; on a device without it, paste the pairing code into the app's text field instead.

## Usage

```
nudge [OPTIONS]

OPTIONS:
    --resume <id>        Resume a previous session from ~/.nudge/projects/<cwd>/<id>.jsonl
                         (the id is shown in the TUI title bar)
    --list               List this project's saved sessions (name, id, branch,
                         transcript size, last used), most-recent-first, then exit
    --thinking <mode>    Thinking display: summarized (default) or omitted
    --daemon             Run the session headless (no TUI); hosts over $NUDGE_RELAY,
                         or a local Unix socket with --socket
    --connect            Attach a TUI to a running --daemon (with --pair-code, or
                         --socket for a local one)
    --pair-code <code>   (--connect) Attach using a pairing code from the host's QR
    --socket <path>      Host/attach over a local Unix socket instead of the relay
                         (for debugging the transport without a relay)
    -h, --help           Show help
```

Set `NUDGE_RELAY` (e.g. in `~/.nudge/config.env`) to a relay WebSocket URL to enable phone handoff:
`NUDGE_RELAY=wss://relay.example.com`.

### Detaching and phone handoff

The session outlives its front-end. Inside the TUI, `/background` detaches it — the agent keeps running and buffering output — and pressing `Enter` reattaches, replaying the full history.

When `NUDGE_RELAY` is set, `/background` dials the relay and shows a pairing QR; scan it (or paste the code into `nudge --connect --pair-code <code>`) to drive the *live* session from your phone or another machine. If no relay is configured, `/background` still pauses the session — it just shows no QR.

You can also start a session with no TUI at all: `nudge --daemon` hosts it headless over the relay, and `nudge --connect --pair-code <code>` attaches a front-end. For debugging the transport without a relay, `--daemon --socket <path>` and `--connect --socket <path>` host/attach over a local Unix socket instead. For now a backgrounded or daemon session lives only as long as its launching process — surviving a closed terminal is planned.

### TUI controls

| Key | Action |
|---|---|
| `Enter` | send message |
| `Alt+Enter` / `Ctrl+Enter` / trailing `\` + `Enter` | insert newline (paste keeps newlines) |
| `Ctrl-O` | expand / collapse tool results and thinking |
| mouse wheel / `PgUp` `PgDn` / `Home` `End` | scroll / jump / resume tail-follow |
| `y` / `n` / `Esc` | answer permission prompt |
| `Ctrl-C` (or `Ctrl-D` on empty input) | quit |

### Slash commands

Type these as a single-line message starting with `/` (multi-line input that happens to start with `/`, e.g. a pasted path, still goes to the model):

| Command | Action |
|---|---|
| `/model` | open the model picker |
| `/mcp` | list loaded MCP servers and the dormant ones available to load |
| `/mcp load <name>` / `/mcp unload <name>` | connect / disconnect a dormant server mid-session |
| `/session-rename [name]` | rename the session; bare, the agent derives a name (git branch + short id in a repo, else an LLM-suggested summary) |
| `/background` (alias `/bg`) | detach and run the agent headless; with `NUDGE_RELAY` set, also shows a pairing QR — reattach with `Enter` |

## Features

- **Agentic loop** — the model plans, calls tools, observes results, and iterates until the task is done, bounded by an iteration budget that ends gracefully (the agent explains it hit the cap and asks how to proceed).
- **Tool surface** — `Bash`, `Read`, `Edit` (modify + append modes), `CreateNew`, `Grep` (structured ripgrep), `Glob`, `TodoWrite`. Tool names and field shapes are wire-compatible with Claude Code where they overlap, so your muscle memory transfers — only the bill changes.
- **TUI** (ratatui) — collapsed-by-default action display: each tool call is one compact group with a live status bullet (spinner → ok/error) and a one-line result row; `Ctrl-O` expands everything, including the model's thinking. Title bar shows session id, cwd, git branch, model, and platform.
- **Permission gating** — shell-executing and file-mutating tools prompt before running; read-only tools auto-allow. For `Bash`, the model must state an *intent* ("count lines in all Rust files") shown as the action label, while the permission prompt always shows the raw command — you approve what runs, not the label.
- **MCP client** — connect to external [Model Context Protocol](https://modelcontextprotocol.io) servers declared in a project-local `.mcp.json`. Their tools are discovered at startup and merged into the model's tool list (namespaced `server__tool`), indistinguishable from built-ins. Permission follows each tool's `readOnlyHint` annotation — read-only tools auto-allow, the rest prompt. Both local **stdio** subprocesses and remote **Streamable HTTP** servers are supported, the latter with static-token, OAuth (dynamic registration), or OAuth (pre-registered client) auth. Servers load in three layers — always-on foundational tools, always-on user servers from `.mcp.json`, and a built-in **dormant** catalog the user loads/unloads mid-session (`/mcp load <name>`) to keep the default context lean.
- **Sessions** — every conversation is appended to a JSONL log under `~/.nudge/projects/<flattened-cwd>/<uuid>.jsonl`; `--resume <id>` restores it, with strict truncation of any incomplete trailing turn.
- **Detachable sessions + phone handoff** — the agent loop is decoupled from the UI, so a session outlives its front-end. `/background` detaches the TUI while the agent keeps working; reattach later and the full history replays. With `NUDGE_RELAY` set, `/background` also dials an end-to-end-encrypted relay and shows a pairing QR, so you can take over the live session from your phone (or from any other terminal running `nudge --connect --pair-code`). A session can also run fully headless (`--daemon`).
- **Prompt caching** — layered `cache_control` breakpoints on the system prompt plus a floating breakpoint that walks forward along the chat history. At ~100-message depth this cuts billed input ~7× and shortens time-to-first-token accordingly.
- **Adaptive thinking** — the model decides when to reason; `--thinking omitted` hides the reasoning text for faster first tokens at the same cost.

### Not yet supported

A few capabilities common to mature coding agents are deliberately absent today, called out here to set expectations. All are on the roadmap and under active development:

- **Skills** — reusable, model-invokable capability bundles (à la Claude Code skills). Not yet supported.
- **Sub-agents** — spawning child agents to parallelize or isolate sub-tasks; the loop runs a single agent for now.
- **Web access** — no built-in web fetch or search tool, so the agent can't browse the internet on its own (you can wire one up via an MCP server in the meantime).
- **Image input** — input is text-only; pasting screenshots, diagrams, or other images isn't supported yet.
- **Automatic context compaction** — there's prompt caching but no auto-summarization when the context window fills. A long task instead stops gracefully at the iteration budget and hands back to you, rather than compacting history to keep going.


## Development

The repo ships a `mise.toml` of dev tasks (`mise run` lists them); `mise run ci` mirrors the GitLab CI pipeline exactly — run it before pushing. Install the Rust toolchain with [rustup](https://rustup.rs); mise is a task runner only and does not manage it.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full toolchain setup, local checks, and merge-request workflow.

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

Under active development — interfaces and on-disk formats change without notice or apology. The terminal agent, remote control, and Android app all cover their core flows; expect sharp edges. Open an issue and it might get fixed by the very thing that caused it.

## License

[MIT](LICENSE) © 2026 Hongtao Yang
