use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::core::ControllerEvent;
use crate::core::session::{LoggedMessage, Resumed, Session};
use crate::core::store::SessionStore;
use crate::llm::ContentBlock;

pub mod backend;
pub mod context;
pub mod file_state;
mod import;
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
// User turns are attributed from each entry's persisted sender; entries from
// pre-sender logs carry none and replay unattributed (empty sender), which
// renderers show as a plain own-style turn.
pub fn replay_events(entries: &[LoggedMessage], dropped: usize) -> Vec<ControllerEvent> {
    let mut out = Vec::new();
    if dropped > 0 {
        out.push(ControllerEvent::Warn {
            text: format!(
                "dropped {dropped} trailing entr{} from session log (strict truncation)",
                if dropped == 1 { "y" } else { "ies" }
            ),
        });
    }
    for entry in entries {
        let msg = &entry.message;
        match msg.role.as_str() {
            "user" => {
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => {
                            out.push(ControllerEvent::UserMessage {
                                text: text.clone(),
                                sender: entry
                                    .sender
                                    .as_ref()
                                    .map(|w| w.name.clone())
                                    .unwrap_or_default(),
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

// Session storage policy: one shared SQLite database at ~/.nudge/nudge.db. The
// core session/store mechanism is agnostic to this; the single-db-in-home layout
// is a coding-agent convention (a different agent type could store elsewhere).
fn db_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME env var not set")?;
    Ok(PathBuf::from(home).join(".nudge").join("nudge.db"))
}

// Every session entry point (new, resume, list) funnels through here: open the
// shared store and, on first contact, migrate the legacy JSONL layout into it.
// The marker is set only after a fully successful import, so a crash mid-import
// retries on the next open (idempotently — imported ids are skipped).
fn open_store() -> Result<SessionStore> {
    let mut store = SessionStore::open(&db_path()?)?;
    if !store.legacy_import_done()? {
        import::run(&mut store)?;
        store.mark_legacy_import_done()?;
    }
    Ok(store)
}

pub fn open_new() -> Result<Session> {
    let cwd = std::env::current_dir().context("could not determine cwd")?;
    let store = open_store()?;
    Session::create(cwd, store)
}

// Resume by either a session uuid or a human name: a name is resolved to its id
// against this cwd's sessions before opening (see `store::resolve_reference`).
pub fn open_resume(reference: &str) -> Result<Resumed> {
    let cwd = std::env::current_dir().context("could not determine cwd")?;
    let store = open_store()?;
    let id = store.resolve_reference(&cwd.display().to_string(), reference)?;
    Session::open(&id, cwd, store)
}

// One row for `nudge --list`: a session in the current project, with its name if
// it's been renamed. `last_activity` (the store row's commit-touched timestamp)
// sorts most-recent-first so the list reads like a recency-ordered history.
// `turns` is the live (non-superseded) message count, a proxy for how much
// history the session holds.
pub struct SessionListing {
    pub id: String,
    pub name: Option<String>,
    pub branch: Option<String>,
    pub last_activity: chrono::DateTime<chrono::Utc>,
    pub turns: i64,
}

// Enumerate the current project's sessions from the store, most recently active
// first. A fresh database yields an empty list (no sessions here yet).
pub fn list_sessions() -> Result<Vec<SessionListing>> {
    let cwd = std::env::current_dir().context("could not determine cwd")?;
    let store = open_store()?;
    store
        .list_by_cwd(&cwd.display().to_string())?
        .into_iter()
        .map(|l| {
            let last_activity = chrono::DateTime::parse_from_rfc3339(&l.row.last_activity)
                .with_context(|| format!("invalid last_activity on session {}", l.row.id))?
                .with_timezone(&chrono::Utc);
            Ok(SessionListing {
                id: l.row.id,
                name: l.row.name,
                branch: l.row.branch,
                last_activity,
                turns: l.turns,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ClientIdentity;
    use crate::llm::Message;

    // Replayed user turns are attributed from each entry's persisted sender; a
    // pre-sender entry replays with an empty sender (rendered unattributed).
    #[test]
    fn replay_seeds_sender_from_entry_metadata() {
        let user = |text: &str| Message {
            role: "user".into(),
            content: vec![ContentBlock::Text { text: text.into() }],
        };
        let entries = vec![
            LoggedMessage {
                message: user("hi"),
                sender: Some(ClientIdentity::human("alice")),
            },
            LoggedMessage {
                message: user("legacy"),
                sender: None,
            },
        ];

        let events = replay_events(&entries, 0);
        match &events[0] {
            ControllerEvent::UserMessage { text, sender } => {
                assert_eq!(text, "hi");
                assert_eq!(sender, "alice");
            }
            other => panic!("expected attributed UserMessage, got {other:?}"),
        }
        match &events[1] {
            ControllerEvent::UserMessage { text, sender } => {
                assert_eq!(text, "legacy");
                assert_eq!(sender, "");
            }
            other => panic!("expected unattributed UserMessage, got {other:?}"),
        }
        assert!(matches!(events.last(), Some(ControllerEvent::TurnComplete)));
    }
}
