use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

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
    // Server-side tool call (web search or fetch, or the code execution that
    // filters their results) that Anthropic already ran inside the request. Never dispatched
    // locally; round-trip unmodified — `extra` keeps fields like `caller`.
    ServerToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(flatten)]
        extra: serde_json::Map<String, serde_json::Value>,
    },
    // Opaque: either a list of results (with `encrypted_content` the API needs
    // back verbatim) or an error object.
    WebSearchToolResult {
        tool_use_id: String,
        content: serde_json::Value,
        #[serde(flatten)]
        extra: serde_json::Map<String, serde_json::Value>,
    },
    // Opaque: the fetched document or an error object.
    WebFetchToolResult {
        tool_use_id: String,
        content: serde_json::Value,
        #[serde(flatten)]
        extra: serde_json::Map<String, serde_json::Value>,
    },
    // Any other block type (code-execution results, future server tools), kept
    // verbatim so a new type never fails the whole response. Also catches a
    // malformed known block instead of erroring.
    #[serde(untagged)]
    Other(serde_json::Value),
}

pub const WEB_SEARCH_TOOL: &str = "web_search";
pub const WEB_FETCH_TOOL: &str = "web_fetch";
pub const CODE_EXECUTION_NOTICE: &str = "ran code in Anthropic's sandbox";

// The sandbox is provisioned for dynamic filtering, but the model may run any code
// there; the tools are `code_execution`, `bash_code_execution`, `text_editor_code_execution`.
pub fn ran_code_execution(content: &[ContentBlock]) -> bool {
    content.iter().any(
        |b| matches!(b, ContentBlock::ServerToolUse { name, .. } if name.ends_with("code_execution")),
    )
}

// For display only: a cited answer arrives split into many text blocks (some
// empty, some mid-sentence or mid-table), so each run of consecutive text
// blocks is joined back into one; empty runs are dropped. The fragments carry
// their own whitespace, hence no separator.
pub fn merge_text_runs(content: &[ContentBlock]) -> Vec<Cow<'_, ContentBlock>> {
    let mut out = Vec::new();
    let mut run = String::new();
    for block in content {
        if let ContentBlock::Text { text } = block {
            run.push_str(text);
            continue;
        }
        if !run.is_empty() {
            out.push(Cow::Owned(ContentBlock::Text {
                text: std::mem::take(&mut run),
            }));
        }
        out.push(Cow::Borrowed(block));
    }
    if !run.is_empty() {
        out.push(Cow::Owned(ContentBlock::Text { text: run }));
    }
    out
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

pub fn web_fetch_url(input: &serde_json::Value) -> String {
    input
        .get("url")
        .and_then(|u| u.as_str())
        .unwrap_or_default()
        .to_string()
}

// The URL with the document's kind and size, never its body (a page can be
// tens of thousands of tokens); or the error code with `is_error` set.
pub fn web_fetch_result(content: &serde_json::Value) -> (String, bool) {
    if content["type"] != "web_fetch_result" {
        return (
            format!(
                "web fetch error: {}",
                content["error_code"].as_str().unwrap_or("unknown")
            ),
            true,
        );
    }
    let source = &content["content"]["source"];
    let data = source["data"].as_str().unwrap_or_default();
    let size = if source["type"] == "base64" {
        let kind = match source["media_type"].as_str() {
            Some("application/pdf") => "PDF",
            Some(other) => other,
            None => "binary",
        };
        format!("{kind}, {}B base64", approx(data.len()))
    } else {
        format!("text, {} chars", approx(data.chars().count()))
    };
    (
        format!(
            "fetched {} ({size})",
            content["url"].as_str().unwrap_or_default()
        ),
        false,
    )
}

fn approx(n: usize) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
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
// ignore; `thinking_display` is a reasoning-visibility hint and `web_search`
// asks for the provider's server-side search and fetch, shaped to what the model
// supports. Anything a provider can't express is its own concern, not the caller's.
pub struct Request<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub thinking_display: &'a str,
    pub system: Vec<SystemBlock>,
    pub tools: Vec<serde_json::Value>,
    // Index of the last tool belonging to the stable prefix; the provider may
    // place a cache breakpoint there. None disables tool-level caching.
    pub tool_cache_boundary: Option<usize>,
    pub web_search: bool,
    // The server-side execution container to resume; set only on a
    // pause_turn continuation (the API rejects it on a request without
    // pending code execution).
    pub container: Option<&'a str>,
    pub messages: &'a [Message],
}

pub struct Response {
    pub content: Vec<ContentBlock>,
    pub stop_reason: String,
    pub usage: Usage,
    pub container_id: Option<String>,
    // A provider-side degradation the user should know about (e.g. unknown
    // model capabilities), surfaced once by the loop.
    pub warning: Option<String>,
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
    // The tool array as it would go on the wire (provider-added tools
    // included), for `--print-prompt`.
    fn wire_tools(
        &self,
        req: &Request<'_>,
    ) -> impl std::future::Future<Output = Vec<serde_json::Value>> + Send {
        std::future::ready(req.tools.clone())
    }
}
