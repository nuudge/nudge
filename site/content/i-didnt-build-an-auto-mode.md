+++
title = "I didn't build an auto mode — I used a supervisor instead"
date = 2026-07-23
description = "Auto mode in today's coding agents means vacating the reviewer seat. A case study in doing the opposite: the agent I was already talking to stayed in the reviewer seat as supervisor while a spawned worker did the work — the supervisor approved 170 tool calls, ruled on the hard decisions, and escalated only what a machine can't know."
+++

*Auto mode in today's coding agents means vacating the reviewer seat. Here's what happened when I did the opposite: the agent I was already talking to stayed in the reviewer seat, a freshly spawned worker did the work, and a risky history-rewriting refactor ran start to finish with almost none of my attention.*

## Auto mode is a permission bypass

Every coding agent has some version of auto mode, and they're mostly the same trade. claude-code gives you auto-accept for edits, allowlist rules, and `--dangerously-skip-permissions` — the name says it all. You want throughput, so you remove the reviewer. The gate is either a static rule or nothing.

The problem is that a rule sees strings, not situations. It can't tell `rm -rf` on a scratch clone (fine) from `rm -rf` on the source repo (disaster). Both match the same pattern. So you either approve everything by hand, or you close your eyes and hope.

I was planning to add an auto mode to [nudge](https://github.com/nuudge/nudge), my from-scratch coding agent. Then I used sub-agents on a real task at my day job, and realized I don't need one. The agent I was already talking to spawned a worker, kept the reviewer seat for itself, and *became* the auto mode — a reviewer that actually reads what it approves.

## The task

The job: split an internal Python monorepo into two repos — a CLI/tooling repo and a service repo — using `git filter-repo`, preserving history in both halves. This is about as unforgiving as refactoring gets. History rewriting, fresh clones, destructive operations, a dependency graph to cut cleanly, CI to carve up, docs and cross-repo links to fix. One wrong `--path` set and you're re-doing everything; one careless commit in the source repo and you've polluted the thing you're splitting.

Not a demo repo. The actual codebase my team ships from.

### My setup

First, the planning happened the normal way: me and the agent, interactively, producing a plan file in the repo root. Boundaries, path sets, dependency split, verification steps. That part stayed human-paced because that's where the judgment lives.

Then, instead of executing the plan myself — or letting one agent churn through it unsupervised — I asked the agent to spawn a worker and supervise it. Note what didn't happen here: I didn't switch modes, didn't launch some special supervisor process, didn't configure anything. The same session I'd been planning in opened a connection to a new agent and changed jobs — from doing the work to reviewing it. The supervisor wrote the worker's brief itself. Two lines from that brief carry the whole architecture:

> Treat me as the user: MessagePeer is the ONLY channel that reaches me — when you need input or approval, send a message and end your turn; my reply will arrive as your next instruction.

> Message me whenever a plan step turns out wrong in practice, a decision is ambiguous, or verification fails — do not improvise around failures silently.

The brief also named four mandatory checkpoints: report the computed path sets before running filter-repo, stop before anything destructive, ask before rewriting cross-repo links, and file a final report. Plus hard constraints: work only on fresh clones, never touch the source working copy, push nothing anywhere.

Then I mostly got out of the way.

### The execution

- The worker made **170 supervised tool calls**. Every one surfaced to the supervisor as a check-in; the supervisor reviewed and approved every one. None reached me.
- **4 checkpoints** where the worker stopped and waited for a ruling.
- **~8 interventions from me**, total — and only for things no machine could know: the real URLs of repos that didn't exist yet, whose name goes on the commits, two scope decisions.

Two moments show what the reviewer seat is actually worth.

**Checkpoint 1.** The worker finished analyzing the repo and reported its proposed path sets — along with two genuine problems the plan didn't cover: a test that reached across the new repo boundary, and some automation that belonged to neither half cleanly. A static permission rule has no opinion about any of that, but the supervisor did: delete the cross-boundary test on one side, keep a trimmed variant on the other, defer the automation and note it as a follow-up. It ruled on both points the worker raised and let it continue. All of this happened without my intervention — I only read about it later.

**The leaked secret.** Mid-task, the worker found a hardcoded API token in a script — sitting in the git history it was about to copy into two brand-new repos. It flagged the finding and escalated to the supervisor instead of working around it. The supervisor decided on a history scrub: redact the token via filter-repo's replace-text mechanism, and report the finding up to me. An unsupervised run would have carried that secret, silently, into two fresh histories.

That's the difference in one sentence: auto mode removes judgment from the loop; this adds a second layer of it.

### Done isn't done

When the final report came in — both repos carved, tests green, history preserved — I told the supervisor to keep the worker around while I verified the result.

Good thing. Over the next hour I found three follow-ups: commit authorship needed rewriting, a component belonged in the other repo after all, and some CI jobs we'd deferred needed migrating — which meant *inverting* one of the supervisor's earlier checkpoint decisions. Each one went to the same live worker, which still had the entire split in context. No re-explaining, no cold start.

## What auto mode can't do

Point by point, against the two mechanisms claude-code gives you today:

| claude-code | this session |
|---|---|
| The gate is a static rule or nothing. | The gate was an agent holding the plan. It approved `rm -rf` on fresh clones while the brief forbade touching the source repo at all — a distinction no pattern matcher can express. |
| Sub-agents are one-shot: task in, final report out. The worker can't ask a question mid-run. | The worker stopped at checkpoints and raised blockers; the supervisor ruled without waking me. |
| No way to steer a running sub-agent. | Scope changes flowed down mid-run as ordinary messages — including reversing an earlier decision. |
| A sub-agent's permission prompts fall through to the human. | 170 prompts absorbed by the supervisor. I answered eight things, all of them things only I could know. |
| The sub-agent dies after its report. | "Keep the worker around" — three follow-up tasks to the same agent, full context intact. |
| Failures get improvised around silently. | Standing escalation duty in the brief. That's how the hardcoded token became a history scrub instead of a leak. |

## This is symmetric communication paying off

Here's the part I care about most: none of this is a feature. There is no auto-mode subsystem in nudge, no supervisor mode, no approval-routing engine. This whole setup is one design idea cashing out.

The idea, from [an earlier post](/an-agent-is-just-another-client/): **an agent is just another client.** In nudge there is exactly one way to talk to an agent — you attach to its session, you observe what it does, you send it input — and it doesn't matter whether the thing attaching is my terminal, my phone, or another agent. Human and agent are equal citizens of the same protocol.

Everything in this post falls out of that symmetry:

- The supervisor attaches to the worker exactly the way my phone attaches to any session. It sees the same events, answers the same permission prompts. That's the approval stream.
- The worker's channel back to the supervisor is the same one my own messages travel on. That's the checkpoints and the escalations.
- "Supervisor" and "worker" aren't roles built into the system — there's no supervisor type, no worker type. They're just two ordinary connections pointing opposite ways, plus a brief that says who defers to whom. The same session that was my planning partner became a supervisor by opening one connection. Nothing about it changed.

That's also why "done isn't done" worked: the worker isn't a function call that returns and evaporates. It's a session, as real as the one I type into, so it stays alive, holds its context, and takes follow-up work — from me or from its supervisor, through the same channel.

The bet in that earlier post was: define one way to talk to an agent, make humans and agents equal citizens of it, and you never write the second subsystem. Auto mode is the second subsystem I didn't write.

## What it costs in tokens

Supervision isn't free, and the honest number is worth stating. I measured both transcripts from this session.

The traffic itself is trivial. The worker's tool outputs never reach the supervisor — each check-in is a one-line summary, about 180 tokens, and the supervisor's approval is about 16. Across 170 check-ins that's ~35K new tokens, roughly 3% of what the worker itself consumed.

The real cost is elsewhere: every check-in is a full inference for the supervisor, re-reading its whole context. Since the supervisor wakes for every worker tool call, it makes about as many LLM calls as the worker does — so the overhead ratio is roughly *supervisor context size versus worker context size*. In this session the supervisor carried the entire planning conversation, so its context rivaled the worker's, and supervision added roughly **60% to total spend** (with prompt caching; without it, this pattern would be a non-starter).

That 60% is not intrinsic. It scales with how much the supervisor holds in its head, not with how much work the worker does. A lean supervisor — context of just the plan and running summaries — drops the same 170 approvals to roughly **20% overhead**. 20% more cost for a reviewer that reads every action and knows the plan is a tradeoff I'll take on any task where the blast radius is real.

## The honest limits

Two caveats, so this doesn't read like marketing.

**The supervisor can rubber-stamp.** It approved 170 out of 170 tool calls here. If you're counting vetoes, the reviewer looks idle. But the value never was vetoes — it was judgment at the decision points: the checkpoint rulings, the token scrub, knowing which eight things to escalate. The mandatory checkpoints are what force substantive review; a supervisor without them is just latency.

**The constraints are prompt-level.** "Never push, never touch the source repo" lived in the brief, not in an enforcement layer. It held, this time. Hard guardrails under the soft ones are still worth building.

Both are engineering follow-ups, not holes in the idea.

## Where this lands

Auto mode asks: how much review am I willing to give up for speed? That's the wrong question. The right one is: who should be doing the reviewing at each altitude? In this session I was the engineering manager, the supervisor was the staff engineer, and the worker was the junior doing the work. Everyone reviewed the layer below, and the expensive human attention — mine — was spent only where it was irreplaceable.

This is the third thing that one symmetry has produced, after [driving a session from my phone](/drive-a-coding-agent-from-your-phone/) and [two agents talking as equals](/an-agent-is-just-another-client/). None of them were built as features. When humans and agents share one protocol, every new capability is a question of who connects to whom — not what to build next.

I'm not building an auto mode. There's already one running, and it reads what it signs.

---

*nudge is an open-source coding agent built from scratch in Rust. [Browse the code](https://github.com/nuudge/nudge), or read the design that made this free in [An agent is just another client](/an-agent-is-just-another-client/).*
