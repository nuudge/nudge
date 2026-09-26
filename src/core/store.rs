use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

use crate::core::identity::{ClientIdentity, ClientKind};

// Bump when the schema changes; `open` refuses a database from a NEWER version
// (an older binary must not scribble on a schema it doesn't understand) and will
// host forward migrations for older ones once any exist.
const SCHEMA_VERSION: i64 = 1;

// The relational session store (issue #26): one SQLite database owning every
// session and its transcript. This is the storage MECHANISM — schema, queries,
// invariants. Where the database file lives is the caller's policy (`coding`
// points it at ~/.nudge/nudge.db), mirroring how `Session::create(cwd, dir)`
// kept path policy out of core.
//
// Invariants owned here:
// - append-only audit: message rows are never deleted; resume's strict
//   truncation marks rows `superseded` instead (excluded from reads, kept on
//   disk), fixing the JSONL latent bug where truncated junk was replayed
//   mid-transcript on a later resume.
// - byte-exact content: `content` is stored and returned as the caller's
//   serialized JSON string, never re-serialized through an intermediate value —
//   resume and the floating cache breakpoint depend on the round-trip.
// - one writer per session: ordinals are assigned in a write transaction; a
//   second writer on the same session id fails loudly on the primary key
//   instead of silently interleaving (which JSONL allowed).
pub struct SessionStore {
    conn: Connection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Running,
    Ended,
}

