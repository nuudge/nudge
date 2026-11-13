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
}

impl Session {
    pub fn create(cwd: PathBuf, store: SessionStore) -> Result<Self> {
        Self::create_with(cwd, store, None, None)
    }

    pub fn create_spawned(
        cwd: PathBuf,
        store: SessionStore,
        spawned_by: &str,
        spawn_task: &str,
    ) -> Result<Self> {
        Self::create_with(cwd, store, Some(spawned_by), Some(spawn_task))
    }

    fn create_with(
        cwd: PathBuf,
        store: SessionStore,
        spawned_by: Option<&str>,
        spawn_task: Option<&str>,
    ) -> Result<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        store.insert_session(&id, &cwd.display().to_string(), spawned_by, spawn_task)?;
        Ok(Self {
            id,
            cwd,
            name: None,
            auto: false,
            store,
            staged: Vec::new(),
        })
    }

    // Truncates to a boundary the Messages API accepts and marks the dropped tail
    // superseded, so a later resume doesn't replay it mid-transcript.
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
        truncate_to_clean_boundary(&mut entries);
        let dropped = original_len - entries.len();
        if dropped > 0 {
            let last_kept = match entries.len() {
                0 => -1,
                n => ordinals[n - 1],
            };
            store.mark_superseded_after(id, last_kept)?;
        }

        Ok(Resumed {
            session: Self {
                id: id.to_string(),
                cwd,
                name: row.name,
                auto,
                store,
                staged: Vec::new(),
            },
            entries,
            dropped,
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
    pub fn log_from(&mut self, message: &Message, sender: Option<&ClientIdentity>) -> Result<()> {
        let content = serde_json::to_string(message).context("serializing message")?;
        self.store
            .append_message(&self.id, &message.role, &content, sender)?;
        Ok(())
    }

    // `message` must be the clean, unattributed form (see `LoggedMessage`).
    pub fn stage(&mut self, message: &Message, sender: Option<&ClientIdentity>) {
        self.staged.push((message.clone(), sender.cloned()));
    }

    pub fn commit(&mut self) -> Result<()> {
        for (message, sender) in std::mem::take(&mut self.staged) {
            self.log_from(&message, sender.as_ref())?;
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

// Keep through the last assistant turn with no ToolUse: the only point where the next
// role is "user" and no tool_use awaits a result. Everything after is discarded.
fn truncate_to_clean_boundary(entries: &mut Vec<LoggedMessage>) {
    let mut cutoff: Option<usize> = None;
    for (i, entry) in entries.iter().enumerate().rev() {
        let msg = &entry.message;
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
        Some(n) => entries.truncate(n),
        None => entries.clear(),
    }
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
            store_at(&db)
                .resolve_reference("/proj", "early-name")
                .unwrap(),
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

    // The second open proves the dropped tail was persisted as superseded.
    #[test]
    fn open_truncates_orphaned_tail_and_supersedes_it() {
        let db = temp_db();
        let mut s = Session::create(cwd(), store_at(&db)).unwrap();
        let id = s.id.clone();
        s.log_from(&text_msg("user", "hi"), None).unwrap();
        s.log_from(&text_msg("assistant", "ok"), None).unwrap();
        s.log_from(&text_msg("user", "no reply yet"), None).unwrap();

        let resumed = Session::open(&id, cwd(), store_at(&db)).unwrap();
        assert_eq!(resumed.dropped, 1);
        assert_eq!(resumed.entries.len(), 2);

        let resumed = Session::open(&id, cwd(), store_at(&db)).unwrap();
        assert_eq!(resumed.dropped, 0);
        assert_eq!(resumed.entries.len(), 2);

        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn create_spawned_records_provenance_edge() {
        let db = temp_db();
        let parent = Session::create(cwd(), store_at(&db)).unwrap();
        let child =
            Session::create_spawned(cwd(), store_at(&db), &parent.id, "fix the bug").unwrap();

        let row = store_at(&db).session(&child.id).unwrap().unwrap();
        assert_eq!(row.spawned_by.as_deref(), Some(parent.id.as_str()));
        assert_eq!(row.spawn_task.as_deref(), Some("fix the bug"));
        let row = store_at(&db).session(&parent.id).unwrap().unwrap();
        assert!(row.spawned_by.is_none());
        assert!(row.spawn_task.is_none());

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
