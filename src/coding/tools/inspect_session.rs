use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::core::store::{InspectRow, SessionListing, SessionStore};
use crate::llm::{ContentBlock, Message};

const DEFAULT_LAST_N: i64 = 10;
const MAX_LAST_N: i64 = 50;
const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 100;
const TASK_CHARS: usize = 200;
const TEXT_CHARS: usize = 300;
const TOOL_CHARS: usize = 150;
const RESULT_CHARS: usize = 200;
const SNIPPET_CHARS: usize = 150;

#[derive(Deserialize)]
struct Input {
    mode: String,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    last_n: Option<i64>,
    #[serde(default)]
    around_ordinal: Option<i64>,
    #[serde(default)]
    include_superseded: bool,
    #[serde(default)]
    all_projects: bool,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    query: Option<String>,
}

pub fn schema() -> Value {
    json!({
        "name": "InspectSession",
        "description": "Read past agent sessions from the session store (read-only). Three modes:\n- `inspect`: one session's metadata (state, spawn provenance, timestamps) and a compact transcript view — the last N entries by default, or a window centered on `around_ordinal` (e.g. an ordinal from a search hit). `include_superseded` adds the audit view: entries dropped by resume truncation, flagged.\n- `list`: sessions for the current project (or `all_projects`) — id, name, state, turns, last activity, spawn edges.\n- `search`: naive substring search over stored message content; each hit carries session, ordinal, timestamp, and a capped snippet. Scope to one session with `session`.\nlist/search disclose truncation with a leading \"showing N of M\" line when the limit cut results.\n\nWhen to use: answering \"what did session X actually do?\" for an ENDED, dismissed, or crashed session; finding an old session; recovering context from history. For a LIVE peer you hold a connection to, prefer MessagePeer — the peer compresses with its own reasoning, while a raw transcript is the expensive channel. State honesty: 'running' can also mean the process crashed (nothing marks a crash) — judge liveness by last_activity.",
        "input_schema": {
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["inspect", "list", "search"],
                    "description": "What to do: inspect one session, list sessions, or search message content."
                },
                "session": {
                    "type": "string",
                    "description": "inspect mode (required) or search mode (optional): session id, or a session name (resolved within the current project). In search mode, scopes the search to that one session."
                },
                "last_n": {
                    "type": "integer",
                    "description": "inspect mode: how many transcript entries to show — the trailing N, or the window size around `around_ordinal`. Default 10, max 50."
                },
                "around_ordinal": {
                    "type": "integer",
                    "description": "inspect mode: center the transcript view on this ordinal (as reported by search hits and entry headers) instead of the tail. Window size is `last_n`."
                },
                "include_superseded": {
                    "type": "boolean",
                    "description": "inspect mode: include entries dropped by resume truncation (the audit view), flagged per entry. Default false."
                },
                "all_projects": {
                    "type": "boolean",
                    "description": "list/search modes: cover every project instead of the current directory. Default false."
                },
                "limit": {
                    "type": "integer",
                    "description": "list/search modes: max rows/hits. Default 20, max 100."
                },
                "query": {
                    "type": "string",
                    "description": "search mode: literal substring to find in message content (no regex)."
                }
            },
            "required": ["mode"]
        }
    })
}

pub fn summarize(input: &Value) -> String {
    match input["mode"].as_str().unwrap_or("?") {
        "inspect" => format!("inspect {}", input["session"].as_str().unwrap_or("?")),
        "list" => {
            if input["all_projects"].as_bool().unwrap_or(false) {
                "list sessions (all projects)".into()
            } else {
                "list sessions".into()
            }
        }
        "search" => {
            let query = input["query"].as_str().unwrap_or("?");
            match input["session"].as_str() {
                Some(session) => format!("search /{query}/ in {session}"),
                None => format!("search /{query}/"),
            }
        }
        other => format!("{other}?"),
    }
}

pub async fn execute(input: &Value) -> Result<String> {
    let store = crate::coding::open_store()?;
    let cwd = std::env::current_dir()
        .context("could not determine cwd")?
        .display()
        .to_string();
    run(input, &store, &cwd)
}

