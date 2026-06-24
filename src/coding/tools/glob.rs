use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;
use tokio::process::Command;

const DEFAULT_HEAD_LIMIT: usize = 200;

#[derive(Deserialize)]
struct GlobInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    head_limit: Option<usize>,
}

pub fn schema() -> Value {
    json!({
        "name": "Glob",
        "description": "List files matching a glob pattern, gitignore-aware (uses ripgrep's file walker). Output is sorted for stable, cache-friendly results.\n\nFor content search, use Grep instead — Glob only enumerates paths. Reach for Bash for archive listings (`tar -tvf`), `find -exec`, or non-gitignore-aware walks.",
        "input_schema": {
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern. Examples: 'src/**/*.rs', '*.md', '!**/target/**' to exclude. Use '**' for recursive."
                },
                "path": {
                    "type": "string",
                    "description": "Absolute path to a directory to search in. Defaults to the working directory."
                },
                "head_limit": {
                    "type": "integer",
                    "description": "Max paths to return before truncation. Default 200. Pass 0 to disable."
                }
            },
            "required": ["pattern"]
        }
    })
}

pub fn summarize(input: &Value) -> String {
    let pattern = input["pattern"].as_str().unwrap_or("?");
    match input["path"].as_str() {
        Some(p) => format!("{pattern} in {}", super::display_path(p)),
        None => pattern.to_string(),
    }
}

pub async fn execute(input: &Value) -> Result<String> {
    let parsed: GlobInput =
        serde_json::from_value(input.clone()).context("Glob: invalid input shape")?;

    let mut args: Vec<String> = vec![
        "--files".into(),
        "--color=never".into(),
        format!("--glob={}", parsed.pattern),
    ];

    if let Some(p) = &parsed.path {
        if !Path::new(p).is_absolute() {
            bail!("Glob: path must be absolute, got {p}");
        }
        args.push(p.clone());
    }

    let output = Command::new("rg")
        .args(&args)
        .output()
        .await
        .context("Glob: failed to spawn rg (is ripgrep installed?)")?;

    let status = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if status != 0 && status != 1 {
        bail!("rg exited {status}: {}", stderr.trim());
    }

    // rg's --files emits in filesystem-walk order, which is non-deterministic
    // across runs. Sort for stable output — cheap on file lists and helps the
    // model see consistent results across calls.
    let mut lines: Vec<&str> = stdout.lines().collect();
    if lines.is_empty() {
        return Ok("(no matches)".into());
    }
    lines.sort_unstable();

    let head_limit = parsed.head_limit.unwrap_or(DEFAULT_HEAD_LIMIT);
    if head_limit == 0 || lines.len() <= head_limit {
        return Ok(lines.join("\n"));
    }
    let truncated = lines[..head_limit].join("\n");
    let remaining = lines.len() - head_limit;
    Ok(format!(
        "{truncated}\n... [output truncated: {remaining} more paths — raise head_limit or narrow the pattern]"
    ))
}
