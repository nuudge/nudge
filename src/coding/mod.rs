use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::core::ControllerEvent;
use crate::core::session::{LoggedMessage, Resumed, Session};
use crate::core::store::{SessionState, SessionStore};
use crate::llm::{
    CODE_EXECUTION_NOTICE, ContentBlock, WEB_FETCH_TOOL, WEB_SEARCH_TOOL, merge_text_runs,
    ran_code_execution, web_fetch_result, web_fetch_url, web_search_query, web_search_results,
};

pub mod backend;
pub mod context;
pub mod file_state;
mod import;
pub mod mcp;
pub mod prompt;
pub mod skills;
pub mod tools;

pub use backend::{CodingBackend, print_preamble};

// Emits flat events (ToolUseStart, ToolResult separately) so the controller's
// live merge reassembles them and one render path serves live and replay.
// Lives in `coding` because it needs `tools::summarize`.
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
                        | ContentBlock::RedactedThinking { .. }
                        | ContentBlock::ServerToolUse { .. }
                        | ContentBlock::WebSearchToolResult { .. }
                        | ContentBlock::WebFetchToolResult { .. }
                        | ContentBlock::Other(_) => {}
                    }
                }
            }
            "assistant" => {
                for block in merge_text_runs(&msg.content) {
                    match block.as_ref() {
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
                        ContentBlock::ServerToolUse {
                            id, name, input, ..
                        } if name == WEB_SEARCH_TOOL => {
                            out.push(ControllerEvent::ToolUseStart {
                                id: id.clone(),
                                name: "WebSearch".into(),
                                summary: web_search_query(input),
                            });
                        }
                        ContentBlock::WebSearchToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            let (content, is_error) = web_search_results(content);
                            out.push(ControllerEvent::ToolResult {
                                id: tool_use_id.clone(),
                                content,
                                is_error,
                            });
                        }
                        ContentBlock::ServerToolUse {
                            id, name, input, ..
                        } if name == WEB_FETCH_TOOL => {
                            out.push(ControllerEvent::ToolUseStart {
                                id: id.clone(),
                                name: "WebFetch".into(),
                                summary: web_fetch_url(input),
                            });
                        }
                        ContentBlock::WebFetchToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            let (content, is_error) = web_fetch_result(content);
                            out.push(ControllerEvent::ToolResult {
                                id: tool_use_id.clone(),
                                content,
                                is_error,
                            });
                        }
                        ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. }
                        | ContentBlock::ToolResult { .. }
                        | ContentBlock::ServerToolUse { .. }
                        | ContentBlock::Other(_) => {}
                    }
                }
                if ran_code_execution(&msg.content) {
                    out.push(ControllerEvent::Notice {
                        text: CODE_EXECUTION_NOTICE.into(),
                    });
                }
            }
            _ => {}
        }
    }
    // A saved transcript ends at a turn boundary; TurnComplete resets the
    // status line, and live events re-set it if the daemon is mid-turn.
    if !out.is_empty() {
        out.push(ControllerEvent::TurnComplete);
    }
    out
}

fn db_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME env var not set")?;
    Ok(PathBuf::from(home).join(".nudge").join("nudge.db"))
}

// The marker is set only after a fully successful import, so a crash
// mid-import retries on the next open.
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

pub fn open_new_spawned(spawned_by: &str, spawn_task: &str) -> Result<Session> {
    let cwd = std::env::current_dir().context("could not determine cwd")?;
    let store = open_store()?;
    Session::create_spawned(cwd, store, spawned_by, spawn_task)
}

// `reference` is a session uuid or a name, resolved against this cwd's sessions.
pub fn open_resume(reference: &str) -> Result<Resumed> {
    let cwd = std::env::current_dir().context("could not determine cwd")?;
    let store = open_store()?;
    let id = store.resolve_reference(&cwd.display().to_string(), reference)?;
    Session::open(&id, cwd, store)
}

// `state` is best-effort: 'ended' is written only on graceful teardown, so a
// crashed session still reads 'running'.
pub struct SessionListing {
    pub id: String,
    pub name: Option<String>,
    pub branch: Option<String>,
    pub state: SessionState,
    pub last_activity: chrono::DateTime<chrono::Utc>,
    pub turns: i64,
}

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
                state: l.row.state,
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

    #[test]
    fn replay_merges_cited_text_fragments_around_search_cards() {
        let text = |t: &str| ContentBlock::Text { text: t.into() };
        let entries = vec![LoggedMessage {
            message: Message {
                role: "assistant".into(),
                content: vec![
                    text("| a |\n"),
                    text(""),
                    text("| b |"),
                    ContentBlock::ServerToolUse {
                        id: "srv1".into(),
                        name: "web_search".into(),
                        input: serde_json::json!({"query": "q"}),
                        extra: Default::default(),
                    },
                    ContentBlock::WebSearchToolResult {
                        tool_use_id: "srv1".into(),
                        content: serde_json::json!([]),
                        extra: Default::default(),
                    },
                    text("x"),
                    text("y"),
                ],
            },
            sender: None,
        }];

        let events: Vec<String> = replay_events(&entries, 0)
            .into_iter()
            .map(|ev| match ev {
                ControllerEvent::AssistantText { text } => format!("text {text:?}"),
                ControllerEvent::ToolUseStart { id, .. } => format!("start {id}"),
                ControllerEvent::ToolResult { id, .. } => format!("result {id}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            events,
            [
                r#"text "| a |\n| b |""#,
                "start srv1",
                "result srv1",
                r#"text "xy""#,
                "TurnComplete",
            ]
        );
    }

    #[test]
    fn replay_renders_web_fetch_cards() {
        let fetch = |id: &str, url: &str, content: serde_json::Value| {
            [
                ContentBlock::ServerToolUse {
                    id: id.into(),
                    name: "web_fetch".into(),
                    input: serde_json::json!({"url": url}),
                    extra: Default::default(),
                },
                ContentBlock::WebFetchToolResult {
                    tool_use_id: id.into(),
                    content,
                    extra: Default::default(),
                },
            ]
        };
        let mut content = vec![ContentBlock::Text {
            text: "Reading the PDF.".into(),
        }];
        content.extend(fetch(
            "srvf1",
            "https://example.com/paper.pdf",
            serde_json::json!({"type": "web_fetch_result", "url": "https://example.com/paper.pdf",
                               "content": {"type": "document",
                                           "source": {"type": "base64", "media_type": "application/pdf",
                                                      "data": "A".repeat(1_200_000)}}}),
        ));
        content.extend(fetch(
            "srvf2",
            "https://evil.example/",
            serde_json::json!({"type": "web_fetch_tool_result_error", "error_code": "url_not_in_prior_context"}),
        ));
        let entries = vec![LoggedMessage {
            message: Message {
                role: "assistant".into(),
                content,
            },
            sender: None,
        }];

        let events: Vec<String> = replay_events(&entries, 0)
            .into_iter()
            .map(|ev| match ev {
                ControllerEvent::AssistantText { text } => format!("text {text}"),
                ControllerEvent::ToolUseStart { id, name, summary } => {
                    format!("start {id} {name} {summary}")
                }
                ControllerEvent::ToolResult {
                    id,
                    content,
                    is_error,
                } => format!("result {id} {is_error} {content}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            events,
            [
                "text Reading the PDF.",
                "start srvf1 WebFetch https://example.com/paper.pdf",
                "result srvf1 false fetched https://example.com/paper.pdf (PDF, 1.2MB base64)",
                "start srvf2 WebFetch https://evil.example/",
                "result srvf2 true web fetch error: url_not_in_prior_context",
                "TurnComplete",
            ]
        );
    }
}