// Split from `execute` so tests inject a scratch store and cwd. Read-only: only
// query methods of the store are called here.
pub(crate) fn run(input: &Value, store: &SessionStore, cwd: &str) -> Result<String> {
    let input: Input =
        serde_json::from_value(input.clone()).context("InspectSession: invalid input shape")?;
    match input.mode.as_str() {
        "inspect" => {
            let Some(reference) = &input.session else {
                bail!("InspectSession: inspect mode requires a `session` (id or name)");
            };
            let last_n = input.last_n.unwrap_or(DEFAULT_LAST_N).clamp(1, MAX_LAST_N);
            inspect(
                store,
                cwd,
                reference,
                last_n,
                input.around_ordinal,
                input.include_superseded,
            )
        }
        "list" => {
            let limit = input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
            list(store, cwd, input.all_projects, limit)
        }
        "search" => {
            let Some(query) = &input.query else {
                bail!("InspectSession: search mode requires a `query` string");
            };
            let limit = input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
            search(
                store,
                cwd,
                query,
                input.session.as_deref(),
                input.all_projects,
                limit,
            )
        }
        other => bail!("InspectSession: unknown mode {other:?}; expected inspect | list | search"),
    }
}

fn inspect(
    store: &SessionStore,
    cwd: &str,
    reference: &str,
    last_n: i64,
    around_ordinal: Option<i64>,
    include_superseded: bool,
) -> Result<String> {
    let id = store.resolve_reference(cwd, reference)?;
    let Some(row) = store.session(&id)? else {
        bail!("no session {reference:?} in the store (by id or name in this project)");
    };
    let turns = store.turn_count(&id)?;

    let mut out = String::new();
    out.push_str(&format!(
        "session {} ({})\n",
        row.id,
        row.name.as_deref().unwrap_or("unnamed")
    ));
    out.push_str(&format!(
        "state: {} | created: {} | last activity: {}\n",
        row.state.as_str(),
        row.created,
        row.last_activity
    ));
    out.push_str(&format!("cwd: {}\n", row.cwd));
    if let Some(parent) = &row.spawned_by {
        out.push_str(&format!(
            "spawned by: {parent} — task: {}\n",
            truncate(row.spawn_task.as_deref().unwrap_or("?"), TASK_CHARS)
        ));
    }
    out.push_str(&format!("live turns: {turns}\n"));

    let entries = match around_ordinal {
        Some(center) => store.transcript_around(&id, center, last_n, include_superseded)?,
        None => store.transcript_tail(&id, last_n, include_superseded)?,
    };
    if entries.is_empty() {
        match around_ordinal {
            Some(center) => out.push_str(&format!("\n(no entries around ordinal {center})\n")),
            None => out.push_str("\n(no transcript entries)\n"),
        }
        return Ok(out);
    }
    match around_ordinal {
        Some(center) => out.push_str(&format!("\nentries around ordinal {center}:\n")),
        None => out.push_str(&format!(
            "\nlast {} entr{}:\n",
            entries.len(),
            plural_ies(entries.len())
        )),
    }
    for entry in &entries {
        out.push_str(&render_entry(entry));
    }
    Ok(out)
}

