// End-to-end transport tests over a bare broker with events injected directly, so
// only the transport seam is exercised; broker invariants live in the host tests.
use std::path::PathBuf;
use std::time::Duration;

use tokio::time::timeout;

use super::daemon::PeerAccept;
use super::encryption::Cipher;
use super::wire::{ClientFrame, ServerFrame};
use super::{RelayClient, RelayLeg, SocketClient, bind_listener, run_relay_daemon, serve_socket};
use crate::core::BrokerHandle;
use crate::core::host::spawn_bare_broker;
use crate::core::peer::{PeerRegistration, PeerSet};
use crate::core::{
    AgentEvent, ClientIdentity, ClientKind, ClientProfile, Controller, ControllerEvent,
    SessionHandle, UiEvent,
};

// Kept short: `sockaddr_un.sun_path` is 104 bytes on macOS and the temp dir alone
// can be ~50, so a full UUID would overflow it.
fn unique_socket_path() -> PathBuf {
    let token = &uuid::Uuid::new_v4().simple().to_string()[..12];
    std::env::temp_dir().join(format!("ra-it-{token}.sock"))
}

fn spawn_socket_host(
    broker: BrokerHandle,
    identity: ClientIdentity,
) -> (
    PathBuf,
    tokio::task::JoinHandle<()>,
    tokio::sync::mpsc::UnboundedReceiver<PeerRegistration>,
) {
    let path = unique_socket_path();
    let listener = bind_listener(&path).expect("bind socket listener");
    let (registrar, reg_rx) = tokio::sync::mpsc::unbounded_channel();
    let serve = tokio::spawn(serve_socket(
        listener,
        broker,
        PeerAccept {
            identity,
            registrar,
        },
    ));
    (path, serve, reg_rx)
}

async fn attach_at(client: &SocketClient, after_seq: Option<u64>) -> Controller {
    timeout(
        Duration::from_secs(5),
        client.connect(after_seq, ClientIdentity::human("test")),
    )
    .await
    .expect("attach timed out")
    .expect("attach errored")
    .expect("attach refused (Busy)")
}

async fn next_event(ctrl: &mut Controller) -> Option<ControllerEvent> {
    timeout(Duration::from_secs(5), ctrl.events.recv())
        .await
        .expect("timed out waiting for a controller event")
}

async fn expect_text(ctrl: &mut Controller, want: &str) {
    match next_event(ctrl).await {
        Some(ControllerEvent::AssistantText { text }) if text == want => {}
        other => panic!("expected AssistantText({want:?}), got {other:?}"),
    }
}

