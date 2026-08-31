use anyhow::Result;
use serde_json::Value;
use std::path::PathBuf;
use tokio::sync::mpsc;

use super::supervision::peer_notice;
use super::{AgentConfig, AgentIo, Backend, LoopInput, run_agent};
use crate::core::events::{AgentEvent, ControllerEvent, UiEvent};
use crate::core::host::Controller;
use crate::core::identity::{ClientIdentity, ClientKind};
use crate::core::peer::{PeerFactory, PeerRegistration, PeerSet};
use crate::core::session::Session;
use crate::core::{SessionHandle, SessionHost};
use crate::llm::{ContentBlock, Message, Provider, Request, Response, SystemBlock, Usage};

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

// Closes each turn with an end_turn reply, except the `fail_on`-th call (1-indexed),
// which errors — how tests drive a provider failure mid-turn to exercise rollback.
struct FailOnNthProvider {
    calls: std::sync::Mutex<usize>,
    fail_on: usize,
}
impl Provider for FailOnNthProvider {
    async fn complete(&self, _req: &Request<'_>) -> Result<Response> {
        let mut n = self.calls.lock().unwrap();
        *n += 1;
        if *n == self.fail_on {
            anyhow::bail!("simulated provider failure");
        }
        Ok(end_turn_response("ok"))
    }
    async fn count_tokens(&self, _req: &Request<'_>) -> Result<u64> {
        Ok(0)
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
    async fn handle_mcp(
        &mut self,
        _cmd: &super::McpCommand,
        _notify: &mpsc::Sender<AgentEvent>,
    ) -> String {
        String::new()
    }
    fn mcp_catalog(&self) -> Vec<crate::core::McpServerInfo> {
        Vec::new()
    }
}

fn mk_cfg() -> AgentConfig {
    AgentConfig {
        model: "fake-model".into(),
        max_tokens: 64,
        max_iterations: 4,
        thinking_display: "omitted".into(),
        models: Vec::new(),
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
        peer_dialer: None,
        self_handle: crate::core::host::spawn_bare_broker(Vec::new()).handle,
        multi_driver: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

// A SUPERVISED peer's activity surfaces to this agent's own front-end as a Notice
// (the parent's watch substrate). Registered at runtime via the registrar.
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
        .send(supervised_reg(peer_ctrl, agent_who("child-1")))
        .unwrap();
    peer_ev
        .send(ControllerEvent::AssistantText {
            text: "peer working".into(),
        })
        .unwrap();

    // The registration now emits a "connected to peer" Notice first; the activity
    // Notice (what this test is about) follows it.
    let mut saw = None;
    while let Some(ev) = agent_rx.recv().await {
        if let AgentEvent::Notice { text } = ev {
            if text.contains("connected to peer") {
                continue;
            }
            saw = Some(text);
            break;
        }
    }
    let text = saw.expect("expected a peer activity Notice");
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

// #52: a peer handed to a LIVE loop through the composition-root wiring —
// `PeerWiring.register_rx` → `AgentIo.peer_register_rx` — lands in the loop. Built on
// the real `SessionHost::spawn` path (not `mk_io`), so it proves the seam the runtime
// producer will drive: sending a registration through the channel makes the peer show
// up in the re-emitted Capabilities roster.
#[tokio::test]
async fn runtime_registration_through_spawn_wiring_lands_in_the_loop() {
    use crate::core::PeerWiring;

    let (session, dir) = mk_session();
    let (peer_reg_tx, peer_reg_rx) = mpsc::unbounded_channel();
    let host = SessionHost::spawn(
        mk_cfg(),
        FakeProvider,
        FakeBackend,
        session,
        Vec::new(),
        Vec::new(),
        PeerWiring {
            factory: None,
            initial_peers: PeerSet::default(),
            register_rx: Some(peer_reg_rx),
            dialer: None,
        },
    );

    let mut ctrl = host
        .attach(ClientIdentity::human("watcher"))
        .await
        .expect("initial attach");

    // Drive the registrar the way the runtime producer will: a completed edge arrives
    // through the channel. The loop registers it and re-advertises the roster.
    let (peer_ctrl, _peer_ev, _peer_ui) = fake_peer();
    peer_reg_tx
        .send(PeerRegistration::new(peer_ctrl, agent_who("remote-1")))
        .unwrap();

    let saw_peer = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut saw_notice = false;
        loop {
            match ctrl.events.recv().await {
                // A runtime edge announces itself, so the "connecting…" line resolves.
                Some(ControllerEvent::Notice { text })
                    if text.contains("connected to peer remote-1") =>
                {
                    saw_notice = true;
                }
                Some(ControllerEvent::Capabilities { peers, .. })
                    if peers.iter().any(|p| p.name == "remote-1") =>
                {
                    break saw_notice;
                }
                Some(_) => continue,
                None => break false,
            }
        }
    })
    .await
    .expect("timed out waiting for the peer to appear in Capabilities");
    assert!(
        saw_peer,
        "the registered peer must appear in the roster and announce a connect Notice"
    );

    host.shutdown().await.unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// #53: the /connect-peer command hands the pasted code to the injected dialer (off the
// loop, non-blocking) and the completed forward edge lands via the registrar. A stub
// dialer stands in for the network — it records the code it was handed and registers a
// peer, whose far ends a keeper task holds alive so the edge isn't immediately reaped.
#[tokio::test]
async fn connect_peer_command_invokes_dialer_and_registers_edge() {
    use crate::core::{BrokerHandle, PeerDialer, PeerWiring};

    let (session, dir) = mk_session();
    let (reg_tx, reg_rx) = mpsc::unbounded_channel::<PeerRegistration>();
    let (code_tx, mut code_rx) = mpsc::unbounded_channel::<String>();

    let dialer: PeerDialer = Box::new(move |code: String, _self: BrokerHandle| {
        let reg_tx = reg_tx.clone();
        let code_tx = code_tx.clone();
        Box::pin(async move {
            let _ = code_tx.send(code);
            let (ev_tx, ev_rx) = mpsc::unbounded_channel::<ControllerEvent>();
            let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>(16);
            // Hold the far ends so the registered peer isn't reaped before we observe it.
            tokio::spawn(async move {
                let _keep = (ev_tx, ui_rx);
                std::future::pending::<()>().await;
            });
            reg_tx
                .send(PeerRegistration::new(
                    Controller {
                        events: ev_rx,
                        ui_tx,
                    },
                    agent_who("remote-peer"),
                ))
                .map_err(|_| anyhow::anyhow!("loop gone"))?;
            Ok(())
        })
    });

    let host = SessionHost::spawn(
        mk_cfg(),
        FakeProvider,
        FakeBackend,
        session,
        Vec::new(),
        Vec::new(),
        PeerWiring {
            factory: None,
            initial_peers: PeerSet::default(),
            register_rx: Some(reg_rx),
            dialer: Some(dialer),
        },
    );
    let mut ctrl = host
        .attach(ClientIdentity::human("watcher"))
        .await
        .expect("attach");

    ctrl.ui_tx
        .send(UiEvent::Command {
            line: "/connect-peer nudge:pastedcode".into(),
        })
        .await
        .unwrap();

    // The dialer was invoked with exactly the pasted code (parse + dispatch + hand-off).
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), code_rx.recv())
        .await
        .expect("dialer was not invoked")
        .expect("code");
    assert_eq!(got, "nudge:pastedcode");

