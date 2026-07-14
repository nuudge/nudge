use anyhow::Result;
use serde_json::Value;
use std::future::Future;
use tokio::sync::mpsc;

use super::events::{AgentEvent, ControllerEvent, UiEvent};
use super::host::BrokerHandle;
use super::identity::{ClientIdentity, ClientKind};
use super::peer::{PeerFactory, PeerId, PeerRegistration, PeerSet};
use crate::core::session::Session;
use crate::llm::{ContentBlock, Message, Provider, Request, SystemBlock};

mod dispatch;
mod naming;
mod peer_tools;

use dispatch::dispatch_tools;
use naming::{fallback_name, short_id, title_from_response, title_prompt};

pub struct AgentConfig {
    pub model: String,
    pub max_tokens: u32,
    pub max_iterations: usize,
    // "summarized" (default) shows thinking text; "omitted" sends signature only
    // (faster TTFT). Cost is identical — this only changes wire-level visibility.
    pub thinking_display: String,
}

// Everything the loop needs from the concrete agent (tools, prompt/context,
// control handling). The coding agent implements this; the loop stays unaware
// of tool implementations, MCP, CLAUDE.md, or any cwd-specific concern — that
// is what keeps this module from depending on the layer above it.
pub trait Backend {
    // Rebuilt each turn so volatile context (env, git, dir listing) stays fresh.
    fn system_blocks(&self) -> Vec<SystemBlock>;
    // Tool schemas, plus the stable/dynamic cache-boundary index (None = no caching).
    fn tool_schemas(&self) -> (Vec<Value>, Option<usize>);
    fn tool_summary(&self, name: &str, input: &Value) -> String;
    fn requires_permission(&self, name: &str) -> bool;
    fn permission_summary(&self, name: &str, input: &Value) -> String;
    // May emit events (e.g. an OAuth URL) through `notify`.
    fn execute(
        &mut self,
        name: &str,
        input: &Value,
        notify: &mpsc::Sender<AgentEvent>,
    ) -> impl Future<Output = Result<String>> + Send;
    // A control event the loop doesn't own (e.g. MCP load/unload/list); true if consumed.
    fn handle_control(
        &mut self,
        ev: &UiEvent,
        notify: &mpsc::Sender<AgentEvent>,
    ) -> impl Future<Output = bool> + Send;
    // Re-read each turn boundary so a mid-session `git checkout` reaches the header.
    fn git_branch(&self) -> Option<String> {
        None
    }
}

// One inbound event plus the identity of the client that sent it. The broker stamps
// the identity from its registry (the attach handshake) — a client never claims its
// own — so the loop can trust it when attributing a peer's message in the transcript.
// `None` marks broker-internal sends (e.g. the final Quit), which need no sender.
pub type LoopInput = (Option<ClientIdentity>, UiEvent);

// The loop's I/O, bundled so the signature stays readable as it grows. `ui_rx` /
// `agent_tx` are the inbound-drive / outbound-event halves (who drives me, what I
// emit); `peers` is the set of agents I'm a *client* of (whom I drive/observe), and
// `peer_register_rx` delivers peers handed to me at runtime (e.g. a spawned child).
// A top-level session has an empty `PeerSet` and a registrar that never fires, so the
// two peer-related select arms are inert and behavior is byte-for-byte unchanged.
pub struct AgentIo {
    pub ui_rx: mpsc::Receiver<LoopInput>,
    pub agent_tx: mpsc::Sender<AgentEvent>,
    pub peers: PeerSet,
    pub peer_register_rx: Option<mpsc::UnboundedReceiver<PeerRegistration>>,
    // Executor behind the model-facing Spawn tool (None = this agent may not spawn);
    // `self_handle` reaches this agent's OWN broker, handed to the factory so a
    // spawned child can attach back — the return edge.
    pub peer_factory: Option<PeerFactory>,
    pub self_handle: BrokerHandle,
}

