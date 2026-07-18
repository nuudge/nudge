# Subagents

Ask the agent to spawn a subagent and it delegates: the child is a full peer agent with
its own session, working the task in parallel while the parent (and you) watch its activity
stream. There is no special subagent runtime — a child agent attaches to its parent the
exact same way as any other client (a TUI, the Android app). Same handshake, same wire. The
entire multi-agent story is one tiny protocol.

## How it works

Ask for a subagent in plain language — *"spawn a subagent to analyze the largest files in
this repo"* — and the agent calls its `Spawn` tool (gated: you approve every spawn, since a
subagent spends tokens autonomously). The child it creates is not a stripped-down task
runner; it is **a full nudge agent**: same tools, its own conversation and session log (it
shows up in `nudge --list` and can be `--resume`d later), running in the same directory
under a role prompt that tells it who spawned it and that its one obligation is to deliver
its result back.

While the child works:

- **You watch** — its activity streams into your TUI as `[peer child-…]` notices.
- **The parent supervises** — the child's gated tool calls (shell, file writes) check in
  with the parent, which decides each one *with its full conversation as context*: routine
  calls are approved, wrong turns are **denied with a corrective instruction** the child
  picks up as its next input, and anything the parent can't judge — destructive,
  irreversible, off-task — is **escalated to your permission prompt**, named (`peer child-…
  — rm -rf …`), exactly like the parent's own gated calls. The parent cannot approve a
  spawn's way around you: escalations and spawns always terminate at a human.
- **They converse** — either side can message the other (`MessagePeer`); a message arrives
  as the peer's next instruction, so you can have the parent redirect the child mid-task,
  or the child ask its parent a clarifying question.
- **It reports and retires** — the child messages its result back when done (or when
  blocked); the parent can then keep it around for follow-ups or dismiss it (`DismissPeer`,
  also gated). Dismissal ends the child's process; its session log persists.

The economics are the point: the child burns its own context reading files and running
commands, and the parent's transcript records only the spawn, compact supervision verdicts,
and the final report — you get parallel work without paying twice for the same context.

## In practice

Beyond parallelism, spawning a subagent is the way to get **hands-off autonomy without
turning off the safety rail**. When you delegate a task, the child's gated tool calls check
in with the *parent*, not with you — and the parent clears the routine ones from its own
context, only escalating the genuinely dangerous ones to your prompt. You approve the plan
once (the spawn); the parent handles the stream of approvals that would otherwise land on
you.

### Autonomous loops (e.g. polling an endpoint)

Say you need to poll an endpoint until something happens. Done directly, every `curl` is a
`Bash` call that stops and waits for your `y` — so a loop that should run unattended instead
demands a keypress on every iteration, and true autonomy is impossible.

Delegate it instead:

> *"Spawn a subagent to poll `https://…/status` every 30s and message me when it returns
> `ready`."*

Now the child runs the loop, and its repeated poll commands check in with the parent, which
recognizes them as routine and approves them on your behalf. You're not in the loop for each
request — you get the autonomous experience you wanted — while a call that *isn't* routine
(the child suddenly wanting to `rm` something, or hit a different host) still escalates to
you. The child messages you when the condition is met.

### A supervised stand-in for "auto mode"

Other agents ship a global "auto-accept" or "YOLO" mode that approves every tool call for a
whole session. nudge has no such toggle, and doesn't need one — a subagent gives the same
hands-off feel with judgment left switched on. Instead of blanket-approving *everything you
do*, you delegate a scoped task and let the parent supervise it: routine calls are approved
automatically, wrong turns get **denied with a correction** the child picks up as its next
instruction, and only the calls that warrant a human reach you.

The difference from a blanket auto mode is the point: it's *supervised* autonomy, not an
unconditional yes. The rail that routes destructive or off-task calls to a human is still
there — a subagent can never approve its own way around you.

## The design: an agent is just another client

There is no special subagent machinery under the hood — no spawn runtime, no side-band
message bus, no privileged parent object. The whole feature rests on one bet:

> Whatever is on the other end of a connection — a human at a terminal, a phone over the
> relay, or another agent — should be **indistinguishable** to the agent loop. If you
> cannot tell which it is, and it doesn't change how anything behaves, the design is right.

Everything above falls out of four consequences of that bet:

- **One mechanism, every party.** Reaching an agent is always the same operation: attach to
  its session, announce who you are, receive the event stream, send input back. A subagent
  attaches to its parent *exactly* the way your phone does — same handshake, same identity,
  same protocol — which is why humans and agents can share one session with every message
  attributed to its sender.
- **Connections compose; channels don't multiply.** Watching, driving, supervising,
  conversing, spawning — each is a composition of the two primitive half-channels every
  connection already has (*observe* the event stream, *drive* the input). Watch-mode isn't a
  mode, it's a second attach. A subagent's message to its parent isn't a new pathway — it
  lands on the same input as your keyboard.
- **Bidirectional means two connections, not a special duplex.** Spawning a child mutually
  attaches: the parent holds an ordinary connection to the child (watch it, steer it), and
  the child holds one back (report, ask questions). Two one-way edges — there is no
  "parent/child channel" type anywhere in the code.
- **Roles are emergent, not typed.** Parent and child run the same loop, the same tools,
  the same code paths. What makes the child a *subagent* is only the direction of creation
  (who spawned whom — which is what grants supervision and dismissal rights) and the role
  prompt it runs under. Swap the prompt and the same machinery is a peer, a reviewer, a
  pair-programmer.

The payoff for the discipline: capabilities compose for free. Because a peer is just a
client, agents on *different machines* need no new design — the same attach over the
encrypted relay (the thing your phone already uses) will carry agent-to-agent edges across
the network. That one small protocol is the entire multi-agent story.

## Further reading

- The full design write-up, including the five principles and the build order, lives in
  [Symmetric communication](symmetric-communication.md).
- The story of how this design emerged from a bug is in the blog post
  [*An agent is just another client*](../blog_posts/an-agent-is-just-another-client.refined.md).
