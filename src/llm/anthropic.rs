use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

use super::{ContentBlock, Message, Provider, Request, Response, Usage};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const COUNT_TOKENS_URL: &str = "https://api.anthropic.com/v1/messages/count_tokens";

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
        }
    }

    // Shared wire shaping: serialize system blocks with `cache_control` on the
    // cache-flagged ones, apply the tool cache marker at `tool_cache_boundary`,
    // and the floating history breakpoint on the messages. Both `complete` and
    // `count_tokens` build on this; the per-call fields (max_tokens, thinking)
    // differ because the count_tokens endpoint rejects `max_tokens`.
    fn wire_parts(req: &Request<'_>) -> (Vec<serde_json::Value>, Vec<serde_json::Value>, Vec<serde_json::Value>) {
        let system: Vec<serde_json::Value> = req
            .system
            .iter()
            .map(|b| {
                if b.cache {
                    json!({ "type": "text", "text": b.text,
                            "cache_control": { "type": "ephemeral" } })
                } else {
                    json!({ "type": "text", "text": b.text })
                }
            })
            .collect();

        let mut tools = req.tools.clone();
        if let Some(i) = req.tool_cache_boundary
            && let Some(serde_json::Value::Object(obj)) = tools.get_mut(i)
        {
            obj.insert("cache_control".into(), json!({ "type": "ephemeral" }));
        }

        (system, tools, messages_with_floating_breakpoint(req.messages))
    }
}

impl Provider for AnthropicProvider {
    async fn complete(&self, req: &Request<'_>) -> Result<Response> {
        let (system, tools, messages) = Self::wire_parts(req);
        let body = json!({
            "model": req.model,
            "system": system,
            "max_tokens": req.max_tokens,
            "thinking": { "type": "adaptive", "display": req.thinking_display },
            "tools": tools,
            "messages": messages,
        });
        let resp: MessagesResponse = self
            .client
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("API request failed")?
            .error_for_status()
            .context("API returned error status")?
            .json()
            .await
            .context("failed to parse API response")?;
        Ok(Response {
            content: resp.content,
            stop_reason: resp.stop_reason,
            usage: Usage {
                input_tokens: resp.usage.input_tokens,
                output_tokens: resp.usage.output_tokens,
                cache_creation_input_tokens: resp.usage.cache_creation_input_tokens,
                cache_read_input_tokens: resp.usage.cache_read_input_tokens,
            },
        })
    }

    async fn count_tokens(&self, req: &Request<'_>) -> Result<u64> {
        // The count_tokens endpoint rejects `max_tokens` and doesn't need
        // `thinking`; it accepts the same system/tools/messages shape (cache
        // markers included, which it tolerates).
        let (system, tools, messages) = Self::wire_parts(req);
        let body = json!({
            "model": req.model,
            "system": system,
            "tools": tools,
            "messages": messages,
        });
        let v: serde_json::Value = self
            .client
            .post(COUNT_TOKENS_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("count_tokens request failed")?
            .error_for_status()
            .context("count_tokens returned error status")?
            .json()
            .await
            .context("failed to parse count_tokens")?;
        v["input_tokens"]
            .as_u64()
            .context("count_tokens response missing input_tokens")
    }
}

#[derive(Deserialize, Debug)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    stop_reason: String,
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Deserialize, Debug, Default)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

// Serialize `messages` for the API request body, injecting a `cache_control`
// breakpoint on the last content block of the most recent assistant turn.
// The breakpoint floats forward with the conversation: each request marks a
// stable position (the model's already-emitted output, bytes fixed forever),
// so the next request's 20-block lookback finds this entry and reads the
// cached prefix instead of re-paying for the full history. cache_control is
// request-time-only metadata; we don't carry it on the persisted Message
// struct (which would corrupt the wire-faithful schema for JSONL replay).
fn messages_with_floating_breakpoint(messages: &[Message]) -> Vec<serde_json::Value> {
    let breakpoint_idx = messages.iter().rposition(|m| m.role == "assistant");
    messages
        .iter()
        .enumerate()
        .map(|(i, msg)| {
            let mut content: Vec<serde_json::Value> = msg
                .content
                .iter()
                .map(|b| serde_json::to_value(b).expect("ContentBlock always serializes"))
                .collect();
            if Some(i) == breakpoint_idx {
                // cache_control cannot be placed on a thinking/redacted_thinking
                // block (the API rejects it). In practice the last block of an
                // assistant turn is always text or tool_use because thinking
                // comes first, but be defensive — walk back to the last block
                // whose type accepts the marker.
                if let Some(last_idx) = content.iter().rposition(|v| {
                    !matches!(
                        v.get("type").and_then(|t| t.as_str()),
                        Some("thinking") | Some("redacted_thinking")
                    )
                }) && let serde_json::Value::Object(obj) = &mut content[last_idx]
                {
                    obj.insert("cache_control".into(), json!({ "type": "ephemeral" }));
                }
            }
            json!({ "role": msg.role, "content": content })
        })
        .collect()
}