pub async fn run_agent<P: Provider, B: Backend>(
    mut cfg: AgentConfig,
    provider: P,
    mut backend: B,
    mut session: Session,
    initial_messages: Vec<Message>,
    io: AgentIo,
) -> Result<()> {
    let AgentIo {
        mut ui_rx,
        agent_tx,
        mut peers,
        mut peer_register_rx,
        peer_factory,
        self_handle,
    } = io;
    let mut messages: Vec<Message> = initial_messages;
    // Index of `messages` after the last completed turn. On API error mid-turn we roll
    // back here so the next turn lands on a valid alternating-role boundary (the API
    // rejects consecutive user turns, or a tool_use turn missing its tool_results).
    let mut last_good_snapshot: usize = messages.len();

    // Last (model, git_branch, name) emitted as SessionInfo, so we only re-emit on an
    // actual change (a /model switch, a `git checkout`, a /session-rename).
    let mut last_session_ctx = (
        cfg.model.clone(),
        backend.git_branch(),
        session.name.clone(),
    );

    // OUTER loop: one iteration per user turn.
    loop {
        let user_text = loop {
            tokio::select! {
                ui = ui_rx.recv() => match ui {
                Some((who, UiEvent::UserMessage { text })) => break attribute(who.as_ref(), text),
                Some((_, UiEvent::SetModel { model })) => {
                    cfg.model = model;
                    emit_session_info_if_changed(
                        &agent_tx,
                        &cfg.model,
                        backend.git_branch(),
                        &session,
                        &mut last_session_ctx,
                    )
                    .await;
                }
                Some((_, UiEvent::RenameSession { name: requested })) => {
                    let branch = backend.git_branch();
                    let name = match requested {
                        // Tier 1: explicit name, used verbatim (trimmed).
                        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
                        // Tier 2: inside a git repo, branch + a short id so two
                        // sessions on the same branch don't share a label.
                        _ => match &branch {
                            Some(b) => format!("{}-{}", b, short_id(&session.id)),
                            // Tier 3: no repo — ask the model for a title. Awaited
                            // inline (not via a `&P`-capturing helper) to keep the loop
                            // future `Send`.
                            None => match title_prompt(&messages) {
                                Some(prompt) => {
                                    let probe = [Message {
                                        role: "user".into(),
                                        content: vec![ContentBlock::Text { text: prompt }],
                                    }];
                                    let req = Request {
                                        model: &cfg.model,
                                        max_tokens: 1024,
                                        thinking_display: "omitted",
                                        system: Vec::new(),
                                        tools: Vec::new(),
                                        tool_cache_boundary: None,
                                        messages: &probe,
                                    };
                                    provider
                                        .complete(&req)
                                        .await
                                        .ok()
                                        .and_then(|r| title_from_response(&r))
                                        .unwrap_or_else(|| fallback_name(&session))
                                }
                                None => fallback_name(&session),
                            },
                        },
                    };
                    finalize_rename(
                        name,
                        branch,
                        &cfg,
                        &mut session,
                        &agent_tx,
                        &mut last_session_ctx,
                    )
                    .await;
                }
                Some((_, UiEvent::Quit)) | None => return Ok(()),
                Some((_, ev)) => {
                    backend.handle_control(&ev, &agent_tx).await;
                }
                },
                reg = recv_registration(&mut peer_register_rx) => match reg {
                    Some(reg) => {
                        peers.register(reg);
                    }
                    None => peer_register_rx = None,
                },
                (pid, ev) = peers.recv() => {
                    supervise_peer_event(&mut peers, &agent_tx, pid, ev).await;
                }
            }
        };

        messages.push(Message {
            role: "user".into(),
            content: vec![ContentBlock::Text { text: user_text }],
        });
        session.log(messages.last().unwrap()).await?;

        // INNER loop: model + tool turns until non-tool-use stop.
        for iteration in 0..cfg.max_iterations {
            let (mut tools, tool_cache_boundary) = backend.tool_schemas();
            // Loop-level peer tools ride after the backend's array (never inside the
            // cached stable prefix); offered per capability (factory / held peers).
            tools.extend(peer_tools::schemas(&peers, &peer_factory));
            let req = Request {
                model: &cfg.model,
                max_tokens: cfg.max_tokens,
                thinking_display: &cfg.thinking_display,
                system: backend.system_blocks(),
                tools,
                tool_cache_boundary,
                messages: &messages,
            };

            let resp = match provider.complete(&req).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = agent_tx
                        .send(AgentEvent::Error {
                            message: format!("{e:#}"),
                        })
                        .await;
                    messages.truncate(last_good_snapshot);
                    break;
                }
            };

            let _ = agent_tx
                .send(AgentEvent::Usage {
                    in_tokens: resp.usage.input_tokens,
                    out_tokens: resp.usage.output_tokens,
                    cache_write: resp.usage.cache_creation_input_tokens,
                    cache_read: resp.usage.cache_read_input_tokens,
                })
                .await;

            let assistant_msg = Message {
                role: "assistant".into(),
                content: resp.content,
            };
            session.log(&assistant_msg).await?;

            for block in &assistant_msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        let _ = agent_tx
                            .send(AgentEvent::AssistantText { text: text.clone() })
                            .await;
                    }
                    ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                        let _ = agent_tx
                            .send(AgentEvent::AssistantThinking {
                                text: thinking.clone(),
                            })
                            .await;
                    }
                    _ => {}
                }
            }

            if resp.stop_reason != "tool_use" {
                messages.push(assistant_msg);
                last_good_snapshot = messages.len();
                let _ = agent_tx.send(AgentEvent::TurnComplete).await;
                emit_session_info_if_changed(
                    &agent_tx,
                    &cfg.model,
                    backend.git_branch(),
                    &session,
                    &mut last_session_ctx,
                )
                .await;
                break;
            }

            let (mut tool_results, denied) = dispatch_tools(
                &assistant_msg,
                &agent_tx,
                &mut backend,
                &mut peers,
                &peer_factory,
                &self_handle,
            )
            .await;
            messages.push(assistant_msg);

            // After a denial, pause for fresh user guidance that rides along in the
            // same tool_results turn, so the model sees "denied — try this" in one step.
            if denied {
                let _ = agent_tx.send(AgentEvent::TurnComplete).await;
                emit_session_info_if_changed(
                    &agent_tx,
                    &cfg.model,
                    backend.git_branch(),
                    &session,
                    &mut last_session_ctx,
                )
                .await;
                loop {
                    match ui_rx.recv().await {
                        Some((who, UiEvent::UserMessage { text })) => {
                            tool_results.push(ContentBlock::Text {
                                text: attribute(who.as_ref(), text),
                            });
                            break;
                        }
                        Some((_, UiEvent::SetModel { model })) => {
                            cfg.model = model;
                            emit_session_info_if_changed(
                                &agent_tx,
                                &cfg.model,
                                backend.git_branch(),
                                &session,
                                &mut last_session_ctx,
                            )
                            .await;
                        }
                        Some((_, UiEvent::Quit)) | None => return Ok(()),
                        Some((_, ev)) => {
                            backend.handle_control(&ev, &agent_tx).await;
                        }
                    }
                }
            }

            let user_msg = Message {
                role: "user".into(),
                content: tool_results,
            };
            session.log(&user_msg).await?;
            messages.push(user_msg);

            if iteration == cfg.max_iterations - 1 {
                // Close the maxed-out turn on a valid alternating-role boundary —
                // otherwise it ends on user(tool_results) and the next user turn would
                // be two consecutive user turns, which the API rejects.
                let notice = format!(
                    "I've reached the iteration limit ({}) for this turn. The work above is partial. Tell me how you'd like to proceed, or reply 'continue' to resume.",
                    cfg.max_iterations
                );
                let synthetic = Message {
                    role: "assistant".into(),
                    content: vec![ContentBlock::Text {
                        text: notice.clone(),
                    }],
                };
                session.log(&synthetic).await?;
                let _ = agent_tx
                    .send(AgentEvent::AssistantText { text: notice })
                    .await;
                messages.push(synthetic);
                last_good_snapshot = messages.len();
                let _ = agent_tx.send(AgentEvent::MaxIterations).await;
                emit_session_info_if_changed(
                    &agent_tx,
                    &cfg.model,
                    backend.git_branch(),
                    &session,
                    &mut last_session_ctx,
                )
                .await;
            }
        }
    }
}

