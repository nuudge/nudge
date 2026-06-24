use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;
use tokio::process::Command;

const DEFAULT_HEAD_LIMIT: usize = 200;

#[derive(Deserialize)]
struct GrepInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(rename = "type", default)]
    file_type: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    output_mode: Option<String>,
    #[serde(default)]
    context_lines: Option<u32>,
    #[serde(default)]
    head_limit: Option<usize>,
}

pub fn schema() -> Value {
    json!({
        "name": "Grep",
        "description": "Search file contents using ripgrep. Prefer this over `Bash rg` — the schema constrains arguments, output is normalized, and gitignore is honored.\n\nThree output modes:\n- `content` (default): matching lines with `file:line:` prefixes. Use `context_lines` for surrounding lines.\n- `files_with_matches`: just the file paths containing matches. Faster; use for discovery.\n- `count`: per-file match counts.\n\nDefaults: case-sensitive, no context, 200-line cap. Raise `head_limit` or narrow the search with `glob`/`type`/`path` when output is truncated.\n\nReach for Bash instead only when the search needs a pipeline (`rg ... | head | sort -u`) or VCS-aware scope (`git grep`).",
        "input_schema": {
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern (Rust regex syntax — same as ripgrep)."
                },
                "path": {
                    "type": "string",
                    "description": "Absolute path to a file or directory to search. Defaults to the working directory."
                },
                "glob": {
                    "type": "string",
                    "description": "Filter files by glob (e.g. '*.rs', '!*.lock'). Multiple needs separate calls."
                },
                "type": {
                    "type": "string",
                    "description": "Filter by file type (e.g. 'rust', 'py', 'md'). See `rg --type-list` for the full set."
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Case-insensitive match. Default false."
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Output format. Default 'content'."
                },
                "context_lines": {
                    "type": "integer",
                    "description": "Lines of context around each match (-C). Only applies to content mode. Default 0."
                },
                "head_limit": {
                    "type": "integer",
                    "description": "Max output lines before truncation. Default 200. Pass 0 to disable (risky on large repos)."
                }
            },
            "required": ["pattern"]
        }
    })
}

pub fn summarize(input: &Value) -> String {
    let pattern = input["pattern"].as_str().unwrap_or("?");
    let mut parts = vec![format!("/{pattern}/")];
    if let Some(p) = input["path"].as_str() {
        parts.push(format!("in {}", super::display_path(p)));
    }
    if let Some(g) = input["glob"].as_str() {
        parts.push(format!("glob={g}"));
    }
    if let Some(t) = input["type"].as_str() {
        parts.push(format!("type={t}"));
    }
    parts.join(" ")
}

pub async fn execute(input: &Value) -> Result<String> {
    let parsed: GrepInput =
        serde_json::from_value(input.clone()).context("Grep: invalid input shape")?;

    let mut args: Vec<String> = vec!["--no-heading".into(), "--color=never".into()];

    let mode = parsed.output_mode.as_deref().unwrap_or("content");
    match mode {
        "content" => {
            args.push("-n".into());
            args.push("--with-filename".into());
            if let Some(c) = parsed.context_lines
                && c > 0
            {
                args.push(format!("-C{c}"));
            }
        }
        "files_with_matches" => args.push("--files-with-matches".into()),
        "count" => args.push("--count".into()),
        other => bail!(
            "Grep: invalid output_mode {other:?}; expected content | files_with_matches | count"
        ),
    }

    if parsed.case_insensitive {
        args.push("-i".into());
    }
    if let Some(g) = &parsed.glob {
        args.push(format!("--glob={g}"));
    }
    if let Some(t) = &parsed.file_type {
        args.push(format!("--type={t}"));
    }

    // `--` separates flags from the pattern so a pattern starting with `-` isn't
    // misparsed as an option.
    args.push("--".into());
    args.push(parsed.pattern.clone());

    if let Some(p) = &parsed.path {
        if !Path::new(p).is_absolute() {
            bail!("Grep: path must be absolute, got {p}");
        }
        args.push(p.clone());
    }

    let output = Command::new("rg")
        .args(&args)
        .output()
        .await
        .context("Grep: failed to spawn rg (is ripgrep installed?)")?;

    let status = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // rg exits 1 on "no matches" — that's a normal result for an agent, not an error.
    if status == 1 && stderr.is_empty() {
        return Ok("(no matches)".into());
    }
    if status != 0 && status != 1 {
        bail!("rg exited {status}: {}", stderr.trim());
    }

    let head_limit = parsed.head_limit.unwrap_or(DEFAULT_HEAD_LIMIT);
    if head_limit == 0 {
        return Ok(stdout.into_owned());
    }

    let lines: Vec<&str> = stdout.lines().collect();
    if lines.len() <= head_limit {
        Ok(stdout.into_owned())
    } else {
        let truncated = lines[..head_limit].join("\n");
        let remaining = lines.len() - head_limit;
        Ok(format!(
            "{truncated}\n... [output truncated: {remaining} more lines — raise head_limit or narrow with glob/type/path]"
        ))
    }
}
