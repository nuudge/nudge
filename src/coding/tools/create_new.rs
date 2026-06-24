use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::Path;

pub fn schema() -> Value {
    json!({
        "name": "CreateNew",
        "description": "Create a NEW file at `file_path` with the given `content` (defaults to empty if omitted). Fails if the file already exists — this is intentional, so you can't silently overwrite contents you haven't seen. To modify an existing file, use Edit; to wholesale-replace it, delete it first via Bash then call CreateNew. Auto-creates any missing parent directories. file_path must be absolute. Writes are atomic (tmp file + rename).",
        "input_schema": {
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path. Must not already exist."
                },
                "content": {
                    "type": "string",
                    "description": "Full file contents. Optional; defaults to empty string (touch-like)."
                }
            },
            "required": ["file_path"]
        }
    })
}

pub fn summarize(input: &Value) -> String {
    super::display_path(input["file_path"].as_str().unwrap_or("<missing file_path>"))
}

pub async fn execute(input: &Value) -> Result<String> {
    let file_path = input["file_path"]
        .as_str()
        .context("CreateNew: missing 'file_path' input")?;
    let content = input["content"].as_str().unwrap_or("");

    let path = Path::new(file_path);
    if !path.is_absolute() {
        bail!("CreateNew: file_path must be absolute, got {file_path}");
    }

    let exists = tokio::fs::try_exists(path)
        .await
        .with_context(|| format!("failed to stat {file_path}"))?;
    if exists {
        bail!(
            "CreateNew: {file_path} already exists. Use Edit to modify it, or delete it first via Bash for a wholesale replacement."
        );
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !tokio::fs::try_exists(parent).await.unwrap_or(false)
    {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create parent dirs for {file_path}"))?;
    }

    let file_name = path
        .file_name()
        .context("CreateNew: file_path has no file name")?
        .to_string_lossy()
        .into_owned();
    let tmp = path.with_file_name(format!("{file_name}.nudge.tmp"));
    tokio::fs::write(&tmp, content)
        .await
        .with_context(|| format!("failed to write tmp file {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("failed to rename {} to {file_path}", tmp.display()))?;

    Ok(format!("Created {file_path}"))
}
