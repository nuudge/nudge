use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::core::store::{ImportedMessage, SessionRow, SessionState, SessionStore};
use crate::llm::Message;

// Read-only on the JSONL side: the legacy files stay as the archive. Bad lines
// and files are counted and skipped; only a store write error aborts, leaving
// the marker unset so the next open retries (per-session atomic, dedup by id).
pub(super) fn run(store: &mut SessionStore) -> Result<()> {
    let home = std::env::var("HOME").context("HOME env var not set")?;
    let root = PathBuf::from(home).join(".nudge").join("projects");
    let stats = import_tree(&root, store)?;
    if stats.sessions > 0 || stats.skipped_lines > 0 || stats.skipped_files > 0 {
        eprintln!(
            "imported {} legacy session(s) ({} messages) into the session store; \
             skipped {} line(s), {} file(s)",
            stats.sessions, stats.messages, stats.skipped_lines, stats.skipped_files
        );
    }
    Ok(())
}

#[derive(Default)]
struct ImportStats {
    sessions: usize,
    messages: usize,
    skipped_lines: usize,
    skipped_files: usize,
}

fn import_tree(root: &Path, store: &mut SessionStore) -> Result<ImportStats> {
    let mut stats = ImportStats::default();
    let Ok(dirs) = std::fs::read_dir(root) else {
        return Ok(stats);
    };
    for entry in dirs.flatten() {
        if entry.path().is_dir() {
            import_dir(&entry.path(), store, &mut stats)?;
        }
    }
    Ok(stats)
}

fn import_dir(dir: &Path, store: &mut SessionStore, stats: &mut ImportStats) -> Result<()> {
    let index = read_index(&dir.join("index.json"));
    let Ok(entries) = std::fs::read_dir(dir) else {
        stats.skipped_files += 1;
        return Ok(());
    };

    // The dir name is a lossy flattening of cwd, so cwd comes from envelopes;
    // index-only sessions borrow a sibling's, since a legacy dir shared one cwd.
    let mut dir_cwd: Option<String> = None;
    let mut seen: Vec<String> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            stats.skipped_files += 1;
            continue;
        };
        seen.push(id.to_string());

        let mut cwd: Option<String> = None;
        let mut messages: Vec<ImportedMessage> = Vec::new();
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match parse_line(line) {
                Some((message, line_cwd)) => {
                    cwd = cwd.or(line_cwd);
                    messages.push(message);
                }
                None => stats.skipped_lines += 1,
            }
        }
        let cwd = cwd.or_else(|| dir_cwd.clone()).unwrap_or_default();
        if dir_cwd.is_none() && !cwd.is_empty() {
            dir_cwd = Some(cwd.clone());
        }

        let meta = index.get(id);
        let updated = meta.map(|e| e.updated.clone()).filter(|u| !u.is_empty());
        let created = messages
            .first()
            .map(|m| m.timestamp.clone())
            .or_else(|| updated.clone())
            .unwrap_or_default();
        let last_activity = updated
            .or_else(|| messages.last().map(|m| m.timestamp.clone()))
            .unwrap_or_else(|| created.clone());
        let row = SessionRow {
            id: id.to_string(),
            cwd,
            name: meta.map(|e| e.name.clone()),
            branch: meta.and_then(|e| e.branch.clone()),
            created,
            last_activity,
            state: SessionState::Ended,
            spawned_by: None,
            spawn_task: None,
        };
        if store.import_session(&row, &messages)? {
            stats.sessions += 1;
            stats.messages += messages.len();
        }
    }

    // Index entries without a transcript import as empty named sessions so
    // resume by name keeps working.
    for (id, entry) in &index {
        if seen.iter().any(|s| s == id) {
            continue;
        }
        let row = SessionRow {
            id: id.clone(),
            cwd: dir_cwd.clone().unwrap_or_default(),
            name: Some(entry.name.clone()),
            branch: entry.branch.clone(),
            created: entry.updated.clone(),
            last_activity: entry.updated.clone(),
            state: SessionState::Ended,
            spawned_by: None,
            spawn_task: None,
        };
        if store.import_session(&row, &[])? {
            stats.sessions += 1;
        }
    }
    Ok(())
}

// `sender` is lenient (absent or malformed reads as None) so old lines import
// unattributed; `message` must parse.
fn parse_line(line: &str) -> Option<(ImportedMessage, Option<String>)> {
    let envelope: Value = serde_json::from_str(line).ok()?;
    let message: Message = serde_json::from_value(envelope.get("message")?.clone()).ok()?;
    let sender = envelope
        .get("sender")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let timestamp = envelope
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let cwd = envelope
        .get("cwd")
        .and_then(Value::as_str)
        .map(String::from);
    let content = serde_json::to_string(&message).ok()?;
    Some((
        ImportedMessage {
            timestamp,
            role: message.role,
            content,
            sender,
        },
        cwd,
    ))
}

