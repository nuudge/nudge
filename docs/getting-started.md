# Getting started

nudge is three components: the **terminal agent** (the whole product on its own), an
optional **relay** (the public meeting point that enables phone handoff and cross-machine
attach), and the optional **Android app**. Build whichever you need — the agent stands
alone.

This page covers installing and configuring the agent. For the relay see [Remote control &
relay](remote-and-relay.md); for the phone client see [Mobile app](mobile-app.md).

## Run from source

Requires Rust (edition 2024, install via [rustup](https://rustup.rs)) and an Anthropic API
key. CI builds on stable Rust.

```bash
git clone https://github.com/nuudge/nudge.git && cd nudge
echo 'ANTHROPIC_API_KEY=sk-ant-...' > .env   # .env is gitignored
cargo run
```

The agent operates in whatever directory you launch it from:

```bash
cd /path/to/your/project
cargo run --manifest-path /path/to/nudge/Cargo.toml
```

## Install the binary

`cargo install` builds an optimized `nudge` binary into `~/.cargo/bin` so you can run it
from any directory:

```bash
cargo install --path .                                # from a local checkout
cargo install --git https://github.com/nuudge/nudge   # straight from git
```

## Download a prebuilt binary

To skip the Rust toolchain, grab a released build for your platform from [the releases
page](https://github.com/nuudge/nudge/releases) (built by CI from the tagged source). Pick
the asset for your OS and CPU, make it executable, and put it on your `PATH`:

```bash
# Linux x86-64 — needs glibc 2.35+ (Ubuntu 22.04 / Debian 12 or newer)
curl -fL -o nudge "https://github.com/nuudge/nudge/releases/latest/download/nudge-x86_64-unknown-linux-gnu"
# macOS — Apple Silicon (M-series)
curl -fL -o nudge "https://github.com/nuudge/nudge/releases/latest/download/nudge-aarch64-apple-darwin"

chmod +x nudge && sudo mv nudge /usr/local/bin/   # or anywhere on your PATH
```

Each binary ships a matching `.sha256` on the release if you want to verify the download.
The binaries are **not code-signed**, so on macOS Gatekeeper blocks the first launch — clear
the quarantine flag with `xattr -d com.apple.quarantine /usr/local/bin/nudge` (or right-click
→ Open once). No prebuilt is published for older Linux (glibc < 2.35), Linux on ARM, Intel
Macs, or Windows — build from source with `cargo install` above.

## Configure your API key

The installed binary reads `ANTHROPIC_API_KEY` from the environment. There are three
sources, in increasing precedence:

1. **Global config** at `~/.nudge/config.env` — so you don't set it per project:

   ```bash
   mkdir -p ~/.nudge
   echo 'ANTHROPIC_API_KEY=sk-ant-...' > ~/.nudge/config.env
   nudge
   ```

2. **A `.env` in the current directory** — takes precedence over the global config.

3. **An exported shell variable** — overrides both:

   ```bash
   export ANTHROPIC_API_KEY=sk-ant-...
   nudge
   ```

`~/.nudge/config.env` is also where you set `NUDGE_RELAY` to enable phone handoff — see
[Remote control & relay](remote-and-relay.md).

## Next steps

- [Terminal agent](terminal-agent.md) — CLI flags, TUI controls, and slash commands.
- [Remote control & relay](remote-and-relay.md) — detach a session and drive it from
  elsewhere.
- [MCP servers](mcp.md) — give the agent extra tools.
