use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::core::identity::ClientIdentity;
use crate::core::store::{SessionState, SessionStore};
use crate::llm::{ContentBlock, Message};

pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    // Metadata on the row only; the uuid stays the identity, so renaming never breaks resume.
    pub name: Option<String>,
    pub auto: bool,
    store: SessionStore,
    // The in-flight turn's entries, flushed on `commit` or dropped on `rollback`, so the
    // store only ever holds the committed prefix the live model kept.
    staged: Vec<(Message, Option<ClientIdentity>)>,
}

// Stored text is clean: attribution (peer prefix, UI stamp) is derived from `sender`
// at build time. Entries without a sender may carry an already-baked prefix.
pub struct LoggedMessage {
    pub message: Message,
    pub sender: Option<ClientIdentity>,
}

pub struct Resumed {
    pub session: Session,
    pub entries: Vec<LoggedMessage>,
    pub dropped: usize,
    // Closing entries appended to finish an interrupted turn.
    pub added: usize,
}

impl Session {
    pub fn create(cwd: PathBuf, store: SessionStore) -> Result<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        store.insert_session(&id, &cwd.display().to_string(), None, None)?;
        Ok(Self {
            id,
            cwd,
            name: None,
            auto: false,
            store,
            staged: Vec::new(),
        })
    }

    // Brings the transcript to a state the Messages API accepts: an unanswered trailing
    // prompt is marked superseded, and an interrupted turn is closed rather than dropped.
    pub fn open(id: &str, cwd: PathBuf, store: SessionStore) -> Result<Resumed> {
        let row = store
            .session(id)?
            .with_context(|| format!("no session {id} in the store"))?;
        let auto = store.auto(id)?;
        store.set_state(id, SessionState::Running)?;

        let mut ordinals = Vec::new();
        let mut entries: Vec<LoggedMessage> = Vec::new();
        for msg_row in store.load_transcript(id)? {
            let message: Message = serde_json::from_str(&msg_row.content).with_context(|| {
                format!(
                    "invalid message at ordinal {} of session {id}",
                    msg_row.ordinal
                )
            })?;
            ordinals.push(msg_row.ordinal);
            entries.push(LoggedMessage {
                message,
                sender: msg_row.sender,
            });
        }

        let original_len = entries.len();
        drop_unanswered_prompts(&mut entries);
        let dropped = original_len - entries.len();
        if dropped > 0 {
            let last_kept = match entries.len() {
                0 => -1,
                n => ordinals[n - 1],
            };
            store.mark_superseded_after(id, last_kept)?;
        }

        let mut session = Self {
            id: id.to_string(),
            cwd,
            name: row.name,
            auto,
            store,
            staged: Vec::new(),
        };
        // Persisted so the store keeps matching what the model is sent.
        let closing = closing_messages(&entries);
        let added = closing.len();
        for message in closing {
            session.log_from(&message, None)?;
            entries.push(LoggedMessage {
                message,
                sender: None,
            });
        }

        Ok(Resumed {
            session,
            entries,
            dropped,
            added,
        })
    }

    pub fn set_name(&mut self, name: String, branch: Option<String>) -> Result<()> {
        self.store.set_name(&self.id, &name, branch.as_deref())?;
        self.name = Some(name);
        Ok(())
    }

    // The in-memory flag changes even if saving fails, so a toggle always takes
    // effect for the running session; the error only means a resume won't restore it.
    pub fn set_auto(&mut self, on: bool) -> Result<()> {
        self.auto = on;
        self.store.set_auto(&self.id, on)
    }

    // Computed daemon-side (it knows HOME) so a remote client just renders the string.
    pub fn cwd_display(&self) -> String {
        tilde_path(&self.cwd)
    }

    // `message` must be the clean, unattributed form (see `LoggedMessage`). Serialized
    // once and stored byte-exact, which the floating cache breakpoint depends on.
    pub fn log_from(&mut self, message: &Message, sender: Option<&ClientIdentity>) -> Result<i64> {
        let content = serde_json::to_string(message).context("serializing message")?;
        self.store
            .append_message(&self.id, &message.role, &content, sender)
    }

    // `message` must be the clean, unattributed form (see `LoggedMessage`).
    pub fn stage(&mut self, message: &Message, sender: Option<&ClientIdentity>) {
        self.staged.push((message.clone(), sender.cloned()));
    }

    pub fn commit(&mut self) -> Result<()> {
        let mut ordinals = None;
        for (message, sender) in std::mem::take(&mut self.staged) {
            let ordinal = self.log_from(&message, sender.as_ref())?;
            ordinals = Some((ordinals.map_or(ordinal, |(first, _)| first), ordinal));
        }
        if let Some((first, last)) = ordinals {
            tracing::info!(first, last, "turn committed");
        }
        Ok(())
    }

    pub fn rollback(&mut self) {
        self.staged.clear();
    }

    // A crash never reaches this, leaving 'running'; last_activity is the staleness signal.
    pub fn end(&self) -> Result<()> {
        self.store.set_state(&self.id, SessionState::Ended)
    }
}

