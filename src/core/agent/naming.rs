use crate::core::session::Session;
use crate::llm::{ContentBlock, Message, Response};

// First 8 chars of the uuid; guarded against a short id.
pub(super) fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}

// Tier-3 fallback: the cwd's own name, else a short id.
pub(super) fn fallback_name(session: &Session) -> String {
    session
        .cwd
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(sanitize_title)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| short_id(&session.id).to_string())
}

// Tier-3 title request prompt. None when nothing has been said yet (the caller
// then falls back).
pub(super) fn title_prompt(messages: &[Message]) -> Option<String> {
    let digest = conversation_digest(messages)?;
    Some(format!(
        "Below is the start of a coding session.\n\n{digest}\n\nReply with ONLY a short, lowercase, kebab-case title (3-6 words joined by hyphens) describing the task. No quotes, no punctuation, no explanation."
    ))
}

// First non-empty text block of the response, normalized.
pub(super) fn title_from_response(resp: &Response) -> Option<String> {
    let raw = resp.content.iter().find_map(|b| match b {
        ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
        _ => None,
    })?;
    let title = sanitize_title(raw);
    (!title.is_empty()).then_some(title)
}

// Bounded plain-text digest of the conversation start, capped to stay cheap.
fn conversation_digest(messages: &[Message]) -> Option<String> {
    const CAP: usize = 1500;
    let mut out = String::new();
    for m in messages {
        for block in &m.content {
            if let ContentBlock::Text { text } = block
                && !text.trim().is_empty()
            {
                out.push_str(text.trim());
                out.push('\n');
            }
        }
        if out.len() >= CAP {
            break;
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(CAP).collect())
    }
}

// Normalize arbitrary text into a kebab-case label (lowercase alphanumerics,
// other runs collapsed to one hyphen, length-capped).
fn sanitize_title(raw: &str) -> String {
    const MAX_LEN: usize = 60;
    let mut out = String::new();
    let mut pending_hyphen = false;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_hyphen && !out.is_empty() {
                out.push('-');
            }
            pending_hyphen = false;
            out.push(c.to_ascii_lowercase());
        } else {
            pending_hyphen = true;
        }
    }
    // All ASCII, so a byte truncation is char-safe; trim any dangling hyphen.
    out.truncate(MAX_LEN);
    out.trim_end_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Message {
        Message {
            role: "user".into(),
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    #[test]
    fn sanitize_title_kebabs_and_trims() {
        assert_eq!(
            sanitize_title("Fix the Auth Retry Logic!"),
            "fix-the-auth-retry-logic"
        );
        assert_eq!(sanitize_title("  --Hello,  World--  "), "hello-world");
        assert_eq!(sanitize_title("add-user-login"), "add-user-login");
        assert_eq!(sanitize_title("!!! ??? ..."), "");
    }

    #[test]
    fn sanitize_title_caps_length() {
        let long = "word ".repeat(40);
        assert!(sanitize_title(&long).len() <= 60);
    }

    #[test]
    fn title_from_response_normalizes_first_text() {
        let resp = Response {
            content: vec![
                ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: "sig".into(),
                },
                ContentBlock::Text {
                    text: "  Add User Login Flow  ".into(),
                },
            ],
            stop_reason: "end_turn".into(),
            usage: Default::default(),
        };
        assert_eq!(
            title_from_response(&resp).as_deref(),
            Some("add-user-login-flow")
        );
    }

    #[test]
    fn title_prompt_none_without_text() {
        assert!(title_prompt(&[]).is_none());
        // A turn carrying only non-text content has nothing to summarize.
        let toolish = Message {
            role: "assistant".into(),
            content: vec![ContentBlock::ToolUse {
                id: "t".into(),
                name: "Bash".into(),
                input: serde_json::json!({}),
            }],
        };
        assert!(title_prompt(&[toolish]).is_none());
    }

    #[test]
    fn title_prompt_embeds_conversation_digest() {
        let prompt = title_prompt(&[user("Please refactor the parser")]).unwrap();
        assert!(prompt.contains("Please refactor the parser"));
        assert!(prompt.contains("kebab-case"));
    }
}
