use anyhow::{Context, Result};
use std::env;
use std::path::PathBuf;

/// Layer the .env files into the process environment. Side-effect only: this
/// makes the files' contents visible to every later `env::var` read in the
/// process (including the dynamic MCP `token_env`/`client_secret_env` vars
/// named in `.mcp.json`), but requires and types nothing itself.
///
/// Precedence is "first load wins" — `dotenvy` never overrides a var already
/// present in the environment — so the effective order is:
/// real shell env > project `.env` > global `~/.nudge/config.env`.
pub fn load_dotenv() {
    let _ = dotenvy::dotenv();
    if let Some(home) = env::var_os("HOME") {
        let path = PathBuf::from(home).join(".nudge").join("config.env");
        let _ = dotenvy::from_path(path);
    }
}

/// nudge's own configuration, read from the process environment after
/// [`load_dotenv`] has layered the .env files in. This is the canonical list
/// of config vars the agent reads directly; required-ness is encoded in the
/// field types.
pub struct Config {
    /// `ANTHROPIC_API_KEY` — required to talk to the API.
    pub anthropic_api_key: String,
    /// `NUDGE_RELAY` — relay WebSocket URL for phone handoff. Optional: a plain
    /// local session runs and backgrounds without it, just with no QR.
    pub relay: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            anthropic_api_key: env::var("ANTHROPIC_API_KEY").context(
                "ANTHROPIC_API_KEY not set (shell env, project .env, or ~/.nudge/config.env)",
            )?,
            relay: env::var("NUDGE_RELAY").ok(),
        })
    }
}