// Lives in `core` so the daemon seed and the agent loop format the cwd identically.
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

// Drops trailing prompts that never got a reply; they would precede the next prompt as
// two consecutive user turns.
fn drop_unanswered_prompts(entries: &mut Vec<LoggedMessage>) {
    while entries.last().is_some_and(|e| {
        e.message.role == "user"
            && !e
                .message
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
    }) {
        entries.pop();
    }
}

// The messages that close an interrupted turn so the API accepts the transcript: an
// error tool_result for each tool_use left without one, then an assistant turn so the
// next prompt doesn't follow a user turn.
fn closing_messages(entries: &[LoggedMessage]) -> Vec<Message> {
    let Some(last) = entries.last() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if last.message.role == "assistant" {
        let results: Vec<ContentBlock> = last
            .message
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: "Interrupted: this tool call did not run.".into(),
                    is_error: true,
                }),
                _ => None,
            })
            .collect();
        if results.is_empty() {
            return out;
        }
        out.push(Message {
            role: "user".into(),
            content: results,
        });
    }
    out.push(Message {
        role: "assistant".into(),
        content: vec![ContentBlock::Text {
            text: "This turn was interrupted before it finished; the work above may be partial."
                .into(),
        }],
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::identity::ClientKind;

    // File-backed so `open` can attach a second connection to the same data.
    fn temp_db() -> PathBuf {
        std::env::temp_dir().join(format!("nudge-session-{}.db", uuid::Uuid::new_v4()))
    }

    fn store_at(db: &Path) -> SessionStore {
        SessionStore::open(db).unwrap()
    }

    fn cwd() -> PathBuf {
        PathBuf::from("/proj")
    }

    fn text_msg(role: &str, text: &str) -> Message {
        Message {
            role: role.into(),
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    #[test]
    fn log_from_persists_sender_and_open_restores_it() {
        let db = temp_db();
        let mut s = Session::create(cwd(), store_at(&db)).unwrap();
        let id = s.id.clone();
        let who = ClientIdentity {
            kind: ClientKind::Agent,
            name: "child-abc".into(),
            session_id: None,
            task: None,
        };
        s.log_from(&text_msg("user", "clean text"), Some(&who))
            .unwrap();
        s.log_from(&text_msg("assistant", "ok"), None).unwrap();

        let resumed = Session::open(&id, cwd(), store_at(&db)).unwrap();
        assert_eq!(resumed.entries.len(), 2);
        match &resumed.entries[0].message.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "clean text"),
            other => panic!("expected clean text block, got {other:?}"),
        }
        let sender = resumed.entries[0]
            .sender
            .as_ref()
            .expect("sender persisted");
        assert_eq!(sender.name, "child-abc");
        assert_eq!(sender.kind, ClientKind::Agent);
        assert!(resumed.entries[1].sender.is_none());

        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn set_name_persists_to_store() {
        let db = temp_db();
        let mut s = Session::create(cwd(), store_at(&db)).unwrap();
        let id = s.id.clone();
        s.set_name("auth-fix".into(), Some("main".into())).unwrap();

        assert_eq!(s.name.as_deref(), Some("auth-fix"));
        let row = store_at(&db).session(&id).unwrap().expect("row written");
        assert_eq!(row.name.as_deref(), Some("auth-fix"));
        assert_eq!(row.branch.as_deref(), Some("main"));

        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn auto_persists_and_open_restores_it() {
        let db = temp_db();
        let mut s = Session::create(cwd(), store_at(&db)).unwrap();
        let id = s.id.clone();
        assert!(!s.auto);
        s.set_auto(true).unwrap();
        assert!(s.auto);

        let resumed = Session::open(&id, cwd(), store_at(&db)).unwrap();
        assert!(resumed.session.auto);

        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn open_reloads_persisted_name() {
        let db = temp_db();
        let id = {
            let mut s = Session::create(cwd(), store_at(&db)).unwrap();
            // A clean assistant turn so open()'s truncation keeps the transcript.
            s.log_from(&text_msg("assistant", "hi"), None).unwrap();
            s.set_name("my-label".into(), None).unwrap();
            s.id.clone()
        };

        let resumed = Session::open(&id, cwd(), store_at(&db)).unwrap();
        assert_eq!(resumed.session.name.as_deref(), Some("my-label"));

        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn rename_before_first_turn_is_resumable() {
        let db = temp_db();
        let id = {
            let mut s = Session::create(cwd(), store_at(&db)).unwrap();
            s.set_name("early-name".into(), None).unwrap();
            s.id.clone()
        };
        assert_eq!(
            store_at(&db).sessions_named("/proj", "early-name").unwrap()[0].id,
            id
        );
        let resumed = Session::open(&id, cwd(), store_at(&db)).unwrap();
        assert_eq!(resumed.session.name.as_deref(), Some("early-name"));
        assert!(resumed.entries.is_empty());

        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn open_of_never_logged_session_is_empty_and_unknown_id_errors() {
        let db = temp_db();
        let id = Session::create(cwd(), store_at(&db)).unwrap().id;

        let resumed = Session::open(&id, cwd(), store_at(&db)).unwrap();
        assert!(resumed.entries.is_empty());
        assert!(resumed.session.name.is_none());

        assert!(Session::open("ghost", cwd(), store_at(&db)).is_err());

        std::fs::remove_file(&db).ok();
    }

    // The second open proves the dropped prompt was persisted as superseded.
    #[test]
    fn open_supersedes_an_unanswered_trailing_prompt() {
        let db = temp_db();
        let mut s = Session::create(cwd(), store_at(&db)).unwrap();
        let id = s.id.clone();
        s.log_from(&text_msg("user", "hi"), None).unwrap();
        s.log_from(&text_msg("assistant", "ok"), None).unwrap();
        s.log_from(&text_msg("user", "no reply yet"), None).unwrap();

        let resumed = Session::open(&id, cwd(), store_at(&db)).unwrap();
        assert_eq!(resumed.dropped, 1);
        assert_eq!(resumed.added, 0);
        assert_eq!(resumed.entries.len(), 2);

        let resumed = Session::open(&id, cwd(), store_at(&db)).unwrap();
        assert_eq!(resumed.dropped, 0);
        assert_eq!(resumed.added, 0);
        assert_eq!(resumed.entries.len(), 2);

        std::fs::remove_file(&db).ok();
    }

    fn tool_use_msg(id: &str) -> Message {
        Message {
            role: "assistant".into(),
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: "CreateNew".into(),
                input: serde_json::json!({"file_path": "/proj/big.py"}),
            }],
        }
    }

    fn tool_result_msg(id: &str) -> Message {
        Message {
            role: "user".into(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: "ok".into(),
                is_error: false,
            }],
        }
    }

    // One long turn where every assistant message calls a tool, ending in a tool_use
    // that never got a result: the turn is closed, not dropped.
    #[test]
    fn open_closes_an_interrupted_tool_turn_and_persists_the_repair() {
        let db = temp_db();
        let mut s = Session::create(cwd(), store_at(&db)).unwrap();
        let id = s.id.clone();
        s.log_from(&text_msg("user", "write it"), None).unwrap();
        for i in 0..5 {
            s.log_from(&tool_use_msg(&format!("t{i}")), None).unwrap();
            s.log_from(&tool_result_msg(&format!("t{i}")), None)
                .unwrap();
        }
        s.log_from(&tool_use_msg("cut"), None).unwrap();

        let resumed = Session::open(&id, cwd(), store_at(&db)).unwrap();
        assert_eq!(resumed.dropped, 0);
        assert_eq!(resumed.added, 2);
        let msgs: Vec<&Message> = resumed.entries.iter().map(|e| &e.message).collect();
        assert_eq!(msgs.len(), 14);
        match msgs[12].content.as_slice() {
            [
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error: true,
                    ..
                },
            ] => assert_eq!(tool_use_id, "cut"),
            other => panic!("expected an error tool_result for the cut call, got {other:?}"),
        }
        assert_eq!(msgs[13].role, "assistant");
        assert!(
            msgs.windows(2).all(|w| w[0].role != w[1].role),
            "roles must alternate"
        );
        for (i, m) in msgs.iter().enumerate() {
            for b in &m.content {
                if let ContentBlock::ToolUse { id, .. } = b {
                    assert!(
                        msgs[i + 1].content.iter().any(|r| matches!(
                            r,
                            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id
                        )),
                        "tool_use {id} has no tool_result in the next message"
                    );
                }
            }
        }

        let reopened = Session::open(&id, cwd(), store_at(&db)).unwrap();
        assert_eq!(reopened.added, 0);
        assert_eq!(reopened.entries.len(), 14, "the repair is persisted once");

        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn end_marks_row_ended_and_open_marks_running_again() {
        let db = temp_db();
        let s = Session::create(cwd(), store_at(&db)).unwrap();
        let id = s.id.clone();
        let state = |db: &Path| store_at(db).session(&id).unwrap().unwrap().state;

        assert_eq!(state(&db), SessionState::Running);
        s.end().unwrap();
        assert_eq!(state(&db), SessionState::Ended);

        let _resumed = Session::open(&id, cwd(), store_at(&db)).unwrap();
        assert_eq!(state(&db), SessionState::Running);

        std::fs::remove_file(&db).ok();
    }
}