impl SessionState {
    fn as_str(self) -> &'static str {
        match self {
            SessionState::Running => "running",
            SessionState::Ended => "ended",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "running" => Ok(SessionState::Running),
            "ended" => Ok(SessionState::Ended),
            other => bail!("unknown session state in store: {other:?}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub cwd: String,
    pub name: Option<String>,
    pub branch: Option<String>,
    pub created: String,
    pub last_activity: String,
    pub state: SessionState,
    pub spawned_by: Option<String>,
    pub spawn_task: Option<String>,
}

// One transcript entry as stored: the wire-faithful message JSON plus the
// structured sender metadata the JSONL envelope carried as a nested object.
#[derive(Debug, Clone)]
pub struct MessageRow {
    pub ordinal: i64,
    pub timestamp: String,
    pub content: String,
    pub sender: Option<ClientIdentity>,
}

// A session row joined with its live (non-superseded) message count, for --list.
#[derive(Debug, Clone)]
pub struct SessionListing {
    pub row: SessionRow,
    pub turns: i64,
}

impl SessionStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("could not create store dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("could not open session store {}", path.display()))?;
        // WAL so readers never block writers across connections/processes; the
        // pragma returns the resulting mode, so query it rather than execute it.
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
            .context("enabling WAL on session store")?;
        Self::init(conn)
    }

    // Tests only: a private in-memory database (no WAL — it's meaningless there).
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory().context("opening in-memory store")?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", true)
            .context("enabling foreign keys")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .context("setting busy timeout")?;

        let version: Option<i64> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .unwrap_or(None) // no meta table yet: fresh database
            .and_then(|v| v.parse().ok());

        match version {
            None => {
                conn.execute_batch(SCHEMA)
                    .context("creating store schema")?;
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                    params![SCHEMA_VERSION.to_string()],
                )
                .context("recording schema version")?;
            }
            Some(v) if v == SCHEMA_VERSION => {}
            Some(v) if v > SCHEMA_VERSION => {
                bail!(
                    "session store schema is version {v}, but this nudge only supports \
                     up to {SCHEMA_VERSION} — it was written by a newer nudge; upgrade"
                );
            }
            Some(v) => bail!("no migration path from session store schema version {v}"),
        }
        Ok(Self { conn })
    }

    pub fn insert_session(
        &self,
        id: &str,
        cwd: &str,
        spawned_by: Option<&str>,
        spawn_task: Option<&str>,
    ) -> Result<()> {
        let now = now();
        self.conn
            .execute(
                "INSERT INTO sessions (id, cwd, created, last_activity, state, spawned_by, spawn_task)
                 VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    cwd,
                    now,
                    SessionState::Running.as_str(),
                    spawned_by,
                    spawn_task
                ],
            )
            .with_context(|| format!("inserting session {id}"))?;
        Ok(())
    }

    pub fn session(&self, id: &str) -> Result<Option<SessionRow>> {
        self.conn
            .query_row(
                "SELECT id, cwd, name, branch, created, last_activity, state, spawned_by, spawn_task
                 FROM sessions WHERE id = ?1",
                params![id],
                session_row,
            )
            .optional()
            .with_context(|| format!("loading session {id}"))
    }

    pub fn set_name(&self, id: &str, name: &str, branch: Option<&str>) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE sessions SET name = ?2, branch = ?3, last_activity = ?4 WHERE id = ?1",
                params![id, name, branch, now()],
            )
            .with_context(|| format!("renaming session {id}"))?;
        if n == 0 {
            bail!("cannot rename: no session {id} in the store");
        }
        Ok(())
    }

    pub fn set_state(&self, id: &str, state: SessionState) -> Result<()> {
        self.conn
            .execute(
                "UPDATE sessions SET state = ?2, last_activity = ?3 WHERE id = ?1",
                params![id, state.as_str(), now()],
            )
            .with_context(|| format!("updating state of session {id}"))?;
        Ok(())
    }

    // Append one transcript entry, assigning the next ordinal (superseded rows
    // included in the max — audit order is total). The ordinal read and the
    // insert share an immediate transaction so a concurrent writer on the same
    // session surfaces as a constraint/busy error, never a silent interleave.
    pub fn append_message(
        &mut self,
        session_id: &str,
        role: &str,
        content_json: &str,
        sender: Option<&ClientIdentity>,
    ) -> Result<i64> {
        let now = now();
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .context("starting append transaction")?;
        let ordinal: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM messages WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .context("assigning message ordinal")?;
        tx.execute(
            "INSERT INTO messages
               (session_id, ordinal, timestamp, role, content,
                sender_kind, sender_name, sender_session_id, sender_task)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                session_id,
                ordinal,
                now,
                role,
                content_json,
                sender.map(|s| kind_str(&s.kind)),
                sender.map(|s| s.name.as_str()),
                sender.and_then(|s| s.session_id.as_deref()),
                sender.and_then(|s| s.task.as_deref()),
            ],
        )
        .with_context(|| format!("appending message to session {session_id}"))?;
        tx.execute(
            "UPDATE sessions SET last_activity = ?2 WHERE id = ?1",
            params![session_id, now],
        )
        .context("touching session last_activity")?;
        tx.commit().context("committing append")?;
        Ok(ordinal)
    }

    // The live transcript: every non-superseded entry in append order.
    pub fn load_transcript(&self, session_id: &str) -> Result<Vec<MessageRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT ordinal, timestamp, content,
                        sender_kind, sender_name, sender_session_id, sender_task
                 FROM messages
                 WHERE session_id = ?1 AND superseded = 0
                 ORDER BY ordinal",
            )
            .context("preparing transcript query")?;
        let rows = stmt
            .query_map(params![session_id], message_row)
            .context("querying transcript")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .with_context(|| format!("loading transcript of session {session_id}"))?;
        Ok(rows)
    }

    // Strict-truncation bookkeeping: mark every live entry past `last_kept_ordinal`
    // superseded (pass -1 to supersede the whole transcript). The rows stay on
    // disk — truncation narrows the model-facing view, it never unlogs.
    pub fn mark_superseded_after(&self, session_id: &str, last_kept_ordinal: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE messages SET superseded = 1
                 WHERE session_id = ?1 AND superseded = 0 AND ordinal > ?2",
                params![session_id, last_kept_ordinal],
            )
            .with_context(|| format!("superseding truncated tail of session {session_id}"))?;
        Ok(())
    }

    // Resolve a user-supplied reference (uuid or human name) to a session id,
    // scoped to `cwd` for names (per-project resume semantics, as today). A
    // reference that IS a session id wins outright; otherwise the most recently
    // active session with that name in this cwd; otherwise the reference passes
    // through unchanged so the caller's open surfaces a clear not-found error.
    pub fn resolve_reference(&self, cwd: &str, reference: &str) -> Result<String> {
        if self.session(reference)?.is_some() {
            return Ok(reference.to_string());
        }
        let by_name: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM sessions WHERE cwd = ?1 AND name = ?2
                 ORDER BY last_activity DESC LIMIT 1",
                params![cwd, reference],
                |r| r.get(0),
            )
            .optional()
            .context("resolving session reference by name")?;
        Ok(by_name.unwrap_or_else(|| reference.to_string()))
    }

    // Sessions for one project directory, most recently active first, with live
    // turn counts — the `--list` query.
    pub fn list_by_cwd(&self, cwd: &str) -> Result<Vec<SessionListing>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT s.id, s.cwd, s.name, s.branch, s.created, s.last_activity,
                        s.state, s.spawned_by, s.spawn_task,
                        (SELECT COUNT(*) FROM messages m
                         WHERE m.session_id = s.id AND m.superseded = 0) AS turns
                 FROM sessions s
                 WHERE s.cwd = ?1
                 ORDER BY s.last_activity DESC",
            )
            .context("preparing list query")?;
        let rows = stmt
            .query_map(params![cwd], |r| {
                Ok(SessionListing {
                    row: session_row(r)?,
                    turns: r.get(9)?,
                })
            })
            .context("querying sessions by cwd")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("listing sessions")?;
        Ok(rows)
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn kind_str(kind: &ClientKind) -> &'static str {
    match kind {
        ClientKind::Human => "human",
        ClientKind::Agent => "agent",
    }
}

