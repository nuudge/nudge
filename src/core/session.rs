use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::core::identity::ClientIdentity;
use crate::core::store::{SessionState, SessionStore};
use crate::llm::{ContentBlock, Message};

pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    // Human-readable label for the session, layered on top of the immutable
    // uuid `id`. `None` until the user renames (see `set_name`). The uuid stays
    // the stored identity (session row, message rows, resume cursor); the name
    // is pure metadata on the session row, so renaming never touches the
    // transcript or breaks resume.
    pub name: Option<String>,
    store: SessionStore,
    // Log entries produced by the in-flight turn but not yet in the store. They mirror
    // the messages the loop pushed past its `last_good_snapshot`: `commit` flushes them
    // when the turn lands on a valid boundary, `rollback` drops them when a provider
    // error (or steering failure) rolls memory back — so the store always holds exactly
    // the committed prefix of the transcript, never an entry the live model didn't keep.
    staged: Vec<(Message, Option<ClientIdentity>)>,
}

// One logged transcript entry: the model-facing message plus, for a typed user
// turn, the identity of whoever sent it. The stored text is CLEAN — attribution
// (the `[message from peer …]` prefix the model sees, the `name > ` stamp a UI
// shows) is derived from `sender` at build time, never baked into the log.
// Entries stored without a sender read back as `None` (unattributed); their user
// text may carry an already-baked prefix, which the absent sender leaves untouched.
pub struct LoggedMessage {
    pub message: Message,
    pub sender: Option<ClientIdentity>,
}

pub struct Resumed {
    pub session: Session,
    pub entries: Vec<LoggedMessage>,
    // Count of trailing entries discarded by strict truncation (orphaned
    // tool_use, mid-flight tool_results, or a user prompt with no reply).
    // Surfaced to the TUI so the user knows their log was partially dropped.
    pub dropped: usize,
}

impl Session {
    // `store` is the storage endpoint (the caller's policy — e.g. the shared
    // ~/.nudge/nudge.db); session identity (the uuid) and the transcript rows are
    // the mechanism owned here, where the database file lives is not.
    pub fn create(cwd: PathBuf, store: SessionStore) -> Result<Self> {
        Self::create_with(cwd, store, None, None)
    }

    // A session spawned by another agent: identical, plus the provenance edge —
    // spawned_by (the parent's session id, a foreign key) and the spawn task —
    // recorded on the row so "which child did what, and why" is queryable later.
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
            store,
            staged: Vec::new(),
        })
    }

    // Re-open a session by ID from the store. Applies strict truncation so the
    // returned message vec ends on a valid alternating-role boundary that the
    // Messages API will accept on the next request; the truncated tail is marked
    // superseded in the store (excluded from future reads, never deleted) so a
    // later resume doesn't replay the orphaned entries mid-transcript. The row
    // flips back to 'running' — a resumed session is live again.
    pub fn open(id: &str, cwd: PathBuf, store: SessionStore) -> Result<Resumed> {
        let row = store
            .session(id)?
            .with_context(|| format!("no session {id} in the store"))?;
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
                store,
                staged: Vec::new(),
            },
            entries,
            dropped,
        })
    }

    // Set (or replace) the session's human label. The uuid `id` — session row,
    // message rows, resume cursor — is untouched; this only updates the row's
    // name/branch columns. `branch` is recorded as context for the `--list` picker.
    pub fn set_name(&mut self, name: String, branch: Option<String>) -> Result<()> {
        self.store.set_name(&self.id, &name, branch.as_deref())?;
        self.name = Some(name);
        Ok(())
    }

    // The session's cwd as a display string with $HOME collapsed to `~` — the form
    // shown in a controller's header. Computed daemon-side (it knows HOME) so a remote
    // client just renders the string.
    pub fn cwd_display(&self) -> String {
        tilde_path(&self.cwd)
    }

    // Log a message with the identity of whoever sent it (a typed user turn).
    // The message's text must be the clean, unattributed form — see `LoggedMessage`.
    // The message is serialized ONCE here and stored byte-exact; `open` reads those
    // same bytes back, which the floating cache breakpoint depends on. The write is
    // a single sub-millisecond INSERT at a turn boundary, so it runs inline rather
    // than through spawn_blocking.
    pub fn log_from(&mut self, message: &Message, sender: Option<&ClientIdentity>) -> Result<()> {
        let content = serde_json::to_string(message).context("serializing message")?;
        self.store
            .append_message(&self.id, &message.role, &content, sender)?;
        Ok(())
    }

    // Buffer a log entry for the in-flight turn. The message's text must be the clean,
    // unattributed form (see `LoggedMessage`); it reaches the store only when `commit`
    // runs.
    pub fn stage(&mut self, message: &Message, sender: Option<&ClientIdentity>) {
        self.staged.push((message.clone(), sender.cloned()));
    }

    // The turn landed on a valid boundary: flush every staged entry to the transcript
    // and clear the buffer. A write error surfaces as the turn's error exactly as an
    // eager `log` would today — no entry is silently dropped on a failed flush.
    pub fn commit(&mut self) -> Result<()> {
        for (message, sender) in std::mem::take(&mut self.staged) {
            self.log_from(&message, sender.as_ref())?;
        }
        Ok(())
    }

    // The turn rolled back (provider error, steering failure): drop its staged entries
    // so they never reach the store.
    pub fn rollback(&mut self) {
        self.staged.clear();
    }

    // Graceful teardown: mark the row 'ended'. Called once when the agent loop
    // winds down (quit, dismissal, or a loop error); a crash never gets here,
    // honestly leaving 'running' — last_activity is the staleness signal then.
    pub fn end(&self) -> Result<()> {
        self.store.set_state(&self.id, SessionState::Ended)
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

    // File-backed temp database so `open` can attach a fresh store connection to
    // the same data a `create`d session wrote (WAL allows both live at once).
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

    // log_from persists the sender next to the (clean) message; open restores it.
    // Messages logged without a sender (assistant turns) round-trip as None.
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

    // set_name updates the session row and the in-memory name; a fresh store
    // connection sees the persisted name with its branch context.
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

    // A renamed session's label survives a create → log → open round-trip: open
    // reloads the name from the session row keyed by the uuid.
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

    // The reported edge: a fresh session renamed before its first turn must still
    // be resumable — the session row exists from create, so it resolves by name
    // and opens as an empty, named session.
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

    // A created-but-never-logged session opens as empty (its row exists from
    // create), while an unknown id is a genuine error.
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

    // Strict truncation on open: the orphaned tail is dropped from the returned
    // entries AND marked superseded in the store, so a second open sees a clean
    // transcript with nothing left to drop.
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

    // The provenance edge: a spawned session's row carries spawned_by (an
    // enforced FK to the parent) and the task; a plain create leaves both NULL.
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

    // The state lifecycle: born running, ended on graceful teardown, running
    // again when a resume re-opens it.
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