#[tokio::test]
async fn socket_round_trip_replay_resume_and_guest_quit() {
    let bb = spawn_bare_broker(Vec::new());
    let (path, serve, _reg_rx) = spawn_socket_host(bb.handle.clone(), agent_id("host"));

    let client = SocketClient::new(path.clone());

    let mut a = client
        .attach(ClientIdentity::human("test"))
        .await
        .expect("initial attach");

    bb.agent_tx
        .send(AgentEvent::AssistantText { text: "one".into() })
        .await
        .unwrap();
    bb.agent_tx
        .send(AgentEvent::AssistantText { text: "two".into() })
        .await
        .unwrap();
    expect_text(&mut a, "one").await;
    expect_text(&mut a, "two").await;

    drop(a);

    // after_seq = Some(0) skips "one" (seq 0) and replays only "two".
    let mut b = attach_at(&client, Some(0)).await;
    expect_text(&mut b, "two").await;
    bb.agent_tx
        .send(AgentEvent::AssistantText {
            text: "three".into(),
        })
        .await
        .unwrap();
    expect_text(&mut b, "three").await;

    b.ui_tx.send(UiEvent::Quit).await.unwrap();
    assert!(
        next_event(&mut b).await.is_none(),
        "a guest Quit must close the guest's connection"
    );

    // The session survived the guest quit: a fresh attach replays everything.
    let mut c = attach_at(&client, None).await;
    expect_text(&mut c, "one").await;
    expect_text(&mut c, "two").await;
    expect_text(&mut c, "three").await;

    drop(c);
    serve.abort();
    drop(bb);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn socket_serves_two_clients_at_once() {
    let bb = spawn_bare_broker(Vec::new());
    let (path, serve, _reg_rx) = spawn_socket_host(bb.handle.clone(), agent_id("host"));
    let client = SocketClient::new(path.clone());

    let mut a = attach_at(&client, None).await;
    let mut b = attach_at(&client, None).await;
    bb.agent_tx
        .send(AgentEvent::AssistantText {
            text: "hi-all".into(),
        })
        .await
        .unwrap();
    expect_text(&mut a, "hi-all").await;
    expect_text(&mut b, "hi-all").await;

    serve.abort();
    bb.task.abort();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn bind_listener_rebinds_a_stale_socket_and_restricts_it() {
    use std::os::unix::fs::PermissionsExt;

    let path = unique_socket_path();
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists(), "a dropped listener leaves its file behind");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let live = bind_listener(&path).expect("a stale socket file must be rebound");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(
        bind_listener(&path).is_err(),
        "a live socket must not be stolen"
    );

    drop(live);
    let _ = std::fs::remove_file(&path);
}

// Returns (A's edge to B, B's edge to A) after A dials B's socket.
async fn dial_socket_peer(
    a: &BrokerHandle,
    path: &std::path::Path,
    reg_b_rx: &mut tokio::sync::mpsc::UnboundedReceiver<PeerRegistration>,
) -> (PeerRegistration, PeerRegistration) {
    let (reg_a_tx, mut reg_a_rx) = tokio::sync::mpsc::unbounded_channel::<PeerRegistration>();
    timeout(
        Duration::from_secs(5),
        SocketClient::new(path.to_path_buf()).dial_peer(a.clone(), reg_a_tx, agent_id("peer-a")),
    )
    .await
    .expect("dial_peer timed out")
    .expect("dial_peer failed");

    let peer_b = timeout(Duration::from_secs(5), reg_a_rx.recv())
        .await
        .expect("A registrar timeout")
        .expect("A received peer B");
    let peer_a = timeout(Duration::from_secs(5), reg_b_rx.recv())
        .await
        .expect("B registrar timeout")
        .expect("B received peer A");
    assert_eq!(peer_b.who.name, "peer-b");
    assert_eq!(peer_a.who.name, "peer-a");
    (peer_b, peer_a)
}

// The socket counterpart of the relay duplex test: one dial yields both edges.
#[tokio::test]
async fn socket_peer_of_a_human_attended_session_is_unsupervised() {
    let bb_a = spawn_bare_broker(Vec::new());
    let bb_b = spawn_bare_broker(Vec::new());
    let _human = bb_b
        .handle
        .attach(ClientIdentity::human("h"))
        .await
        .unwrap();
    let (path, serve, mut reg_b_rx) = spawn_socket_host(bb_b.handle.clone(), agent_id("peer-b"));

    let (peer_b, peer_a) = dial_socket_peer(&bb_a.handle, &path, &mut reg_b_rx).await;
    assert!(
        !peer_b.supervised && !peer_a.supervised,
        "both edges unsupervised"
    );
    assert!(!peer_a.spawner, "a plain peer is not the target's spawner");

    let mut peers_a = PeerSet::default();
    let mut peers_b = PeerSet::default();
    peers_a.register(peer_b);
    peers_b.register(peer_a);
    for (tx, peers, text) in [
        (&bb_b.agent_tx, &mut peers_a, "from-b"),
        (&bb_a.agent_tx, &mut peers_b, "from-a"),
    ] {
        tx.send(AgentEvent::AssistantText { text: text.into() })
            .await
            .unwrap();
        match timeout(Duration::from_secs(5), peers.recv())
            .await
            .expect("observe timeout")
        {
            (_, Some(ControllerEvent::AssistantText { text: got })) => assert_eq!(got, text),
            other => panic!("expected to observe {text:?}, got {other:?}"),
        }
    }

    serve.abort();
    bb_a.task.abort();
    bb_b.task.abort();
    let _ = std::fs::remove_file(&path);
}

// With no answerer attached, the dialer becomes the target's supervisor and can
// answer its prompts; the target's edge back stays unsupervised.
#[tokio::test]
async fn socket_peer_of_an_unattended_session_supervises_it() {
    let bb_a = spawn_bare_broker(Vec::new());
    let bb_b = spawn_bare_broker(Vec::new());
    let (path, serve, mut reg_b_rx) = spawn_socket_host(bb_b.handle.clone(), agent_id("peer-b"));

    let (peer_b, peer_a) = dial_socket_peer(&bb_a.handle, &path, &mut reg_b_rx).await;
    assert!(peer_b.supervised, "the dialer holds a supervised edge");
    assert!(!peer_a.supervised, "the target never supervises its dialer");
    assert!(
        peer_a.spawner,
        "the target reports stalls up to its supervisor"
    );

    let mut forward = peer_b.controller;
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel::<bool>();
    bb_b.agent_tx
        .send(AgentEvent::PermissionRequest {
            tool_use_id: "t1".into(),
            tool_name: "Bash".into(),
            summary: "run ls".into(),
            respond: resp_tx,
        })
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut forward).await,
        Some(ControllerEvent::PermissionRequest { .. })
    ));
    forward
        .ui_tx
        .send(UiEvent::PermissionResponse {
            tool_use_id: "t1".into(),
            allow: true,
        })
        .await
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(5), resp_rx).await,
        Ok(Ok(true)),
        "the supervisor's answer must resolve the target's prompt"
    );

    serve.abort();
    bb_a.task.abort();
    bb_b.task.abort();
    let _ = std::fs::remove_file(&path);
}