fn session_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    let state: String = r.get(6)?;
    Ok(SessionRow {
        id: r.get(0)?,
        cwd: r.get(1)?,
        name: r.get(2)?,
        branch: r.get(3)?,
        created: r.get(4)?,
        last_activity: r.get(5)?,
        state: SessionState::parse(&state).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, e.into())
        })?,
        spawned_by: r.get(7)?,
        spawn_task: r.get(8)?,
    })
}

fn message_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRow> {
    let sender_kind: Option<String> = r.get(3)?;
    let sender = match sender_kind {
        None => None,
        Some(kind) => Some(ClientIdentity {
            kind: match kind.as_str() {
                "human" => ClientKind::Human,
                "agent" => ClientKind::Agent,
                other => {
                    return Err(rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        anyhow::anyhow!("unknown sender kind in store: {other:?}").into(),
                    ));
                }
            },
            name: r.get(4)?,
            session_id: r.get(5)?,
            task: r.get(6)?,
        }),
    };
    Ok(MessageRow {
        ordinal: r.get(0)?,
        timestamp: r.get(1)?,
        content: r.get(2)?,
        sender,
    })
}

const SCHEMA: &str = "
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE sessions (
    id            TEXT PRIMARY KEY,
    cwd           TEXT NOT NULL,
    name          TEXT,
    branch        TEXT,
    created       TEXT NOT NULL,
    last_activity TEXT NOT NULL,
    state         TEXT NOT NULL DEFAULT 'running',
    spawned_by    TEXT REFERENCES sessions(id),
    spawn_task    TEXT
);
CREATE INDEX sessions_cwd ON sessions(cwd, last_activity DESC);
CREATE INDEX sessions_name ON sessions(name) WHERE name IS NOT NULL;

