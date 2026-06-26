# Contributing to nudge

Thanks for your interest in nudge. It's under active development with sharp
edges, so fixes, features, and docs are all welcome. This guide covers the
toolchain, the local checks, and how to get a change merged.

If anything here is unclear or out of date, open an issue — that counts as a
contribution too.

## Toolchain setup

### Rust (the agent and the relay)

nudge is built with **Rust edition 2024**, so you need a recent toolchain.
Install it with [rustup](https://rustup.rs):

```bash
# match CI exactly (recommended)
rustup toolchain install 1.96.0
rustup component add rustfmt clippy

# IDE support
rustup component add rust-analyzer rust-src
```

CI builds and lints on **Rust 1.96.0**. Recent stable will usually build fine,
but Clippy's lints change between toolchain versions — so to avoid passing
locally and failing in CI, target 1.96.0. The lint job runs Clippy with
`-D warnings`, meaning **any warning is a hard failure**.

> **Note on mise.** The repo ships a `mise.toml`, but mise is now used only as
> a **task runner** — it no longer installs or pins the Rust toolchain (mise
> can't manage components like `rust-analyzer` and `rust-src`). Install Rust via
> rustup as above; mise is optional and just gives you task shortcuts (below).

### Android app

Contributing to the Android client (`android/`) additionally needs the
**Android SDK** and a **JDK 21** (minimum device API is 26 / Android 8.0). The
simplest path is to open `android/` in Android Studio. See the
[README](README.md#the-android-app-optional) for the command-line build.

### Relay / deployment

The relay is a standalone workspace crate (`cargo build -p relay`) and needs
nothing beyond the Rust toolchain. Provisioning and deploying a public relay box
is documented in [`deploy/README.md`](deploy/README.md).

## Building and running

```bash
cargo build              # build everything
cargo run                # run the agent from the current directory
cargo build -p relay     # build just the relay
```

The agent needs an `ANTHROPIC_API_KEY`; see the
[README quick start](README.md#quick-start) for how to provide it. `.env` and
`.mcp.json` are gitignored — never commit secrets.

## Checks before you push

Run the full check bundle locally before opening a merge request. It mirrors the
GitLab CI pipeline exactly:

```bash
mise run ci              # fmt --check + clippy -D warnings + tests
```

To auto-format and auto-fix lints before committing:

```bash
mise run fmt             # clippy --fix, then cargo fmt
```

`mise run` (or `mise tasks`) lists every available task.

If you'd rather not use mise, run the underlying commands directly — they are
exactly what CI runs:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --all
```

## Submitting a change

1. Create a branch off `main`.
2. Keep the change focused; unrelated cleanups belong in their own MR.
3. Write a descriptive commit message. Follow the existing prefix style:
   `feature:`, `fix:`, `refactor:`, `doc:`, `chore:`.
4. Make sure `mise run ci` passes.
5. Open a **Merge Request** against `main`. CI must be green to merge.

For larger or design-affecting changes, open an issue first to discuss the
approach before investing the work.

## Where things live

The codebase is layered — `coding → core → llm`, plus `transport` and `tui`. The
[How it works](README.md#how-it-works) section of the README has a module map
that's the fastest way to find where a change belongs.

## Reporting bugs and security issues

- **Bugs / feature requests:** open an issue with steps to reproduce.
- **Security vulnerabilities:** nudge handles end-to-end-encrypted sessions, so
  please report security issues **privately** to the maintainer rather than in a
  public issue, to allow a fix before disclosure.

## License

By contributing, you agree that your contributions are licensed under the
[MIT License](LICENSE) that covers the project.
