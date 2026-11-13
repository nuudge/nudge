use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

pub mod anthropic;

pub use anthropic::AnthropicProvider;

// Provider-neutral; it matches the Anthropic wire shape, but nothing above the
// provider layer may assume that.
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
    // `signature` must go back to the API unmodified to keep reasoning
    // continuity across tool calls.
    Thinking {
        thinking: String,
        signature: String,
    },
    // Opaque; round-trip unmodified.
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
    // Already run by Anthropic inside the request; never dispatched locally.
    // Round-trip unmodified — `extra` keeps fields like `caller`.
    ServerToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(flatten)]
        extra: serde_json::Map<String, serde_json::Value>,
    },
    // Opaque: `encrypted_content` must go back verbatim.
    WebSearchToolResult {
        tool_use_id: String,
        content: serde_json::Value,
        #[serde(flatten)]
        extra: serde_json::Map<String, serde_json::Value>,
    },
    WebFetchToolResult {
        tool_use_id: String,
        content: serde_json::Value,
        #[serde(flatten)]
        extra: serde_json::Map<String, serde_json::Value>,
    },
    // Kept verbatim so an unknown or malformed block never fails the whole response.
    #[serde(untagged)]
    Other(serde_json::Value),
}

pub const WEB_SEARCH_TOOL: &str = "web_search";
pub const WEB_FETCH_TOOL: &str = "web_fetch";
pub const CODE_EXECUTION_NOTICE: &str = "ran code in Anthropic's sandbox";

// Matches `code_execution`, `bash_code_execution` and `text_editor_code_execution`.
pub fn ran_code_execution(content: &[ContentBlock]) -> bool {
    content.iter().any(
        |b| matches!(b, ContentBlock::ServerToolUse { name, .. } if name.ends_with("code_execution")),
    )
}

// Display only: a cited answer arrives split into many text blocks. The
// fragments carry their own whitespace, hence no separator.
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

// Never the body: a page can be tens of thousands of tokens.
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

// Providers without prompt caching ignore `cache`.
pub struct SystemBlock {
    pub text: String,
    pub cache: bool,
}

pub struct Request<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub thinking_display: &'a str,
    pub system: Vec<SystemBlock>,
    pub tools: Vec<serde_json::Value>,
    // Last tool of the stable prefix; None disables tool-level caching.
    pub tool_cache_boundary: Option<usize>,
    pub web_search: bool,
    // Set only on a pause_turn continuation: the API rejects it on a request
    // without pending code execution.
    pub container: Option<&'a str>,
    pub messages: &'a [Message],
}

pub struct Response {
    pub content: Vec<ContentBlock>,
    pub stop_reason: String,
    pub usage: Usage,
    pub container_id: Option<String>,
    pub warning: Option<String>,
    // Repeats on every request while the cleared results stay in history.
    pub context_cleared: Option<String>,
}

// Methods return `impl Future + Send` so the generic, tokio-spawned loop can
// prove its future is Send. Implementors may still write `async fn`.
pub trait Provider {
    fn complete(
        &self,
        req: &Request<'_>,
    ) -> impl std::future::Future<Output = Result<Response>> + Send;
    fn count_tokens(
        &self,
        req: &Request<'_>,
    ) -> impl std::future::Future<Output = Result<u64>> + Send;
    // Includes provider-added tools, for `--print-prompt`.
    fn wire_tools(
        &self,
        req: &Request<'_>,
    ) -> impl std::future::Future<Output = Vec<serde_json::Value>> + Send {
        std::future::ready(req.tools.clone())
    }
}
