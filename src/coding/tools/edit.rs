use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::coding::file_state::FileState;

pub fn schema() -> Value {
    json!({
        "name": "Edit",
        "description": "Modify an EXISTING file. Two modes:\n\n- `modify` (default): replace an exact occurrence of `old_string` with `new_string`. The old_string must match EXACTLY ONCE in the file — if it appears multiple times, include more surrounding context to make it unique. `old_string` and `new_string` are required.\n\n- `append`: append `new_string` to the end of the file. `old_string` is ignored. Use this for log-style additions or section additions at EOF.\n\nThe file must already exist for either mode. To create a new file, use CreateNew instead. file_path must be absolute. Writes are atomic (tmp file + rename).\n\nRead-before-Edit invariant: you must Read the file earlier in this session, and the file must not have been modified externally since that Read. If you haven't Read it, Read it first. If the error says it was modified externally, Read it again to see the current contents before editing.",
        "input_schema": {
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to an existing file."
                },
                "mode": {
                    "type": "string",
                    "enum": ["modify", "append"],
                    "description": "Operation mode. Defaults to 'modify' if omitted."
                },
                "old_string": {
                    "type": "string",
                    "description": "Required in modify mode: the exact text to replace, must be unique in the file. Ignored in append mode."
                },
                "new_string": {
                    "type": "string",
                    "description": "In modify mode: the replacement text. In append mode: the text to append at the end of the file."
                }
            },
            "required": ["file_path", "new_string"]
        }
    })
}

pub fn summarize(input: &Value) -> String {
    let path = super::display_path(input["file_path"].as_str().unwrap_or("<missing file_path>"));
    match input["mode"].as_str().unwrap_or("modify") {
        "append" => format!("{path} (append)"),
        _ => path,
    }
}

pub async fn execute(input: &Value, file_state: &Arc<Mutex<FileState>>) -> Result<String> {
    let file_path = input["file_path"]
        .as_str()
        .context("Edit: missing 'file_path' input")?;
    let new_string = input["new_string"]
        .as_str()
        .context("Edit: missing 'new_string' input")?;
    let mode = input["mode"].as_str().unwrap_or("modify");

    let path = Path::new(file_path);
    if !path.is_absolute() {
        bail!("Edit: file_path must be absolute, got {file_path}");
    }

    let exists = tokio::fs::try_exists(path)
        .await
        .with_context(|| format!("failed to stat {file_path}"))?;
    if !exists {
        bail!("Edit: file does not exist: {file_path}. Use CreateNew to create a new file.");
    }

    // Read-before-Edit + external-modification check. Runs after the existence
    // check so a missing-file error fires with its more specific message.
    file_state.lock().await.check_edit(path)?;

    match mode {
        "modify" => {
            let old_string = input["old_string"]
                .as_str()
                .context("Edit: 'old_string' is required in modify mode")?;
            if old_string.is_empty() {
                bail!(
                    "Edit: old_string must be non-empty in modify mode. For appending to a file, use mode='append'."
                );
            }
            let content = tokio::fs::read_to_string(path)
                .await
                .with_context(|| format!("failed to read {file_path}"))?;
            let matches = content.matches(old_string).count();
            if matches == 0 {
                bail!("Edit: old_string not found in {file_path}");
            }
            if matches > 1 {
                bail!(
                    "Edit: old_string matches {matches} locations in {file_path}; add more surrounding context to make it unique"
                );
            }
            let new_content = content.replacen(old_string, new_string, 1);
            write_atomic(path, &new_content).await?;
            file_state.lock().await.record_write(path)?;
            Ok(format!("Edited {file_path}"))
        }
        "append" => {
            // Direct append via OpenOptions rather than read+concat+atomic-rename:
            // the rename path would require reading the entire file into memory
            // just to glue bytes on the end, which is wasteful on large files
            // (logs are the prime use case). Trade-off: a process kill mid-write
            // can leave a partial tail rather than the all-or-nothing guarantee
            // modify mode has. Acceptable — the conventional shape (>>, tee -a,
            // log writers) all do the same.
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .await
                .with_context(|| format!("failed to open {file_path} for append"))?;
            file.write_all(new_string.as_bytes())
                .await
                .with_context(|| format!("failed to append to {file_path}"))?;
            file.flush()
                .await
                .with_context(|| format!("failed to flush {file_path}"))?;
            file_state.lock().await.record_write(path)?;
            Ok(format!(
                "Appended {} bytes to {file_path}",
                new_string.len()
            ))
        }
        other => bail!("Edit: invalid mode {other:?}; expected 'modify' or 'append'"),
    }
}

async fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !tokio::fs::try_exists(parent).await.unwrap_or(false)
    {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create parent dirs for {}", path.display()))?;
    }
    let file_name = path
        .file_name()
        .context("Edit: file_path has no file name")?
        .to_string_lossy()
        .into_owned();
    let tmp = path.with_file_name(format!("{file_name}.nudge.tmp"));
    tokio::fs::write(&tmp, content)
        .await
        .with_context(|| format!("failed to write tmp file {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("failed to rename {} to {}", tmp.display(), path.display()))?;
    Ok(())
}