// Emit SessionInfo only when the (model, branch, name) tuple changed, so headers track
// the daemon without an identical event flooding the replay buffer each turn. `branch`
// is read by the caller *before* the await — holding `&Backend` across it would force a
// `B: Sync` bound on the whole loop.
async fn emit_session_info_if_changed(
    tx: &mpsc::Sender<AgentEvent>,
    model: &str,
    branch: Option<String>,
    session: &Session,
    last: &mut (String, Option<String>, Option<String>),
) {
    let current = (model.to_string(), branch, session.name.clone());
    if current == *last {
        return;
    }
    *last = current.clone();
    let _ = tx
        .send(AgentEvent::SessionInfo {
            model: current.0,
            cwd: session.cwd_display(),
            git_branch: current.1,
            session_id: session.id.clone(),
            session_name: current.2,
        })
        .await;
}

// Persist a resolved label and broadcast it: a Notice (so the user — and the TUI, which
// can't know a daemon-derived name synchronously — sees the final label) plus a re-emit
// of SessionInfo through the change-detecting helper.
async fn finalize_rename(
    name: String,
    branch: Option<String>,
    cfg: &AgentConfig,
    session: &mut Session,
    tx: &mpsc::Sender<AgentEvent>,
    last_ctx: &mut (String, Option<String>, Option<String>),
) {
    match session.set_name(name.clone(), branch.clone()) {
        Ok(()) => {
            let _ = tx
                .send(AgentEvent::Notice {
                    text: format!("session renamed to '{name}'"),
                })
                .await;
            emit_session_info_if_changed(tx, &cfg.model, branch, session, last_ctx).await;
        }
        Err(e) => {
            let _ = tx
                .send(AgentEvent::Error {
                    message: format!("rename failed: {e:#}"),
                })
                .await;
        }
    }
}

// Await the next peer handed in at runtime; pend forever once the registrar is gone
// (or was never wired), so the loop's select arm stays quiet for a peerless session.
async fn recv_registration(
    rx: &mut Option<mpsc::UnboundedReceiver<PeerRegistration>>,
) -> Option<PeerRegistration> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

// Fold the broker-stamped sender into the text that enters the transcript: an agent
// peer's message is named, so the model knows which peer spoke; a human's message
// stays bare, exactly as it always has. Keep the prefix format stable — the model
// learns to reference it.
fn attribute(who: Option<&ClientIdentity>, text: String) -> String {
    match who {
        Some(w) if w.kind == ClientKind::Agent => {
            format!("[message from peer {}]\n{text}", w.name)
        }
        _ => text,
    }
}

// Handle one event observed from a peer this agent drives. Step-3 supervision is
// deliberately minimal: activity surfaces to this agent's own front-end as a Notice
// (the watch substrate), and a permission check-in is auto-approved so the peer stays
// unblocked.
//
// PLACEHOLDER (step 5 replaces): the auto-approve becomes a real steering turn — the
// parent decides the peer's check-in with its full transcript in context (and gains
// escalation-to-human). For now it always allows, which is why the spawn path stays
// gated behind the hidden --spawn-demo flag.
async fn supervise_peer_event(
    peers: &mut PeerSet,
    agent_tx: &mpsc::Sender<AgentEvent>,
    pid: PeerId,
    ev: Option<ControllerEvent>,
) {
    let name = peers.display_name(pid);
    match ev {
        None => {
            peers.remove(pid);
            let _ = agent_tx
                .send(AgentEvent::Notice {
                    text: format!("[peer {name}] disconnected"),
                })
                .await;
        }
        Some(ControllerEvent::PermissionRequest {
            tool_use_id,
            tool_name,
            summary,
        }) => {
            let _ = agent_tx
                .send(AgentEvent::Notice {
                    text: format!("[peer {name}] auto-approved {tool_name}: {summary}"),
                })
                .await;
            peers
                .drive(
                    pid,
                    UiEvent::PermissionResponse {
                        tool_use_id,
                        allow: true,
                    },
                )
                .await;
        }
        Some(other) => {
            if let Some(text) = peer_notice(&name, &other) {
                let _ = agent_tx.send(AgentEvent::Notice { text }).await;
            }
        }
    }
}

