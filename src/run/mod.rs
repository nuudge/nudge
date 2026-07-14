pub mod connect;
mod daemon;
mod local;

use anyhow::Result;

use crate::cli::Cli;
use crate::coding;
use crate::config::Config;
use crate::core::{self, AgentConfig, Backend, ClientIdentity};
use crate::llm;
use crate::models::{DEFAULT_MODEL, MODELS, owned_models, resolve_models};
use crate::spawn;
use crate::tui;

pub const MAX_TOKENS: u32 = 16384;
pub const MAX_ITERATIONS: usize = 25;

// The local user's identity, announced at attach. `$USER` if set, else a neutral
// default — a `--name` override can come later.
pub fn local_identity() -> ClientIdentity {
    let name = std::env::var("USER").unwrap_or_else(|_| "human".into());
    ClientIdentity::human(name)
}

// Own a session and run its agent loop in-process: shared setup (session, provider,
// MCP, skills, host), then hand off to the headless daemon or the local TUI.
pub async fn host(cli: Cli) -> Result<()> {
    let config = Config::from_env()?;
    // Cloned before the provider takes ownership below, so a spawned child can
    // build its own provider with the same key.
    let api_key = config.anthropic_api_key.clone();

    let (session, initial_messages, dropped) = match &cli.resume {
        None => (coding::open_new()?, Vec::new(), 0),
        Some(id) => {
            let r = coding::open_resume(id)?;
            (r.session, r.messages, r.dropped)
        }
    };

    let thinking_display = cli.thinking.as_display();
    let who = local_identity();
    let mut ui_cfg = tui::UiConfig {
        session_id: session.id.clone(),
        session_name: session.name.clone(),
        model: DEFAULT_MODEL.into(),
        thinking_display: thinking_display.clone(),
        // Filled in the local branch when --relay arms remote pairing.
        pairing_qr: None,
        pairing_code: None,
        // This process hosts the agent loop: it's the owner TUI (cosmetic badge only).
        is_owner: true,
        user_name: who.name.clone(),
        models: owned_models(MODELS),
    };
    let cfg = AgentConfig {
        model: DEFAULT_MODEL.into(),
        max_tokens: MAX_TOKENS,
        max_iterations: MAX_ITERATIONS,
        thinking_display,
    };
    let provider = llm::AnthropicProvider::new(config.anthropic_api_key);

    ui_cfg.models = resolve_models(&provider, MODELS).await;

    // Connect to MCP servers declared in the project-local `.mcp.json` before
    // the TUI takes the screen, so connection logs print cleanly to stderr.
    // Missing config = no servers; bad config or failed connects degrade
    // gracefully (logged, skipped) — the agent still runs with built-in tools.
    let mcp_specs = match coding::mcp::load_config(&session.cwd) {
        Ok(specs) => specs,
        Err(e) => {
            eprintln!("[mcp] config error: {e:#}");
            Vec::new()
        }
    };
    let mcp = coding::mcp::McpRegistry::bootstrap(&mcp_specs).await;
    for line in &mcp.connect_log {
        eprintln!("{line}");
    }

    // Discover Skills under ~/.nudge/skills/ (personal) and <cwd>/.nudge/skills/
    // (project) before the TUI takes the screen, so discovery — including a
    // malformed SKILL.md being skipped — prints cleanly to stderr.
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let skills = coding::skills::SkillRegistry::discover(&session.cwd, home.as_deref());
    for line in &skills.discovery_log {
        eprintln!("{line}");
    }

    if cli.print_prompt {
        return coding::print_preamble(&cfg, &provider, &session, &mcp, &skills).await;
    }

    let backend = coding::CodingBackend::new(session.cwd.clone(), mcp, skills);

    // Pre-translate the resumed transcript to controller events and seed the
    // host's replay buffer with it, so the TUI (and later a remote client)
    // rebuilds history purely from the event stream — no front-end-side JSONL
    // replay. `initial_messages` still seeds the model's conversation in the loop.
    let mut seed = coding::replay_events(&initial_messages, dropped, &who.name);
    // Prepend the initial session context so every controller has a header on its very
    // first attach, before any turn completes. The loop re-emits SessionInfo on each
    // turn boundary (and on /model) to keep it live; clients only ever render it.
    seed.insert(
        0,
        core::ControllerEvent::SessionInfo {
            model: cfg.model.clone(),
            cwd: session.cwd_display(),
            git_branch: backend.git_branch(),
            session_id: session.id.clone(),
            session_name: session.name.clone(),
        },
    );
    // The executor behind the model-facing Spawn tool: this session may create
    // subagents (which themselves may not — the factory builds children without one).
    let factory = spawn::peer_factory(api_key.clone(), session.id.clone());
    let host = core::SessionHost::spawn(
        cfg,
        provider,
        backend,
        session,
        initial_messages,
        seed,
        core::PeerWiring {
            factory: Some(factory),
            initial_peers: Default::default(),
        },
    );

    // The relay base URL for phone handoff (and the relay daemon). Optional: a plain
    // local session without it still runs and backgrounds — just no phone handoff.
    let relay = config.relay;

    if cli.daemon {
        daemon::run(host, cli.socket, relay).await
    } else {
        local::run(host, ui_cfg, who, relay).await
    }
}