    // The forward edge lands through the registrar: the peer shows up in the roster.
    let landed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match ctrl.events.recv().await {
                Some(ControllerEvent::Capabilities { peers, .. })
                    if peers.iter().any(|p| p.name == "remote-peer") =>
                {
                    break true;
                }
                Some(_) => continue,
                None => break false,
            }
        }
    })
    .await
    .expect("timed out waiting for the peer in Capabilities");
    assert!(landed, "the dialed peer must appear in the roster");

    host.shutdown().await.unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// A supervised peer registration, as the Spawn path produces (steered + dismissable).
fn supervised_reg(controller: Controller, who: ClientIdentity) -> PeerRegistration {
    PeerRegistration {
        controller,
        who,
        host: None,
        supervised: true,
        spawner: false,
    }
}

// A supervised peer's check-in drives a steering inference; an approve verdict routes
// allow back to the peer, the check-in carries the capped activity digest, and the
// whole exchange is recorded in the transcript (resting on an assistant turn).
#[tokio::test]
async fn supervised_check_in_is_steered_to_approval() {
    let (session, dir) = mk_session();
    let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_tools = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![
            tool_use_response("RespondToPeer", serde_json::json!({"verdict": "approve"})),
            end_turn_response("ok"),
        ]),
        seen_messages: seen_messages.clone(),
        seen_tools: seen_tools.clone(),
    };

    let mut peers = PeerSet::default();
    let (peer_ctrl, peer_ev, mut peer_ui) = fake_peer();
    peers.register(supervised_reg(peer_ctrl, agent_who("child-1")));

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

    // Some activity first, so the check-in has a digest to carry.
    peer_ev
        .send(ControllerEvent::ToolUseStart {
            id: "c1".into(),
            name: "Bash".into(),
            summary: "listing files".into(),
        })
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
        other => panic!("expected the approve routed to the peer, got {other:?}"),
    }

    // The steering request carried the check-in + digest.
    let transcript = seen_messages.lock().unwrap().clone();
    let checkin = match &transcript[0].content[0] {
        ContentBlock::Text { text } => text.clone(),
        other => panic!("expected the check-in turn, got {other:?}"),
    };
    assert!(
        checkin.contains("[check-in from peer child-1]"),
        "{checkin}"
    );
    assert!(checkin.contains("run ls"), "{checkin}");
    assert!(
        checkin.contains("requested Bash: listing files"),
        "{checkin}"
    );

    // The exchange is recorded compactly and rests on an assistant turn: the next
    // human turn arrives after just [check-in, assistant close].
    ui_tx
        .send((None, UiEvent::UserMessage { text: "hi".into() }))
        .await
        .unwrap();
    while let Some(ev) = agent_rx.recv().await {
        if matches!(ev, AgentEvent::TurnComplete) {
            break;
        }
    }
    let transcript = seen_messages.lock().unwrap().clone();
    assert_eq!(transcript.len(), 3, "{transcript:?}");
    match &transcript[1].content[0] {
        ContentBlock::Text { text } => {
            assert!(text.contains("Approved child-1's Bash call"), "{text}")
        }
        other => panic!("expected the assistant close, got {other:?}"),
    }

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// A deny verdict routes the block and then the redirect message — the peer paused on
// denial, so the message arrives as its fresh instruction.
#[tokio::test]
async fn steering_deny_redirects_the_peer() {
    let (session, dir) = mk_session();
    let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_tools = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![tool_use_response(
            "RespondToPeer",
            serde_json::json!({"verdict": "deny", "message": "use Grep instead"}),
        )]),
        seen_messages: seen_messages.clone(),
        seen_tools: seen_tools.clone(),
    };

    let mut peers = PeerSet::default();
    let (peer_ctrl, peer_ev, mut peer_ui) = fake_peer();
    peers.register(supervised_reg(peer_ctrl, agent_who("child-1")));

    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, _agent_rx) = mpsc::channel(16);
    let task = tokio::spawn(run_agent(
        mk_cfg(),
        provider,
        FakeBackend,
        session,
        Vec::new(),
        mk_io(ui_rx, agent_tx, peers, None),
    ));

    peer_ev
        .send(ControllerEvent::PermissionRequest {
            tool_use_id: "t1".into(),
            tool_name: "Bash".into(),
            summary: "rm -rf".into(),
        })
        .unwrap();

    match peer_ui.recv().await {
        Some(UiEvent::PermissionResponse { tool_use_id, allow }) => {
            assert_eq!(tool_use_id, "t1");
            assert!(!allow);
        }
        other => panic!("expected the deny routed to the peer, got {other:?}"),
    }
    match peer_ui.recv().await {
        Some(UiEvent::UserMessage { text }) => assert_eq!(text, "use Grep instead"),
        other => panic!("expected the redirect message, got {other:?}"),
    }

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// An escalate verdict surfaces the peer's request on this agent's own broker (named),
// and the human's answer is routed down to the peer.
#[tokio::test]
async fn steering_escalates_to_the_human() {
    let (session, dir) = mk_session();
    let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_tools = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![tool_use_response(
            "RespondToPeer",
            serde_json::json!({"verdict": "escalate"}),
        )]),
        seen_messages: seen_messages.clone(),
        seen_tools: seen_tools.clone(),
    };

    let mut peers = PeerSet::default();
    let (peer_ctrl, peer_ev, mut peer_ui) = fake_peer();
    peers.register(supervised_reg(peer_ctrl, agent_who("child-1")));

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

    peer_ev
        .send(ControllerEvent::PermissionRequest {
            tool_use_id: "t1".into(),
            tool_name: "Bash".into(),
            summary: "push to main".into(),
        })
        .unwrap();

    // The escalated request reaches this agent's own event stream, named.
    loop {
        match agent_rx.recv().await {
            Some(AgentEvent::PermissionRequest {
                tool_use_id,
                summary,
                respond,
                ..
            }) => {
                assert_eq!(tool_use_id, "t1");
                assert!(summary.contains("peer child-1"), "{summary}");
                respond.send(true).unwrap();
                break;
            }
            Some(_) => {}
            None => panic!("loop ended before escalation"),
        }
    }
    match peer_ui.recv().await {
        Some(UiEvent::PermissionResponse { allow, .. }) => assert!(allow),
        other => panic!("expected the human's answer routed to the peer, got {other:?}"),
    }

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// A steering call that yields no verdict (here: a plain text response) must deny
// safely and roll the dangling check-in back — the next human turn starts clean.
#[tokio::test]
async fn steering_failure_denies_safely_and_rolls_back() {
    let (session, dir) = mk_session();
    let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_tools = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![
            end_turn_response("hmm, tricky"),
            end_turn_response("ok"),
        ]),
        seen_messages: seen_messages.clone(),
        seen_tools: seen_tools.clone(),
    };

    let mut peers = PeerSet::default();
    let (peer_ctrl, peer_ev, mut peer_ui) = fake_peer();
    peers.register(supervised_reg(peer_ctrl, agent_who("child-1")));

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

    peer_ev
        .send(ControllerEvent::PermissionRequest {
            tool_use_id: "t1".into(),
            tool_name: "Bash".into(),
            summary: "run ls".into(),
        })
        .unwrap();

    match peer_ui.recv().await {
        Some(UiEvent::PermissionResponse { allow, .. }) => assert!(!allow),
        other => panic!("expected the safe deny, got {other:?}"),
    }

    // The dangling check-in was rolled back: the next turn's transcript is just the
    // human message.
    ui_tx
        .send((None, UiEvent::UserMessage { text: "hi".into() }))
        .await
        .unwrap();
    while let Some(ev) = agent_rx.recv().await {
        if matches!(ev, AgentEvent::TurnComplete) {
            break;
        }
    }
    let transcript = seen_messages.lock().unwrap().clone();
    assert_eq!(transcript.len(), 1, "{transcript:?}");

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// An UNSUPERVISED peer's permission request is never answered by this agent — its
// own supervisor (or a human) holds that decision. This is what stops a child
// rubber-stamping its parent's gated calls. It is not narrated either (the peer's
// prompts are its own session's business); the peer's Error event is the ordering
// sentinel proving the prompt was processed before we assert.
#[tokio::test]
async fn unsupervised_check_in_is_not_answered() {
    let (session, dir) = mk_session();
    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(16);

    let mut peers = PeerSet::default();
    let (peer_ctrl, peer_ev, mut peer_ui) = fake_peer();
    peers.register(PeerRegistration::new(peer_ctrl, agent_who("parent-1")));

    let task = tokio::spawn(run_agent(
        mk_cfg(),
        FakeProvider,
        FakeBackend,
        session,
        Vec::new(),
        mk_io(ui_rx, agent_tx, peers, None),
    ));

    peer_ev
        .send(ControllerEvent::PermissionRequest {
            tool_use_id: "t1".into(),
            tool_name: "Bash".into(),
            summary: "run ls".into(),
        })
        .unwrap();
    peer_ev
        .send(ControllerEvent::Error {
            message: "sentinel".into(),
        })
        .unwrap();

    // Peer events are FIFO on one channel: the sentinel's notice proves the prompt
    // was processed — silently (no "asks to use" narration precedes it).
    loop {
        match agent_rx.recv().await {
            Some(AgentEvent::Notice { text }) if text.contains("sentinel") => break,
            Some(AgentEvent::Notice { text }) => {
                assert!(
                    !text.contains("asks to use"),
                    "unsupervised prompt must not narrate: {text}"
                );
            }
            Some(_) => {}
            None => panic!("expected the sentinel Notice"),
        }
    }

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    // The loop ended without ever sending the peer anything; its channel just closes.
    assert!(peer_ui.recv().await.is_none(), "peer must not be answered");
    std::fs::remove_dir_all(&dir).ok();
}

