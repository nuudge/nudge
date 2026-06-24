use anyhow::{Result, bail};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::coding::file_state::FileState;

mod bash;
mod create_new;
mod edit;
mod glob;
mod grep;
mod read;
mod todo_write;

pub async fn dispatch(
    name: &str,
    input: &Value,
    file_state: &Arc<Mutex<FileState>>,
) -> Result<String> {
    match name {
        "Bash" => bash::execute(input).await,
        "Read" => read::execute(input, file_state).await,
        "Edit" => edit::execute(input, file_state).await,
        "CreateNew" => create_new::execute(input).await,
        "Grep" => grep::execute(input).await,
        "Glob" => glob::execute(input).await,
        "TodoWrite" => todo_write::execute(input).await,
        other => bail!("unknown tool: {other}"),
    }
}

pub fn requires_permission(name: &str) -> bool {
    // Stateless / read-only tools auto-allow; everything that touches the
    // filesystem or runs commands gates on the per-call permission prompt.
    !matches!(name, "Read" | "Grep" | "Glob" | "TodoWrite")
}

pub fn schemas() -> Vec<Value> {
    vec![
        bash::schema(),
        read::schema(),
        edit::schema(),
        create_new::schema(),
        grep::schema(),
        glob::schema(),
        todo_write::schema(),
    ]
}

// One-line roster injected into the system prompt. The full mechanics for each
// tool live in its schema description; these snippets only orient. Kept beside
// `schemas()` so the two tool lists stay in lockstep.
pub fn roster() -> String {
    [
        (
            "Read",
            "read a text file (line-numbered; supports offset/limit line ranges)",
        ),
        (
            "Edit",
            "modify an existing file (replace a unique match, or append at EOF)",
        ),
        ("CreateNew", "create a new file; fails if it already exists"),
        (
            "Grep",
            "search file contents (ripgrep; normalized output, gitignore-aware)",
        ),
        (
            "Glob",
            "list files matching a glob pattern (gitignore-aware)",
        ),
        (
            "Bash",
            "run shell commands; the fallback for anything without a dedicated tool",
        ),
        (
            "TodoWrite",
            "maintain a structured task list for multi-step work",
        ),
    ]
    .iter()
    .map(|(name, snippet)| format!("- {name}: {snippet}"))
    .collect::<Vec<_>>()
    .join("\n")
}

pub fn summarize(name: &str, input: &Value) -> String {
    match name {
        "Bash" => bash::summarize(input),
        "Read" => read::summarize(input),
        "Edit" => edit::summarize(input),
        "CreateNew" => create_new::summarize(input),
        "Grep" => grep::summarize(input),
        "Glob" => glob::summarize(input),
        "TodoWrite" => todo_write::summarize(input),
        _ => format!("{name}({input})"),
    }
}

// What the permission modal shows. For Bash the display summary is the
// model-stated *intent*, but a permission decision must be made against the
// actual command — an intent like "list files" says nothing about what the
// command really does. Other tools' summaries already are their inputs.
pub fn permission_summary(name: &str, input: &Value) -> String {
    match name {
        "Bash" => bash::permission_summary(input),
        _ => summarize(name, input),
    }
}

// Whether the collapsed TUI row should preview the first line of output.
// Read/Grep/Glob output is positional noise out of context (line-numbered
// file content, a lone match, a path list) — a line count tells the user
// more. Bash/Edit/CreateNew/TodoWrite output is a *message* (exit status,
// error text, confirmation), which is worth previewing.
pub fn preview_output(name: &str) -> bool {
    !matches!(name, "Read" | "Grep" | "Glob")
}

// Display-side path shortening: the schemas require absolute paths, but in
// collapsed headers an absolute path makes every entry start with the same
// long cwd prefix — and right-truncation then cuts off exactly the part
// that differs. Relativize for display only; paths outside cwd stay absolute.
pub fn display_path(path: &str) -> String {
    let Ok(cwd) = std::env::current_dir() else {
        return path.to_string();
    };
    match std::path::Path::new(path).strip_prefix(&cwd) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".into(),
        Ok(rel) => rel.display().to_string(),
        Err(_) => path.to_string(),
    }
}
