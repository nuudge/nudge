use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

// A hang inside one tool call can't be broken by max_iterations, hence the
// deadline; the cap keeps verbose output from flooding the context.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTPUT_BYTES: usize = 16 * 1024;

pub fn schema() -> Value {
    json!({
        "name": "Bash",
        "description": "Run a shell command on the user's machine via `sh -c`. Returns combined stdout/stderr prefixed with the exit status. Use for tests, git operations, deletes, and ad-hoc one-liners — plus reads and searches the dedicated tools don't model: tail-from-end, pipelines that filter/transform (`jq <`, `rg ... | head`), VCS-mediated reads (`git show REV:file`, `git grep`), archive/binary inspection.\n\nIMPORTANT: when a dedicated tool covers the operation, use it — going through Bash loses the schema constraints and normalized output that make the dedicated tool the right call for the common case:\n- Read a file: use Read (NOT `cat`/`head`/`tail`/`sed -n`)\n- Edit a file: use Edit (NOT `sed -i`/`perl -i`/in-place `awk`; to add at EOF use Edit's append mode, NOT `cat >>`/`echo >>`/a heredoc)\n- Create a file: use CreateNew (NOT `>` or `cat <<EOF` to a new path)\n- Search file contents: use Grep (NOT a bare `grep`/`rg`)\n- List or find files: use Glob (NOT a bare `ls`/`find -name`)\nThis applies to a single simple command hitting one of those on an ordinary file. A pipeline or composed command (`cat f | jq`, `rg ... | head`, `grep ... ; cargo build`) is legitimately Bash — the dedicated tools don't model composition.\n\nEach command runs to completion under a 120s timeout (the process is killed on overrun) and its combined output is capped, with the middle elided if it overflows. Don't launch long-running or interactive processes (dev servers, file watchers, REPLs) — they'll hit the timeout and be killed.\n\n`cd` does not persist between calls — each runs in a fresh shell from the working directory, and neither does any other shell state (exported vars, shell functions) set in a prior call.\n\nThe shell is non-interactive and non-login: it inherits the environment nudge was started with (the launching shell's exported vars) but does NOT source `~/.zshrc`/`~/.bashrc`/profile — so rc-defined aliases, functions, and variables are unavailable. Reference tools by their real name/path, not via shell aliases.",
        "input_schema": {
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to run."
                },
                "intent": {
                    "type": "string",
                    "description": "One short clause (5-10 words) stating what this command accomplishes, e.g. 'count lines in all Rust files'. Shown to the user as the action label while the command runs — commands are often long or cryptic, and the intent lets the user follow along at a glance. The raw command is still shown alongside the intent whenever permission is requested."
                }
            },
            "required": ["command", "intent"]
        }
    })
}

fn command_of(input: &Value) -> &str {
    input["command"].as_str().unwrap_or("<missing command>")
}

fn intent_of(input: &Value) -> Option<&str> {
    input["intent"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub fn summarize(input: &Value) -> String {
    match intent_of(input) {
        Some(intent) => intent.to_string(),
        None => command_of(input).to_string(),
    }
}

// The user is approving what will run, so show the command, not just the intent.
pub fn permission_summary(input: &Value) -> String {
    let command = command_of(input);
    match intent_of(input) {
        Some(intent) => format!("{intent}\n\n$ {command}"),
        None => format!("$ {command}"),
    }
}

pub async fn execute(input: &Value) -> Result<String> {
    let command = input["command"]
        .as_str()
        .context("Bash: missing 'command' input")?;
    run(command, COMMAND_TIMEOUT).await
}

async fn run(command: &str, deadline: Duration) -> Result<String> {
    // kill_on_drop: on timeout the dropped future drops the Child, killing it
    // instead of orphaning it.
    let child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("failed to spawn shell")?;

    let output = match timeout(deadline, child.wait_with_output()).await {
        Ok(result) => result.context("failed to wait on shell")?,
        Err(_) => bail!(
            "command timed out after {}s and was killed. Bash runs each command to completion — it can't host a long-running or interactive process (dev server, watcher, REPL). Re-run in a bounded form, or background the process outside the agent.",
            deadline.as_secs()
        ),
    };

    // stderr last so a failure's diagnostics survive middle-truncation.
    let mut body = String::from_utf8_lossy(&output.stdout).into_owned();
    body.push_str(&String::from_utf8_lossy(&output.stderr));
    let status = output.status.code().unwrap_or(-1);
    Ok(format!("[exit {status}]\n{}", cap_output(&body)))
}

// Elides the middle: the head (first errors) and tail (summary, diagnostics)
// carry the most signal.
fn cap_output(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s.to_string();
    }
    let half = MAX_OUTPUT_BYTES / 2;
    let head_end = floor_char_boundary(s, half);
    let tail_start = ceil_char_boundary(s, s.len() - half);
    let omitted = tail_start - head_end;
    format!(
        "{}\n[... {omitted} bytes of output truncated ...]\n{}",
        &s[..head_end],
        &s[tail_start..]
    )
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn under_cap_is_untouched() {
        let s = "hello world";
        assert_eq!(cap_output(s), s);
    }

    #[test]
    fn over_cap_elides_middle_keeping_both_ends() {
        let s = format!(
            "{}{}",
            "A".repeat(MAX_OUTPUT_BYTES),
            "Z".repeat(MAX_OUTPUT_BYTES)
        );
        let out = cap_output(&s);
        assert!(out.len() < s.len());
        assert!(out.starts_with("AAAA"));
        assert!(out.ends_with("ZZZZ"));
        assert!(out.contains("bytes of output truncated"));
    }

    #[test]
    fn truncation_never_splits_a_utf8_sequence() {
        let s = "€".repeat(MAX_OUTPUT_BYTES);
        let out = cap_output(&s);
        assert!(out.starts_with('€'));
        assert!(out.ends_with('€'));
    }

    #[tokio::test]
    async fn normal_command_reports_exit_and_output() {
        let out = execute(&json!({"command": "echo hi", "intent": "test"}))
            .await
            .unwrap();
        assert_eq!(out, "[exit 0]\nhi\n");
    }

    #[tokio::test]
    async fn nonzero_exit_is_output_not_error() {
        let out = execute(&json!({"command": "exit 3", "intent": "test"}))
            .await
            .unwrap();
        assert_eq!(out, "[exit 3]\n");
    }

    #[tokio::test]
    async fn overrunning_command_times_out_and_errors() {
        let started = std::time::Instant::now();
        let err = run("sleep 30", Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(err.to_string().contains("timed out"));
    }
}