// DismissPeer (gated) removes a supervised peer: dropping the Peer ends the held
// connection (and, for a real child, its owned SessionHost). Observable here as the
// peer's event channel closing.
#[tokio::test]
async fn dismiss_peer_ends_the_supervised_child() {
    let (session, dir) = mk_session();
    let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_tools = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![
            tool_use_response("DismissPeer", serde_json::json!({"peer": "child-1"})),
            end_turn_response("done"),
        ]),
        seen_messages: seen_messages.clone(),
        seen_tools: seen_tools.clone(),
    };

    let mut peers = PeerSet::default();
    let (peer_ctrl, peer_ev, _peer_ui) = fake_peer();
    peers.register(supervised_reg(peer_ctrl, agent_who("child-1")));

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
        .send((
            None,
            UiEvent::UserMessage {
                text: "dismiss the child".into(),
            },
        ))
        .await
        .unwrap();
    loop {
        match agent_rx.recv().await {
            Some(AgentEvent::PermissionRequest {
                tool_name, respond, ..
            }) => {
                assert_eq!(tool_name, "DismissPeer");
                respond.send(true).unwrap();
            }
            Some(AgentEvent::TurnComplete) => break,
            Some(_) => {}
            None => panic!("loop ended early"),
        }
    }

    // The Peer (and its controller) is gone: the far event sender now errors.
    assert!(
        peer_ev
            .send(ControllerEvent::Notice { text: "?".into() })
            .is_err(),
        "the dismissed peer's channel must be closed"
    );
    let transcript = seen_messages.lock().unwrap().clone();
    assert!(
        transcript.iter().any(|m| m.content.iter().any(|b| matches!(
            b,
            ContentBlock::ToolResult { content, is_error: false, .. }
                if content.contains("dismissed child-1")
        ))),
        "expected the dismissal record: {transcript:?}"
    );

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// Dismissal is supervised-only: the return edge to your own spawner is refused.
#[tokio::test]
async fn dismiss_refuses_an_unsupervised_peer() {
    let (session, dir) = mk_session();
    let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_tools = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![
            tool_use_response("DismissPeer", serde_json::json!({"peer": "parent-1"})),
            end_turn_response("ok"),
        ]),
        seen_messages: seen_messages.clone(),
        seen_tools: seen_tools.clone(),
    };

    let mut peers = PeerSet::default();
    let (parent_ctrl, parent_ev, _parent_ui) = fake_peer();
    peers.register(PeerRegistration::new(parent_ctrl, agent_who("parent-1")));
    // A supervised peer must exist for the DismissPeer schema to be offered at all.
    let (child_ctrl, _child_ev, _child_ui) = fake_peer();
    peers.register(supervised_reg(child_ctrl, agent_who("child-1")));

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
    loop {
        match agent_rx.recv().await {
            Some(AgentEvent::PermissionRequest { respond, .. }) => {
                respond.send(true).unwrap();
            }
            Some(AgentEvent::TurnComplete) => break,
            Some(_) => {}
            None => panic!("loop ended early"),
        }
    }

    // Refused — and the parent edge is still alive.
    let transcript = seen_messages.lock().unwrap().clone();
    assert!(
        transcript.iter().any(|m| m.content.iter().any(|b| matches!(
            b,
            ContentBlock::ToolResult { content, is_error: true, .. }
                if content.contains("not a subagent you spawned")
        ))),
        "expected the refusal: {transcript:?}"
    );
    assert!(
        parent_ev
            .send(ControllerEvent::Notice { text: "?".into() })
            .is_ok(),
        "the unsupervised peer must remain held"
    );

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

    // The log stores the other half of the invariant: clean text + the sender,
    // so resume can re-derive exactly the attributed form the provider saw.
    let jsonl = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .expect("session log written");
    let first = std::fs::read_to_string(&jsonl)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    let envelope: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(envelope["message"]["content"][0]["text"], "task done");
    assert_eq!(envelope["sender"]["name"], "child-1");

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
                supervised: true,
                spawner: false,
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
            peer_dialer: None,
            self_handle: crate::core::host::spawn_bare_broker(Vec::new()).handle,
            multi_driver: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            peer_dialer: None,
            self_handle: crate::core::host::spawn_bare_broker(Vec::new()).handle,
            multi_driver: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

