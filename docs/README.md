# nudge documentation

Everything beyond the [project README](../README.md). Start with **Getting started**,
then dive into whichever component you're using.

## Getting started

- **[Getting started](getting-started.md)** — install the agent (from source, `cargo
  install`, or a prebuilt binary), configure your API key, and run your first session.

## Components

- **[Terminal agent](terminal-agent.md)** — the core binary: CLI flags, TUI controls,
  slash commands, the header (model / branch / cwd / session id) and per-turn token
  display, thinking modes, and sessions/resume.
- **[Remote control & relay](remote-and-relay.md)** — detach a session, hand it off to
  your phone, run multi-client co-op, and use the shared relay or host your own.
- **[Mobile app (Android)](mobile-app.md)** — turn your phone into a live front-end:
  features, install (prebuilt APK or build from source), and pairing.
- **[Subagents](subagents.md)** — spawn, supervise, converse with, and dismiss child
  agents, and the symmetric-communication design they fall out of.
- **[MCP servers](mcp.md)** — connect external Model Context Protocol servers: the
  three tool layers, `/mcp` commands, and the full `.mcp.json` config reference.
- **[Skills](skills.md)** — package reusable expertise as loadable folders.

## Concepts

- **[Security](security.md)** — end-to-end encryption, the ciphertext-blind relay, the
  permission model, and the tool-design choices that keep edits safe.
- **[Symmetric communication](symmetric-communication.md)** — the design philosophy behind
  remote control, multi-client co-op, and subagents: a human, a phone, and an agent are the
  same client.

## Operation

- **[Roadmap](roadmap.md)** — what's deliberately not supported yet and what's coming,
  with links to the tracking issues.
- **[Architecture](../ARCHITECTURE.md)** — the layered module design, the session
  host/broker runtime, and selected design decisions (developer-facing).
- **[Deploying the relay](../deploy/README.md)** — stand up your own public relay box
  with TLS.
- **[Contributing](../CONTRIBUTING.md)** — toolchain, local checks, and the PR workflow.
