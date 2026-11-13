use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

use crate::coding::context::{
    agents_md_block, collect_git_info, stable_env_block, volatile_env_block,
};
use crate::coding::file_state::FileState;
use crate::coding::mcp::{LOAD_TOOL_NAME, McpRegistry};
use crate::coding::prompt::system_prompt_body;
use crate::coding::skills::{SkillRegistry, USE_SKILL_NAME};
use crate::coding::tools;
use crate::core::agent::McpCommand;
use crate::core::session::Session;
use crate::core::{AgentConfig, AgentEvent, Backend, McpServerInfo};
use crate::llm::{ContentBlock, Message, Provider, Request, SystemBlock};

pub struct CodingBackend {
    system_prompt: String,
    stable_env: String,
    agents_md: Option<String>,
    role_preamble: Option<String>,
    cwd: PathBuf,
    mcp: McpRegistry,
    skills: SkillRegistry,
    web_search: bool,
    // Resumed sessions start with an empty tracker; see file_state.rs.
    file_state: Arc<Mutex<FileState>>,
}

impl CodingBackend {
    pub fn new(cwd: PathBuf, mcp: McpRegistry, skills: SkillRegistry, web_search: bool) -> Self {
        Self {
            // Built once so they stay byte-stable and anchor the cached prefix.
            system_prompt: system_prompt_body(web_search),
            stable_env: stable_env_block(&cwd),
            agents_md: agents_md_block(&cwd),
            role_preamble: None,
            cwd,
            mcp,
            skills,
            web_search,
            file_state: Arc::new(Mutex::new(FileState::default())),
        }
    }

    // Role by prompt, not by type: the loop stays role-free. AGENTS.md stays
    // because a subagent does real work in the repo.
    pub fn into_subagent(mut self, role_preamble: String) -> Self {
        self.role_preamble = Some(role_preamble);
        self
    }
}

impl Backend for CodingBackend {
    fn system_blocks(&self) -> Vec<SystemBlock> {
        let volatile_env = volatile_env_block(&self.cwd);
        build_system_blocks(
            &self.system_prompt,
            &self.role_preamble,
            &self.agents_md,
            &self.stable_env,
            &volatile_env,
        )
    }

    fn tool_schemas(&self) -> (Vec<serde_json::Value>, Option<usize>) {
        build_tool_array(&self.mcp, &self.skills)
    }

    fn web_search(&self) -> bool {
        self.web_search
    }

    fn tool_summary(&self, name: &str, input: &serde_json::Value) -> String {
        tools::summarize(name, input)
    }

    fn git_branch(&self) -> Option<String> {
        collect_git_info(&self.cwd).map(|g| g.branch)
    }

    fn requires_permission(&self, name: &str) -> bool {
        // load_tool always gates: connecting can spawn a subprocess or start an
        // OAuth browser flow.
        if name == LOAD_TOOL_NAME {
            true
        } else if name == USE_SKILL_NAME {
            // Only reads a user-installed file; any script it names still gates at Bash.
            false
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
        } else if name == USE_SKILL_NAME {
            let skill = input
                .get("skill")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("use_skill requires a `skill` string"))?;
            self.skills.load_body(skill)
        } else if self.mcp.is_mcp_tool(name) {
            self.mcp.call(name, input).await
        } else {
            tools::dispatch(name, input, &self.file_state).await
        }
    }

    async fn handle_mcp(&mut self, cmd: &McpCommand, notify: &mpsc::Sender<AgentEvent>) -> String {
        match cmd {
            McpCommand::Load(name) => match self.mcp.load(name, Some(notify)).await {
                Ok(n) => format!("[mcp] loaded '{name}' ({n} tools) — available next turn"),
                Err(e) => format!("[mcp] load '{name}' failed: {e:#}"),
            },
            McpCommand::Unload(name) => match self.mcp.unload(name) {
                Ok(()) => format!("[mcp] unloaded '{name}'"),
                Err(e) => format!("[mcp] unload '{name}' failed: {e:#}"),
            },
            McpCommand::List => {
                format!("[mcp] servers:\n{}", self.mcp.status_lines().join("\n"))
            }
            McpCommand::Usage => "usage: /mcp | /mcp load <name> | /mcp unload <name>".into(),
        }
    }

    fn mcp_catalog(&self) -> Vec<McpServerInfo> {
        self.mcp
            .catalog()
            .into_iter()
            .map(|(name, description, loaded)| McpServerInfo {
                name,
                description,
                loaded,
            })
            .collect()
    }
}

// New schemas join the tool array on the next inner-loop iteration, so the
// model can call them in its next response, not the current one.
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

