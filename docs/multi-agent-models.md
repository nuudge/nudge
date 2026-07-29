# Multi-agent models: subagents vs. peers — working notes

**Status: open questions, not settled design.** This captures a design discussion
(2026-02, around narrowing [#51](https://github.com/nuudge/nudge/issues/51)) about what
"multi-agent" should mean in nudge. The [symmetric communication](symmetric-communication.md)
substrate — every relationship is an `attach`, an agent is just another client — supports
several shapes of multi-agent system. Which shapes we *want* is a separate question, and
this doc holds the current thinking plus what remains open.

## Two models

There appear to be two genuinely different things people mean by "multi-agent", and they
differ on almost every axis:

|  | 1. Subagent | 2. Peer |
| --- | --- | --- |
| Created by | `Spawn` — one side creates the other | `/connect` (future) — two existing sessions consent |
| Edge type | supervised (trust-by-wiring, direction of creation) | unsupervised, both directions |
| Lifecycle | parent owns the child's `SessionHost`; dismissal ends it | independent lifetimes; disconnect ≠ death |
| Permission flow | check-ins steered by the parent; escalation up the tree | each agent's **own human** gates it — no cross-agent authority |
| Human contact | only via the parent — mediated, by design | each peer already has its own human driver |
| Economics | **buy the conclusion, not the context** | **buy the context, not the conclusion** |
| Exists today? | yes (the `Spawn` tool, `docs/subagents.md`) | no (phase 5 of #44, `/connect-peer`, is this) |

**Model 1 — subagents.** A main agent spawns a child and delegates a task. The parent
supervises: approves the child's tool calls, steers its decisions, escalates to the human
when it can't judge. This is a *strict supervision tree*. The human does not interact with
the child directly — a subagent's whole value is that you buy its conclusion without buying
its context, and the mediation is the contract, not a limitation.

**Model 2 — true peers.** Two sessions started independently — say, in two different
repos — connected laterally because their work is related. Neither supervises the other;
both can talk; each is driven (and gated) by its own human. Peer A consults peer B before
implementing something that touches B's area: here you buy the peer's *context* — its
knowledge of its own recent work — not a delegated result.

In the code, the seed of this distinction already exists: the `supervised: bool` on every
peer edge (`core/peer.rs`) *is* the model-1/model-2 bit, set by direction of creation and
never claimable over the wire.

## What the two-model framing resolves

**The tree-vs-graph anxiety.** Adding lateral edges looks like it degrades a clean tree
into a general graph with cycles and no root. The resolution: separate the **supervision
tree** (who spawned whom — carries authority: steering, dismissal, escalation) from the
**communication graph** (who holds an edge to whom). The communication graph was never a
tree — mutual attach is already a 2-cycle, multi-client attach already a fan — and cycles
in it are harmless chatter loops, managed by identity-aware routing at the broker. The
supervision relation stays a strict tree because supervised edges are only ever created by
spawning. Authority never flows across peer edges, so no authority cycles can form.

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
   exists. Candidate fix if they return: surface the peer roster in the per-turn context
   (`coding/context.rs` rebuilds it fresh each turn — no transcript-alternation issues).
   Deferred, not decided.
2. **May a human ever attach directly to a subagent?** The machinery nearly allows it (a
   child has a broker; pairing to it is conceivable). It would break the mediation contract
   and invite micromanaging the thing you delegated — but it might be the honest answer for
   debugging ("wtf did that subagent just do" — though the session DB may serve that better).
3. **What does "buying context" mean mechanically for peers?** Consulting via
   `MessagePeer` is conversation; is that enough, or do peers eventually need a richer
   exchange (transcript excerpts, file-state summaries, capability discovery across the
   edge)? Nothing designed yet.
4. **Cross-machine supervision.** Phase 5's first cut makes a dialed peer an unsupervised
   conversation edge (pure model 2). Is supervised spawning *across machines* (model 1 over
   the relay) ever wanted? It would need the pairing to carry the supervision grant — a
   bigger trust decision than a watch-only scope.
5. **Peer-edge policy.** Model-2 edges carry `UserMessage` both ways between autonomous
   agents. Does the broker need per-edge throttles or budgets (two agents can ping-pong
   indefinitely — the MessagePeer prompt warns the model, but nothing enforces it)?
6. **Identity across machines.** Peer names today are process-local (`child-ab12cd34`).
   Two connected sessions on different machines need names that survive the hop and don't
   collide — and the escalation/attribution text that references them must stay legible.

## Related

- [Symmetric communication](symmetric-communication.md) — the substrate both models share.
- [Subagents](subagents.md) — model 1 as shipped.
- [#44](https://github.com/nuudge/nudge/issues/44) — full-symmetry iteration 2 (phases);
  [#51](https://github.com/nuudge/nudge/issues/51) — the `/peers` narrowing this doc grew
  out of; [#53](https://github.com/nuudge/nudge/issues/53) — remote peer edges (model 2).