// Pipes the first two WebSocket connections together verbatim, ignoring the path.
async fn spawn_test_relay() -> u16 {
    use futures::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        // Upgrade each as it arrives so the daemon's handshake completes before the
        // client connects.
        let (s1, _) = listener.accept().await.unwrap();
        let ws1 = accept_async(s1).await.unwrap();
        let (s2, _) = listener.accept().await.unwrap();
        let ws2 = accept_async(s2).await.unwrap();
        let (mut t1, mut r1) = ws1.split();
        let (mut t2, mut r2) = ws2.split();
        loop {
            tokio::select! {
                m = r1.next() => match m {
                    Some(Ok(msg)) => if t2.send(msg).await.is_err() { break },
                    _ => break,
                },
                m = r2.next() => match m {
                    Some(Ok(msg)) => if t1.send(msg).await.is_err() { break },
                    _ => break,
                },
            }
        }
    });
    port
}

#[tokio::test]
async fn relay_round_trip_event_command_and_quit_detaches() {
    let port = spawn_test_relay().await;
    let url = format!("ws://127.0.0.1:{port}/test");

    let cipher = Cipher::generate();
    let bb = spawn_bare_broker(Vec::new());
    let daemon = tokio::spawn(run_relay_daemon(
        vec![RelayLeg {
            dial_url: url.clone(),
            cipher: cipher.clone(),
            profile: ClientProfile::human(),
            peer_accept: None,
        }],
        bb.handle.clone(),
    ));

    let client = RelayClient::new(url.clone(), cipher.clone());
    let mut a = client
        .attach(ClientIdentity::human("test"))
        .await
        .expect("relay attach");

    bb.agent_tx
        .send(AgentEvent::AssistantText { text: "hi".into() })
        .await
        .unwrap();
    expect_text(&mut a, "hi").await;

    a.ui_tx
        .send(UiEvent::UserMessage {
            text: "drive".into(),
        })
        .await
        .unwrap();
    match next_event(&mut a).await {
        Some(ControllerEvent::UserMessage { text, .. }) if text == "drive" => {}
        other => panic!("expected echoed UserMessage, got {other:?}"),
    }

    a.ui_tx.send(UiEvent::Quit).await.unwrap();
    assert!(
        next_event(&mut a).await.is_none(),
        "a remote Quit must close the controller's connection (detach)"
    );
    assert!(
        !daemon.is_finished(),
        "the daemon must survive a controller's Quit"
    );

    daemon.abort();
    bb.task.abort();
}

