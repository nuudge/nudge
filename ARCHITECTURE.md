# Architecture

`nudge` is a Claude-Code-style coding agent. It's organized as strict
dependency layers: every arrow points **down**, and no lower layer ever names a
higher one. The agent loop (`core`) is UI- and tool-agnostic; the concrete
coding behavior (`coding`) and the wire protocol (`transport`) are both built on
top of it, and the front-end (`tui`) is generic over how it reaches a session.

## Layered module dependencies

```mermaid
graph TD
  main["main.rs<br/>composition root · CLI (clap)<br/>wires everything, picks run mode"]

  tui["tui/<br/>ratatui + crossterm front-end<br/>generic over SessionHandle"]
  transport["transport/<br/>wire protocol · daemon · client<br/>encryption · pairing (QR)"]
  coding["coding/<br/>CodingBackend · tools · mcp<br/>prompt · context · file_state"]
  core["core/<br/>agent loop · SessionHost/broker<br/>session log · event types · traits"]
  llm["llm/<br/>Provider trait<br/>AnthropicProvider · Message model"]

  main --> tui
  main --> transport
  main --> coding
  main --> core

  tui --> core
  transport --> core
  coding --> core
  coding --> llm
  core --> llm

  subgraph external [external processes]
    api["Anthropic API"]
    mcpsrv["MCP servers<br/>(child / HTTP)"]
    relay["relay binary<br/>(relay/src/main.rs)<br/>ciphertext-blind WS relay"]
  end

  llm -. https .-> api
  coding -. rmcp .-> mcpsrv
  transport -. wss .-> relay
```

## Runtime: the session host, broker, and front-ends

A `SessionHost` owns two long-lived tokio tasks - the **agent loop**
(`run_agent`) and a **broker** - plus the channels between them. The loop's
channels terminate at the broker, not at any front-end, so a front-end attaching
and detaching never ends the session. The loop ends only on an explicit `Quit`.

```mermaid
graph LR
  subgraph host [SessionHost · one process]
    loop["agent loop · run_agent<br/>OUTER: per user turn<br/>INNER: model + tool calls<br/>until non-tool stop"]
    broker["broker task<br/>single-attach lock<br/>replay buffer · fan-out<br/>AgentEvent → ControllerEvent"]
    session["Session<br/>append-only JSONL log<br/>~/.nudge/projects/&lt;cwd&gt;/"]
  end

  backend["CodingBackend<br/>tools · permissions · MCP<br/>system prompt + context"]
  provider["AnthropicProvider"]

  loop -->|"complete() / count_tokens()"| provider
  loop -->|"execute / schemas / permission"| backend
  loop -->|"log(message)"| session
  loop -->|"AgentEvent (+ oneshot for permission)"| broker
  broker -->|"UiEvent"| loop

  subgraph fe [front-end · same or remote process]
    controller["Controller<br/>events: Receiver&lt;ControllerEvent&gt;<br/>ui_tx: Sender&lt;UiEvent&gt;"]
    ui["tui::run (ratatui)"]
  end

  broker -->|"ControllerEvent (Clone, Serialize)"| controller
  controller -->|"UiEvent"| broker
  controller <--> ui
```

### Event vocabulary (`core/events.rs`)

- **`UiEvent`** - front-end → loop: `UserMessage`, `SetModel`, `LoadServer` /
  `UnloadServer` / `ListServers`, `PermissionResponse`, `Quit`.
- **`AgentEvent`** - loop → broker: assistant text/thinking, tool start/result,
  usage, `PermissionRequest` (carries a `oneshot` reply slot), `Notice`,
  `Error`. Deliberately **not** serializable.
- **`ControllerEvent`** - broker → front-end: a `Clone + Serialize` mirror of
  `AgentEvent` with the `oneshot` terminated and `UserMessage` echoes +
  `PermissionResolved` markers injected, so any controller (live or
  attach-replay) reconstructs the whole transcript from this one stream.

## How a front-end reaches the session: `SessionHandle`

`tui::run` is generic over the `SessionHandle` trait, so the front-end code is
byte-for-byte identical whether it drives the loop in-process or across a wire.

