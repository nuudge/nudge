use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

use crate::llm::{ContentBlock, Message};

pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    pub log_path: PathBuf,
}

pub struct Resumed {
    pub session: Session,
    pub messages: Vec<Message>,
    // Count of trailing entries discarded by strict truncation (orphaned
    // tool_use, mid-flight tool_results, or a user prompt with no reply).
    // Surfaced to the TUI so the user knows their log was partially dropped.
    pub dropped: usize,
}

impl Session {
    // `dir` is the storage directory (the caller's policy — e.g. a cwd-keyed
    // project folder); the log lives at `<dir>/<id>.jsonl`. Session identity
    // (the uuid) and the JSONL transcript are the mechanism owned here; where
    // it lands on disk is not.
    pub fn create(cwd: PathBuf, dir: PathBuf) -> Result<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("could not create session dir {}", dir.display()))?;
        let log_path = dir.join(format!("{id}.jsonl"));
        Ok(Self { id, cwd, log_path })
    }

    // Re-open a session by ID from `dir`. Applies strict truncation so the
    // returned message vec ends on a valid alternating-role boundary that the
    // Messages API will accept on the next request.
    pub fn open(id: &str, cwd: PathBuf, dir: PathBuf) -> Result<Resumed> {
        let log_path = dir.join(format!("{id}.jsonl"));

        let raw = std::fs::read_to_string(&log_path)
            .with_context(|| format!("could not read session log {}", log_path.display()))?;

        let mut messages: Vec<Message> = Vec::new();
        for (i, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let envelope: Value = serde_json::from_str(line).with_context(|| {
                format!("invalid JSON on line {} of {}", i + 1, log_path.display())
            })?;
            let msg_value = envelope.get("message").with_context(|| {
                format!(
                    "missing `message` field on line {} of {}",
                    i + 1,
                    log_path.display()
                )
            })?;
            let msg: Message = serde_json::from_value(msg_value.clone()).with_context(|| {
                format!("invalid message on line {} of {}", i + 1, log_path.display())
            })?;
            messages.push(msg);
        }

        let original_len = messages.len();
        truncate_to_clean_boundary(&mut messages);
        let dropped = original_len - messages.len();

        Ok(Resumed {
            session: Self {
                id: id.to_string(),
                cwd,
                log_path,
            },
            messages,
            dropped,
        })
    }

    // The session's cwd as a display string with $HOME collapsed to `~` — the form
    // shown in a controller's header. Computed daemon-side (it knows HOME) so a remote
    // client just renders the string.
    pub fn cwd_display(&self) -> String {
        tilde_path(&self.cwd)
    }

    pub async fn log(&self, message: &Message) -> Result<()> {
        let event = json!({
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "sessionId": self.id,
            "cwd": self.cwd.display().to_string(),
            "message": message,
        });
        let line = format!("{event}\n");
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .await
            .with_context(|| format!("could not open session log {}", self.log_path.display()))?;
        file.write_all(line.as_bytes())
            .await
            .context("failed to write to session log")?;
        Ok(())
    }
}

// Collapse a leading $HOME to `~` for display. Falls back to the full path when HOME
// is unset or isn't a prefix. Lives here (in `core`) so both the daemon seed and the
// agent loop can format the cwd identically, without depending on the TUI layer.
pub fn tilde_path(path: &Path) -> String {
    let display = path.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => match display.strip_prefix(&home) {
            Some("") => "~".into(),
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => display,
        },
        _ => display,
    }
}

// Strict truncation: keep only up to and including the most recent assistant
// turn whose content carries no ToolUse blocks. That is the only state where
// the next expected role is "user" and there is no dangling tool_use awaiting
// a tool_result — i.e., a valid place to hand back to the outer loop and wait
// for the next user message. Anything beyond (orphaned tool_use after a crash,
// stray tool_results, a user prompt that never got a reply) is discarded.
fn truncate_to_clean_boundary(messages: &mut Vec<Message>) {
    let mut cutoff: Option<usize> = None;
    for (i, msg) in messages.iter().enumerate().rev() {
        let is_clean_assistant = msg.role == "assistant"
            && !msg
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
        if is_clean_assistant {
            cutoff = Some(i + 1);
            break;
        }
    }
    match cutoff {
        Some(n) => messages.truncate(n),
        None => messages.clear(),
    }
}