// Map a peer's observed event to a one-line Notice for this agent's front-end, or None
// for the noisy/internal events (usage, session info, turn markers) that add no watch
// value.
fn peer_notice(name: &str, ev: &ControllerEvent) -> Option<String> {
    match ev {
        ControllerEvent::AssistantText { text } => Some(format!("[peer {name}] {text}")),
        ControllerEvent::ToolUseStart {
            name: tool,
            summary,
            ..
        } => Some(format!("[peer {name}] uses {tool}: {summary}")),
        ControllerEvent::PermissionResolved { tool_name, allow } => Some(format!(
            "[peer {name}] {} {tool_name}",
            if *allow { "allowed" } else { "denied" }
        )),
        // A peer's own Notices are deliberately NOT re-narrated. Under mutual attach I am
        // an attached controller of my peer, so a Notice I emit about it is fanned back
        // to it; if it re-narrated my Notices (and I its), one event would amplify into an
        // unbounded `[peer a] [peer b] [peer a] …` cascade. Every re-narration is itself a
        // Notice, so refusing to re-narrate Notices breaks the cycle at one hop. (The real
        // fix is identity-aware fan-out that never routes supervision chatter to peer
        // agents — deferred; see docs/symmetric-communication.md open questions.)
        ControllerEvent::Notice { .. } => None,
        ControllerEvent::Error { message } => Some(format!("[peer {name}] error: {message}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::host::Controller;
    use crate::core::identity::{ClientIdentity, ClientKind};
    use crate::core::{SessionHandle, SessionHost};
    use crate::llm::{Response, Usage};
    use std::path::PathBuf;

    // A provider that always closes the turn with a one-line assistant reply — enough
    // to prove a turn ran without touching the network.
    struct FakeProvider;
    impl Provider for FakeProvider {
        async fn complete(&self, _req: &Request<'_>) -> Result<Response> {
            Ok(Response {
                content: vec![ContentBlock::Text { text: "ok".into() }],
                stop_reason: "end_turn".into(),
                usage: Usage::default(),
            })
        }
        async fn count_tokens(&self, _req: &Request<'_>) -> Result<u64> {
            Ok(0)
        }
    }

    // Like FakeProvider, but records the transcript of every request so tests can
    // assert exactly what entered the model's context (e.g. attribution prefixes).
    struct RecordingProvider {
        seen: std::sync::Arc<std::sync::Mutex<Vec<Message>>>,
    }
    impl Provider for RecordingProvider {
        async fn complete(&self, req: &Request<'_>) -> Result<Response> {
            *self.seen.lock().unwrap() = req.messages.to_vec();
            Ok(Response {
                content: vec![ContentBlock::Text { text: "ok".into() }],
                stop_reason: "end_turn".into(),
                usage: Usage::default(),
            })
        }
        async fn count_tokens(&self, _req: &Request<'_>) -> Result<u64> {
            Ok(0)
        }
    }

    // Plays a fixed sequence of responses (e.g. a tool_use turn then an end_turn),
    // recording each request's transcript and tool names — how tests script the model
    // calling a loop-level peer tool and then assert what came back to it.
    struct ScriptedProvider {
        responses: std::sync::Mutex<Vec<Response>>,
        seen_messages: std::sync::Arc<std::sync::Mutex<Vec<Message>>>,
        seen_tools: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }
    impl Provider for ScriptedProvider {
        async fn complete(&self, req: &Request<'_>) -> Result<Response> {
            *self.seen_messages.lock().unwrap() = req.messages.to_vec();
            *self.seen_tools.lock().unwrap() = req
                .tools
                .iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str).map(String::from))
                .collect();
            Ok(self.responses.lock().unwrap().remove(0))
        }
        async fn count_tokens(&self, _req: &Request<'_>) -> Result<u64> {
            Ok(0)
        }
    }

    fn tool_use_response(name: &str, input: Value) -> Response {
        Response {
            content: vec![ContentBlock::ToolUse {
                id: "tu1".into(),
                name: name.into(),
                input,
            }],
            stop_reason: "tool_use".into(),
            usage: Usage::default(),
        }
    }

    fn end_turn_response(text: &str) -> Response {
        Response {
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: "end_turn".into(),
            usage: Usage::default(),
        }
    }

    // A no-tool backend: the loop needs a Backend, but these tests exercise input
    // routing (peers, drive edges), not tool dispatch.
    struct FakeBackend;
    impl Backend for FakeBackend {
        fn system_blocks(&self) -> Vec<SystemBlock> {
            Vec::new()
        }
        fn tool_schemas(&self) -> (Vec<Value>, Option<usize>) {
            (Vec::new(), None)
        }
        fn tool_summary(&self, _name: &str, _input: &Value) -> String {
            String::new()
        }
        fn requires_permission(&self, _name: &str) -> bool {
            false
        }
        fn permission_summary(&self, _name: &str, _input: &Value) -> String {
            String::new()
        }
        async fn execute(
            &mut self,
            _name: &str,
            _input: &Value,
            _notify: &mpsc::Sender<AgentEvent>,
        ) -> Result<String> {
            Ok(String::new())
        }
        async fn handle_control(
            &mut self,
            _ev: &UiEvent,
            _notify: &mpsc::Sender<AgentEvent>,
        ) -> bool {
            false
        }
    }

    fn mk_cfg() -> AgentConfig {
        AgentConfig {
            model: "fake-model".into(),
            max_tokens: 64,
            max_iterations: 4,
            thinking_display: "omitted".into(),
        }
    }

    fn mk_session() -> (Session, PathBuf) {
        let dir = std::env::temp_dir().join(format!("nudge-agent-{}", uuid::Uuid::new_v4()));
        let session = Session::create(dir.clone(), dir.clone()).unwrap();
        (session, dir)
    }

    fn agent_who(name: &str) -> ClientIdentity {
        ClientIdentity {
            kind: ClientKind::Agent,
            name: name.into(),
            session_id: None,
            task: None,
        }
    }

    // AgentIo for a direct-run loop test: no spawn factory, and a self_handle from a
    // bare broker (only ever exercised by the Spawn tool).
    fn mk_io(
        ui_rx: mpsc::Receiver<LoopInput>,
        agent_tx: mpsc::Sender<AgentEvent>,
        peers: PeerSet,
        peer_register_rx: Option<mpsc::UnboundedReceiver<PeerRegistration>>,
    ) -> AgentIo {
        AgentIo {
            ui_rx,
            agent_tx,
            peers,
            peer_register_rx,
            peer_factory: None,
            self_handle: crate::core::host::spawn_bare_broker(Vec::new()).handle,
        }
    }

    // A synthetic peer: the `Controller` the loop holds, plus the far ends a test uses
    // to inject the peer's events and observe what the loop drives back to it.
    fn fake_peer() -> (
        Controller,
        mpsc::UnboundedSender<ControllerEvent>,
        mpsc::Receiver<UiEvent>,
    ) {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (ui_tx, ui_rx) = mpsc::channel(16);
        (
            Controller {
                events: ev_rx,
                ui_tx,
            },
            ev_tx,
            ui_rx,
        )
    }

    // A peer attached at runtime drives the loop with no user message: its activity
    // surfaces to this agent's own front-end as a Notice (the watch substrate).
    #[tokio::test]
    async fn peer_activity_surfaces_as_a_notice() {
        let (session, dir) = mk_session();
        let (ui_tx, ui_rx) = mpsc::channel(16);
        let (agent_tx, mut agent_rx) = mpsc::channel(16);
        let (reg_tx, reg_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_agent(
            mk_cfg(),
            FakeProvider,
            FakeBackend,
            session,
            Vec::new(),
            mk_io(ui_rx, agent_tx, PeerSet::default(), Some(reg_rx)),
        ));

        let (peer_ctrl, peer_ev, _peer_ui) = fake_peer();
        reg_tx
            .send(PeerRegistration::new(peer_ctrl, agent_who("child-1")))
            .unwrap();
        peer_ev
            .send(ControllerEvent::AssistantText {
                text: "peer working".into(),
            })
            .unwrap();

        let mut saw = None;
        while let Some(ev) = agent_rx.recv().await {
            if let AgentEvent::Notice { text } = ev {
                saw = Some(text);
                break;
            }
        }
        let text = saw.expect("expected a peer Notice");
        assert!(
            text.contains("child-1"),
            "notice should name the peer: {text}"
        );
        assert!(
            text.contains("peer working"),
            "notice should carry the activity: {text}"
        );

        ui_tx.send((None, UiEvent::Quit)).await.unwrap();
        task.await.unwrap().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    // A peer's permission check-in is answered (step-3 placeholder auto-approve): the
    // loop drives a PermissionResponse back up the peer's ui_tx — the parent→peer drive
    // edge.
    #[tokio::test]
    async fn peer_permission_check_in_is_auto_approved() {
        let (session, dir) = mk_session();
        let (ui_tx, ui_rx) = mpsc::channel(16);
        let (agent_tx, _agent_rx) = mpsc::channel(16);
        let (reg_tx, reg_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_agent(
            mk_cfg(),
            FakeProvider,
            FakeBackend,
            session,
            Vec::new(),
            mk_io(ui_rx, agent_tx, PeerSet::default(), Some(reg_rx)),
        ));

        let (peer_ctrl, peer_ev, mut peer_ui) = fake_peer();
        reg_tx
            .send(PeerRegistration::new(peer_ctrl, agent_who("child-1")))
            .unwrap();
        peer_ev
            .send(ControllerEvent::PermissionRequest {
                tool_use_id: "t1".into(),
                tool_name: "Bash".into(),
                summary: "run ls".into(),
            })
            .unwrap();

        match peer_ui.recv().await {
            Some(UiEvent::PermissionResponse { tool_use_id, allow }) => {
                assert_eq!(tool_use_id, "t1");
                assert!(allow);
            }
            other => panic!("expected a PermissionResponse driven to the peer, got {other:?}"),
        }

        ui_tx.send((None, UiEvent::Quit)).await.unwrap();
        task.await.unwrap().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    // A peerless session is unchanged: a user message on ui_rx drives one turn.
    #[tokio::test]
    async fn childless_session_still_drives_a_turn() {
        let (session, dir) = mk_session();
        let (ui_tx, ui_rx) = mpsc::channel(16);
        let (agent_tx, mut agent_rx) = mpsc::channel(16);
        let task = tokio::spawn(run_agent(
            mk_cfg(),
            FakeProvider,
            FakeBackend,
            session,
            Vec::new(),
            mk_io(ui_rx, agent_tx, PeerSet::default(), None),
        ));

        ui_tx
            .send((None, UiEvent::UserMessage { text: "hi".into() }))
            .await
            .unwrap();

        let mut saw_text = false;
        while let Some(ev) = agent_rx.recv().await {
            match ev {
                AgentEvent::AssistantText { text } => {
                    assert_eq!(text, "ok");
                    saw_text = true;
                }
                AgentEvent::TurnComplete => break,
                _ => {}
            }
        }
        assert!(saw_text, "expected the assistant reply for the driven turn");

        ui_tx.send((None, UiEvent::Quit)).await.unwrap();
        task.await.unwrap().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    // The return edge: an agent holding a `Controller` to a peer (obtained by the same
    // `attach` a human uses, but announcing `ClientKind::Agent`) drives that peer and
    // observes its reply — end-to-end through the peer's broker + loop. This is what
    // makes a spawned pair symmetric: the child reaches the parent exactly as the parent
    // reaches the child.
    #[tokio::test]
    async fn return_edge_peer_drives_the_agent_it_holds() {
        let (session, dir) = mk_session();
        let parent = SessionHost::spawn(
            mk_cfg(),
            FakeProvider,
            FakeBackend,
            session,
            Vec::new(),
            Vec::new(),
            crate::core::peer::PeerWiring::default(),
        );

        // The "child" attaches to the parent as an agent — this is the return edge.
        let mut child_ctrl = parent.attach(agent_who("child-1")).await.unwrap();

        child_ctrl
            .ui_tx
            .send(UiEvent::UserMessage {
                text: "from child".into(),
            })
            .await
            .unwrap();

        let mut saw_reply = false;
        while let Some(ev) = child_ctrl.events.recv().await {
            if let ControllerEvent::AssistantText { text } = ev {
                assert_eq!(text, "ok");
                saw_reply = true;
                break;
            }
        }
        assert!(
            saw_reply,
            "the agent should take a turn driven over the return edge"
        );

        parent.shutdown().await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    // Guards against the mutual-attach amplification cascade: a peer's own Notice must
    // never be re-narrated (every re-narration is a Notice, so re-narrating them would
    // loop unboundedly under mutual attach), while genuine primary activity still is.
    #[test]
    fn peer_notice_does_not_renarrate_a_peer_notice() {
        assert_eq!(
            peer_notice(
                "child-1",
                &ControllerEvent::Notice {
                    text: "[peer parent] anything".into()
                }
            ),
            None,
        );
        assert_eq!(
            peer_notice(
                "child-1",
                &ControllerEvent::AssistantText {
                    text: "hello".into()
                }
            ),
            Some("[peer child-1] hello".to_string()),
        );
    }

    // A broker-stamped agent sender folds into the transcript *named*, so the model
    // knows which peer spoke.
    #[tokio::test]
    async fn peer_message_is_attributed_in_the_transcript() {
        let (session, dir) = mk_session();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (ui_tx, ui_rx) = mpsc::channel(16);
        let (agent_tx, mut agent_rx) = mpsc::channel(16);
        let task = tokio::spawn(run_agent(
            mk_cfg(),
            RecordingProvider { seen: seen.clone() },
            FakeBackend,
            session,
            Vec::new(),
            mk_io(ui_rx, agent_tx, PeerSet::default(), None),
        ));

        ui_tx
            .send((
                Some(agent_who("child-1")),
                UiEvent::UserMessage {
                    text: "task done".into(),
                },
            ))
            .await
            .unwrap();
        while let Some(ev) = agent_rx.recv().await {
            if matches!(ev, AgentEvent::TurnComplete) {
                break;
            }
        }

        let transcript = seen.lock().unwrap().clone();
        match &transcript[0].content[0] {
            ContentBlock::Text { text } => {
                assert_eq!(text, "[message from peer child-1]\ntask done")
            }
            other => panic!("expected the attributed user turn, got {other:?}"),
        }

        ui_tx.send((None, UiEvent::Quit)).await.unwrap();
        task.await.unwrap().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    // A human sender's message enters the transcript bare — exactly today's behavior.
    #[tokio::test]
    async fn human_message_stays_bare_in_the_transcript() {
        let (session, dir) = mk_session();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (ui_tx, ui_rx) = mpsc::channel(16);
        let (agent_tx, mut agent_rx) = mpsc::channel(16);
        let task = tokio::spawn(run_agent(
            mk_cfg(),
            RecordingProvider { seen: seen.clone() },
            FakeBackend,
            session,
            Vec::new(),
            mk_io(ui_rx, agent_tx, PeerSet::default(), None),
        ));

        ui_tx
            .send((
                Some(ClientIdentity::human("alice")),
                UiEvent::UserMessage { text: "hi".into() },
            ))
            .await
            .unwrap();
        while let Some(ev) = agent_rx.recv().await {
            if matches!(ev, AgentEvent::TurnComplete) {
                break;
            }
        }

        let transcript = seen.lock().unwrap().clone();
        match &transcript[0].content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hi"),
            other => panic!("expected the bare user turn, got {other:?}"),
        }

        ui_tx.send((None, UiEvent::Quit)).await.unwrap();
        task.await.unwrap().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    // The model calls MessagePeer → the named peer's ui_rx receives the message (the
    // exact human input path on its side), and the ok tool_result lands in the caller's
    // transcript. The MessagePeer schema is only offered because a peer is held.
    #[tokio::test]
    async fn message_peer_tool_drives_the_named_peer() {
        let (session, dir) = mk_session();
        let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_tools = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = ScriptedProvider {
            responses: std::sync::Mutex::new(vec![
                tool_use_response(
                    "MessagePeer",
                    serde_json::json!({"peer": "child-1", "message": "do X"}),
                ),
                end_turn_response("done"),
            ]),
            seen_messages: seen_messages.clone(),
            seen_tools: seen_tools.clone(),
        };

        let mut peers = PeerSet::default();
        let (peer_ctrl, _peer_ev, mut peer_ui) = fake_peer();
        peers.register(PeerRegistration::new(peer_ctrl, agent_who("child-1")));

        let (ui_tx, ui_rx) = mpsc::channel(16);
        let (agent_tx, mut agent_rx) = mpsc::channel(16);
        let task = tokio::spawn(run_agent(
            mk_cfg(),
            provider,
            FakeBackend,
            session,
            Vec::new(),
            mk_io(ui_rx, agent_tx, peers, None),
        ));

        ui_tx
            .send((None, UiEvent::UserMessage { text: "go".into() }))
            .await
            .unwrap();
        while let Some(ev) = agent_rx.recv().await {
            if matches!(ev, AgentEvent::TurnComplete) {
                break;
            }
        }

        // The peer received the message on its human input path.
        match peer_ui.recv().await {
            Some(UiEvent::UserMessage { text }) => assert_eq!(text, "do X"),
            other => panic!("peer expected the driven message, got {other:?}"),
        }
        // The schema was offered (a peer is held) and the ok result reached the model.
        assert!(seen_tools.lock().unwrap().contains(&"MessagePeer".into()));
        let transcript = seen_messages.lock().unwrap().clone();
        assert!(
            transcript.iter().any(|m| m.content.iter().any(|b| matches!(
                b,
                ContentBlock::ToolResult { content, is_error: false, .. }
                    if content.contains("message sent to child-1")
            ))),
            "expected the ok tool_result in the transcript: {transcript:?}"
        );

        ui_tx.send((None, UiEvent::Quit)).await.unwrap();
        task.await.unwrap().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    // Addressing a peer that doesn't exist returns an error tool_result carrying the
    // current roster, so the model can self-correct on its next step.
    #[tokio::test]
    async fn message_peer_unknown_name_lists_roster() {
        let (session, dir) = mk_session();
        let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_tools = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = ScriptedProvider {
            responses: std::sync::Mutex::new(vec![
                tool_use_response(
                    "MessagePeer",
                    serde_json::json!({"peer": "nobody", "message": "hello"}),
                ),
                end_turn_response("ok"),
            ]),
            seen_messages: seen_messages.clone(),
            seen_tools: seen_tools.clone(),
        };

        let mut peers = PeerSet::default();
        let (peer_ctrl, _peer_ev, _peer_ui) = fake_peer();
        peers.register(PeerRegistration::new(peer_ctrl, agent_who("child-1")));

        let (ui_tx, ui_rx) = mpsc::channel(16);
        let (agent_tx, mut agent_rx) = mpsc::channel(16);
        let task = tokio::spawn(run_agent(
            mk_cfg(),
            provider,
            FakeBackend,
            session,
            Vec::new(),
            mk_io(ui_rx, agent_tx, peers, None),
        ));

        ui_tx
            .send((None, UiEvent::UserMessage { text: "go".into() }))
            .await
            .unwrap();
        while let Some(ev) = agent_rx.recv().await {
            if matches!(ev, AgentEvent::TurnComplete) {
                break;
            }
        }

        let transcript = seen_messages.lock().unwrap().clone();
        assert!(
            transcript.iter().any(|m| m.content.iter().any(|b| matches!(
                b,
                ContentBlock::ToolResult { content, is_error: true, .. }
                    if content.contains("no peer named 'nobody'") && content.contains("child-1")
            ))),
            "expected the roster-bearing error tool_result: {transcript:?}"
        );

        ui_tx.send((None, UiEvent::Quit)).await.unwrap();
        task.await.unwrap().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    // A stub factory that hands out a pre-built synthetic peer, so the test controls
    // both far ends. The slot proves whether the factory ran (denial must not call it).
    fn stub_factory(slot: std::sync::Arc<std::sync::Mutex<Option<Controller>>>) -> PeerFactory {
        Box::new(move |task, _self_handle| {
            let slot = slot.clone();
            Box::pin(async move {
                let controller = slot
                    .lock()
                    .unwrap()
                    .take()
                    .expect("factory called more than once");
                let mut who = agent_who("child-test");
                who.session_id = Some("sess-1".into());
                who.task = Some(task);
                Ok(PeerRegistration {
                    controller,
                    who,
                    host: None,
                })
            })
        })
    }

    // The model calls Spawn → the human gates it → on approval the factory runs, the
    // child is registered (its later activity surfaces as a Notice), and the
    // tool_result records name/id/task in the caller's transcript — the durable
    // "who is child-X" record.
    #[tokio::test]
    async fn spawn_tool_gates_then_registers_the_child() {
        let (session, dir) = mk_session();
        let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_tools = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = ScriptedProvider {
            responses: std::sync::Mutex::new(vec![
                tool_use_response("Spawn", serde_json::json!({"task": "count files"})),
                end_turn_response("spawned"),
            ]),
            seen_messages: seen_messages.clone(),
            seen_tools: seen_tools.clone(),
        };

        let (peer_ctrl, peer_ev, _peer_ui) = fake_peer();
        let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(peer_ctrl)));

        let (ui_tx, ui_rx) = mpsc::channel(16);
        let (agent_tx, mut agent_rx) = mpsc::channel(16);
        let task = tokio::spawn(run_agent(
            mk_cfg(),
            provider,
            FakeBackend,
            session,
            Vec::new(),
            AgentIo {
                ui_rx,
                agent_tx,
                peers: PeerSet::default(),
                peer_register_rx: None,
                peer_factory: Some(stub_factory(slot)),
                self_handle: crate::core::host::spawn_bare_broker(Vec::new()).handle,
            },
        ));

        ui_tx
            .send((None, UiEvent::UserMessage { text: "go".into() }))
            .await
            .unwrap();

        // Spawn gates: answer the permission round-trip, then run to TurnComplete.
        loop {
            match agent_rx.recv().await {
                Some(AgentEvent::PermissionRequest {
                    tool_name,
                    summary,
                    respond,
                    ..
                }) => {
                    assert_eq!(tool_name, "Spawn");
                    assert!(summary.contains("spawn a subagent"), "summary: {summary}");
                    respond.send(true).unwrap();
                }
                Some(AgentEvent::TurnComplete) => break,
                Some(_) => {}
                None => panic!("loop ended early"),
            }
        }

        // Both peer tools were offered (a factory exists), and the spawn record —
        // name, session id, task — landed in the transcript.
        let tools = seen_tools.lock().unwrap().clone();
        assert!(tools.contains(&"Spawn".into()) && tools.contains(&"MessagePeer".into()));
        let transcript = seen_messages.lock().unwrap().clone();
        assert!(
            transcript.iter().any(|m| m.content.iter().any(|b| matches!(
                b,
                ContentBlock::ToolResult { content, is_error: false, .. }
                    if content.contains("spawned peer child-test (session sess-1)")
                        && content.contains("count files")
            ))),
            "expected the spawn record in the transcript: {transcript:?}"
        );

        // The child is genuinely registered: its activity now drives the loop.
        peer_ev
            .send(ControllerEvent::AssistantText {
                text: "child working".into(),
            })
            .unwrap();
        loop {
            match agent_rx.recv().await {
                Some(AgentEvent::Notice { text }) if text.contains("child-test") => break,
                Some(_) => {}
                None => panic!("expected the registered child's Notice"),
            }
        }

        ui_tx.send((None, UiEvent::Quit)).await.unwrap();
        task.await.unwrap().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    // Denying the Spawn permission must not run the factory; the model sees the
    // denial tool_result.
    #[tokio::test]
    async fn spawn_denial_does_not_run_the_factory() {
        let (session, dir) = mk_session();
        let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_tools = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = ScriptedProvider {
            responses: std::sync::Mutex::new(vec![
                tool_use_response("Spawn", serde_json::json!({"task": "count files"})),
                end_turn_response("ok"),
            ]),
            seen_messages: seen_messages.clone(),
            seen_tools: seen_tools.clone(),
        };

        let (peer_ctrl, _peer_ev, _peer_ui) = fake_peer();
        let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(peer_ctrl)));

        let (ui_tx, ui_rx) = mpsc::channel(16);
        let (agent_tx, mut agent_rx) = mpsc::channel(16);
        let task = tokio::spawn(run_agent(
            mk_cfg(),
            provider,
            FakeBackend,
            session,
            Vec::new(),
            AgentIo {
                ui_rx,
                agent_tx,
                peers: PeerSet::default(),
                peer_register_rx: None,
                peer_factory: Some(stub_factory(slot.clone())),
                self_handle: crate::core::host::spawn_bare_broker(Vec::new()).handle,
            },
        ));

        ui_tx
            .send((None, UiEvent::UserMessage { text: "go".into() }))
            .await
            .unwrap();
        loop {
            match agent_rx.recv().await {
                Some(AgentEvent::PermissionRequest { respond, .. }) => {
                    respond.send(false).unwrap();
                }
                Some(AgentEvent::TurnComplete) => break,
                Some(_) => {}
                None => panic!("loop ended early"),
            }
        }

        // A denial pauses for fresh guidance; supply it so the turn closes.
        ui_tx
            .send((
                None,
                UiEvent::UserMessage {
                    text: "never mind".into(),
                },
            ))
            .await
            .unwrap();
        while let Some(ev) = agent_rx.recv().await {
            if matches!(ev, AgentEvent::TurnComplete) {
                break;
            }
        }

        assert!(
            slot.lock().unwrap().is_some(),
            "denial must not run the factory"
        );
        let transcript = seen_messages.lock().unwrap().clone();
        assert!(
            transcript.iter().any(|m| m.content.iter().any(|b| matches!(
                b,
                ContentBlock::ToolResult { content, is_error: true, .. }
                    if content.contains("denied")
            ))),
            "expected the denial tool_result: {transcript:?}"
        );

        ui_tx.send((None, UiEvent::Quit)).await.unwrap();
        task.await.unwrap().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
}
