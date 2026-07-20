+++
title = "An agent is just another client"
date = 2026-07-10
description = "How a small bug in my hand-rolled coding agent turned into one idea: there should be exactly one way to talk to an agent, and it shouldn't matter whether a human or another agent is on the other end."
+++

*How a small bug in my hand-rolled coding agent turned into one idea: there should be exactly one way to talk to an agent, and it shouldn't matter whether a human or another agent is on the other end.*

## The missing channel

I'm building a coding agent from scratch, nudge, a claude-code-style harness in Rust. Not a wrapper around someone's SDK. The loop, the tool protocol, the context management, all of it by hand, because the point is to understand how these systems actually work.

For a while the thing I wanted next was sub-agents: let the main agent spawn a helper, send it off to do a scoped task, supervise it, and fold the result back in. I built a first cut. A parent could spawn a child, watch it work, and approve or deny the child's tool calls.

Then I hit a wall, a small, almost embarrassing one. The child was editing a file. It stopped and asked, in effect: *"this file only has three lines — should I add the fourth?"* And the parent had no way to answer. It could watch the child's tool calls stream by. It could approve or deny a permission request. But it couldn't send the child a sentence. There was no channel for "here's what I think, keep going." A human had to lean in and relay the message by hand.

That's not a missing feature. It's a missing edge. The parent→child relationship was a one-way street: the parent observed the child and returned yes/no verdicts, and that was the whole vocabulary.

My first instinct was the obvious one: add a message bus between agents. A `MessagePeer` tool, a little inbox, some routing. I started. Then I stopped, because I'd seen this movie before.

## What I already had

Here's what I already had, and had sunk real effort into: nudge can be driven from a terminal, from another terminal over a Unix socket, or from my phone over an encrypted relay. All three go through one abstraction. A front-end "attaches" to a session and gets back a `Controller`:

```rust
trait SessionHandle {
    async fn attach(&self) -> Option<Controller>;
}

struct Controller {
    events: Receiver<ControllerEvent>, // what the agent emits — you observe
    ui_tx:  Sender<UiEvent>,           // what you send it   — you drive
}
```

The terminal, the socket client, and the relay client each implement `SessionHandle`. The UI code is generic over it. It literally can't tell which transport it got. Local versus remote is a transport swap, not a separate code path.

So I had a clean, transport-agnostic way for a human to reach an agent. And I was about to build a second, totally separate, in-process-only way for an agent to reach an agent.

That's when it clicked. Why are those two different things? A human driving an agent and an agent driving an agent are the same shape: you observe what it does, and you send it input. The parent didn't need a message bus. It needed to be a client of the child, the exact same kind of client my phone already is.

**An agent is just another client.**

## The principles that fell out

Once I took that seriously, a handful of principles organized every decision after it:

1. **An agent is just another client.** Whatever's on the other end — human, phone, or peer agent — is indistinguishable to the loop. Roles like parent/child or supervisor/worker aren't types or branches. They emerge from which direction a connection points and what system prompt an agent runs under. The goal state has no `if peer { … } else { … }`.

2. **One mechanism, every transport.** Reaching an agent is always `attach → Controller`, whether that's an in-process channel, a Unix socket, or an encrypted WebSocket.

3. **Compose primitives; don't add channels.** Every capability — watching, driving, supervising, discussing, spawning — should be a composition of two primitive half-channels (observe and drive) over uniform connections. When something new comes up, the question is "which composition is this?" not "what channel do I add?" My aborted message bus failed exactly this test: it added a channel instead of composing, so it would have worked in-process and nowhere else.

4. **Bidirectional by construction, not by feature.** A single connection is a one-way relationship: A drives and observes B. Two agents as equals is just two connections, each a client of the other. There's no "duplex peer" object.

5. **The symmetry test.** If you can't tell whether a human or an agent is on the other end, and nothing behaves differently, the design is right.

## What it actually took

The idea is clean. Making the existing code live up to it took two real pieces of work.

**Multi-attach.** My broker, the thing that sits between the agent loop and the front-ends, assumed a single attached client at a time. It literally held an `Option<Controller>` and refused a second. But "an agent is just another client" plus "watching is just attaching" changes the shape. A session now needs to fan its event stream out to N clients and merge input back from all of them. So the broker became multi-attach. It fans every event to every attached controller — over per-client channels, so one slow consumer can't stall the others or the loop — and it merges every controller's input into the loop's single input. A permission prompt now goes to everyone attached, and the first answer wins.

The nice part: this one change subsumed three things I'd otherwise have built separately. Watch-mode, where a human watches while someone else drives? A second attach. A phone and a laptop on the same session? Two attaches. A supervising agent plus a human looking over its shoulder? Two attaches. They stopped being features and became the same feature.

**The handshake.** If clients are anonymous, a shared session is illegible: you can't tell whose message is whose. So attaching became a tiny identity handshake. Every client, human or agent, announces itself:

```rust
Attach { after_seq, who: ClientIdentity { kind, name, session_id, task } }
```

The daemon records it and stamps every turn with who sent it. `kind` is `Human` or `Agent`. A spawned agent carries its `session_id` and its assigned `task`; a human just carries a name. Crucially it's the same frame for both: that's what keeps the human path and the agent path a single protocol, not two that merely look alike.

## Testing the design

Here's the test that convinced me the design was right.

I opened a session on my laptop in the terminal. I backgrounded it, which arms a relay handoff, and attached to the same session from the nudge Android app on my phone. Then I foregrounded the laptop again. Instead of one client kicking the other off, both stayed live. I typed on the laptop; the message showed up on the phone, labeled with my laptop username. I typed on the phone; it showed up on the laptop, labeled with the phone's device name. The agent answered into both. A permission prompt appeared on both; I answered on one and it cleared on the other.

Two clients. Two transports — one in-process, one over an encrypted relay across the internet. Two languages — the terminal is Rust, the phone app is Kotlin. And the session couldn't tell the difference, because there was no difference to tell. The Kotlin client speaks the exact same handshake and event protocol as the Rust one; I have byte-for-byte serialization tests pinning the two together.

That's the symmetry test passing in the real world. You genuinely can't tell who's on the other end, and nothing in the system needs to.

## The tradeoff

Most agent stacks treat sub-agents as a bespoke construct: a spawn API, a privileged parent, a side-band message bus. Then remote control is another bespoke construct, human-in-the-loop is another, multi-agent is another. Every mode becomes its own subsystem.

The bet here is the opposite. Define one way to talk to an agent. Make humans and agents equal citizens of it. Then every richer behavior is a composition of the same two primitives over the same uniform connections, and you never write the second subsystem.

## What's next

The foundation is built and proven; the payoff is the easy part now. The original bug, the parent that couldn't answer its child, gets fixed by giving the child a client handle back to the parent. Spawning a sub-agent becomes: create an agent, and have the two mutually attach. The parent is a client of the child (it already was); the child becomes a client of the parent (the new return edge). Two ordinary attaches pointing opposite ways. And the message the parent couldn't send? It's an ordinary message up the child's drive channel: the exact path my phone's messages already take.

Connecting to an agent running on another machine, across the world, is the same operation with the relay transport instead of the in-process one. I mostly already wrote it. I just didn't realize I was writing it for agents too.

More on the spawn itself, and what it's actually like to have two agents talk to each other as equals, in a future post.

---

*nudge is an open-source coding agent built from scratch in Rust. [Browse the code](https://github.com/nuudge/nudge), or read the full design write-up in [`docs/symmetric-communication.md`](https://github.com/nuudge/nudge/blob/main/docs/symmetric-communication.md).*