#[derive(serde::Deserialize)]
struct IndexEntry {
    name: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    updated: String,
}

fn read_index(index_path: &Path) -> std::collections::BTreeMap<String, IndexEntry> {
    std::fs::read_to_string(index_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::identity::ClientKind;
    use crate::core::session::Session;
    use crate::llm::ContentBlock;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nudge-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn temp_db() -> PathBuf {
        std::env::temp_dir().join(format!("nudge-import-{}.db", uuid::Uuid::new_v4()))
    }

    fn envelope(role: &str, blocks: Value, ts: &str, sender: Option<Value>) -> String {
        let mut e = serde_json::json!({
            "timestamp": ts,
            "sessionId": "s",
            "cwd": "/proj/alpha",
            "message": {"role": role, "content": blocks},
        });
        if let Some(s) = sender {
            e["sender"] = s;
        }
        e.to_string()
    }

    fn text(t: &str) -> Value {
        serde_json::json!([{"type": "text", "text": t}])
    }

    #[test]
    fn import_recovers_messages_senders_names_and_cwd() {
        let root = temp_root();
        let dir = root.join("proj-alpha");
        std::fs::create_dir_all(&dir).unwrap();
        let lines = [
            envelope(
                "user",
                text("clean text"),
                "2026-01-01T00:00:00+00:00",
                Some(serde_json::json!({
                    "kind": "Agent", "name": "child-1",
                    "session_id": null, "task": null
                })),
            ),
            envelope("assistant", text("ok"), "2026-01-01T00:01:00+00:00", None),
            envelope(
                "user",
                text("again"),
                "2026-01-01T00:02:00+00:00",
                Some(serde_json::json!(42)),
            ),
        ];
        std::fs::write(dir.join("aaaa.jsonl"), lines.join("\n")).unwrap();
        std::fs::write(
            dir.join("index.json"),
            r#"{"aaaa":{"name":"auth-fix","branch":"main","updated":"2026-02-02T00:00:00+00:00"}}"#,
        )
        .unwrap();

        let db = temp_db();
        let mut store = SessionStore::open(&db).unwrap();
        let stats = import_tree(&root, &mut store).unwrap();
        assert_eq!(stats.sessions, 1);
        assert_eq!(stats.messages, 3);
        assert_eq!(stats.skipped_lines, 0);
        assert_eq!(stats.skipped_files, 0);

        let row = store.session("aaaa").unwrap().expect("session imported");
        assert_eq!(row.cwd, "/proj/alpha");
        assert_eq!(row.name.as_deref(), Some("auth-fix"));
        assert_eq!(row.branch.as_deref(), Some("main"));
        assert_eq!(row.state, SessionState::Ended);
        assert_eq!(row.created, "2026-01-01T00:00:00+00:00");
        assert_eq!(row.last_activity, "2026-02-02T00:00:00+00:00");

        let rows = store.load_transcript("aaaa").unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].timestamp, "2026-01-01T00:00:00+00:00");
        let sender = rows[0].sender.as_ref().expect("sender round-trips");
        assert_eq!(sender.kind, ClientKind::Agent);
        assert_eq!(sender.name, "child-1");
        assert!(rows[1].sender.is_none(), "pre-sender line is unattributed");
        assert!(rows[2].sender.is_none(), "malformed sender reads as None");
        let expected = serde_json::to_string(&Message {
            role: "user".into(),
            content: vec![ContentBlock::Text {
                text: "clean text".into(),
            }],
        })
        .unwrap();
        assert_eq!(rows[0].content, expected);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn malformed_line_skips_the_line_not_the_session() {
        let root = temp_root();
        let dir = root.join("p");
        std::fs::create_dir_all(&dir).unwrap();
        let lines = [
            envelope("user", text("hi"), "t1", None),
            "not json at all".to_string(),
            r#"{"timestamp":"t2","cwd":"/proj/alpha","no_message_here":true}"#.to_string(),
            envelope("assistant", text("ok"), "t3", None),
        ];
        std::fs::write(dir.join("aaaa.jsonl"), lines.join("\n")).unwrap();

        let db = temp_db();
        let mut store = SessionStore::open(&db).unwrap();
        let stats = import_tree(&root, &mut store).unwrap();
        assert_eq!(stats.sessions, 1);
        assert_eq!(stats.messages, 2);
        assert_eq!(stats.skipped_lines, 2);
        assert_eq!(store.load_transcript("aaaa").unwrap().len(), 2);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn unreadable_file_skips_that_session_only() {
        let root = temp_root();
        let dir = root.join("p");
        std::fs::create_dir_all(&dir).unwrap();
        // A directory named like a transcript makes read_to_string fail.
        std::fs::create_dir_all(dir.join("bad.jsonl")).unwrap();
        std::fs::write(
            dir.join("good.jsonl"),
            envelope("user", text("hi"), "t", None),
        )
        .unwrap();

        let db = temp_db();
        let mut store = SessionStore::open(&db).unwrap();
        let stats = import_tree(&root, &mut store).unwrap();
        assert_eq!(stats.skipped_files, 1);
        assert_eq!(stats.sessions, 1);
        assert!(store.session("good").unwrap().is_some());
        assert!(store.session("bad").unwrap().is_none());

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn index_only_session_imports_empty_named_with_sibling_cwd() {
        let root = temp_root();
        let dir = root.join("p");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("aaaa.jsonl"),
            envelope("assistant", text("hi"), "t", None),
        )
        .unwrap();
        std::fs::write(
            dir.join("index.json"),
            r#"{"bbbb":{"name":"early-name","updated":"2026-03-03T00:00:00+00:00"}}"#,
        )
        .unwrap();

        let db = temp_db();
        let mut store = SessionStore::open(&db).unwrap();
        let stats = import_tree(&root, &mut store).unwrap();
        assert_eq!(stats.sessions, 2);

        let row = store.session("bbbb").unwrap().expect("empty session row");
        assert_eq!(row.name.as_deref(), Some("early-name"));
        assert_eq!(row.cwd, "/proj/alpha", "borrows the sibling's cwd");
        assert_eq!(row.last_activity, "2026-03-03T00:00:00+00:00");
        assert!(store.load_transcript("bbbb").unwrap().is_empty());
        assert_eq!(
            store
                .resolve_reference("/proj/alpha", "early-name")
                .unwrap(),
            "bbbb"
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn import_is_idempotent_across_retries() {
        let root = temp_root();
        let dir = root.join("p");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("aaaa.jsonl"),
            envelope("user", text("hi"), "t", None),
        )
        .unwrap();

        let db = temp_db();
        let mut store = SessionStore::open(&db).unwrap();
        assert_eq!(import_tree(&root, &mut store).unwrap().sessions, 1);
        let again = import_tree(&root, &mut store).unwrap();
        assert_eq!(again.sessions, 0, "existing ids are skipped");
        assert_eq!(store.load_transcript("aaaa").unwrap().len(), 1);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_file(&db).ok();
    }

    #[test]
    fn missing_projects_root_imports_nothing_successfully() {
        let db = temp_db();
        let mut store = SessionStore::open(&db).unwrap();
        let stats = import_tree(Path::new("/nonexistent/projects"), &mut store).unwrap();
        assert_eq!(stats.sessions, 0);
        std::fs::remove_file(&db).ok();
    }

    // Import lands all rows; the first resume supersedes (not deletes) the
    // orphaned tail: an unanswered prompt and a dangling tool_use.
    #[test]
    fn resume_of_imported_session_truncates_and_supersedes_the_orphaned_tail() {
        let root = temp_root();
        let dir = root.join("p");
        std::fs::create_dir_all(&dir).unwrap();
        let tool_use = serde_json::json!([
            {"type": "text", "text": "let me look"},
            {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}}
        ]);
        let lines = [
            envelope("user", text("hi"), "t1", None),
            envelope("assistant", text("done"), "t2", None),
            envelope("user", text("no reply ever came"), "t3", None),
            envelope("assistant", tool_use, "t4", None),
        ];
        std::fs::write(dir.join("aaaa.jsonl"), lines.join("\n")).unwrap();

        let db = temp_db();
        let mut store = SessionStore::open(&db).unwrap();
        assert_eq!(import_tree(&root, &mut store).unwrap().messages, 4);

        let resumed = Session::open("aaaa", PathBuf::from("/proj/alpha"), store).unwrap();
        assert_eq!(resumed.dropped, 2);
        assert_eq!(resumed.entries.len(), 2);

        let conn = rusqlite::Connection::open(&db).unwrap();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(
            count("SELECT COUNT(*) FROM messages WHERE session_id='aaaa'"),
            4
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM messages WHERE session_id='aaaa' AND superseded=1"),
            2
        );
        assert_eq!(
            count("SELECT MIN(ordinal) FROM messages WHERE session_id='aaaa' AND superseded=1"),
            2
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_file(&db).ok();
    }
}