```mermaid
graph TD
  tuirun["tui::run&lt;H: SessionHandle&gt;"]

  tuirun --> sh{SessionHandle}

  sh --> inproc["SessionHost<br/>(local TUI / daemon host)<br/>direct channels to broker"]
  sh --> sock["SocketClient<br/>(--connect)<br/>local Unix socket"]
  sh --> relayc["RelayClient<br/>(--connect --relay/--pair-code)<br/>E2E-encrypted WebSocket"]

  sock -. "newline-JSON frames" .-> daemon["transport daemon<br/>(--daemon)"]
  relayc -. "sealed Ws frames" .-> relaybox["relay binary"]
  relaybox -. "sealed Ws frames" .-> daemonr["transport relay daemon<br/>(--daemon --relay)"]
  daemon --> brokerd["broker_handle()"]
  daemonr --> brokerd
```

The `SessionHandle` trait exposes three operations:

- **`attach`** — bind a front-end; returns `None` if the broker's single-attach
  lock is held elsewhere.
- **`attach_force`** — same but overrides the lock (local TUI force-takeover,
  so the physically-present machine can reclaim a session a phone left attached).
  Only `SessionHost` overrides the default; remote `--connect` clients never force.
- **`detach`** — release the front-end without ending the session (`/background`);
  the loop keeps running headless and buffers events for the next `attach`.

The `transport` layer puts the broker's in-memory `Controller` stream onto a
wire. `wire` defines the framed protocol over a transport-agnostic seam
(`FrameReader`/`FrameWriter`) with two codecs: newline-JSON for the local Unix
socket and a `Ws` codec for the relayed WebSocket. `encryption` (dryoc) seals
frames for the relay path - the relay box only ever sees ciphertext; `pairing`
mints/encodes the QR that carries the relay URL, room id, and E2E key to a
device.

## The `/background` handoff hook

`SessionHost::set_handoff_hook` registers a closure that fires **once**, lazily,
on the first `/background` command. The hook is wired in `main.rs` (not `core`)
to keep `core` below the transport layer. Which transport the hook opens depends
on the CLI flags at startup:

- **`--relay <url>` (normal mode)** — the hook dials OUT to the relay so a phone
  can attach. A `Pairing` (room id + E2E key) is generated at startup; the QR
  and pairing code are surfaced in the TUI's pair screen on `/background`. No
  local socket is bound.
- **default (no `--relay`)** — the hook binds a local Unix socket
  (`~/.nudge/daemon.sock` by default) and emits a notice with the
  `nudge --connect` command needed to attach from another terminal.

## Run modes (selected in `main.rs`)

| Invocation                                   | Topology                                                                                          |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| `nudge`                                      | In-process `SessionHost` + local TUI. First `/background` lazily binds a Unix socket for handoff. |
| `nudge --relay <url>`                        | In-process `SessionHost` + local TUI. First `/background` dials out to relay; TUI shows a QR.   |
| `nudge --daemon`                             | Headless host bound to a Unix socket; clients attach with `--connect`.                            |
| `nudge --daemon --relay <url> [--pair]`      | Headless host dials **out** to the relay; `--pair` mints a room + key and prints a QR.            |
| `nudge --connect [--socket\|--relay\|--pair-code]` | Front-end only (`SocketClient`/`RelayClient`); owns no loop.                              |
| `nudge --gen-key`, `nudge --print-prompt`    | Standalone one-shot actions, then exit.                                                           |

## The coding agent (`coding/`)

`CodingBackend` implements `core::Backend` — the only thing the loop knows about
tools. It owns:

- **`tools/`** - `Bash`, `Read`, `Edit`, `CreateNew`, `Grep`, `Glob`,
  `TodoWrite`, dispatched by name. Read-only tools (`Read`/`Grep`/`Glob`/
  `TodoWrite`) auto-allow; the rest gate on a per-call permission prompt.
- **`mcp/`** - connects MCP servers from project-local `.mcp.json`, plus a
  catalog of dormant servers loadable at runtime via `/mcp load`.
- **`prompt` / `context`** - assembles the system prompt with fresh env, git,
  and directory context each turn.
- **`file_state`** - tracks read files to enforce the read-before-edit
  invariant.