// resume_messages derives attribution at build time from each entry's persisted
// sender: an agent sender always gets the `[message from peer …]` prefix; a human is
// named too when the transcript is multi-party (two or more distinct senders) and
// stays bare in a solo transcript; a pre-sender entry (None — its text may carry an
// already-baked prefix) passes through untouched.
#[test]
fn resume_messages_applies_attribution_from_persisted_sender() {
    use crate::core::session::LoggedMessage;

    let user = |text: &str| Message {
        role: "user".into(),
        content: vec![ContentBlock::Text { text: text.into() }],
    };
    let entries = vec![
        LoggedMessage {
            message: user("do it"),
            sender: Some(ClientIdentity {
                kind: ClientKind::Agent,
                name: "child-x".into(),
                session_id: None,
                task: None,
            }),
        },
        LoggedMessage {
            message: user("hello"),
            sender: Some(ClientIdentity::human("alice")),
        },
        LoggedMessage {
            message: user("[message from peer old]\nlegacy"),
            sender: None,
        },
    ];

    let msgs = super::resume_messages(&entries);
    let texts: Vec<&str> = msgs
        .iter()
        .map(|m| match &m.content[0] {
            ContentBlock::Text { text } => text.as_str(),
            other => panic!("expected text block, got {other:?}"),
        })
        .collect();
    assert_eq!(texts[0], "[message from peer child-x]\ndo it");
    // Two distinct senders → a multi-party transcript, so the human is named too.
    assert_eq!(texts[1], "[message from alice]\nhello");
    assert_eq!(texts[2], "[message from peer old]\nlegacy");

    // A solo transcript: the lone human stays bare.
    let solo = vec![
        LoggedMessage {
            message: user("just me"),
            sender: Some(ClientIdentity::human("alice")),
        },
        LoggedMessage {
            message: user("still me"),
            sender: Some(ClientIdentity::human("alice")),
        },
    ];
    let msgs = super::resume_messages(&solo);
    match &msgs[0].content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "just me"),
        other => panic!("expected text block, got {other:?}"),
    }
}

