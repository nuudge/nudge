use anyhow::Result;
use serde::{Deserialize, Serialize};

pub mod anthropic;

pub use anthropic::AnthropicProvider;

// The conversation message model. This is the neutral shape the rest of the
// agent works in; each provider is responsible for serializing it to and from
// its own wire format. It happens to match the Anthropic wire shape today, but
// nothing above the provider layer may assume that.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Message {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    // Adaptive thinking response shape. `thinking` is the summarized reasoning
    // (empty when display: "omitted"); `signature` is the opaque encrypted
    // representation of the full thinking that must be round-tripped back to
    // the API unmodified to preserve reasoning continuity across tool calls.
    Thinking {
        thinking: String,
        signature: String,
    },
    // Safety-filtered thinking. Opaque `data` field; round-trip unmodified.
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    // Server-side tool call (web search) that Anthropic already ran inside the
    // request. Never dispatched locally; round-trip unmodified.
    ServerToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    // Opaque: either a list of results (with `encrypted_content` the API needs
    // back verbatim) or an error object.
    WebSearchToolResult {
        tool_use_id: String,
        content: serde_json::Value,
    },
}

pub fn web_search_query(input: &serde_json::Value) -> String {
    input
        .get("query")
        .and_then(|q| q.as_str())
        .unwrap_or_default()
        .to_string()
}

// One `title — url` line per result, or the error code with `is_error` set.
pub fn web_search_results(content: &serde_json::Value) -> (String, bool) {
    match content.as_array() {
        Some(results) => (
            results
                .iter()
                .map(|r| {
                    format!(
                        "{} — {}",
                        r["title"].as_str().unwrap_or_default(),
                        r["url"].as_str().unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            false,
        ),
        None => (
            format!(
                "web search error: {}",
                content["error_code"].as_str().unwrap_or("unknown")
            ),
            true,
        ),
    }
}

#[derive(Debug, Default, Clone)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

// One block of system-prompt content. `cache` asks the provider to place a
// prompt-cache breakpoint at this block; providers without prompt caching
// ignore it. The caller decides which blocks are stable enough to cache.
pub struct SystemBlock {
    pub text: String,
    pub cache: bool,
}

// A provider-neutral completion request. Caching is expressed as hints
// (`SystemBlock.cache`, `tool_cache_boundary`) the provider may honor or
// ignore; `thinking_display` is a reasoning-visibility hint. Anything a
// provider can't express is its own concern, not the caller's.
pub struct Request<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub thinking_display: &'a str,
    pub system: Vec<SystemBlock>,
    pub tools: Vec<serde_json::Value>,
    // Index of the last tool belonging to the stable prefix; the provider may
    // place a cache breakpoint there. None disables tool-level caching.
    pub tool_cache_boundary: Option<usize>,
    pub messages: &'a [Message],
}

pub struct Response {
    pub content: Vec<ContentBlock>,
    pub stop_reason: String,
    pub usage: Usage,
}

// The seam every model backend implements. Kept deliberately thin — with one
// provider today, a richer abstraction would just encode Anthropic's shape.
// Expect this to change when a second provider lands.
// Methods return `impl Future + Send` (not bare `async fn`) so the loop, which
// is generic over the provider and spawned onto the tokio runtime, can prove
// its future is Send. Implementors may still write `async fn`.
pub trait Provider {
    fn complete(
        &self,
        req: &Request<'_>,
    ) -> impl std::future::Future<Output = Result<Response>> + Send;
    fn count_tokens(
        &self,
        req: &Request<'_>,
    ) -> impl std::future::Future<Output = Result<u64>> + Send;
}
