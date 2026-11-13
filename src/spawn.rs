use anyhow::Context;

use crate::coding;
use crate::core::{self, AgentConfig, ClientIdentity, ClientKind};
use crate::llm;
use crate::run::MAX_TOKENS;

// `session_id`/`task` give a peer legible provenance in the other side's transcript.
fn agent_identity(
    name: String,
    session_id: Option<String>,
    task: Option<String>,
) -> ClientIdentity {
    ClientIdentity {
        kind: ClientKind::Agent,
        name,
        session_id,
        task,
    }
}

// The returned registration carries the child's `SessionHost`, which the spawner's
// `Peer` owns as the keep-alive: reaping the peer ends the child.
pub fn peer_factory(
    api_key: String,
    parent_session_id: String,
    model: String,
    max_iterations: usize,
    web_search: bool,
) -> core::PeerFactory {
    Box::new(move |task: String, parent: core::BrokerHandle| {
        let api_key = api_key.clone();
        let parent_session_id = parent_session_id.clone();
        let model = model.clone();
        Box::pin(async move {
            let session = coding::open_new_spawned(&parent_session_id, &task)?;
            let child_id = session.id.clone();
            let cwd = session.cwd.clone();

            let short: String = child_id.chars().take(8).collect();
            let parent_short: String = parent_session_id.chars().take(8).collect();
            let child_who =
                agent_identity(format!("child-{short}"), Some(child_id), Some(task.clone()));
            let parent_who = agent_identity(
                format!("parent-{parent_short}"),
                Some(parent_session_id),
                Some(task.clone()),
            );

            // Built-ins only: no .mcp.json servers.
            let mcp = coding::mcp::McpRegistry::bootstrap(&[]).await;
            let skills = coding::skills::SkillRegistry::discover(&cwd, None);
            let backend = coding::CodingBackend::new(cwd, mcp, skills, web_search)
                .into_subagent(coding::prompt::subagent_role(&parent_who.name));
            let cfg = AgentConfig {
                model,
                max_tokens: MAX_TOKENS,
                max_iterations,
                thinking_display: "omitted".into(),
                // The only controller is the spawning agent, which renders no picker.
                models: Vec::new(),
            };
            let provider = llm::AnthropicProvider::new(api_key);

            // `attach(who)` announces the ATTACHER, and the broker stamps it on every
            // message through that controller, so a swapped side mislabels the edge.
            // The return edge is seeded at spawn so the child can message its spawner
            // from turn one; agent_peer can't answer the parent's permission prompts.
            let parent_ctrl = parent
                .attach_as(child_who.clone(), core::ClientProfile::agent_peer())
                .await
                .context("child could not attach back to its spawner")?;
            let mut child_peers = core::peer::PeerSet::default();
            let mut parent_reg = core::PeerRegistration::new(parent_ctrl, parent_who.clone());
            // Marks the return edge so the child reports up it when it stalls
            // (e.g. hits its iteration limit) — waking the spawner's model.
            parent_reg.spawner = true;
            child_peers.register(parent_reg);

            let child = core::SessionHost::spawn(
                cfg,
                provider,
                backend,
                session,
                Vec::new(),
                Vec::new(),
                core::PeerWiring {
                    factory: None,
                    initial_peers: child_peers,
                    register_rx: None,
                    // /connect-peer is human-only, and a child has no human.
                    dialer: None,
                },
            );

            // Supervisor profile: the one edge allowed to answer the child's permission
            // check-ins. Events buffer until the spawner's loop drains them.
            let child_ctrl = child
                .attach_as(parent_who.clone(), core::ClientProfile::supervisor())
                .await
                .context("could not attach to the spawned child")?;
            let _ = child_ctrl
                .ui_tx
                .send(core::UiEvent::UserMessage { text: task })
                .await;

            Ok(core::PeerRegistration {
                controller: child_ctrl,
                who: child_who,
                host: Some(child),
                // Direction of creation: the spawner steers this peer's check-ins
                // and may dismiss it. The child's return edge stays unsupervised.
                supervised: true,
                spawner: false,
            })
        })
    })
}