// Like `spawn_test_relay`, but records every Binary payload it forwards.
async fn spawn_capturing_relay() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>) {
    use futures::{SinkExt, StreamExt};
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message;

    let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (s1, _) = listener.accept().await.unwrap();
        let ws1 = accept_async(s1).await.unwrap();
        let (s2, _) = listener.accept().await.unwrap();
        let ws2 = accept_async(s2).await.unwrap();
        let (mut t1, mut r1) = ws1.split();
        let (mut t2, mut r2) = ws2.split();
        loop {
            tokio::select! {
                m = r1.next() => match m {
                    Some(Ok(msg)) => {
                        if let Message::Binary(b) = &msg { sink.lock().unwrap().push(b.clone()); }
                        if t2.send(msg).await.is_err() { break }
                    }
                    _ => break,
                },
                m = r2.next() => match m {
                    Some(Ok(msg)) => {
                        if let Message::Binary(b) = &msg { sink.lock().unwrap().push(b.clone()); }
                        if t1.send(msg).await.is_err() { break }
                    }
                    _ => break,
                },
            }
        }
    });
    (port, captured)
}

// Every forwarded byte must hide the marker, fail to parse as a frame, and still
// open under the key (so it's our ciphertext, not noise).
#[tokio::test]
async fn relay_sees_only_ciphertext() {
    const MARKER: &str = "TOP-SECRET-PLAINTEXT-MARKER";

    let (port, captured) = spawn_capturing_relay().await;
    let url = format!("ws://127.0.0.1:{port}/test");
    let cipher = Cipher::generate();

    let bb = spawn_bare_broker(Vec::new());
    let daemon = tokio::spawn(run_relay_daemon(
        vec![RelayLeg {
            dial_url: url.clone(),
            cipher: cipher.clone(),
            profile: ClientProfile::human(),
            peer_accept: None,
        }],
        bb.handle.clone(),
    ));

    let client = RelayClient::new(url.clone(), cipher.clone());
    let mut a = client
        .attach(ClientIdentity::human("test"))
        .await
        .expect("relay attach");

    bb.agent_tx
        .send(AgentEvent::AssistantText {
            text: MARKER.into(),
        })
        .await
        .unwrap();
    expect_text(&mut a, MARKER).await;
    a.ui_tx
        .send(UiEvent::UserMessage {
            text: MARKER.into(),
        })
        .await
        .unwrap();
    match next_event(&mut a).await {
        Some(ControllerEvent::UserMessage { text, .. }) if text == MARKER => {}
        other => panic!("expected echoed marker, got {other:?}"),
    }

    a.ui_tx.send(UiEvent::Quit).await.unwrap();
    assert!(
        next_event(&mut a).await.is_none(),
        "Quit detaches the controller"
    );
    daemon.abort();

    let frames = captured.lock().unwrap();
    assert!(!frames.is_empty(), "the relay forwarded nothing to inspect");
    let marker = MARKER.as_bytes();
    for (i, payload) in frames.iter().enumerate() {
        assert!(
            !payload.windows(marker.len()).any(|w| w == marker),
            "frame {i} leaked the plaintext marker through the relay"
        );
        assert!(
            serde_json::from_slice::<ClientFrame>(payload).is_err()
                && serde_json::from_slice::<ServerFrame>(payload).is_err(),
            "frame {i} deserialized as a cleartext protocol frame"
        );
        assert!(
            cipher.open(payload).is_ok(),
            "frame {i} is not openable ciphertext under the session key"
        );
    }

    bb.task.abort();
}

