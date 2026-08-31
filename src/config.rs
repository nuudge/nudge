use anyhow::{Context, Result, bail};
use std::env;
use std::path::{Path, PathBuf};

use crate::cli::Thinking;
use crate::run::MAX_ITERATIONS;

// The model when NUDGE_MODEL is absent everywhere. Fresh configs set it
// explicitly (the template ships it uncommented), so this only covers configs
// that predate the template or had the line removed.
const FALLBACK_MODEL: &str = "claude-fable-5";

/// Layer the .env files into the process environment. Side-effect only: this
/// makes the files' contents visible to every later `env::var` read in the
/// process (including the dynamic MCP `token_env`/`client_secret_env` vars
/// named in `.mcp.json`), but requires and types nothing itself.
///
/// Precedence is "first load wins" — `dotenvy` never overrides a var already
/// present in the environment — so the effective order is:
/// real shell env > project `.env` > global `~/.nudge/config.env`.
///
/// First run also materializes the global file as a template (`cargo install`
/// has no post-install hook, so this is where a fresh install learns what's
/// configurable). Creation is best-effort: a read-only HOME must not stop the
/// agent.
pub fn load_dotenv() {
    let _ = dotenvy::dotenv();
    if let Some(home) = env::var_os("HOME") {
        let path = PathBuf::from(home).join(".nudge").join("config.env");
        if let Err(e) = ensure_global_config(&path) {
            eprintln!("[config] could not create {}: {e:#}", path.display());
        }
        let _ = dotenvy::from_path(path);
    }
}

// Write the template if (and only if) the global config file is missing — an
// existing file is the user's and is never touched. Every entry is commented
// out (documentation, not behavior) except NUDGE_RELAY, which defaults to the
// maintainer's shared relay so phone handoff and remote peers work out of the
// box; the relay is only dialed on explicit action (/background, --daemon,
// /connect-peer), never ambiently.
fn ensure_global_config(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, TEMPLATE)?;
    Ok(())
}

// The first-run template, vendored at the repo root so it's readable without
// digging into source. The test below pins its values to the code constants.
const TEMPLATE: &str = include_str!("../config.env.example");

/// nudge's own configuration, read from the process environment after
/// [`load_dotenv`] has layered the .env files in. This is the canonical list
/// of config vars the agent reads directly; required-ness is encoded in the
/// field types. (`NUDGE_THINKING` and `NUDGE_NAME` are read outside this
/// struct — see [`resolve_thinking`] and `run::local_identity` — because the
/// guest `--connect` path needs them without holding an API key.)
pub struct Config {
    /// `ANTHROPIC_API_KEY` — required to talk to the API.
    pub anthropic_api_key: String,
    /// `NUDGE_RELAY` — relay WebSocket URL for phone handoff. Optional: a plain
    /// local session runs and backgrounds without it, just with no QR.
    pub relay: Option<String>,
    /// `NUDGE_MODEL` — the model a new session starts on; falls back to the
    /// built-in default. Not validated against the catalog: the catalog is
    /// fetched at runtime, and /model accepts arbitrary ids too.
    pub model: String,
    /// `NUDGE_MAX_ITERATIONS` — model calls allowed per turn before the agent
    /// pauses for guidance; falls back to the built-in default.
    pub max_iterations: usize,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            anthropic_api_key: env::var("ANTHROPIC_API_KEY").context(
                "ANTHROPIC_API_KEY not set (shell env, project .env, or ~/.nudge/config.env)",
            )?,
            relay: env::var("NUDGE_RELAY").ok(),
            model: default_model(),
            max_iterations: parse_max_iterations(nonempty_var("NUDGE_MAX_ITERATIONS").as_deref())?,
        })
    }
}

/// The effective default model: `NUDGE_MODEL` > built-in fallback. A free
/// function (like [`resolve_thinking`]) so the guest `--connect` path — which
/// has no API key and thus no `Config` — resolves it identically.
pub fn default_model() -> String {
    nonempty_var("NUDGE_MODEL").unwrap_or_else(|| FALLBACK_MODEL.into())
}

/// The effective thinking display: explicit CLI flag > `NUDGE_THINKING` >
/// "summarized". A free function (not a `Config` field) so the guest
/// `--connect` path — which has no API key and thus no `Config` — resolves it
/// identically.
pub fn resolve_thinking(cli: Option<&Thinking>) -> Result<String> {
    if let Some(t) = cli {
        return Ok(t.as_display());
    }
    thinking_from(nonempty_var("NUDGE_THINKING").as_deref())
}

fn thinking_from(raw: Option<&str>) -> Result<String> {
    match raw {
        None => Ok("summarized".into()),
        Some("summarized") => Ok("summarized".into()),
        Some("omitted") => Ok("omitted".into()),
        Some(other) => bail!("NUDGE_THINKING must be 'summarized' or 'omitted', got '{other}'"),
    }
}

fn parse_max_iterations(raw: Option<&str>) -> Result<usize> {
    match raw {
        None => Ok(MAX_ITERATIONS),
        Some(s) => match s.parse::<usize>() {
            Ok(n) if n >= 1 => Ok(n),
            _ => bail!("NUDGE_MAX_ITERATIONS must be a positive integer, got '{s}'"),
        },
    }
}

// A set-but-empty var (e.g. the template's `NUDGE_NAME=` uncommented without a
// value) reads the same as unset.
fn nonempty_var(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The maintainer's shared relay — a template default, not a code fallback
    // (deleting the line from config.env disables it), so it lives here purely
    // to pin the example file.
    const DEFAULT_RELAY: &str = "wss://35.244.115.57.sslip.io";

    #[test]
    fn template_is_created_once_and_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("nudge-config-{}", uuid::Uuid::new_v4()));
        let path = dir.join(".nudge").join("config.env");

        ensure_global_config(&path).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        // The template ships these four as ACTIVE lines (a seamless default
        // setup); everything else is commented-out documentation. The vendored
        // example hardcodes its values, so pin them to the code constants —
        // and to the validated thinking modes — so the two can't drift apart.
        let active: Vec<&str> = written
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert_eq!(
            active,
            vec![
                format!("NUDGE_MODEL={FALLBACK_MODEL}"),
                format!("NUDGE_THINKING={}", thinking_from(None).unwrap()),
                format!("NUDGE_MAX_ITERATIONS={MAX_ITERATIONS}"),
                format!("NUDGE_RELAY={DEFAULT_RELAY}"),
            ],
            "{written}"
        );

        std::fs::write(&path, "NUDGE_MODEL=my-model\n").unwrap();
        ensure_global_config(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "NUDGE_MODEL=my-model\n"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn thinking_accepts_the_two_modes_and_defaults() {
        assert_eq!(thinking_from(None).unwrap(), "summarized");
        assert_eq!(thinking_from(Some("summarized")).unwrap(), "summarized");
        assert_eq!(thinking_from(Some("omitted")).unwrap(), "omitted");
        assert!(thinking_from(Some("loud")).is_err());
    }

    #[test]
    fn cli_thinking_overrides_the_environment() {
        assert_eq!(
            resolve_thinking(Some(&Thinking::Omitted)).unwrap(),
            "omitted"
        );
    }

    #[test]
    fn max_iterations_parses_and_rejects_garbage() {
        assert_eq!(parse_max_iterations(None).unwrap(), MAX_ITERATIONS);
        assert_eq!(parse_max_iterations(Some("100")).unwrap(), 100);
        assert!(parse_max_iterations(Some("0")).is_err());
        assert!(parse_max_iterations(Some("many")).is_err());
    }
}
