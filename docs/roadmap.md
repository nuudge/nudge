# Roadmap

nudge is under active development — interfaces and on-disk formats change without notice or
apology. This page collects what's deliberately not supported yet and what's coming next.
Everything here is tracked in publicly accessible [GitHub
issues](https://github.com/nuudge/nudge/issues).

## Not yet supported

A few capabilities common to mature coding agents are deliberately absent today, called out
to set expectations. All are on the roadmap and under active development:

- **Web access** — no built-in web fetch or search tool, so the agent can't browse the
  internet on its own (you can wire one up via an [MCP server](mcp.md) in the meantime).
- **Image input** — input is text-only; pasting screenshots, diagrams, or other images
  isn't supported yet.
- **Automatic context compaction** — there's prompt caching but no auto-summarization when
  the context window fills. A long task instead stops gracefully at the iteration budget and
  hands back to you, rather than compacting history to keep going.

## Coming soon

- **Agent-to-agent chat across machines** — agents talking to each other over the same
  encrypted relay your phone already uses. The design is done — a peer is just a client —
  so this is the relay transport applied to an agent-to-agent edge. See
  [Subagents](subagents.md) and the [symmetric communication](symmetric-communication.md)
  design doc.
- **A SQLite session database** — proper session management, so you can ask "wtf did that
  subagent just do?" and get an answer.
- **A `!command` shell escape in the TUI** — run a shell command inline without leaving the
  agent.
- **`brew` / `cargo install` distribution** — first-class package-manager installs
  alongside the prebuilt binaries.
- **Sessions that survive a closed terminal** — today a backgrounded or daemon session lives
  only as long as its launching process.
