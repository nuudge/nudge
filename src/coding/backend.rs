use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

use crate::coding::context::{
    claude_md_block, collect_git_info, stable_env_block, volatile_env_block,
};
use crate::coding::file_state::FileState;
use crate::coding::mcp::{LOAD_TOOL_NAME, McpRegistry};
use crate::coding::prompt::system_prompt_body;
use crate::coding::tools;
use crate::core::session::Session;
use crate::core::{AgentConfig, AgentEvent, Backend, UiEvent};
use crate::llm::{ContentBlock, Message, Provider, Request, SystemBlock};

// The coding agent: it supplies the loop (in `core`) with the system prompt,
// project context, the built-in + MCP tool surface, and tool execution. Owns
// the MCP registry and the Read-before-Edit tracker for the session.
pub struct CodingBackend {
    system_prompt: String,
    stable_env: String,
    claude_md: Option<String>,
    cwd: PathBuf,
    mcp: McpRegistry,
    // Session-scoped Read-before-Edit tracker. Resumed sessions start with an
    // empty tracker — see file_state.rs for the rationale.
    file_state: Arc<Mutex<FileState>>,
}

impl CodingBackend {
    pub fn new(cwd: PathBuf, mcp: McpRegistry) -> Self {
        Self {
            // Built once: the roster is derived from the compile-time tool set,
            // and the env/CLAUDE.md are read at session start, so these stay
            // byte-stable across the session and anchor the cached prefix.
            system_prompt: system_prompt_body(),
            stable_env: stable_env_block(&cwd),
            claude_md: claude_md_block(&cwd),
            cwd,
            mcp,
            file_state: Arc::new(Mutex::new(FileState::default())),
        }
    }
}

impl Backend for CodingBackend {
    fn system_blocks(&self) -> Vec<SystemBlock> {
        let volatile_env = volatile_env_block(&self.cwd);
        build_system_blocks(
            &self.system_prompt,
            &self.claude_md,
            &self.stable_env,
            &volatile_env,
        )
    }

    fn tool_schemas(&self) -> (Vec<serde_json::Value>, Option<usize>) {
        build_tool_array(&self.mcp)
    }

    fn tool_summary(&self, name: &str, input: &serde_json::Value) -> String {
        tools::summarize(name, input)
    }

    fn git_branch(&self) -> Option<String> {
        collect_git_info(&self.cwd).map(|g| g.branch)
    }

    fn requires_permission(&self, name: &str) -> bool {
        // MCP tools are discovered at runtime, so the static built-in classifier
        // can't see them — the registry classifies them from `readOnlyHint`
        // (read-only auto-allows; everything else gates). load_tool always gates:
        // connecting a server can spawn a subprocess or trigger an OAuth browser
        // flow, so the user approves before the model pulls a server in.
        if name == LOAD_TOOL_NAME {
            true
        } else if self.mcp.is_mcp_tool(name) {
            self.mcp.requires_permission(name)
        } else {
            tools::requires_permission(name)
        }
    }

    fn permission_summary(&self, name: &str, input: &serde_json::Value) -> String {
        if name == LOAD_TOOL_NAME {
            format!(
                "load MCP server: {}",
                input.get("server").and_then(|v| v.as_str()).unwrap_or("?")
            )
        } else {
            tools::permission_summary(name, input)
        }
    }

    async fn execute(
        &mut self,
        name: &str,
        input: &serde_json::Value,
        notify: &mpsc::Sender<AgentEvent>,
    ) -> Result<String> {
        if name == LOAD_TOOL_NAME {
            load_dormant(&mut self.mcp, input, notify).await
        } else if self.mcp.is_mcp_tool(name) {
            self.mcp.call(name, input).await
        } else {
            tools::dispatch(name, input, &self.file_state).await
        }
    }

    async fn handle_control(&mut self, ev: &UiEvent, notify: &mpsc::Sender<AgentEvent>) -> bool {
        // Mid-session MCP control (load / unload / list), reported as a Notice
        // for the transcript. Anything else is not ours to handle.
        let text = match ev {
            UiEvent::LoadServer { name } => match self.mcp.load(name, Some(notify)).await {
                Ok(n) => format!("[mcp] loaded '{name}' ({n} tools) — available next turn"),
                Err(e) => format!("[mcp] load '{name}' failed: {e:#}"),
            },
            UiEvent::UnloadServer { name } => match self.mcp.unload(name) {
                Ok(()) => format!("[mcp] unloaded '{name}'"),
                Err(e) => format!("[mcp] unload '{name}' failed: {e:#}"),
            },
            UiEvent::ListServers => {
                format!("[mcp] servers:\n{}", self.mcp.status_lines().join("\n"))
            }
            _ => return false,
        };
        let _ = notify.send(AgentEvent::Notice { text }).await;
        true
    }
}

// Execute the model-driven `load_tool` call: connect the named dormant server
// and report back so the model knows what it gained. `notify` carries any OAuth
// authorize URL out to the TUI. The newly-loaded schemas join the tool array on
// the next inner-loop iteration (it rebuilds from the registry), so the model
// can call them in its next response — not the current one.
async fn load_dormant(
    mcp: &mut McpRegistry,
    input: &serde_json::Value,
    agent_tx: &mpsc::Sender<AgentEvent>,
) -> Result<String> {
    let server = input
        .get("server")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("load_tool requires a `server` string"))?;
    let n = mcp.load(server, Some(agent_tx)).await?;
    Ok(format!(
        "Loaded MCP server '{server}' ({n} tools). Its tools are now available — call them in your next response."
    ))
}

