use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

use crate::llm::{ContentBlock, Message};

pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    pub log_path: PathBuf,
    // Human-readable label for the session, layered on top of the immutable
    // uuid `id`. `None` until the user renames (see `set_name`). The uuid stays
    // the on-disk identity (filename, log envelopes, resume cursor); the name is
    // pure metadata stored in the per-project index, so renaming never touches
    // the transcript or breaks resume.
    pub name: Option<String>,
    // The per-project name index, `<dir>/index.json` (id → entry). Path policy is
    // the caller's (it owns `dir`); the read/write mechanism lives here.
    index_path: PathBuf,
}

// One row of the per-project session index: the human name plus light context
// for the `--list` picker. Keyed by session uuid in the on-disk map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub updated: String,
}

type Index = BTreeMap<String, IndexEntry>;

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
        let index_path = dir.join("index.json");
        Ok(Self {
            id,
            cwd,
            log_path,
            name: None,
            index_path,
        })
    }

    // Re-open a session by ID from `dir`. Applies strict truncation so the
    // returned message vec ends on a valid alternating-role boundary that the
    // Messages API will accept on the next request.
    pub fn open(id: &str, cwd: PathBuf, dir: PathBuf) -> Result<Resumed> {
        let log_path = dir.join(format!("{id}.jsonl"));
        let index_path = dir.join("index.json");
        let name = read_index(&index_path).remove(id).map(|e| e.name);

        // A missing transcript is normally a hard error (a typo'd --resume id). But a
        // session the index knows about (it has a name) with no transcript is just an
        // empty session — renamed before its first turn, or its file removed — so
        // recover it as empty rather than failing. An *unknown* id keeps erroring.
        let raw = match std::fs::read_to_string(&log_path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && name.is_some() => String::new(),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("could not read session log {}", log_path.display()));
            }
        };

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
                format!(
                    "invalid message on line {} of {}",
                    i + 1,
                    log_path.display()
                )
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
                name,
                index_path,
            },
            messages,
            dropped,
        })
    }

    // Set (or replace) the session's human label and persist it to the per-project
    // index. The uuid `id` — filename, log envelopes, resume cursor — is untouched;
    // this only writes the id → {name, branch, updated} row. `branch` is recorded as
    // context for the `--list` picker. Last writer wins on the index file.
    pub fn set_name(&mut self, name: String, branch: Option<String>) -> Result<()> {
        // A rename is an explicit "keep this session" signal, so materialize its
        // transcript now if it hasn't logged anything yet. Without this, a fresh
        // session renamed before its first turn would leave an index entry pointing at
        // a nonexistent <id>.jsonl, and resume would fail with "No such file". Touch the
        // file *before* writing the index so an interruption in between can only leave a
        // resumable file without a name — never a name without a file.
        ensure_log_exists(&self.log_path)?;

        let mut index = read_index(&self.index_path);
        index.insert(
            self.id.clone(),
            IndexEntry {
                name: name.clone(),
                branch,
                updated: chrono::Utc::now().to_rfc3339(),
            },
        );
        write_index(&self.index_path, &index)?;
        self.name = Some(name);
        Ok(())
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

// Read the per-project name index, returning an empty map when the file is absent
// or unparseable — a missing/corrupt index just means "no names yet", never a hard
// error (the uuid identity always works regardless).
pub fn read_index(index_path: &Path) -> Index {
    std::fs::read_to_string(index_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

// Write the index atomically: serialize to a sibling temp file, then rename over
// the target so a crash mid-write can't leave a half-written (and thus unparseable)
// index. The parent dir already exists (the session created it).
fn write_index(index_path: &Path, index: &Index) -> Result<()> {
    let tmp = index_path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(index).context("serializing session index")?;
    std::fs::write(&tmp, &body)
        .with_context(|| format!("writing session index temp {}", tmp.display()))?;
    std::fs::rename(&tmp, index_path)
        .with_context(|| format!("replacing session index {}", index_path.display()))?;
    Ok(())
}

// Create the transcript file if it's absent, without truncating an existing one
// (create + append never clobbers). Mirrors how `log` opens it.
fn ensure_log_exists(log_path: &Path) -> Result<()> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("could not create session log {}", log_path.display()))?;
    Ok(())
}

// Resolve a user-supplied reference (a uuid or a human name) to a concrete session
// id within `dir`. A reference whose transcript file already exists is taken as a
// literal id. Otherwise it's looked up by name in the index (most-recently-updated
// wins when names collide). Falling through returns the reference unchanged so the
// subsequent `open` surfaces a clear "log not found" error.
pub fn resolve_reference(dir: &Path, reference: &str) -> String {
    if dir.join(format!("{reference}.jsonl")).exists() {
        return reference.to_string();
    }
    let index = read_index(&dir.join("index.json"));
    index
        .iter()
        .filter(|(_, e)| e.name == reference)
        .max_by(|a, b| a.1.updated.cmp(&b.1.updated))
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| reference.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    // Unique temp directory per test, mirroring the transport tests' temp-path style.
    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nudge-session-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // set_name writes the index row and updates the in-memory name; a fresh
    // read_index sees the persisted entry with its branch context.
    #[test]
    fn set_name_persists_to_index() {
        let dir = temp_dir();
        let mut s = Session::create(dir.clone(), dir.clone()).unwrap();
        let id = s.id.clone();
        s.set_name("auth-fix".into(), Some("main".into())).unwrap();

        assert_eq!(s.name.as_deref(), Some("auth-fix"));
        let index = read_index(&dir.join("index.json"));
        let entry = index.get(&id).expect("index row written");
        assert_eq!(entry.name, "auth-fix");
        assert_eq!(entry.branch.as_deref(), Some("main"));

        std::fs::remove_dir_all(&dir).ok();
    }

    // A reference whose transcript exists is taken as a literal id; a name resolves
    // to its id (most-recently-updated wins on a collision); an unknown reference is
    // returned unchanged for `open` to error on.
    #[test]
    fn resolve_reference_prefers_file_then_index() {
        let dir = temp_dir();

        // An existing transcript means the reference is a literal id.
        std::fs::write(dir.join("real-id.jsonl"), "").unwrap();
        assert_eq!(resolve_reference(&dir, "real-id"), "real-id");

        // Two sessions share the name "dup"; the later `updated` wins.
        let mut index = Index::new();
        index.insert(
            "old".into(),
            IndexEntry {
                name: "dup".into(),
                branch: None,
                updated: "2026-01-01T00:00:00Z".into(),
            },
        );
        index.insert(
            "new".into(),
            IndexEntry {
                name: "dup".into(),
                branch: None,
                updated: "2026-06-01T00:00:00Z".into(),
            },
        );
        write_index(&dir.join("index.json"), &index).unwrap();
        assert_eq!(resolve_reference(&dir, "dup"), "new");

        // Unknown reference passes through unchanged.
        assert_eq!(resolve_reference(&dir, "nope"), "nope");

        std::fs::remove_dir_all(&dir).ok();
    }

    // A renamed session's label survives a create → log → open round-trip: open
    // reloads the name from the index keyed by the uuid.
    #[tokio::test]
    async fn open_reloads_persisted_name() {
        let dir = temp_dir();
        let id;
        {
            let mut s = Session::create(dir.clone(), dir.clone()).unwrap();
            id = s.id.clone();
            // A clean assistant turn so open()'s truncation keeps the transcript.
            s.log(&Message {
                role: "assistant".into(),
                content: vec![ContentBlock::Text { text: "hi".into() }],
            })
            .await
            .unwrap();
            s.set_name("my-label".into(), None).unwrap();
        }

        let resumed = Session::open(&id, dir.clone(), dir.clone()).unwrap();
        assert_eq!(resumed.session.name.as_deref(), Some("my-label"));

        std::fs::remove_dir_all(&dir).ok();
    }

    // The reported edge: a fresh session renamed before its first turn (no transcript
    // yet) must still be resumable. set_name materializes the transcript, so it
    // resolves by name and opens as an empty, named session.
    #[test]
    fn rename_before_first_turn_is_resumable() {
        let dir = temp_dir();
        let id = {
            let mut s = Session::create(dir.clone(), dir.clone()).unwrap();
            s.set_name("early-name".into(), None).unwrap();
            s.id.clone()
        };
        assert_eq!(resolve_reference(&dir, "early-name"), id);
        let resumed = Session::open(&id, dir.clone(), dir.clone()).unwrap();
        assert_eq!(resumed.session.name.as_deref(), Some("early-name"));
        assert!(resumed.messages.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    // Belt-and-suspenders: a named session whose transcript was removed still opens as
    // empty (recovery), while an unknown id with no transcript is a genuine error.
    #[test]
    fn open_recovers_indexed_session_with_missing_transcript() {
        let dir = temp_dir();
        let id = {
            let mut s = Session::create(dir.clone(), dir.clone()).unwrap();
            s.set_name("kept".into(), None).unwrap();
            s.id.clone()
        };
        std::fs::remove_file(dir.join(format!("{id}.jsonl"))).unwrap();

        let resumed = Session::open(&id, dir.clone(), dir.clone()).unwrap();
        assert!(resumed.messages.is_empty());
        assert_eq!(resumed.session.name.as_deref(), Some("kept"));

        // Unknown id, no transcript, not in index → still errors.
        assert!(Session::open("ghost", dir.clone(), dir.clone()).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
