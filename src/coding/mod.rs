use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::core::ControllerEvent;
use crate::core::session::{Resumed, Session};
use crate::llm::{ContentBlock, Message};

pub mod backend;
pub mod context;
pub mod file_state;
pub mod mcp;
pub mod prompt;
pub mod skills;
pub mod tools;

pub use backend::{CodingBackend, print_preamble};

// Translate a resumed transcript into the `ControllerEvent`s the loop would have
// emitted live, so the broker can seed its replay buffer with the full history.
// This lives in `coding` (not `core`) because it needs `tools::summarize`, a
// coding-agent concern; `core` stays UI/tool-agnostic. Emits *flat* events
// (ToolUseStart then ToolResult as separate entries) — the controller's live
// merge reassembles them by id, exactly as for live events, so one render path
// serves both live and replay. Usage / permission outcomes aren't in the JSONL
// (they're runtime-only), so they're absent here, matching the old seed_replay.
// `owner` attributes the historical user turns: the transcript records no per-message
// identity, so replayed user messages are stamped with the resuming user's name (they
// render as "you" for that user, and as a named party for anyone else who attaches).
pub fn replay_events(messages: &[Message], dropped: usize, owner: &str) -> Vec<ControllerEvent> {
    let mut out = Vec::new();
    if dropped > 0 {
        out.push(ControllerEvent::Warn {
            text: format!(
                "dropped {dropped} trailing entr{} from session log (strict truncation)",
                if dropped == 1 { "y" } else { "ies" }
            ),
        });
    }
    for msg in messages {
        match msg.role.as_str() {
            "user" => {
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => {
                            out.push(ControllerEvent::UserMessage {
                                text: text.clone(),
                                sender: owner.to_string(),
                            });
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            out.push(ControllerEvent::ToolResult {
                                id: tool_use_id.clone(),
                                content: content.clone(),
                                is_error: *is_error,
                            });
                        }
                        ContentBlock::ToolUse { .. }
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. } => {}
                    }
                }
            }
            "assistant" => {
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => {
                            out.push(ControllerEvent::AssistantText { text: text.clone() });
                        }
                        ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                            out.push(ControllerEvent::AssistantThinking {
                                text: thinking.clone(),
                            });
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            out.push(ControllerEvent::ToolUseStart {
                                id: id.clone(),
                                name: name.clone(),
                                summary: tools::summarize(name, input),
                            });
                        }
                        ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. }
                        | ContentBlock::ToolResult { .. } => {}
                    }
                }
            }
            _ => {}
        }
    }
    // A saved transcript is always at a turn boundary, so close the replay with
    // TurnComplete: it resets the status line to idle (the tool events above
    // leave it on "running"/"thinking") and re-detects git. If the daemon's loop
    // is actually mid-turn, the live events that follow re-set the status.
    if !out.is_empty() {
        out.push(ControllerEvent::TurnComplete);
    }
    out
}

// Session storage policy: sessions are keyed by working directory under
// ~/.nudge/projects/<flattened-cwd>/. The core session mechanism is
// agnostic to this; the cwd-keyed layout is a coding-agent convention (a
// different agent type could key by something else entirely).
pub fn open_new() -> Result<Session> {
    let cwd = std::env::current_dir().context("could not determine cwd")?;
    let dir = project_dir(&cwd)?;
    Session::create(cwd, dir)
}

// Resume by either a session uuid or a human name: a name is resolved to its id
// against this project's index before opening (see `session::resolve_reference`).
pub fn open_resume(reference: &str) -> Result<Resumed> {
    let cwd = std::env::current_dir().context("could not determine cwd")?;
    let dir = project_dir(&cwd)?;
    let id = crate::core::session::resolve_reference(&dir, reference);
    Session::open(&id, cwd, dir)
}

// One row for `nudge --list`: a session in the current project, with its name
// (from the index) if it's been renamed. `modified` is the transcript file's mtime,
// used to sort most-recent-first so the list reads like a recency-ordered history.
// `size` is the transcript's byte length, a rough proxy for how much history it holds.
pub struct SessionListing {
    pub id: String,
    pub name: Option<String>,
    pub branch: Option<String>,
    pub modified: std::time::SystemTime,
    pub size: u64,
}

// Enumerate the current project's sessions: every `<id>.jsonl` transcript in the
// cwd-keyed dir, joined with the name index. Returns most-recently-modified first.
// An empty/missing project dir yields an empty list (no sessions here yet).
pub fn list_sessions() -> Result<Vec<SessionListing>> {
    use crate::core::session::read_index;
    let cwd = std::env::current_dir().context("could not determine cwd")?;
    let dir = project_dir(&cwd)?;
    let index = read_index(&dir.join("index.json"));

    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(out), // no sessions recorded for this project yet
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let fs_meta = entry.metadata().ok();
        let modified = fs_meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let size = fs_meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let meta = index.get(id);
        out.push(SessionListing {
            id: id.to_string(),
            name: meta.map(|e| e.name.clone()),
            branch: meta.and_then(|e| e.branch.clone()),
            modified,
            size,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    Ok(out)
}

fn project_dir(cwd: &Path) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME env var not set")?;
    Ok(PathBuf::from(home)
        .join(".nudge")
        .join("projects")
        .join(flatten_cwd(cwd)))
}

fn flatten_cwd(cwd: &Path) -> String {
    // Mirror Claude Code's flattening convention (any non-[a-zA-Z0-9-] → `-`)
    // so that future migration or interop is straightforward. Storage root is
    // ~/.nudge/ rather than ~/.claude/ to keep nudge's sessions
    // separate from a co-installed Claude Code.
    cwd.display()
        .to_string()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}