// The `system` field: prompt body, then optional CLAUDE.md, then the stable
// and volatile env blocks. The cache breakpoints sit on the two env blocks —
// see the breakpoint-budget note at the call site for why the body/claude_md
// blocks carry none.
fn build_system_blocks(
    system_prompt: &str,
    claude_md: &Option<String>,
    stable_env: &str,
    volatile_env: &str,
) -> Vec<SystemBlock> {
    let mut system = vec![SystemBlock {
        text: system_prompt.to_string(),
        cache: false,
    }];
    if let Some(cm) = claude_md {
        system.push(SystemBlock {
            text: cm.clone(),
            cache: false,
        });
    }
    system.push(SystemBlock {
        text: stable_env.to_string(),
        cache: true,
    });
    system.push(SystemBlock {
        text: volatile_env.to_string(),
        cache: true,
    });
    system
}

// Tool array = foundational built-ins + always-on (user-specified) MCP tools,
// then a cache breakpoint, then any loaded-dormant MCP tools. The model sees
// one flat list; dispatch routes by name. The breakpoint marks the
// stable/dynamic boundary: foundational + always-on defs never change
// mid-session, so a dormant load/unload (which only touches the tail after
// this marker) keeps the whole stable prefix a cache hit instead of
// re-processing every schema.
fn build_tool_array(mcp: &McpRegistry) -> (Vec<serde_json::Value>, Option<usize>) {
    let mut tool_schemas = tools::schemas();
    // The load_tool meta-tool is foundational: it never changes mid-session
    // (the dormant catalog is compile-time-fixed), so it sits in the stable
    // prefix above the breakpoint alongside the native tools.
    if let Some(load_tool) = mcp.load_tool_schema() {
        tool_schemas.push(load_tool);
    }
    tool_schemas.extend(mcp.always_on_schemas());
    // Index of the stable/dynamic boundary: everything up to here never
    // changes mid-session, so the provider marks it as the tool cache
    // breakpoint and a later dormant load/unload touches only the tail. There
    // is always at least one foundational tool, so this is a real boundary.
    let cache_boundary = tool_schemas.len().checked_sub(1);
    tool_schemas.extend(mcp.dormant_schemas());
    (tool_schemas, cache_boundary)
}

// `--print-prompt`: reconstruct exactly the system blocks and tool schemas the
// first turn would send, print them, and report the token cost (system-only,
// tools-only, total) via the count_tokens endpoint. A one-shot inspection path
// — no TUI, no conversation. Runs against stdout, so the caller must not have
// taken the screen yet.
pub async fn print_preamble<P: Provider>(
    cfg: &AgentConfig,
    provider: &P,
    session: &Session,
    mcp: &McpRegistry,
) -> Result<()> {
    let system_prompt = system_prompt_body();
    let stable_env = stable_env_block(&session.cwd);
    let volatile_env = volatile_env_block(&session.cwd);
    let claude_md = claude_md_block(&session.cwd);
    let system = build_system_blocks(&system_prompt, &claude_md, &stable_env, &volatile_env);
    let (tools, boundary) = build_tool_array(mcp);

    println!("===== SYSTEM =====");
    for block in &system {
        println!("{}", block.text);
        println!("\n----------");
    }
    println!("\n===== TOOLS ({}) =====", tools.len());
    for t in &tools {
        println!(
            "- {}: {}",
            t["name"].as_str().unwrap_or("?"),
            t["description"].as_str().unwrap_or("")
        );
    }
    println!("\n===== TOOL SCHEMAS (JSON) =====");
    println!("{}", serde_json::to_string_pretty(&tools)?);

    // count_tokens needs a non-empty trailing user message; a 1-char probe
    // adds a negligible, constant offset that cancels in the system/tools delta.
    let probe = [Message {
        role: "user".into(),
        content: vec![ContentBlock::Text { text: ".".into() }],
    }];
    let req = |tools: Vec<serde_json::Value>, boundary: Option<usize>| Request {
        model: &cfg.model,
        max_tokens: cfg.max_tokens,
        thinking_display: &cfg.thinking_display,
        system: build_system_blocks(&system_prompt, &claude_md, &stable_env, &volatile_env),
        tools,
        tool_cache_boundary: boundary,
        messages: &probe,
    };
    let sys_only = provider.count_tokens(&req(Vec::new(), None)).await;
    let with_tools = provider.count_tokens(&req(tools, boundary)).await;
    println!("\n===== TOKENS (count_tokens; includes a 1-char probe message) =====");
    match (sys_only, with_tools) {
        (Ok(s), Ok(a)) => {
            println!("system blocks : {s}");
            println!("tool schemas  : {} (delta)", a.saturating_sub(s));
            println!("total preamble: {a}");
        }
        (s, a) => {
            println!("token count failed: system={s:?} with_tools={a:?}");
        }
    }
    Ok(())
}
