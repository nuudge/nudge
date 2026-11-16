use anyhow::{Context, Result};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use crate::core::ControllerEvent;
use crate::core::reference::{self, Candidate};
use crate::core::session::{LoggedMessage, Resumed, Session};
use crate::core::store::{SessionRow, SessionStore};
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

pub fn new_child(parent: &str, task: &str) -> Result<SessionRow> {
    let cwd = std::env::current_dir().context("could not determine cwd")?;
    new_child_in(&open_store()?, &cwd.display().to_string(), parent, task)
}

fn new_child_in(store: &SessionStore, cwd: &str, parent: &str, task: &str) -> Result<SessionRow> {
    let id = uuid::Uuid::new_v4().to_string();
    store.insert_session(&id, cwd, Some(parent), Some(task))?;
    store
        .session(&id)?
        .with_context(|| format!("no session {id} in the store"))
}

// Only `parent`'s own children match.
pub fn find_child(parent: &str, reference: &str) -> Result<SessionRow> {
    let cwd = std::env::current_dir().context("could not determine cwd")?;
    find_child_in(
        &open_store()?,
        &cwd.display().to_string(),
        parent,
        reference,
    )
}

fn find_child_in(
    store: &SessionStore,
    cwd: &str,
    parent: &str,
    reference: &str,
) -> Result<SessionRow> {
    let row = resolve_session(store, cwd, reference, |_| false)?;
    if row.spawned_by.as_deref() != Some(parent) {
        anyhow::bail!("{reference} is not a child of this session; use /connect-peer to reach it");
    }
    Ok(row)
}

pub fn parent_of(session_id: &str) -> Result<Option<SessionRow>> {
    let store = open_store()?;
    match store.session(session_id)?.and_then(|row| row.spawned_by) {
        Some(parent) => store.session(&parent),
        None => Ok(None),
    }
}

pub fn open_resume(reference: &str) -> Result<Resumed> {
    let cwd = std::env::current_dir().context("could not determine cwd")?;
    let store = open_store()?;
    let row = resolve_session(&store, &cwd.display().to_string(), reference, |_| false)?;
    check_resume_dir(&row, &cwd.display().to_string())?;
    Session::open(&row.id, cwd, store)
}

// The tools are rooted at the cwd, so resuming elsewhere would point a session's history
// at the wrong tree.
fn check_resume_dir(row: &SessionRow, cwd: &str) -> Result<()> {
    if row.cwd != cwd {
        let label = row.name.as_deref().unwrap_or(&row.id);
        anyhow::bail!(
            "session {label} belongs to {dir}; resume it there: cd {dir} && nudge --resume {label}",
            dir = row.cwd
        );
    }
    Ok(())
}

// A running one of several same-named sessions is the one the user means to reach.
pub fn find_session(reference: &str) -> Result<SessionRow> {
    let cwd = std::env::current_dir().context("could not determine cwd")?;
    resolve_session(
        &open_store()?,
        &cwd.display().to_string(),
        reference,
        |id| session_socket_path(id).is_ok_and(|socket| is_listening(&socket)),
    )
}

// Every local session reference resolves here, by the rule in `core::reference`. A name
// counts only in `cwd`; of several sessions sharing it, the first `live` one wins, else
// the newest.
pub(crate) fn resolve_session(
    store: &SessionStore,
    cwd: &str,
    reference: &str,
    live: impl Fn(&str) -> bool,
) -> Result<SessionRow> {
    let candidates = store
        .session(reference)?
        .into_iter()
        .chain(store.sessions_by_short_id(reference)?)
        .chain(store.sessions_named(cwd, reference)?)
        .map(|row| Candidate {
            id: Some(row.id.clone()),
            name: row.name.clone(),
            item: row,
        })
        .collect();
    let mut found = reference::resolve(reference, candidates)?;
    if found.is_empty() {
        anyhow::bail!("no session matches {reference:?}");
    }
    let pick = found.iter().position(|r| live(&r.id)).unwrap_or(0);
    Ok(found.swap_remove(pick))
}

pub fn session_socket_path(session_id: &str) -> Result<PathBuf> {
    Ok(run_dir()?.join(format!("{session_id}.sock")))
}

pub fn session_log_path(session_id: &str) -> Result<PathBuf> {
    Ok(run_dir()?.join(format!("{session_id}.log")))
}

pub fn run_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME env var not set")?;
    let dir = PathBuf::from(home).join(".nudge").join("run");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

