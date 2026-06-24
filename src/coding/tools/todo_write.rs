use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fmt::Write;

#[derive(Deserialize)]
struct TodoInput {
    todos: Vec<Todo>,
}

#[derive(Deserialize)]
struct Todo {
    content: String,
    status: String,
    #[serde(rename = "activeForm")]
    active_form: String,
}

pub fn schema() -> Value {
    json!({
        "name": "TodoWrite",
        "description": "Maintain a structured task list for the current work. Use proactively when the task has 3+ steps, requires planning, or spans multiple files — this surfaces your plan to the user and gives you a scratchpad to reason against between tool calls.\n\nReplace semantics: each call replaces the entire list. Always pass the full updated list, not a delta.\n\nRules:\n- At most one task is `in_progress` at a time. Mark a task `in_progress` BEFORE starting it; mark it `completed` IMMEDIATELY after finishing — do not batch multiple completions.\n- `content` is the imperative form shown when the task is not in progress (\"Add Grep tool\"). `activeForm` is the present-continuous form shown while in progress (\"Adding Grep tool\"). Both are required on every todo.\n- Skip this tool for trivial single-step tasks — using it for everything is noise. The bar is: would the user benefit from seeing the plan?",
        "input_schema": {
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "Imperative form, e.g. \"Add Grep tool\"."
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            },
                            "activeForm": {
                                "type": "string",
                                "description": "Present-continuous form shown while in_progress, e.g. \"Adding Grep tool\"."
                            }
                        },
                        "required": ["content", "status", "activeForm"]
                    }
                }
            },
            "required": ["todos"]
        }
    })
}

pub fn summarize(input: &Value) -> String {
    let Some(todos) = input["todos"].as_array() else {
        return "<invalid todos>".into();
    };
    let total = todos.len();
    let in_progress = todos
        .iter()
        .filter(|t| t["status"] == "in_progress")
        .count();
    let completed = todos.iter().filter(|t| t["status"] == "completed").count();
    let pending = total.saturating_sub(in_progress + completed);
    format!("{total} todos ({pending} pending, {in_progress} active, {completed} done)")
}

pub async fn execute(input: &Value) -> Result<String> {
    let parsed: TodoInput =
        serde_json::from_value(input.clone()).context("TodoWrite: invalid input shape")?;

    let mut in_progress_count = 0;
    for t in &parsed.todos {
        match t.status.as_str() {
            "pending" | "completed" => {}
            "in_progress" => in_progress_count += 1,
            other => bail!(
                "TodoWrite: invalid status {other:?}; expected pending | in_progress | completed"
            ),
        }
        if t.content.trim().is_empty() {
            bail!("TodoWrite: every todo must have a non-empty `content`");
        }
        if t.active_form.trim().is_empty() {
            bail!("TodoWrite: every todo must have a non-empty `activeForm`");
        }
    }
    if in_progress_count > 1 {
        bail!(
            "TodoWrite: only one task may be `in_progress` at a time (found {in_progress_count})"
        );
    }

    // Echo the list back to the model. This is partly for verification (the
    // model sees what was committed) and partly so the tool_result carries
    // human-readable state into the TUI's collapsible result view.
    let mut out = String::from("Todos updated:\n");
    for (i, t) in parsed.todos.iter().enumerate() {
        let marker = match t.status.as_str() {
            "completed" => "[x]",
            "in_progress" => "[~]",
            _ => "[ ]",
        };
        let display = if t.status == "in_progress" {
            &t.active_form
        } else {
            &t.content
        };
        writeln!(out, "  {marker} {}. {display}", i + 1).unwrap();
    }
    Ok(out)
}
