# Multi-agent models: subagents vs. peers — design notes

**These are the maintainer's design notes and thought process, not a user guide.** If you
want to *use* the multi-agent features, read [Subagents](subagents.md) and
[Peer agents](peers.md). This doc records how the design got where it is — the taxonomy,
the decisions and their reasoning, and what remains deliberately unsettled — so future
work (and future contributors) don't re-litigate it from scratch.

**Status: both models are shipped; several design questions remain open.** This began as
a design discussion (2026-02, around narrowing
[#51](https://github.com/nuudge/nudge/issues/51)) about what "multi-agent" should mean in
nudge; the two-model taxonomy below has since been implemented (subagents via `Spawn`,
peers via `/connect-peer`). The [symmetric communication](symmetric-communication.md)
substrate — every relationship is an `attach`, an agent is just another client — supports
several shapes of multi-agent system; this doc holds the taxonomy, what it resolved, and
what remains open.

## Two models

There appear to be two genuinely different things people mean by "multi-agent", and they
differ on almost every axis:

|  | 1. Subagent | 2. Peer |
| --- | --- | --- |
| Created by | `Spawn` — one side creates the other | `/connect-peer` — two existing sessions consent |
| Edge type | supervised (trust-by-wiring, direction of creation) | unsupervised, both directions |
| Lifecycle | parent owns the child's `SessionHost`; dismissal ends it | independent lifetimes; disconnect ≠ death |
| Permission flow | check-ins steered by the parent; escalation up the tree | each agent's **own human** gates it — no cross-agent authority |
| Human contact | only via the parent — mediated, by design | each peer already has its own human driver |
| Economics | **buy the conclusion, not the context** | **buy the context, not the conclusion** |
| Exists today? | yes (the `Spawn` tool, `docs/subagents.md`) | yes (`/connect-peer` over a duplex relay edge, `docs/peers.md`) |

**Model 1 — subagents.** A main agent spawns a child and delegates a task. The parent
supervises: approves the child's tool calls, steers its decisions, escalates to the human
when it can't judge. This is a *strict supervision tree*. The human does not interact with
the child directly — a subagent's whole value is that you buy its conclusion without buying
its context, and the mediation is the contract, not a limitation.

**Model 2 — true peers.** Two sessions started independently — say, in two different
repos — connected laterally because their work is related. Neither supervises the other;
both can talk; each is driven (and gated) by its own human. Peer A consults peer B before
implementing something that touches B's area: here you buy the peer's *context* — its
knowledge of its own recent work — not a delegated result. The exception is a peer with no
one to answer its prompts, such as a headless `--daemon` or an ended session resumed in the
background: the first session to connect supervises it, as a parent would a child.

In the code, this distinction is the `supervised: bool` on every peer edge
(`core/peer.rs`). Spawning sets it. On a dialed edge the *target* grants it, and only when
no attached controller can answer its permission prompts. A dialer can't claim it.

## What the two-model framing resolves

**The tree-vs-graph anxiety.** Adding lateral edges looks like it degrades a clean tree
into a general graph with cycles and no root. The resolution: separate the **supervision
relation** (who supervises whom — carries authority: steering, dismissal, escalation) from
the
**communication graph** (who holds an edge to whom). The communication graph was never a
tree — mutual attach is already a 2-cycle, multi-client attach already a fan — and cycles
in it are harmless chatter loops, managed by identity-aware routing at the broker.
Supervised edges come from spawning, or from a target that has no one to answer its
prompts granting supervision to its first dialer. Authority never crosses an edge to a
session that has an answerer, so a session with a human in it is never supervised.

**The `/message` ventriloquism problem.** A human verb that relays speech into a child
(`/message <peer> <text>`, cut from #51) is wrong in both models. In model 1, the relayed
message arrives at the child stamped with the *parent session's* edge identity —
indistinguishable from the parent agent speaking, and unwitnessed by the parent agent: an
actor whose actions are attributed to someone else. Mediated speech ("ask the main agent to
tell the child") is the model working as intended. In model 2 the verb is unnecessary: each
peer has its own human.

## Open questions

1. **Should humans get subagent lifecycle verbs at all?** `/spawn <task>` and `/dismiss
   <peer>` are model-1 operations with no attribution problem (they're not speech), but a
   human-initiated spawn writes no tool_result, so the *agent* wouldn't know its own child
   exists. (The candidate fix shipped for other reasons: the peer roster now rides the
   per-turn context as a trailing system block.) Deferred, not decided.
2. **May a human ever attach directly to a subagent?** The machinery nearly allows it (a
   child has a broker; pairing to it is conceivable). It would break the mediation contract
   and invite micromanaging the thing you delegated — but it might be the honest answer for
   debugging ("wtf did that subagent just do" — though the session DB may serve that better).
3. **What does "buying context" mean mechanically for peers?** Consulting via
   `MessagePeer` is conversation; is that enough, or do peers eventually need a richer
   exchange (transcript excerpts, file-state summaries, capability discovery across the
   edge)? Nothing designed yet.
4. **Supervision cycles among headless sessions.** Supervision is granted, not only created
   by spawning, so sessions with no human in them could supervise each other in a ring
   (A supervises B, B supervises C, C supervises A), approving each other's tool calls
   with no human in the loop. Only one edge per session pair is kept (#112), which rules
   out a two-session cycle; longer rings are possible if every member is headless. Should
   the broker refuse a grant that would close a cycle?
5. **Peer-edge policy.** Model-2 edges carry `UserMessage` both ways between autonomous
   agents. Consecutive same-sender messages now coalesce into one turn, and the MessagePeer
   prompt teaches batching — but nothing *enforces* a budget; two agents can still ping-pong
   indefinitely. Do edges need broker-level throttles?

## Resolved since first written

- **Cross-machine supervision** (was open question 4): connecting to a session that has no
  one to answer its prompts makes you its supervisor, over the local socket or the relay.
  Start `nudge --daemon --peer` on a server and `/connect-peer <agent code>` from your
  laptop, and your session approves the daemon's tool calls. That gives supervised remote
  delegation without spawning children on another machine. The trust it requires is
  holding the agent pairing code.
- **Identity across machines** (was open question 6): a peer announces its **session name**
  (the `/session-rename` label, else the short session id) at attach; the far side records
  it as-is, never derives it. Rename-after-connect doesn't propagate — acceptable, documented.
- **Recursive spawn**: deferred indefinitely
  ([#54](https://github.com/nuudge/nudge/issues/54), closed not-planned) — the flat tree
  plus lateral peer edges covers every workflow met so far; `factory: None` for children
  stays as the enforcement.
- **Peer activity visibility**: an unsupervised peer's activity (tool calls, narration,
  permission prompts) stays entirely in its own session — only MessagePeer turns, errors,
  and lifecycle cross the edge. Watching a peer = attaching to *its* session with a
  watch-only code. Supervised children remain narrated (clipped) — the parent's only live
  view until question 2 is settled.

## Related

- [Symmetric communication](symmetric-communication.md) — the substrate both models share.
- [Subagents](subagents.md) — model 1 as shipped; [Peer agents](peers.md) — model 2 as
  shipped.
- [#44](https://github.com/nuudge/nudge/issues/44) — full-symmetry iteration 2 (phases);
  [#51](https://github.com/nuudge/nudge/issues/51) — the `/peers` narrowing this doc grew
  out of; [#53](https://github.com/nuudge/nudge/issues/53) — remote peer edges (model 2).