// Cache breakpoints sit only on the two env blocks; the stable_env marker
// covers everything before it.
fn build_system_blocks(
    system_prompt: &str,
    role_preamble: &Option<String>,
    agents_md: &Option<String>,
    stable_env: &str,
    volatile_env: &str,
) -> Vec<SystemBlock> {
    let mut system = vec![SystemBlock {
        text: system_prompt.to_string(),
        cache: false,
    }];
    if let Some(role) = role_preamble {
        system.push(SystemBlock {
            text: role.clone(),
            cache: false,
        });
    }
    if let Some(cm) = agents_md {
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

// The cache breakpoint sits after the session-stable tools, so a dormant
// load/unload only touches the tail and the stable prefix stays a cache hit.
fn build_tool_array(
    mcp: &McpRegistry,
    skills: &SkillRegistry,
) -> (Vec<serde_json::Value>, Option<usize>) {
    let mut tool_schemas = tools::schemas();
    // Meta-tool catalogs are fixed at startup, so they belong in the stable prefix.
    if let Some(load_tool) = mcp.load_tool_schema() {
        tool_schemas.push(load_tool);
    }
    if let Some(use_skill) = skills.use_skill_schema() {
        tool_schemas.push(use_skill);
    }
    tool_schemas.extend(mcp.always_on_schemas());
    let cache_boundary = tool_schemas.len().checked_sub(1);
    tool_schemas.extend(mcp.dormant_schemas());
    (tool_schemas, cache_boundary)
}

// Writes to stdout, so the caller must not have taken the screen yet.
pub async fn print_preamble<P: Provider>(
    cfg: &AgentConfig,
    provider: &P,
    session: &Session,
    mcp: &McpRegistry,
    skills: &SkillRegistry,
    web_search: bool,
) -> Result<()> {
    let system_prompt = system_prompt_body(web_search);
    let stable_env = stable_env_block(&session.cwd);
    let volatile_env = volatile_env_block(&session.cwd);
    let agents_md = agents_md_block(&session.cwd);
    let system = build_system_blocks(
        &system_prompt,
        &None,
        &agents_md,
        &stable_env,
        &volatile_env,
    );
    let (tools, boundary) = build_tool_array(mcp, skills);

    // count_tokens needs a non-empty trailing user message; a 1-char probe
    // adds a negligible, constant offset that cancels in the system/tools delta.
    let probe = [Message {
        role: "user".into(),
        content: vec![ContentBlock::Text { text: ".".into() }],
    }];
    let req = |tools: Vec<serde_json::Value>, boundary: Option<usize>, web_search: bool| Request {
        model: &cfg.model,
        max_tokens: cfg.max_tokens,
        thinking_display: &cfg.thinking_display,
        system: build_system_blocks(
            &system_prompt,
            &None,
            &agents_md,
            &stable_env,
            &volatile_env,
        ),
        tools,
        tool_cache_boundary: boundary,
        web_search,
        container: None,
        messages: &probe,
    };
    let full = req(tools, boundary, web_search);
    let wire_tools = provider.wire_tools(&full).await;

    println!("===== SYSTEM =====");
    for block in &system {
        println!("{}", block.text);
        println!("\n----------");
    }
    println!("\n===== TOOLS ({}) =====", wire_tools.len());
    for t in &wire_tools {
        println!(
            "- {}: {}",
            t["name"].as_str().unwrap_or("?"),
            t["description"].as_str().unwrap_or("")
        );
    }
    println!("\n===== TOOL SCHEMAS (JSON) =====");
    println!("{}", serde_json::to_string_pretty(&wire_tools)?);

    let sys_only = provider.count_tokens(&req(Vec::new(), None, false)).await;
    let with_tools = provider.count_tokens(&full).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn into_subagent_puts_the_role_block_before_agents_md() {
        let dir = std::env::temp_dir().join(format!("nudge-backend-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "PROJECT CONVENTIONS").unwrap();

        let plain = CodingBackend::new(
            dir.clone(),
            McpRegistry::bootstrap(&[]).await,
            SkillRegistry::discover(&dir, None),
            true,
        );
        assert!(
            plain
                .system_blocks()
                .iter()
                .any(|b| b.text.contains("PROJECT CONVENTIONS")),
            "a top-level backend loads AGENTS.md"
        );

        let sub = CodingBackend::new(
            dir.clone(),
            McpRegistry::bootstrap(&[]).await,
            SkillRegistry::discover(&dir, None),
            true,
        )
        .into_subagent("ROLE: report to parent-x".into());
        let blocks = sub.system_blocks();
        assert!(
            blocks[1].text.contains("ROLE: report to parent-x"),
            "the role block sits directly after the prompt body"
        );
        assert!(
            blocks[2].text.contains("PROJECT CONVENTIONS"),
            "a subagent backend keeps AGENTS.md, after its role block"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