// Locate the session's JSONL and return each entry's message role, in order. An
// absent file (nothing ever committed) reads as empty.
fn logged_roles(dir: &std::path::Path) -> Vec<String> {
    logged_lines(dir)
        .iter()
        .map(|line| {
            let env: Value = serde_json::from_str(line).unwrap();
            env["message"]["role"].as_str().unwrap().to_string()
        })
        .collect()
}

// The raw non-empty JSONL lines for the session, or empty if the file is absent.
fn logged_lines(dir: &std::path::Path) -> Vec<String> {
    let path = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"));
    match path.and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(raw) => raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(String::from)
            .collect(),
        None => Vec::new(),
    }
}

async fn drain_until(rx: &mut mpsc::Receiver<AgentEvent>, pred: impl Fn(&AgentEvent) -> bool) {
    while let Some(ev) = rx.recv().await {
        if pred(&ev) {
            return;
        }
    }
    panic!("event stream ended before the awaited event");
}

// A provider error mid-turn rolls the in-memory transcript back AND leaves nothing on
// disk for that turn: the failed turn's user entry is never committed, so the JSONL
// holds only the committed turns and a resume sees no phantom / no consecutive user
// roles. The turn after the failure lands cleanly right behind the last good close.
#[tokio::test]
async fn provider_error_midturn_leaves_no_phantom_log_entry() {
    let (session, dir) = mk_session();
    let id = session.id.clone();
    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(16);
    let task = tokio::spawn(run_agent(
        mk_cfg(),
        FailOnNthProvider {
            calls: std::sync::Mutex::new(0),
            fail_on: 2,
        },
        FakeBackend,
        session,
        Vec::new(),
        mk_io(ui_rx, agent_tx, PeerSet::default(), None),
    ));

    // Turn 1 commits (call 1 succeeds).
    ui_tx
        .send((None, UiEvent::UserMessage { text: "hi".into() }))
        .await
        .unwrap();
    drain_until(&mut agent_rx, |e| matches!(e, AgentEvent::TurnComplete)).await;

    // Turn 2's provider call fails; its user entry must be rolled back, not logged.
    ui_tx
        .send((
            None,
            UiEvent::UserMessage {
                text: "again".into(),
            },
        ))
        .await
        .unwrap();
    drain_until(&mut agent_rx, |e| matches!(e, AgentEvent::Error { .. })).await;

    // Turn 3 commits (call 3 succeeds) — it must follow turn 1's close directly.
    ui_tx
        .send((
            None,
            UiEvent::UserMessage {
                text: "retry".into(),
            },
        ))
        .await
        .unwrap();
    drain_until(&mut agent_rx, |e| matches!(e, AgentEvent::TurnComplete)).await;

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();

    // On disk: only the two committed turns, no "again" phantom, no consecutive users.
    let lines = logged_lines(&dir);
    assert!(
        !lines.iter().any(|l| l.contains("again")),
        "rolled-back turn must not be logged: {lines:?}"
    );
    assert_eq!(
        logged_roles(&dir),
        vec!["user", "assistant", "user", "assistant"],
        "log holds exactly the committed turns"
    );

    // A resume rebuilds the same clean, alternating transcript.
    let resumed = Session::open(&id, dir.clone(), dir.clone()).unwrap();
    let msgs = super::resume_messages(&resumed.entries);
    let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
    assert!(
        roles.windows(2).all(|w| w != ["user", "user"]),
        "resume must not surface consecutive user roles: {roles:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// A steering inference that fails (provider error) safe-denies the peer AND rolls the
// staged check-in back: nothing about the check-in reaches the JSONL.
#[tokio::test]
async fn steering_failure_leaves_no_checkin_in_the_log() {
    let (session, dir) = mk_session();
    let mut peers = PeerSet::default();
    let (peer_ctrl, peer_ev, mut peer_ui) = fake_peer();
    peers.register(supervised_reg(peer_ctrl, agent_who("child-1")));

    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, _agent_rx) = mpsc::channel(16);
    let task = tokio::spawn(run_agent(
        mk_cfg(),
        FailOnNthProvider {
            calls: std::sync::Mutex::new(0),
            fail_on: 1,
        },
        FakeBackend,
        session,
        Vec::new(),
        mk_io(ui_rx, agent_tx, peers, None),
    ));

    peer_ev
        .send(ControllerEvent::PermissionRequest {
            tool_use_id: "t1".into(),
            tool_name: "Bash".into(),
            summary: "run ls".into(),
        })
        .unwrap();

    // The steering inference errors → the peer is safe-denied.
    match peer_ui.recv().await {
        Some(UiEvent::PermissionResponse { allow, .. }) => assert!(!allow),
        other => panic!("expected the safe deny, got {other:?}"),
    }

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();

    // The staged check-in was rolled back — it never reached disk.
    let lines = logged_lines(&dir);
    assert!(
        !lines.iter().any(|l| l.contains("check-in from peer")),
        "rolled-back check-in must not be logged: {lines:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// A successful multi-iteration turn (tool_use then end_turn) commits the exact same
// entries eager logging produced: user, assistant(tool_use), user(tool_results),
// assistant — proving deferral is a no-op on the committed path.
#[tokio::test]
async fn successful_multi_iteration_turn_persists_all_entries() {
    let (session, dir) = mk_session();
    let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_tools = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![
            tool_use_response("Bash", serde_json::json!({})),
            end_turn_response("done"),
        ]),
        seen_messages,
        seen_tools,
    };

    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(16);
    let task = tokio::spawn(run_agent(
        mk_cfg(),
        provider,
        FakeBackend,
        session,
        Vec::new(),
        mk_io(ui_rx, agent_tx, PeerSet::default(), None),
    ));

    ui_tx
        .send((None, UiEvent::UserMessage { text: "go".into() }))
        .await
        .unwrap();
    drain_until(&mut agent_rx, |e| matches!(e, AgentEvent::TurnComplete)).await;

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();

    assert_eq!(
        logged_roles(&dir),
        vec!["user", "assistant", "user", "assistant"],
        "every mid-turn entry is committed when the turn completes"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// /peers reports the held roster (name, kind, supervision) as a Notice; with no
// peers it says so rather than replying with an empty listing.
#[tokio::test]
async fn peers_command_reports_the_roster() {
    let (session, dir) = mk_session();
    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(16);
    let mut peers = PeerSet::default();
    let (peer_ctrl, _peer_ev, _peer_ui) = fake_peer();
    peers.register(supervised_reg(peer_ctrl, agent_who("child-1")));
    let task = tokio::spawn(run_agent(
        mk_cfg(),
        FakeProvider,
        FakeBackend,
        session,
        Vec::new(),
        mk_io(ui_rx, agent_tx, peers, None),
    ));

    ui_tx
        .send((
            None,
            UiEvent::Command {
                line: "/peers".into(),
            },
        ))
        .await
        .unwrap();
    match agent_rx.recv().await {
        Some(AgentEvent::Notice { text }) => {
            assert!(text.contains("child-1"), "roster names the peer: {text}");
            assert!(text.contains("agent"), "roster shows the kind: {text}");
            assert!(
                text.contains("supervised"),
                "roster shows supervision: {text}"
            );
        }
        other => panic!("expected roster Notice, got {other:?}"),
    }

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn peers_command_with_no_peers_says_so() {
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
        .send((
            None,
            UiEvent::Command {
                line: "/peers".into(),
            },
        ))
        .await
        .unwrap();
    match agent_rx.recv().await {
        Some(AgentEvent::Notice { text }) => assert_eq!(text, "no peers held"),
        other => panic!("expected empty-roster Notice, got {other:?}"),
    }

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// The roster rides Capabilities and is re-advertised on peer change: a runtime
// registration emits it with the new peer, and the peer's disconnect (reap) emits
// it again, empty.
#[tokio::test]
async fn peer_change_reemits_capabilities_roster() {
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
    // Registration emits a connect Notice, then the Capabilities roster re-advertise.
    let mut saw_roster = false;
    for _ in 0..4 {
        match agent_rx.recv().await {
            Some(AgentEvent::Capabilities { peers, .. }) => {
                assert_eq!(peers.len(), 1);
                assert_eq!(peers[0].name, "child-1");
                assert!(!peers[0].supervised);
                saw_roster = true;
                break;
            }
            Some(_) => continue,
            None => break,
        }
    }
    assert!(saw_roster, "expected Capabilities after registration");

    // The peer disconnects: the loop reaps it (a Notice narrates that) and
    // re-advertises the now-empty roster.
    drop(peer_ev);
    let mut saw_empty_roster = false;
    for _ in 0..4 {
        match agent_rx.recv().await {
            Some(AgentEvent::Capabilities { peers, .. }) => {
                assert!(peers.is_empty(), "roster empties after the reap");
                saw_empty_roster = true;
                break;
            }
            Some(_) => continue,
            None => break,
        }
    }
    assert!(
        saw_empty_roster,
        "expected a Capabilities re-emit after reap"
    );

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// The model-facing roster block: None when peerless, else names each held peer and
// distinguishes a spawned subagent from a connected peer, so the agent can address
// one without guessing.
#[test]
fn roster_system_text_lists_held_peers() {
    use super::peer_tools::roster_system_text;

    let mut peers = PeerSet::default();
    assert!(roster_system_text(&peers).is_none(), "peerless → no block");

    let (a_ctrl, _a_ev, _a_ui) = fake_peer();
    peers.register(supervised_reg(a_ctrl, agent_who("child-1")));
    let (b_ctrl, _b_ev, _b_ui) = fake_peer();
    peers.register(PeerRegistration::new(b_ctrl, agent_who("mate")));

    let text = roster_system_text(&peers).expect("held peers → a block");
    assert!(text.contains("MessagePeer"), "names the tool: {text}");
    assert!(
        text.contains("child-1 (subagent you spawned)"),
        "supervised peer labelled: {text}"
    );
    assert!(
        text.contains("mate (peer)"),
        "unsupervised peer labelled: {text}"
    );
}

// Consecutive messages from the same sender, already queued when the turn starts, are
// coalesced into ONE user turn (same info to the model, fewer round-trips), attributed
// once. Pre-buffering both before spawning the loop makes the drain deterministic.
#[tokio::test]
async fn consecutive_same_sender_messages_coalesce_into_one_turn() {
    let (session, dir) = mk_session();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(16);

    let who = agent_who("child-1");
    ui_tx
        .send((
            Some(who.clone()),
            UiEvent::UserMessage { text: "one".into() },
        ))
        .await
        .unwrap();
    ui_tx
        .send((Some(who), UiEvent::UserMessage { text: "two".into() }))
        .await
        .unwrap();

    let task = tokio::spawn(run_agent(
        mk_cfg(),
        RecordingProvider { seen: seen.clone() },
        FakeBackend,
        session,
        Vec::new(),
        mk_io(ui_rx, agent_tx, PeerSet::default(), None),
    ));

    while let Some(ev) = agent_rx.recv().await {
        if matches!(ev, AgentEvent::TurnComplete) {
            break;
        }
    }

    let transcript = seen.lock().unwrap().clone();
    assert_eq!(
        transcript.len(),
        1,
        "coalesced into one turn (a single user message), not two: {transcript:?}"
    );
    match &transcript[0].content[0] {
        ContentBlock::Text { text } => {
            assert!(
                text.contains("one") && text.contains("two"),
                "both messages reach the model in one turn: {text}"
            );
            assert!(
                text.starts_with("[message from peer child-1]"),
                "attributed once, to the shared sender: {text}"
            );
        }
        other => panic!("expected a coalesced user turn, got {other:?}"),
    }

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// Different senders are NOT coalesced: a second sender's message is stashed and becomes
// its own turn, so cross-driver messages stay attributable.
#[tokio::test]
async fn different_sender_messages_are_not_coalesced() {
    let (session, dir) = mk_session();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(16);

    ui_tx
        .send((
            Some(agent_who("alice")),
            UiEvent::UserMessage {
                text: "from-a".into(),
            },
        ))
        .await
        .unwrap();
    ui_tx
        .send((
            Some(agent_who("bob")),
            UiEvent::UserMessage {
                text: "from-b".into(),
            },
        ))
        .await
        .unwrap();

    let task = tokio::spawn(run_agent(
        mk_cfg(),
        RecordingProvider { seen: seen.clone() },
        FakeBackend,
        session,
        Vec::new(),
        mk_io(ui_rx, agent_tx, PeerSet::default(), None),
    ));

    // Two distinct turns complete (one per sender).
    let mut completed = 0;
    while let Some(ev) = agent_rx.recv().await {
        if matches!(ev, AgentEvent::TurnComplete) {
            completed += 1;
            if completed == 2 {
                break;
            }
        }
    }

    // The last recorded request holds both turns as separate user messages.
    let transcript = seen.lock().unwrap().clone();
    let user_texts: Vec<&str> = transcript
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| match &m.content[0] {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        user_texts.iter().any(|t| t.contains("from-a"))
            && user_texts.iter().any(|t| t.contains("from-b")),
        "each sender is its own turn: {user_texts:?}"
    );
    assert!(
        user_texts
            .iter()
            .all(|t| !(t.contains("from-a") && t.contains("from-b"))),
        "the two senders' messages are never merged into one turn: {user_texts:?}"
    );

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// An UNSUPERVISED peer's activity is NOT narrated — its work belongs to its own
// session; only its MessagePeer turns, errors, and lifecycle reach this one. The
// peer's Error (always narrated) is the sentinel: events are processed in order, so
// if the AssistantText had narrated, it would arrive before the error notice.
#[tokio::test]
async fn unsupervised_peer_activity_is_not_narrated() {
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
        .send(PeerRegistration::new(peer_ctrl, agent_who("mate")))
        .unwrap();
    peer_ev
        .send(ControllerEvent::AssistantText {
            text: "chatty peer output".into(),
        })
        .unwrap();
    peer_ev
        .send(ControllerEvent::Error {
            message: "sentinel".into(),
        })
        .unwrap();

    let mut first_activity_notice = None;
    while let Some(ev) = agent_rx.recv().await {
        if let AgentEvent::Notice { text } = ev {
            if text.contains("connected to peer") {
                continue;
            }
            first_activity_notice = Some(text);
            break;
        }
    }
    let text = first_activity_notice.expect("expected the sentinel error Notice");
    assert!(
        text.contains("sentinel"),
        "the first narrated notice must be the error sentinel, not the suppressed \
         activity: {text}"
    );
    assert!(
        !text.contains("chatty peer output"),
        "unsupervised activity must not narrate: {text}"
    );

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// End to end through a real SessionHost: once a second distinct human connects, a
// human's message reaches the model NAMED — the multi-driver flag flips at the
// broker and the loop derives the prefix at payload-build time.
#[tokio::test]
async fn second_connected_human_makes_messages_attributed() {
    use crate::core::PeerWiring;

    let (session, dir) = mk_session();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let host = SessionHost::spawn(
        mk_cfg(),
        RecordingProvider { seen: seen.clone() },
        FakeBackend,
        session,
        Vec::new(),
        Vec::new(),
        PeerWiring::default(),
    );

    let alice = host
        .attach(ClientIdentity::human("alice"))
        .await
        .expect("alice attaches");
    let mut bob = host
        .attach(ClientIdentity::human("bob"))
        .await
        .expect("bob attaches");

    bob.ui_tx
        .send(UiEvent::UserMessage { text: "hi".into() })
        .await
        .unwrap();
    // Wait for bob's turn to complete before reading the recorded request.
    let done = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match bob.events.recv().await {
                Some(ControllerEvent::TurnComplete) => break true,
                Some(_) => continue,
                None => break false,
            }
        }
    })
    .await
    .expect("timed out waiting for the turn");
    assert!(done);

    let transcript = seen.lock().unwrap().clone();
    match &transcript[0].content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "[message from bob]\nhi"),
        other => panic!("expected bob's attributed turn, got {other:?}"),
    }

    drop(alice);
    host.shutdown().await.unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// A turn that exhausts max_iterations reports up the spawner edge as a UserMessage
// (the path a MessagePeer takes), so the spawner's model is woken to send 'continue'
// or re-scope — instead of the child stalling until a human notices.
#[tokio::test]
async fn maxed_out_turn_reports_up_the_spawner_edge() {
    let (session, dir) = mk_session();
    let provider = ScriptedProvider {
        // tool_use on every call, so the turn never ends before the budget does.
        responses: std::sync::Mutex::new(vec![
            tool_use_response("FakeTool", serde_json::json!({})),
            tool_use_response("FakeTool", serde_json::json!({})),
        ]),
        seen_messages: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        seen_tools: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let mut cfg = mk_cfg();
    cfg.max_iterations = 2;

    let mut peers = PeerSet::default();
    let (peer_ctrl, _peer_ev, mut peer_ui) = fake_peer();
    let mut reg = PeerRegistration::new(peer_ctrl, agent_who("parent-1"));
    reg.spawner = true;
    peers.register(reg);

    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, _agent_rx) = mpsc::channel(64);
    let task = tokio::spawn(run_agent(
        cfg,
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

    match peer_ui.recv().await {
        Some(UiEvent::UserMessage { text }) => {
            assert!(text.contains("iteration limit (2)"), "{text}");
        }
        other => panic!("expected the limit report driven up the spawner edge, got {other:?}"),
    }

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}