// Pairs `<room>/host` with `<room>/client` like the real relay, so concurrent clients
// can be tested. The kill switch drops every live pair at once; parked spares survive.
//
// The callback's `Err` type is fixed by tungstenite's `Callback` trait; we only return `Ok`.
#[allow(clippy::result_large_err)]
async fn spawn_role_relay() -> (u16, tokio::sync::broadcast::Sender<()>) {
    use futures::{SinkExt, StreamExt};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::{Mutex, oneshot};
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    type Ws = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;
    #[derive(Default)]
    struct Room {
        hosts: Vec<oneshot::Sender<Ws>>,
        clients: Vec<oneshot::Sender<Ws>>,
    }

    let rooms: Arc<Mutex<HashMap<String, Room>>> = Arc::new(Mutex::new(HashMap::new()));
    let (kill_tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let kill = kill_tx.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let rooms = rooms.clone();
            let kill = kill.clone();
            tokio::spawn(async move {
                let mut path = String::new();
                let ws = accept_hdr_async(stream, |req: &Request, resp: Response| {
                    path = req.uri().path().trim_start_matches('/').to_string();
                    Ok(resp)
                })
                .await
                .unwrap();
                let (room_id, role) = path.rsplit_once('/').unwrap();
                let is_host = role == "host";

                let rx = {
                    let mut map = rooms.lock().await;
                    let room = map.entry(room_id.to_string()).or_default();
                    let opposite = if is_host {
                        &mut room.clients
                    } else {
                        &mut room.hosts
                    };
                    if let Some(tx) = opposite.pop() {
                        let _ = tx.send(ws);
                        return; // the parked partner runs the pump
                    }
                    let (tx, rx) = oneshot::channel::<Ws>();
                    if is_host {
                        &mut room.hosts
                    } else {
                        &mut room.clients
                    }
                    .push(tx);
                    rx
                };
                let partner = match rx.await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                let mut killed = kill.subscribe();
                let (mut at, mut ar) = ws.split();
                let (mut bt, mut br) = partner.split();
                loop {
                    tokio::select! {
                        m = ar.next() => match m {
                            Some(Ok(x)) => if bt.send(x).await.is_err() { break },
                            _ => break,
                        },
                        m = br.next() => match m {
                            Some(Ok(x)) => if at.send(x).await.is_err() { break },
                            _ => break,
                        },
                        _ = killed.recv() => break,
                    }
                }
            });
        }
    });
    (port, kill_tx)
}

#[tokio::test]
async fn relay_two_clients_attach_concurrently() {
    let (port, _kill) = spawn_role_relay().await;
    let base = format!("ws://127.0.0.1:{port}/room1");
    let host_url = format!("{base}/host");
    let client_url = format!("{base}/client");

    let cipher = Cipher::generate();
    let bb = spawn_bare_broker(Vec::new());
    let daemon = tokio::spawn(run_relay_daemon(
        vec![RelayLeg {
            dial_url: host_url,
            cipher: cipher.clone(),
            profile: ClientProfile::human(),
            peer_accept: None,
        }],
        bb.handle.clone(),
    ));

    let client = RelayClient::new(client_url, cipher.clone());
    let mut a = timeout(
        Duration::from_secs(5),
        client.attach(ClientIdentity::human("a")),
    )
    .await
    .expect("A attach timed out")
    .expect("A attach failed");
    let mut b = timeout(
        Duration::from_secs(5),
        client.attach(ClientIdentity::human("b")),
    )
    .await
    .expect("B attach timed out (second concurrent client)")
    .expect("B attach failed");

    bb.agent_tx
        .send(AgentEvent::AssistantText {
            text: "hi-all".into(),
        })
        .await
        .unwrap();
    expect_text(&mut a, "hi-all").await;
    expect_text(&mut b, "hi-all").await;

    daemon.abort();
    bb.task.abort();
}

async fn expect_notice(ctrl: &mut Controller, want: &str) {
    match next_event(ctrl).await {
        Some(ControllerEvent::Notice { text }) if text.contains(want) => {}
        other => panic!("expected a Notice containing {want:?}, got {other:?}"),
    }
}

