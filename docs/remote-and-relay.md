# Remote control & relay

The agent loop is decoupled from the UI, so a session outlives any front-end. You can
detach it, reattach from another terminal, hand it off to your phone, or have several
clients drive it at once — all over an end-to-end-encrypted link that only ever sees
ciphertext.

<!-- screenshot: the /background pair screen showing a QR code -->

## Detaching a session

The session outlives its front-end. Inside the TUI, `/background` (alias `/bg`) detaches it
— the agent keeps running and buffering output — and pressing `Enter` reattaches, replaying
the full history.

You can also start a session with no TUI at all: `nudge --daemon` hosts it headless. For
now a backgrounded or daemon session lives only as long as its launching process —
surviving a closed terminal is planned (see the [Roadmap](roadmap.md)).

## Phone handoff

When `NUDGE_RELAY` is set, `/background` dials the relay and shows a pairing QR. Scan it
with the [Android app](mobile-app.md) (or paste the code into `nudge --connect --pair-code
<code>`) to drive the *live* session from your phone or another machine. Approve an edit
from the bus or review a stack trace at dinner — the permission prompt finds you wherever
you're attached. If no relay is configured, `/background` still pauses the session — it just
shows no QR.

The pairing code (a 128-bit rendezvous secret carried inside the QR) is the whole key to
the session: anyone you hand it to can join, and no one else — not even the relay — can
find or decrypt it. See [Security](security.md).

## Multi-client co-op

Sessions are multi-attach. The daemon lives behind a broker, not a terminal, so your
teammate, your phone, and your laptop can all attach to the same running agent at once.
Everyone sees the same event stream, anyone can drive, and a permission you approve on one
client clears on all of them. It's real multi-client concurrency, not screen-sharing —
co-op mode for a coding agent. Each client announces an identity when it attaches, so the
shared transcript shows who said what.

## Headless / debug attach without a relay

For debugging the transport without a relay, host and attach over a local Unix socket:

```bash
nudge --daemon --socket /tmp/nudge.sock     # host headless on a socket
nudge --connect --socket /tmp/nudge.sock    # attach a TUI over the socket
```

`--socket` also attaches another terminal on the same machine without any relay — you just
don't get phone or off-box handoff.

## The relay

Phone handoff and cross-machine attach route through a **relay**: a publicly reachable box
both devices dial *out* to, so they meet even when both sit behind NAT. It's
ciphertext-blind — every frame is end-to-end encrypted before it leaves your device, so the
relay only ever forwards opaque bytes and could not read your session if it tried. You have
two ways to get one.

### Use the shared relay

If you trust the maintainer's relay box, point nudge at it — set it in
`~/.nudge/config.env`, a project `.env`, or your shell:

```bash
NUDGE_RELAY=wss://35.244.115.57.sslip.io
```

The relay can't read your traffic (it's end-to-end encrypted), but you're still trusting
that machine to be online and honest. For truly sensitive workloads, run your own.

### Run your own

The relay is a separate workspace crate, so it builds without dragging in the agent's
dependency tree:

```bash
cargo build --release -p relay      # → target/release/relay
```

That binary is a plain `ws://` loopback pipe. To expose it on the public internet you front
it with TLS (a domain + reverse proxy); the full walk-through — Caddy for automatic HTTPS, a
hardened systemd unit, and an optional `Makefile` that stands up a GCP box end-to-end — is
in [`deploy/README.md`](../deploy/README.md). Then point nudge at it:

```bash
NUDGE_RELAY=wss://your-relay.example.com
```
