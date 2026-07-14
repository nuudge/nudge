use anyhow::Context;

use crate::coding;
use crate::core::{self, AgentConfig, ClientIdentity, ClientKind, SessionHandle};
use crate::llm;
use crate::models::DEFAULT_MODEL;
use crate::run::{MAX_ITERATIONS, MAX_TOKENS};

// Identity a spawned agent announces at attach — the first real use of
// `ClientKind::Agent`. `session_id`/`task` give a peer legible provenance in the other
// side's transcript and Notices.
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

// Build the executor behind the model-facing Spawn tool (`core::PeerFactory`). Each
// call stands up a built-ins-only child agent in the current directory, mutually
// attaches it to the spawner (whose broker handle the loop passes in), kicks it with
// the task, and returns the registration — including the child's `SessionHost`, which
// the spawner's `Peer` then owns as the keep-alive (reaping the peer ends the child).
// The child gets NO factory of its own, so there is no recursive spawning yet.
pub fn peer_factory(api_key: String, parent_session_id: String) -> core::PeerFactory {
    Box::new(move |task: String, parent: core::BrokerHandle| {
        let api_key = api_key.clone();
        let parent_session_id = parent_session_id.clone();
        Box::pin(async move {
            let session = coding::open_new()?;
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

            // Built-ins only: no .mcp.json servers. Skills are still discovered
            // locally (cheap). `into_subagent` installs the role contract (report back
            // to the spawner via MessagePeer) and drops CLAUDE.md — a subagent runs
            // under its spawner's contract, not the repo's human conventions.
            let mcp = coding::mcp::McpRegistry::bootstrap(&[]).await;
            let skills = coding::skills::SkillRegistry::discover(&cwd, None);
            let backend = coding::CodingBackend::new(cwd, mcp, skills)
                .into_subagent(coding::prompt::subagent_role(&parent_who.name));
            let cfg = AgentConfig {
                model: DEFAULT_MODEL.into(),
                max_tokens: MAX_TOKENS,
                max_iterations: MAX_ITERATIONS,
                thinking_display: "omitted".into(),
            };
            let provider = llm::AnthropicProvider::new(api_key);

            // Two edges, and on each one `attach(who)` announces the ATTACHER — the
            // broker stamps that identity on everything sent through the controller,
            // so getting a side wrong mislabels every message on that edge (the
            // child would see its own name on the parent's messages). The
            // registrations are the mirror: each PeerSet records whom that side
            // HOLDS.
            //
            // Return edge (child → parent), seeded into the child's PeerSet at spawn
            // (not raced through a runtime registration), so the child can address
            // its spawner via MessagePeer from its very first turn. The child is the
            // attacher here, so it announces child_who.
            let parent_ctrl = parent
                .attach(child_who.clone())
                .await
                .context("child could not attach back to its spawner")?;
            let mut child_peers = core::peer::PeerSet::default();
            child_peers.register(core::PeerRegistration::new(parent_ctrl, parent_who.clone()));

            let child = core::SessionHost::spawn(
                cfg,
                provider,
                backend,
                session,
                Vec::new(),
                Vec::new(),
                core::PeerWiring {
                    // No factory: a child may not spawn its own subagents yet.
                    factory: None,
                    initial_peers: child_peers,
                },
            );

            // Spawner's edge (parent → child): the parent is the attacher, so it
            // announces parent_who; then it kicks the child with the task. Events
            // buffer until the spawner's loop drains them.
            let child_ctrl = child
                .attach(parent_who.clone())
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
            })
        })
    })
}