fn render_entry(entry: &InspectRow) -> String {
    let mut head = format!("[{}] {}", entry.row.ordinal, entry.role);
    if let Some(who) = &entry.row.sender {
        head.push_str(&format!(" (from {})", who.name));
    }
    if entry.superseded {
        head.push_str(" [superseded]");
    }
    head.push_str(&format!("  {}", entry.row.timestamp));
    let mut out = format!("{head}\n");
    match serde_json::from_str::<Message>(&entry.row.content) {
        Ok(msg) => {
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        out.push_str(&format!("  {}\n", truncate(&one_line(text), TEXT_CHARS)));
                    }
                    ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                        out.push_str(&format!(
                            "  (thinking) {}\n",
                            truncate(&one_line(thinking), TOOL_CHARS)
                        ));
                    }
                    ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {
                        out.push_str("  (thinking)\n");
                    }
                    ContentBlock::ToolUse { name, input, .. } => {
                        out.push_str(&format!(
                            "  tool_use {name}: {}\n",
                            truncate(&one_line(&input.to_string()), TOOL_CHARS)
                        ));
                    }
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => {
                        let tag = if *is_error { " (error)" } else { "" };
                        out.push_str(&format!(
                            "  tool_result{tag}: {}\n",
                            truncate(&one_line(content), RESULT_CHARS)
                        ));
                    }
                }
            }
        }
        // A row that doesn't parse as a typed message still shows, raw.
        Err(_) => out.push_str(&format!(
            "  (unparsed) {}\n",
            truncate(&one_line(&entry.row.content), TEXT_CHARS)
        )),
    }
    out
}

fn list(store: &SessionStore, cwd: &str, all_projects: bool, limit: i64) -> Result<String> {
    let (listings, total) = if all_projects {
        (store.list_all(limit)?, store.session_count()?)
    } else {
        let mut rows = store.list_by_cwd(cwd)?;
        let total = rows.len() as i64;
        rows.truncate(limit as usize);
        (rows, total)
    };
    if listings.is_empty() {
        return Ok(if all_projects {
            "(no sessions in the store)".into()
        } else {
            "(no sessions for this project)".into()
        });
    }
    let mut out = String::new();
    if total > listings.len() as i64 {
        out.push_str(&format!(
            "showing {} of {} sessions\n",
            listings.len(),
            total
        ));
    }
    for l in &listings {
        out.push_str(&render_listing(l, all_projects));
    }
    Ok(out)
}

fn render_listing(l: &SessionListing, with_cwd: bool) -> String {
    let mut line = format!(
        "{}  {}  {}  {} turns  last {}",
        l.row.id,
        l.row.name.as_deref().unwrap_or("-"),
        l.row.state.as_str(),
        l.turns,
        l.row.last_activity
    );
    if let Some(parent) = &l.row.spawned_by {
        line.push_str(&format!("  [child of {}]", short_id(parent)));
    }
    if with_cwd {
        line.push_str(&format!("  ({})", l.row.cwd));
    }
    line.push('\n');
    line
}

fn search(
    store: &SessionStore,
    cwd: &str,
    query: &str,
    session: Option<&str>,
    all_projects: bool,
    limit: i64,
) -> Result<String> {
    let session_id = match session {
        Some(reference) => {
            let id = store.resolve_reference(cwd, reference)?;
            if store.session(&id)?.is_none() {
                bail!("no session {reference:?} in the store (by id or name in this project)");
            }
            Some(id)
        }
        None => None,
    };
    // A session filter fully pins the scope; the cwd filter applies otherwise.
    let scope = if all_projects || session_id.is_some() {
        None
    } else {
        Some(cwd)
    };
    let hits = store.search_messages(query, scope, session_id.as_deref(), limit)?;
    if hits.is_empty() {
        return Ok("(no matches)".into());
    }
    let mut out = String::new();
    let total = store.search_message_count(query, scope, session_id.as_deref())?;
    if total > hits.len() as i64 {
        out.push_str(&format!("showing {} of {} hits\n", hits.len(), total));
    }
    for h in &hits {
        let who = h.sender_name.as_deref().unwrap_or(&h.role);
        let scope_note = if all_projects {
            format!("  ({})", h.cwd)
        } else {
            String::new()
        };
        out.push_str(&format!(
            "{} ({}) [{}] {}  {}: {}{}\n",
            h.session_id,
            h.session_name.as_deref().unwrap_or("unnamed"),
            h.ordinal,
            h.timestamp,
            who,
            snippet(&h.content, query),
            scope_note
        ));
    }
    Ok(out)
}