// After a forced drop the client resumes past the last seen event, with no replay of
// what it already had. A prompt answered during the outage still resolves, since the
// broker matches answers by tool_use_id from any controller.
#[tokio::test]
async fn relay_client_reconnects_and_resumes_after_a_drop() {
    use crate::core::ModelInfo;

    let (port, kill) = spawn_role_relay().await;
    let base = format!("ws://127.0.0.1:{port}/room1");
    let cipher = Cipher::generate();
    let bb = spawn_bare_broker(vec![
        ControllerEvent::SessionInfo {
            model: "m".into(),
            cwd: "/tmp".into(),
            git_branch: None,
            session_id: "s".into(),
            session_name: None,
            auto: false,
        },
        ControllerEvent::Capabilities {
            commands: Vec::new(),
            models: vec![ModelInfo {
                id: "id1".into(),
                label: "L1".into(),
            }],
            mcp: Vec::new(),
            peers: Vec::new(),
        },
    ]);
    let daemon = tokio::spawn(run_relay_daemon(
        vec![RelayLeg {
            dial_url: format!("{base}/host"),
            cipher: cipher.clone(),
            profile: ClientProfile::human(),
            peer_accept: None,
        }],
        bb.handle.clone(),
    ));

    let client = RelayClient::new(format!("{base}/client"), cipher);
    let mut a = timeout(
        Duration::from_secs(5),
        client.attach(ClientIdentity::human("a")),
    )
    .await
    .expect("attach timed out")
    .expect("attach failed");
    assert!(matches!(
        next_event(&mut a).await,
        Some(ControllerEvent::SessionInfo { .. })
    ));
    assert!(matches!(
        next_event(&mut a).await,
        Some(ControllerEvent::Capabilities { .. })
    ));
    for text in ["one", "two"] {
        bb.agent_tx
            .send(AgentEvent::AssistantText { text: text.into() })
            .await
            .unwrap();
        expect_text(&mut a, text).await;
    }
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel::<bool>();
    bb.agent_tx
        .send(AgentEvent::PermissionRequest {
            tool_use_id: "t1".into(),
            tool_name: "Bash".into(),
            summary: "run ls".into(),
            respond: resp_tx,
        })
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut a).await,
        Some(ControllerEvent::PermissionRequest { .. })
    ));

    kill.send(()).unwrap();
    expect_notice(&mut a, "reconnecting").await;
    a.ui_tx
        .send(UiEvent::PermissionResponse {
            tool_use_id: "t1".into(),
            allow: true,
        })
        .await
        .unwrap();
    bb.agent_tx
        .send(AgentEvent::AssistantText {
            text: "three".into(),
        })
        .await
        .unwrap();
    expect_notice(&mut a, "reconnected").await;
    expect_text(&mut a, "three").await;
    match next_event(&mut a).await {
        Some(ControllerEvent::PermissionResolved { allow: true, .. }) => {}
        other => panic!("expected the prompt resolved after reconnect, got {other:?}"),
    }
    assert_eq!(
        timeout(Duration::from_secs(5), resp_rx).await,
        Ok(Ok(true)),
        "the loop never got the answer sent during the outage"
    );
    assert!(
        timeout(Duration::from_millis(300), a.events.recv())
            .await
            .is_err(),
        "a resumed attach replayed events the client already had"
    );

    daemon.abort();
    bb.task.abort();
}

fn agent_id(name: &str) -> ClientIdentity {
    ClientIdentity {
        kind: ClientKind::Agent,
        name: name.into(),
        session_id: None,
        task: None,
    }
}

