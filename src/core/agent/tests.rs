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
    async fn handle_control(&mut self, _ev: &UiEvent, _notify: &mpsc::Sender<AgentEvent>) -> bool {
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
