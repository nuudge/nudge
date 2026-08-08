# Peer agents

Connect two independent nudge sessions so their agents can talk to each other — across
directories, machines, or continents — over the same end-to-end-encrypted relay your
phone uses. Each agent stays under its own human's control: your agent consults theirs,
theirs consults yours, and every tool call is still gated on its own side.

nudge has two multi-agent models, and it helps to know which one you want:

| | Subagent | Peer |
| --- | --- | --- |
| What it is | your agent spawns a child and supervises it | two sessions you and a colleague each run, laterally connected |
| Who gates its tools | the parent agent (escalating the scary ones to you) | each session's own human |
| What you buy | the child's **conclusion** — delegate a task, get a result | the peer's **context** — consult an agent that knows its own codebase |
| How it starts | ask your agent to spawn one ([Subagents](subagents.md)) | `/connect-peer`, below |

This page is about **peers**. For the design behind both, see
[multi-agent models](multi-agent-models.md).

## Prerequisites

- The session being *connected to* (the target) needs a relay: set `$NUDGE_RELAY` (the
  shared relay or [your own](remote-and-relay.md)).
- The *connecting* session needs nothing — the pairing code is self-contained (relay
  URL, room, key).

## Setup: get an agent-peer code from the target

An **agent-peer code** is a third pairing scope, alongside full-access and watch-only.
Whoever holds it can connect an agent to your session — hand it over like you'd hand
over the phone QR.

**Interactive session** (you're working in the TUI):

1. Type `/background`. This arms the relay legs — it's a one-time "enable remote
   access", not a shutdown: the legs live for the life of the process.
2. Press `w` to cycle the pairing on screen: full-access → watch-only → **agent-peer**.
3. Copy the agent-peer code, send it to the other human.
4. Press Enter to foreground and keep working. Your session stays connectable.

**Headless daemon:**

```bash
NUDGE_RELAY=wss://your-relay nudge --daemon --peer
```

prints a code labeled `agent-peer` (alongside the full-access one). Add `--watch` for a
watch-only code too.

## Connect

In the *other* session, paste the code:

```
/connect-peer <code>
```

You'll see `connecting to peer over the relay…`, then `connected to peer <name>` — the
name is the target's session name (rename yours with `/session-rename` before sharing a
code if you want a recognizable name). One connect is **bidirectional**: both agents can
now message each other, and `/peers` on either side lists the other:

```
peers:
- backend-api (agent, unsupervised) — session 3f2a…
```

If it fails you'll get a Notice with the cause — a full-access or watch-only code is
refused (`the far side offered no return edge`); an unreachable relay or a dead target
times out cleanly.

## Using it

Talk to your own agent as usual; ask it to involve the peer:

> ask backend-api how the auth middleware handles token refresh

Your agent uses its `MessagePeer` tool; the message arrives in the peer's session as a
turn marked `[message from peer <your-session-name>]`, its agent answers back the same
way, and the reply lands in your transcript. The agents converse; the humans keep
driving their own sessions throughout.

## What to expect

- **Quiet by design.** The peer's day-to-day activity (its tool calls, its own
  conversation) does **not** stream into your session — only messages addressed to your
  agent, plus errors and connect/disconnect notices. To *watch* a peer work, ask its
  human for a watch-only code and attach to their session directly.
- **Named senders.** Once a second driver is connected (a peer counts), the model sees
  every message attributed — the peer's as `[message from peer <name>]`, yours as
  `[message from <you>]` — so it always knows who's asking and can answer each of you.
- **Your agent knows its roster.** Connected peers appear in the agent's context, so
  "ask backend-api …" works without you spelling out names or the agent guessing.
- **Consecutive messages coalesce.** A peer that fires several messages in a row is
  digested in one turn, not several.
- **Messages land at turn boundaries.** If your agent is mid-task when a peer message
  arrives, it queues and is handled as the next turn — expect a delay, not a loss.
- **Permissions stay local.** A peer never approves (or even sees) your tool prompts,
  and can't end your session. It can run session commands like `/model` — hand the code
  to people you trust accordingly.
- **Disconnects are reaped, not repaired.** If the peer exits or the link drops you get
  `[peer <name>] disconnected` and the roster updates. There's no auto-reconnect yet:
  re-run `/connect-peer` with the same code (it stays valid until the target daemon
  restarts). Deployed relays without keepalive can also drop idle connections or delay
  disconnect detection — known, tracked in
  [#65](https://github.com/nuudge/nudge/issues/65) /
  [#62](https://github.com/nuudge/nudge/issues/62); a retry is safe.

## Command reference

| Surface | What it does |
| --- | --- |
| `/connect-peer <code>` | dial an agent-peer code; one call, bidirectional edge |
| `/peers` | list connected peers and spawned subagents |
| `w` (background screen) | cycle the pairing QR: full-access → watch-only → agent-peer |
| `--daemon --peer` | headless session that prints an agent-peer code |
| `--daemon --watch` | …and/or a watch-only code |

`/connect-peer` is deliberately human-only — an agent can't obtain a pairing code except
through you.
