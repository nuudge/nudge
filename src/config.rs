use anyhow::{Context, Result, bail};
use std::env;
use std::path::{Path, PathBuf};

use crate::cli::Thinking;
use crate::run::MAX_ITERATIONS;

// Only for configs lacking NUDGE_MODEL; the template sets it explicitly.
const FALLBACK_MODEL: &str = "claude-fable-5";

/// Precedence: shell env > project `.env` > `~/.nudge/config.env` (dotenvy never
/// overrides a set var). First run writes the global template: `cargo install` has
/// no post-install hook.
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

// An existing file is the user's and is never touched. The template's relay default
// is dialed only on explicit action, never ambiently.
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

// The test below pins its values to the code constants.
const TEMPLATE: &str = include_str!("../config.env.example");

/// `NUDGE_THINKING` and `NUDGE_NAME` are read outside this struct because the guest
/// `--attach` path needs them without an API key.
pub struct Config {
    pub anthropic_api_key: String,
    pub relay: Option<String>,
    /// Not validated against the catalog: /model accepts arbitrary ids too.
    pub model: String,
    pub max_iterations: usize,
    pub web_search: bool,
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
            web_search: parse_web_search(nonempty_var("NUDGE_WEB_SEARCH").as_deref())?,
        })
    }
}

/// A free function so the keyless guest `--attach` path resolves it identically.
pub fn default_model() -> String {
    nonempty_var("NUDGE_MODEL").unwrap_or_else(|| FALLBACK_MODEL.into())
}

/// CLI flag > `NUDGE_THINKING` > "summarized".
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

fn parse_web_search(raw: Option<&str>) -> Result<bool> {
    match raw {
        None | Some("on") => Ok(true),
        Some("off") => Ok(false),
        Some(other) => bail!("NUDGE_WEB_SEARCH must be 'on' or 'off', got '{other}'"),
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

    // A template default, not a code fallback; here only to pin the example file.
    const DEFAULT_RELAY: &str = "wss://35.244.115.57.sslip.io";

    #[test]
    fn template_is_created_once_and_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("nudge-config-{}", uuid::Uuid::new_v4()));
        let path = dir.join(".nudge").join("config.env");

        ensure_global_config(&path).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        // Pin the template's active lines to the code constants so they can't drift.
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

    #[test]
    fn web_search_defaults_on_and_rejects_garbage() {
        assert!(parse_web_search(None).unwrap());
        assert!(parse_web_search(Some("on")).unwrap());
        assert!(!parse_web_search(Some("off")).unwrap());
        assert!(parse_web_search(Some("no")).is_err());
    }
}
