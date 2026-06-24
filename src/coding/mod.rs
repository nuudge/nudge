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
pub fn replay_events(messages: &[Message], dropped: usize) -> Vec<ControllerEvent> {
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
                            out.push(ControllerEvent::UserMessage { text: text.clone() });
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

pub fn open_resume(id: &str) -> Result<Resumed> {
    let cwd = std::env::current_dir().context("could not determine cwd")?;
    let dir = project_dir(&cwd)?;
    Session::open(id, cwd, dir)
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