// The stored state can't say this: 'ended' is written only on graceful teardown, so a
// crashed session would read 'running' forever.
pub fn is_listening(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

pub struct SessionListing {
    pub id: String,
    pub name: Option<String>,
    pub branch: Option<String>,
    pub live: bool,
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
                live: is_listening(&session_socket_path(&l.row.id)?),
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

    #[test]
    fn resume_refuses_a_session_from_another_directory_with_a_cd_hint() {
        let store = SessionStore::open_in_memory().unwrap();
        store.insert_session("abc", "/projB", None, None).unwrap();
        let row = store.session("abc").unwrap().unwrap();

        assert!(check_resume_dir(&row, "/projB").is_ok());
        let err = check_resume_dir(&row, "/projA").unwrap_err().to_string();
        assert!(
            err.contains("cd /projB && nudge --resume abc"),
            "hint names the dir and the id: {err}"
        );
    }

    const SHARED_A: &str = "0e1b8a08-1111-4000-8000-000000000001";
    const SHARED_B: &str = "0e1b8a08-2222-4000-8000-000000000002";
    const LONE: &str = "f4b65f05-3333-4000-8000-000000000003";

    fn resolved(store: &SessionStore, reference: &str) -> Result<String> {
        resolve_session(store, "/proj", reference, |_| false).map(|r| r.id)
    }

    #[test]
    fn a_full_or_short_id_resolves_from_any_project_and_a_name_only_here() {
        let store = SessionStore::open_in_memory().unwrap();
        store
            .insert_session(LONE, "/elsewhere", None, None)
            .unwrap();
        store.set_name(LONE, "faraway", None).unwrap();
        store.insert_session("local", "/proj", None, None).unwrap();
        store.set_name("local", "worker", None).unwrap();

        assert_eq!(resolved(&store, LONE).unwrap(), LONE);
        assert_eq!(resolved(&store, "f4b65f05").unwrap(), LONE);
        assert_eq!(resolved(&store, "worker").unwrap(), "local");
        for reference in ["faraway", "ghost", "f4b65f0"] {
            let err = resolved(&store, reference).unwrap_err().to_string();
            assert_eq!(err, format!("no session matches {reference:?}"));
        }
    }

    #[test]
    fn an_ambiguous_short_id_fails_listing_the_sessions() {
        let store = SessionStore::open_in_memory().unwrap();
        store.insert_session(SHARED_A, "/proj", None, None).unwrap();
        store
            .insert_session(SHARED_B, "/elsewhere", None, None)
            .unwrap();
        let err = resolved(&store, "0e1b8a08").unwrap_err().to_string();
        assert!(
            err.contains("matches 2 sessions") && err.contains(SHARED_A) && err.contains(SHARED_B),
            "{err}"
        );
        assert_eq!(resolved(&store, SHARED_B).unwrap(), SHARED_B);

        store
            .insert_session(LONE, "/elsewhere", None, None)
            .unwrap();
        store.insert_session("named", "/proj", None, None).unwrap();
        store.set_name("named", "f4b65f05", None).unwrap();
        let err = resolved(&store, "f4b65f05").unwrap_err().to_string();
        assert!(err.contains("matches 2 sessions"), "{err}");
    }

    #[test]
    fn of_same_named_sessions_a_running_one_wins_else_the_newest() {
        let mut store = SessionStore::open_in_memory().unwrap();
        for id in ["older", "newer"] {
            store.insert_session(id, "/proj", None, None).unwrap();
            store.set_name(id, "dup", None).unwrap();
        }
        store.append_message("newer", "user", "{}", None).unwrap();
        let pick = |live: &'static str| {
            resolve_session(&store, "/proj", "dup", |id| id == live)
                .unwrap()
                .id
        };
        assert_eq!(pick("older"), "older", "the live one wins");
        assert_eq!(pick("none"), "newer", "otherwise the newest");
    }

    #[test]
    fn a_new_child_records_its_parent_and_task_and_is_unnamed() {
        let store = SessionStore::open_in_memory().unwrap();
        store.insert_session("parent", "/proj", None, None).unwrap();
        let child = new_child_in(&store, "/proj", "parent", "fix the bug").unwrap();
        assert_eq!(child.spawned_by.as_deref(), Some("parent"));
        assert_eq!(child.spawn_task.as_deref(), Some("fix the bug"));
        assert_eq!(child.cwd, "/proj");
        assert!(child.name.is_none());
    }

    #[test]
    fn only_this_sessions_own_children_are_found_by_id_short_id_or_name() {
        let store = SessionStore::open_in_memory().unwrap();
        for id in ["parent", "other"] {
            store.insert_session(id, "/proj", None, None).unwrap();
        }
        let mine = new_child_in(&store, "/proj", "parent", "a").unwrap();
        let theirs = new_child_in(&store, "/proj", "other", "b").unwrap();
        store.set_name(&mine.id, "worker", None).unwrap();

        for reference in [mine.id.as_str(), &mine.id[..8], "worker"] {
            assert_eq!(
                find_child_in(&store, "/proj", "parent", reference)
                    .unwrap()
                    .id,
                mine.id
            );
        }
        for reference in [theirs.id.as_str(), &theirs.id[..8], "other"] {
            let err = find_child_in(&store, "/proj", "parent", reference).unwrap_err();
            assert!(
                err.to_string().contains("not a child of this session"),
                "{err}"
            );
        }
        let err = find_child_in(&store, "/proj", "parent", "ghost").unwrap_err();
        assert!(err.to_string().contains("no session matches"), "{err}");
    }

    #[test]
    fn liveness_is_whether_the_socket_answers() {
        // Short: macOS caps socket paths at 104 bytes.
        let token = &uuid::Uuid::new_v4().simple().to_string()[..12];
        let socket = std::env::temp_dir().join(format!("live-{token}.sock"));
        assert!(!is_listening(&socket), "no file");

        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(is_listening(&socket), "bound");

        drop(listener);
        // A child forked by a concurrent test holds the listener until its exec closes it.
        let stale = (0..200).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            !is_listening(&socket)
        });
        assert!(stale, "stale file left by a crash");
        std::fs::remove_file(&socket).ok();
    }
}