CREATE TABLE messages (
    session_id        TEXT NOT NULL REFERENCES sessions(id),
    ordinal           INTEGER NOT NULL,
    timestamp         TEXT NOT NULL,
    role              TEXT NOT NULL,
    content           TEXT NOT NULL,
    sender_kind       TEXT,
    sender_name       TEXT,
    sender_session_id TEXT,
    sender_task       TEXT,
    superseded        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (session_id, ordinal)
);
";

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn store() -> SessionStore {
        SessionStore::open_in_memory().unwrap()
    }

    fn temp_db() -> PathBuf {
        std::env::temp_dir().join(format!("nudge-store-{}.db", uuid::Uuid::new_v4()))
    }

    #[test]
    fn open_bootstraps_schema_and_reopen_finds_it() {
        let path = temp_db();
        {
            let s = SessionStore::open(&path).unwrap();
            s.insert_session("a", "/proj", None, None).unwrap();
        }
        let s = SessionStore::open(&path).unwrap();
        let row = s.session("a").unwrap().expect("session survives reopen");
        assert_eq!(row.cwd, "/proj");
        assert_eq!(row.state, SessionState::Running);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn open_refuses_newer_schema() {
        let path = temp_db();
        {
            let s = SessionStore::open(&path).unwrap();
            s.conn
                .execute(
                    "UPDATE meta SET value = '999' WHERE key = 'schema_version'",
                    [],
                )
                .unwrap();
        }
        let err = match SessionStore::open(&path) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("open of a newer-schema store must fail"),
        };
        assert!(err.contains("newer nudge"), "unexpected error: {err}");
        std::fs::remove_file(&path).ok();
    }

    // The load-bearing property: content comes back as the exact bytes stored,
    // with no re-serialization in between.
    #[test]
    fn message_content_round_trips_byte_exact() {
        let mut s = store();
        s.insert_session("a", "/proj", None, None).unwrap();
        // Key order and spacing chosen to differ from serde_json's canonical
        // output — byte equality proves we never re-serialize.
        let content =
            r#"{"role":"assistant",  "content":[{"type":"text","text":"héllo \u0000"}],"zz":1}"#;
        s.append_message("a", "assistant", content, None).unwrap();

        let rows = s.load_transcript("a").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content, content);
    }

    #[test]
    fn append_assigns_ordinals_and_sender_round_trips() {
        let mut s = store();
        s.insert_session("a", "/proj", None, None).unwrap();
        let who = ClientIdentity {
            kind: ClientKind::Agent,
            name: "child-abc".into(),
            session_id: Some("child-session".into()),
            task: Some("do a thing".into()),
        };
        assert_eq!(s.append_message("a", "user", "{}", Some(&who)).unwrap(), 0);
        assert_eq!(s.append_message("a", "assistant", "{}", None).unwrap(), 1);

        let rows = s.load_transcript("a").unwrap();
        let sender = rows[0].sender.as_ref().expect("sender persisted");
        assert_eq!(sender.kind, ClientKind::Agent);
        assert_eq!(sender.name, "child-abc");
        assert_eq!(sender.session_id.as_deref(), Some("child-session"));
        assert_eq!(sender.task.as_deref(), Some("do a thing"));
        assert!(rows[1].sender.is_none());
    }

    // Append-only audit: superseding hides rows from the transcript but never
    // deletes them, and later appends continue the ordinal sequence past them.
    #[test]
    fn superseded_rows_are_hidden_not_deleted_and_ordinals_continue() {
        let mut s = store();
        s.insert_session("a", "/proj", None, None).unwrap();
        for i in 0..4 {
            s.append_message("a", "user", &format!("{{\"n\":{i}}}"), None)
                .unwrap();
        }
        s.mark_superseded_after("a", 1).unwrap();

        let live = s.load_transcript("a").unwrap();
        assert_eq!(live.len(), 2);
        assert_eq!(live.last().unwrap().ordinal, 1);

        // The audit record is intact underneath.
        let total: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = 'a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 4);

        // A new append lands after the superseded tail, keeping audit order total.
        assert_eq!(s.append_message("a", "user", "{}", None).unwrap(), 4);
        let live = s.load_transcript("a").unwrap();
        assert_eq!(live.len(), 3);
        assert_eq!(live.last().unwrap().ordinal, 4);
    }

    #[test]
    fn set_name_updates_row_and_rename_of_unknown_session_errors() {
        let s = store();
        s.insert_session("a", "/proj", None, None).unwrap();
        s.set_name("a", "auth-fix", Some("main")).unwrap();
        let row = s.session("a").unwrap().unwrap();
        assert_eq!(row.name.as_deref(), Some("auth-fix"));
        assert_eq!(row.branch.as_deref(), Some("main"));

        assert!(s.set_name("ghost", "x", None).is_err());
    }

    #[test]
    fn resolve_reference_prefers_id_then_scoped_name_then_passthrough() {
        let mut s = store();
        s.insert_session("real-id", "/proj", None, None).unwrap();

        assert_eq!(s.resolve_reference("/proj", "real-id").unwrap(), "real-id");

        // Two sessions named "dup" in this cwd: the most recently active wins.
        s.insert_session("old", "/proj", None, None).unwrap();
        s.set_name("old", "dup", None).unwrap();
        s.insert_session("new", "/proj", None, None).unwrap();
        s.set_name("new", "dup", None).unwrap();
        s.append_message("new", "user", "{}", None).unwrap();
        assert_eq!(s.resolve_reference("/proj", "dup").unwrap(), "new");

        // A name in another cwd is out of scope.
        s.insert_session("elsewhere", "/other", None, None).unwrap();
        s.set_name("elsewhere", "faraway", None).unwrap();
        assert_eq!(s.resolve_reference("/proj", "faraway").unwrap(), "faraway");

        assert_eq!(s.resolve_reference("/proj", "nope").unwrap(), "nope");
    }

    #[test]
    fn list_by_cwd_scopes_orders_and_counts_live_turns() {
        let mut s = store();
        s.insert_session("a", "/proj", None, None).unwrap();
        s.insert_session("b", "/proj", None, None).unwrap();
        s.insert_session("c", "/other", None, None).unwrap();
        s.append_message("a", "user", "{}", None).unwrap();
        s.append_message("a", "assistant", "{}", None).unwrap();
        s.mark_superseded_after("a", 0).unwrap();
        s.append_message("b", "user", "{}", None).unwrap();

        let listings = s.list_by_cwd("/proj").unwrap();
        assert_eq!(listings.len(), 2);
        // b appended last, so it's most recent.
        assert_eq!(listings[0].row.id, "b");
        assert_eq!(listings[0].turns, 1);
        // a's superseded row is excluded from its live count.
        assert_eq!(listings[1].row.id, "a");
        assert_eq!(listings[1].turns, 1);
    }

    #[test]
    fn spawned_by_edge_and_state_transitions_persist() {
        let s = store();
        s.insert_session("parent", "/proj", None, None).unwrap();
        s.insert_session("child", "/proj", Some("parent"), Some("fix the bug"))
            .unwrap();

        let row = s.session("child").unwrap().unwrap();
        assert_eq!(row.spawned_by.as_deref(), Some("parent"));
        assert_eq!(row.spawn_task.as_deref(), Some("fix the bug"));
        assert_eq!(row.state, SessionState::Running);

        s.set_state("child", SessionState::Ended).unwrap();
        let row = s.session("child").unwrap().unwrap();
        assert_eq!(row.state, SessionState::Ended);

        // The FK is enforced: a dangling spawned_by is a bug, not data.
        assert!(
            s.insert_session("orphan", "/proj", Some("no-such-parent"), None)
                .is_err()
        );
    }
}