// B parks an agent leg, A dials it; each lands in the other's PeerSet under the
// name the other side announced, and all four half-channels work.
#[tokio::test]
async fn duplex_peer_edge_mutual_drive_and_observe_over_one_socket() {
    let port = spawn_test_relay().await;
    let url = format!("ws://127.0.0.1:{port}/peerroom");
    let cipher = Cipher::generate();

    let mut bb_a = spawn_bare_broker(Vec::new());
    let mut bb_b = spawn_bare_broker(Vec::new());
    let (reg_a_tx, mut reg_a_rx) = tokio::sync::mpsc::unbounded_channel::<PeerRegistration>();
    let (reg_b_tx, mut reg_b_rx) = tokio::sync::mpsc::unbounded_channel::<PeerRegistration>();

    let who_a = agent_id("peer-a");
    let who_b = agent_id("peer-b");

    let daemon = tokio::spawn(run_relay_daemon(
        vec![RelayLeg {
            dial_url: url.clone(),
            cipher: cipher.clone(),
            profile: ClientProfile::agent(),
            peer_accept: Some(PeerAccept {
                identity: who_b.clone(),
                registrar: reg_b_tx,
            }),
        }],
        bb_b.handle.clone(),
    ));

    let client = RelayClient::new(url.clone(), cipher.clone());
    timeout(
        Duration::from_secs(5),
        client.dial_peer(bb_a.handle.clone(), reg_a_tx, who_a.clone()),
    )
    .await
    .expect("dial_peer timed out")
    .expect("dial_peer failed");

    let peer_b = timeout(Duration::from_secs(5), reg_a_rx.recv())
        .await
        .expect("A registrar timeout")
        .expect("A received peer B");
    let peer_a = timeout(Duration::from_secs(5), reg_b_rx.recv())
        .await
        .expect("B registrar timeout")
        .expect("B received peer A");
    assert_eq!(
        peer_b.who.name, "peer-b",
        "A holds B under B's announced name"
    );
    assert_eq!(
        peer_a.who.name, "peer-a",
        "B holds A under A's announced name"
    );
    assert!(
        !peer_b.supervised && !peer_a.supervised,
        "both edges unsupervised"
    );

    let mut peers_a = PeerSet::default();
    let mut peers_b = PeerSet::default();
    let id_b = peers_a.register(peer_b);
    let id_a = peers_b.register(peer_a);

    bb_b.agent_tx
        .send(AgentEvent::AssistantText {
            text: "from-b".into(),
        })
        .await
        .unwrap();
    match timeout(Duration::from_secs(5), peers_a.recv())
        .await
        .expect("A observe timeout")
    {
        (id, Some(ControllerEvent::AssistantText { text })) => {
            assert_eq!(id, id_b);
            assert_eq!(text, "from-b");
        }
        other => panic!("A expected to observe B's text, got {other:?}"),
    }

    peers_a
        .drive(
            id_b,
            UiEvent::UserMessage {
                text: "a-drives".into(),
            },
        )
        .await;
    match timeout(Duration::from_secs(5), bb_b.loop_ui_rx.recv())
        .await
        .expect("B loop timeout")
    {
        Some((who, UiEvent::UserMessage { text })) => {
            assert_eq!(text, "a-drives");
            assert_eq!(who.expect("stamped sender").name, "peer-a");
        }
        other => panic!("B's loop expected A's drive, got {other:?}"),
    }

    bb_a.agent_tx
        .send(AgentEvent::AssistantText {
            text: "from-a".into(),
        })
        .await
        .unwrap();
    match timeout(Duration::from_secs(5), peers_b.recv())
        .await
        .expect("B observe timeout")
    {
        (id, Some(ControllerEvent::AssistantText { text })) => {
            assert_eq!(id, id_a);
            assert_eq!(text, "from-a");
        }
        other => panic!("B expected to observe A's text, got {other:?}"),
    }

    peers_b
        .drive(
            id_a,
            UiEvent::UserMessage {
                text: "b-drives".into(),
            },
        )
        .await;
    match timeout(Duration::from_secs(5), bb_a.loop_ui_rx.recv())
        .await
        .expect("A loop timeout")
    {
        Some((who, UiEvent::UserMessage { text })) => {
            assert_eq!(text, "b-drives");
            assert_eq!(who.expect("stamped sender").name, "peer-b");
        }
        other => panic!("A's loop expected B's drive, got {other:?}"),
    }

    daemon.abort();
    bb_a.task.abort();
    bb_b.task.abort();
}

#[tokio::test]
async fn dial_peer_without_return_edge_fails_closed() {
    let port = spawn_test_relay().await;
    let url = format!("ws://127.0.0.1:{port}/plainroom");
    let cipher = Cipher::generate();

    let bb = spawn_bare_broker(Vec::new());
    let daemon = tokio::spawn(run_relay_daemon(
        vec![RelayLeg {
            dial_url: url.clone(),
            cipher: cipher.clone(),
            profile: ClientProfile::human(),
            peer_accept: None,
        }],
        bb.handle.clone(),
    ));

    let (reg_tx, mut reg_rx) = tokio::sync::mpsc::unbounded_channel::<PeerRegistration>();
    let bb_dialer = spawn_bare_broker(Vec::new());
    let client = RelayClient::new(url, cipher);
    let result = timeout(
        Duration::from_secs(20),
        client.dial_peer(bb_dialer.handle.clone(), reg_tx, agent_id("peer-a")),
    )
    .await
    .expect("dial_peer must fail closed, not hang");
    assert!(
        result.is_err(),
        "dialing a leg without a return edge must fail, got {result:?}"
    );
    assert!(
        reg_rx.try_recv().is_err(),
        "no forward peer may be registered on a failed dial"
    );

    daemon.abort();
    bb.task.abort();
    bb_dialer.task.abort();
}
