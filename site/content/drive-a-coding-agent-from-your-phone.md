+++
title = "I made a coding agent you can drive from my phone"
date = 2026-07-05
description = "A build story: how decoupling the agent loop from the terminal turned into a session you can scan onto your phone and drive over an end-to-end-encrypted link."
+++

*A build story: how decoupling the agent loop from the terminal turned into a session you can scan onto your phone and drive from the couch — over a relay that can't read a byte of it.*

I approved a code edit from a bus once. The agent was running on my laptop at home; I was somewhere on a highway, watching a diff scroll up my phone, and I tapped *allow*. Back home the edit landed and the agent kept going. Nobody else's coding agent does this as far as I can tell, so here's how it works — and how a bug-adjacent design decision made it almost free.

<video src="https://github.com/user-attachments/assets/17d6523d-d66f-4ec2-b3eb-6075815539a2" controls width="800"></video>

## The one decision that made it possible

nudge is a coding agent I'm writing from scratch in Rust — no agent SDK, just the raw LLM API and a loop I can actually read. Early on I made one call that everything here rests on: **the agent loop never touches the terminal.**

Instead, the loop talks to a *broker*. The broker sits in the middle: the loop emits events (assistant text, tool calls, results, permission requests) into it, and the broker fans them out to whatever front-end is attached. A front-end sends input back the same way. The terminal UI is just one such front-end.

The moment the loop stopped owning stdin/stdout, the session stopped being tied to a window. It became a thing that *runs* — and front-ends come and go around it. Detach the terminal and the agent keeps working, buffering its output. Reattach later and the whole transcript replays. That's `/background`.

And if a session can survive its terminal detaching… why does the next front-end have to be on the same machine?

## Reaching a session is one operation

Every front-end reaches a session through a single trait:

```rust
trait SessionHandle {
    async fn attach(&self) -> Option<Controller>;
}

struct Controller {
    events: Receiver<ControllerEvent>, // what the agent emits — you observe
    ui_tx:  Sender<UiEvent>,           // what you send it   — you drive
}
```

Local terminal, another terminal over a Unix socket, a phone over the internet — each is just a different implementation of `attach`. The UI code is generic over it and can't tell which transport it got. So "drive it from your phone" isn't a feature bolted onto the terminal agent; it's the *same* attach with a different transport underneath.

## The transport: a relay that can't read your session

The catch with "from your phone" is the network: your laptop and your phone are both behind NAT, so neither can dial the other directly. The fix is a **relay** — a small public box both devices dial *out* to, which copies bytes between them.

The obvious worry is: now my code session flows through some box on the internet. So the relay never sees it. Every frame is end-to-end encrypted on the device before it leaves; the relay only ever forwards opaque ciphertext and holds no state. It's a dumb, blind pipe. If you don't trust mine, the relay is a tiny separate binary — run your own in a few minutes.

Pairing is a QR code. When you `/background` with a relay configured, nudge prints a QR that encodes the relay URL, a one-time 128-bit rendezvous id, and the end-to-end key. Your phone scans it and it's in. An unpaired device can't even *find* your session, let alone decrypt it — and neither can the relay.

<figure>
  <img src="/img/tui_handoff.png" alt="the nudge pair screen: a QR code plus a pairing code">
  <figcaption>After <code>/background</code>: scan this to hand the live session to a phone.</figcaption>
</figure>

## The part I didn't expect: everyone stays live

Here's where it got interesting. My broker originally held *one* attached client. Handing off to the phone meant kicking the laptop off — a baton pass. That felt wrong, so I made the broker multi-attach: it fans every event to *N* clients and merges input from all of them.

Suddenly the phone and the laptop are both live on the same session at once. I type on the laptop, it shows up on the phone; I type on the phone, it shows up on the laptop, each labeled with who sent it. A permission prompt appears on both, and whichever one I answer first wins — the other clears. It's not screen-sharing; it's genuinely two clients driving one running agent. Co-op mode for a coding agent.

<figure>
  <img src="/img/phone_basic.png" alt="the nudge Android app before and after pairing: waiting to attach, then streaming the same live session as the terminal">
  <figcaption>The phone before and after pairing — streaming the same live session as the terminal.</figcaption>
</figure>

## Why this was almost free

The honest punchline: none of this was a phone feature. It was the payoff of one principle — **whatever is on the other end of a connection is just a client**, whether that's your terminal, your phone, or (it turns out) another agent. Decouple the loop from the UI, make "reach a session" a single operation, and put a blind encrypted relay under it, and "drive it from your phone" falls out. So does multi-client co-op. So do sub-agents, but that's the [next post](@/an-agent-is-just-another-client.md).

The Android client is native Kotlin; the terminal is Rust. They speak the exact same wire protocol, pinned by byte-for-byte serialization tests — because to the session there's no difference to tell.

---

*nudge is an open-source coding agent built from scratch in Rust. [Browse the code](https://github.com/nuudge/nudge) or [watch the full demo](https://github.com/user-attachments/assets/17d6523d-d66f-4ec2-b3eb-6075815539a2).*