// A one-line window of the stored content centered on the (case-insensitive)
// match when it's findable, else the content's prefix. Operates on the
// unescaped text: stored content is message JSON, so tool_use/tool_result
// hits would otherwise render raw \n and \" escapes.
fn snippet(content: &str, query: &str) -> String {
    let flat = one_line(&unescape_json(content));
    let lowered = flat.to_lowercase();
    match lowered.find(&query.to_lowercase()) {
        Some(byte_pos) => {
            // Count the prefix on `lowered`, where byte_pos is guaranteed a char
            // boundary — lowercasing can change byte lengths (İ → i̇), so the
            // offset must never index `flat` directly. The count may drift a
            // char or two on such input; the window is fuzzy anyway.
            let chars_before = lowered[..byte_pos].chars().count();
            let start = chars_before.saturating_sub(SNIPPET_CHARS / 2);
            let windowed: String = flat.chars().skip(start).take(SNIPPET_CHARS).collect();
            let prefix = if start > 0 { "…" } else { "" };
            let suffix = if flat.chars().count() > start + SNIPPET_CHARS {
                "…"
            } else {
                ""
            };
            format!("{prefix}{windowed}{suffix}")
        }
        None => truncate(&flat, SNIPPET_CHARS),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// Undo JSON string escapes for display. Deliberately shallow — the input is a
// serialized message, not a lone JSON string, so full parsing doesn't apply;
// unknown escapes pass through unchanged.
fn unescape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn plural_ies(n: usize) -> &'static str {
    if n == 1 { "y" } else { "ies" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::identity::{ClientIdentity, ClientKind};

    fn store() -> SessionStore {
        SessionStore::open_in_memory().unwrap()
    }

    fn text_content(role: &str, text: &str) -> String {
        serde_json::to_string(&Message {
            role: role.into(),
            content: vec![ContentBlock::Text { text: text.into() }],
        })
        .unwrap()
    }

    fn who(name: &str) -> ClientIdentity {
        ClientIdentity {
            kind: ClientKind::Agent,
            name: name.into(),
            session_id: None,
            task: None,
        }
    }

    #[test]
    fn inspect_renders_metadata_sender_and_truncations() {
        let mut s = store();
        s.insert_session("parent", "/proj", None, None).unwrap();
        let long_task = "t".repeat(500);
        s.insert_session("child", "/proj", Some("parent"), Some(&long_task))
            .unwrap();
        s.set_name("child", "worker", Some("main")).unwrap();
        let long_text = "x".repeat(400);
        s.append_message(
            "child",
            "user",
            &text_content("user", &long_text),
            Some(&who("parent-1")),
        )
        .unwrap();
        s.append_message(
            "child",
            "assistant",
            &text_content("assistant", "done"),
            None,
        )
        .unwrap();

        let out = run(&json!({"mode": "inspect", "session": "child"}), &s, "/proj").unwrap();

        assert!(out.contains("session child (worker)"), "{out}");
        assert!(out.contains("state: running"), "{out}");
        assert!(out.contains("spawned by: parent"), "{out}");
        assert!(out.contains("live turns: 2"), "{out}");
        assert!(out.contains("(from parent-1)"), "{out}");
        // Truncation: the 500-char task renders capped at 200, the 400-char text at 300.
        assert!(
            out.contains(&format!("{}…", "t".repeat(TASK_CHARS))),
            "{out}"
        );
        assert!(!out.contains(&"t".repeat(TASK_CHARS + 1)), "{out}");
        assert!(
            out.contains(&format!("{}…", "x".repeat(TEXT_CHARS))),
            "{out}"
        );
        assert!(!out.contains(&"x".repeat(TEXT_CHARS + 1)), "{out}");
    }

    #[test]
    fn inspect_resolves_names_and_unknown_reference_errors() {
        let mut s = store();
        s.insert_session("abc", "/proj", None, None).unwrap();
        s.set_name("abc", "my-work", None).unwrap();
        s.append_message("abc", "user", &text_content("user", "hi"), None)
            .unwrap();

        let out = run(
            &json!({"mode": "inspect", "session": "my-work"}),
            &s,
            "/proj",
        )
        .unwrap();
        assert!(out.contains("session abc (my-work)"), "{out}");

        // A name from another cwd is out of scope; an unknown ref errors cleanly.
        let err = run(
            &json!({"mode": "inspect", "session": "my-work"}),
            &s,
            "/other",
        )
        .unwrap_err();
        assert!(err.to_string().contains("no session"), "{err}");
    }

    #[test]
    fn inspect_last_n_bounds_the_tail() {
        let mut s = store();
        s.insert_session("a", "/proj", None, None).unwrap();
        for i in 0..5 {
            s.append_message("a", "user", &text_content("user", &format!("msg{i}")), None)
                .unwrap();
        }
        let out = run(
            &json!({"mode": "inspect", "session": "a", "last_n": 2}),
            &s,
            "/proj",
        )
        .unwrap();
        assert!(!out.contains("msg2"), "{out}");
        assert!(out.contains("msg3") && out.contains("msg4"), "{out}");
        assert!(out.contains("live turns: 5"), "{out}");
    }

    #[test]
    fn include_superseded_shows_the_flagged_audit_tail() {
        let mut s = store();
        s.insert_session("a", "/proj", None, None).unwrap();
        s.append_message("a", "user", &text_content("user", "kept-user"), None)
            .unwrap();
        s.append_message(
            "a",
            "assistant",
            &text_content("assistant", "kept-reply"),
            None,
        )
        .unwrap();
        s.append_message("a", "user", &text_content("user", "orphaned"), None)
            .unwrap();
        s.mark_superseded_after("a", 1).unwrap();

        let live = run(&json!({"mode": "inspect", "session": "a"}), &s, "/proj").unwrap();
        assert!(!live.contains("orphaned"), "{live}");
        assert!(!live.contains("[superseded]"), "{live}");

        let audit = run(
            &json!({"mode": "inspect", "session": "a", "include_superseded": true}),
            &s,
            "/proj",
        )
        .unwrap();
        assert!(audit.contains("orphaned"), "{audit}");
        assert!(audit.contains("[2] user [superseded]"), "{audit}");
        assert!(!audit.contains("[1] assistant [superseded]"), "{audit}");
    }

    #[test]
    fn list_scopes_to_cwd_unless_all_projects() {
        let mut s = store();
        s.insert_session("here", "/proj", None, None).unwrap();
        s.insert_session("kid", "/proj", Some("here"), Some("subtask"))
            .unwrap();
        s.insert_session("elsewhere", "/other", None, None).unwrap();
        s.append_message("here", "user", &text_content("user", "hi"), None)
            .unwrap();

        let scoped = run(&json!({"mode": "list"}), &s, "/proj").unwrap();
        assert!(
            scoped.contains("here") && scoped.contains("kid"),
            "{scoped}"
        );
        assert!(!scoped.contains("elsewhere"), "{scoped}");
        assert!(scoped.contains("[child of here]"), "{scoped}");
        assert!(scoped.contains("1 turns"), "{scoped}");

        let all = run(&json!({"mode": "list", "all_projects": true}), &s, "/proj").unwrap();
        assert!(
            all.contains("elsewhere") && all.contains("(/other)"),
            "{all}"
        );

        let capped = run(&json!({"mode": "list", "limit": 1}), &s, "/proj").unwrap();
        assert_eq!(capped.lines().count(), 2, "{capped}");
        assert!(capped.starts_with("showing 1 of 2 sessions\n"), "{capped}");
        // An uncut list carries no disclosure line.
        assert!(!scoped.contains("showing"), "{scoped}");
    }

    #[test]
    fn search_matches_literally_and_escapes_like_wildcards() {
        let mut s = store();
        s.insert_session("a", "/proj", None, None).unwrap();
        s.set_name("a", "hay", None).unwrap();
        s.append_message(
            "a",
            "user",
            &text_content("user", "progress: 100% done"),
            None,
        )
        .unwrap();
        s.append_message(
            "a",
            "user",
            &text_content("user", "progress: 100x done"),
            None,
        )
        .unwrap();
        s.insert_session("b", "/other", None, None).unwrap();
        s.append_message("b", "user", &text_content("user", "100% elsewhere"), None)
            .unwrap();

        // `%` matches only itself — the `100x` row must not hit.
        let out = run(&json!({"mode": "search", "query": "100%"}), &s, "/proj").unwrap();
        assert!(out.contains("100% done"), "{out}");
        assert!(!out.contains("100x"), "{out}");
        assert!(!out.contains("elsewhere"), "scoped to cwd: {out}");
        assert!(out.contains("a (hay) [0]"), "{out}");

        let all = run(
            &json!({"mode": "search", "query": "100%", "all_projects": true}),
            &s,
            "/proj",
        )
        .unwrap();
        assert!(
            all.contains("elsewhere") && all.contains("(/other)"),
            "{all}"
        );

        // `_` is likewise literal: "a_b" must not match "axb".
        s.append_message("a", "user", &text_content("user", "axb"), None)
            .unwrap();
        let out = run(&json!({"mode": "search", "query": "a_b"}), &s, "/proj").unwrap();
        assert!(out.contains("(no matches)"), "{out}");
    }

    #[test]
    fn search_snippet_centers_on_the_match_in_long_content() {
        let mut s = store();
        s.insert_session("a", "/proj", None, None).unwrap();
        let long = format!("{} NEEDLE {}", "before ".repeat(100), "after ".repeat(100));
        s.append_message("a", "user", &text_content("user", &long), None)
            .unwrap();

        let out = run(&json!({"mode": "search", "query": "NEEDLE"}), &s, "/proj").unwrap();
        assert!(out.contains("NEEDLE"), "{out}");
        assert!(out.contains('…'), "windowed, not the whole row: {out}");
        // The window is bounded — far-away prefix content is not included.
        assert!(out.lines().next().unwrap().len() < 400, "{out}");
    }

    #[test]
    fn unknown_mode_and_missing_args_error_cleanly() {
        let s = store();
        assert!(run(&json!({"mode": "nope"}), &s, "/proj").is_err());
        assert!(run(&json!({"mode": "inspect"}), &s, "/proj").is_err());
        assert!(run(&json!({"mode": "search"}), &s, "/proj").is_err());
    }

    #[test]
    fn search_scopes_to_one_session_by_name_and_rejects_unknown() {
        let mut s = store();
        s.insert_session("a", "/proj", None, None).unwrap();
        s.set_name("a", "alpha", None).unwrap();
        s.insert_session("b", "/proj", None, None).unwrap();
        s.append_message("a", "user", &text_content("user", "NEEDLE in a"), None)
            .unwrap();
        s.append_message("b", "user", &text_content("user", "NEEDLE in b"), None)
            .unwrap();

        let scoped = run(
            &json!({"mode": "search", "query": "NEEDLE", "session": "alpha"}),
            &s,
            "/proj",
        )
        .unwrap();
        assert!(scoped.contains("NEEDLE in a"), "{scoped}");
        assert!(!scoped.contains("NEEDLE in b"), "{scoped}");

        // A session filter reaches by id even across projects.
        s.insert_session("c", "/other", None, None).unwrap();
        s.append_message("c", "user", &text_content("user", "NEEDLE in c"), None)
            .unwrap();
        let by_id = run(
            &json!({"mode": "search", "query": "NEEDLE", "session": "c"}),
            &s,
            "/proj",
        )
        .unwrap();
        assert!(by_id.contains("NEEDLE in c"), "{by_id}");
        assert!(!by_id.contains("NEEDLE in a"), "{by_id}");

        let err = run(
            &json!({"mode": "search", "query": "NEEDLE", "session": "ghost"}),
            &s,
            "/proj",
        )
        .unwrap_err();
        assert!(err.to_string().contains("no session"), "{err}");
    }

    #[test]
    fn search_hits_carry_the_entry_timestamp() {
        let mut s = store();
        s.insert_session("a", "/proj", None, None).unwrap();
        s.append_message("a", "user", &text_content("user", "NEEDLE"), None)
            .unwrap();
        let stored_ts = s.transcript_tail("a", 1, false).unwrap()[0]
            .row
            .timestamp
            .clone();

        let out = run(&json!({"mode": "search", "query": "NEEDLE"}), &s, "/proj").unwrap();
        assert!(out.contains(&stored_ts), "{out}");
    }

    #[test]
    fn around_ordinal_windows_the_middle_and_handles_out_of_range() {
        let mut s = store();
        s.insert_session("a", "/proj", None, None).unwrap();
        for i in 0..30 {
            s.append_message("a", "user", &text_content("user", &format!("msg{i}")), None)
                .unwrap();
        }

        let out = run(
            &json!({"mode": "inspect", "session": "a", "around_ordinal": 15, "last_n": 5}),
            &s,
            "/proj",
        )
        .unwrap();
        assert!(out.contains("entries around ordinal 15:"), "{out}");
        // Window is [13, 17]: start = 15 - 5/2, five entries.
        for i in 13..=17 {
            assert!(out.contains(&format!("msg{i}\n")), "{out}");
        }
        assert!(
            !out.contains("msg12\n") && !out.contains("msg18\n"),
            "{out}"
        );

        let past_end = run(
            &json!({"mode": "inspect", "session": "a", "around_ordinal": 500}),
            &s,
            "/proj",
        )
        .unwrap();
        assert!(
            past_end.contains("(no entries around ordinal 500)"),
            "{past_end}"
        );

        // A window anchored before the start clamps to the transcript head.
        let at_start = run(
            &json!({"mode": "inspect", "session": "a", "around_ordinal": 0, "last_n": 4}),
            &s,
            "/proj",
        )
        .unwrap();
        assert!(
            at_start.contains("msg0\n") && at_start.contains("msg3\n"),
            "{at_start}"
        );
        assert!(!at_start.contains("msg4\n"), "{at_start}");
    }

    #[test]
    fn search_snippet_unescapes_json_escapes() {
        let mut s = store();
        s.insert_session("a", "/proj", None, None).unwrap();
        // Stored content is message JSON: the text's newline and quotes are
        // escaped on disk (\n, \") and must not render that way in the snippet.
        s.append_message(
            "a",
            "user",
            &text_content("user", "line1\nline2 \"quoted\""),
            None,
        )
        .unwrap();

        let out = run(&json!({"mode": "search", "query": "line2"}), &s, "/proj").unwrap();
        assert!(out.contains("line1 line2 \"quoted\""), "{out}");
        assert!(!out.contains("\\n") && !out.contains("\\\""), "{out}");
    }

    #[test]
    fn search_disclosure_line_appears_only_when_hits_were_cut() {
        let mut s = store();
        s.insert_session("a", "/proj", None, None).unwrap();
        for i in 0..3 {
            s.append_message(
                "a",
                "user",
                &text_content("user", &format!("NEEDLE {i}")),
                None,
            )
            .unwrap();
        }

        let cut = run(
            &json!({"mode": "search", "query": "NEEDLE", "limit": 2}),
            &s,
            "/proj",
        )
        .unwrap();
        assert!(cut.starts_with("showing 2 of 3 hits\n"), "{cut}");
        assert_eq!(cut.lines().count(), 3, "{cut}");

        let full = run(&json!({"mode": "search", "query": "NEEDLE"}), &s, "/proj").unwrap();
        assert!(!full.contains("showing"), "{full}");
    }

    // Lowercasing can grow byte lengths (İ U+0130 → i + U+0307), so the match
    // offset from the lowered string must never index the original — this input
    // panicked on a mid-char slice before the fix.
    #[test]
    fn snippet_survives_case_folding_that_changes_byte_length() {
        let content = format!("{} NEEDLE after the dotted capital", "İ".repeat(40));
        let out = snippet(&content, "needle");
        assert!(out.contains("NEEDLE"), "{out}");
    }
}
