use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::fmt::Write;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::coding::file_state::FileState;

const DEFAULT_LIMIT: u64 = 2000;
const MAX_LINE_CHARS: usize = 2000;

pub fn schema() -> Value {
    json!({
        "name": "Read",
        "description": "Read a file from disk with line numbers prepended (format: right-padded line number, tab, content). Returns up to `limit` lines starting at line `offset` (1-indexed). Defaults: offset 1, limit 2000. To read a specific line range, set both — e.g., lines 65–95 → offset=65, limit=31. This covers the `sed -n 'M,Np'` / `head -n N` use case for ordinary text files; reach for Bash only when you need pipelines, tail-from-end, or other shell semantics Read doesn't model. Lines longer than 2000 characters are truncated with a trailing marker. file_path must be absolute.",
        "input_schema": {
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to read."
                },
                "offset": {
                    "type": "integer",
                    "description": "1-indexed line to start reading from. Default 1."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read. Default 2000."
                }
            },
            "required": ["file_path"]
        }
    })
}

pub fn summarize(input: &Value) -> String {
    super::display_path(input["file_path"].as_str().unwrap_or("<missing file_path>"))
}

pub async fn execute(input: &Value, file_state: &Arc<Mutex<FileState>>) -> Result<String> {
    let file_path = input["file_path"]
        .as_str()
        .context("Read: missing 'file_path' input")?;
    let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = input["limit"].as_u64().unwrap_or(DEFAULT_LIMIT) as usize;

    let path = Path::new(file_path);
    if !path.is_absolute() {
        bail!("Read: file_path must be absolute, got {file_path}");
    }

    let content = tokio::fs::read_to_string(file_path)
        .await
        .with_context(|| format!("failed to read {file_path}"))?;

    // Record AFTER the read succeeds so a failed read (missing/unreadable file)
    // doesn't pollute the tracker. Use a sync-side block scoped tight so we
    // don't hold the lock across other awaits.
    file_state.lock().await.record_read(path)?;

    let mut out = String::new();
    let mut emitted = 0;
    for (idx, line) in content.lines().enumerate() {
        let lineno = idx + 1;
        if lineno < offset {
            continue;
        }
        if emitted >= limit {
            break;
        }
        let rendered = if line.chars().count() > MAX_LINE_CHARS {
            let prefix: String = line.chars().take(MAX_LINE_CHARS).collect();
            format!("{prefix}... [line truncated]")
        } else {
            line.to_string()
        };
        writeln!(out, "{lineno:>6}\t{rendered}").unwrap();
        emitted += 1;
    }

    if emitted == 0 {
        return Ok(format!(
            "(no lines returned: file is empty or offset {offset} is past end)"
        ));
    }
    Ok(out)
}
