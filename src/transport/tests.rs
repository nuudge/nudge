// End-to-end transport test: a real `SocketClient` driving a real broker over a
// real Unix socket via the standalone daemon (`run_daemon`, the `--daemon --socket`
// debug host). The broker has no agent loop behind it — events are injected directly
// (see `spawn_bare_broker`) — so this exercises only the transport seam: framing, the
// attach handshake, seq assignment + `after_seq` filtering, and the guest-quit-as-detach
// rule. Broker-internal invariants (single-attach lock, force-takeover, permission
// correlation) are covered by the host unit tests; wire framing by the wire tests.
use std::path::PathBuf;
use std::time::Duration;

use tokio::time::timeout;

use super::encryption::Cipher;
use super::wire::{ClientFrame, ServerFrame};
use super::{RelayClient, SocketClient, bind_listener, run_daemon, run_relay_daemon};
use crate::core::host::spawn_bare_broker;
use crate::core::{AgentEvent, Controller, ControllerEvent, SessionHandle, UiEvent};

// A collision-free socket path under the system temp dir. The name is kept short
// on purpose: a Unix socket path must fit in `sockaddr_un.sun_path` (104 bytes on
// macOS), and the temp dir alone can be ~50, so a full UUID would overflow it.
fn unique_socket_path() -> PathBuf {
    let token = &uuid::Uuid::new_v4().simple().to_string()[..12];
    std::env::temp_dir().join(format!("ra-it-{token}.sock"))
}

// Open a fresh connection at the given resume cursor, failing loudly instead of
// hanging if the daemon never accepts or refuses. The handoff server accepts one
// connection at a time, so a reattach simply blocks until the previous connection's
// detach lets the server loop back to `accept` — no busy/retry dance needed.
async fn attach_at(client: &SocketClient, after_seq: Option<u64>) -> Controller {
    timeout(Duration::from_secs(5), client.connect(after_seq))
        .await
        .expect("attach timed out")
        .expect("attach errored")
        .expect("attach refused (Busy) — unexpected with a serial accept loop")
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
    let path = unique_socket_path();
    let bb = spawn_bare_broker(Vec::new());
    let listener = bind_listener(&path).expect("bind daemon listener");
    let serve = tokio::spawn(run_daemon(listener, path.clone(), bb.handle.clone()));

    let client = SocketClient::new(path.clone());

    // Attach over the socket via the product path (`SessionHandle::attach`, full
    // replay). The buffer is empty, so nothing replays yet.
    let mut a = client.attach().await.expect("initial attach");

    // Two injected loop events buffer and stream live to A, in order.
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

    // Disconnect mid-session: dropping the controller sends a Detach frame; the
    // session lives on headless. (The serial server returns from A's handler here,
    // freeing it to accept the next connection.)
    drop(a);

    // Reattach with a resume cursor: after_seq = Some(0) skips "one" (seq 0) and
    // replays only "two" (seq 1) — the replay-from-seq correctness the roadmap calls
    // out. Live events then continue on the same stream.
    let mut b = attach_at(&client, Some(0)).await;
    expect_text(&mut b, "two").await;
    bb.agent_tx
        .send(AgentEvent::AssistantText {
            text: "three".into(),
        })
        .await
        .unwrap();
    expect_text(&mut b, "three").await;

    // A guest quit is a detach, not a kill: B's connection ends, but the session
    // survives.
    b.ui_tx.send(UiEvent::Quit).await.unwrap();
    assert!(
        next_event(&mut b).await.is_none(),
        "a guest Quit must close the guest's connection"
    );

    // Proof the session survived the guest quit: a fresh attach replays the whole
    // history, "three" included.
    let mut c = attach_at(&client, None).await;
    expect_text(&mut c, "one").await;
    expect_text(&mut c, "two").await;
    expect_text(&mut c, "three").await;

    // Tear down the accept loop + broker and remove the socket file.
    drop(c);
    serve.abort();
    drop(bb);
    let _ = std::fs::remove_file(&path);
}

// A throwaway in-test relay: it accepts two WebSocket connections and pipes
// messages between them verbatim, ignoring the rendezvous path (only one pair
// exists here). Pairing-by-id, isolation, and the unpaired-waiter timeout are
// covered by the relay binary's own tests; this exercises the *agent* side of the
// WS transport — the `Ws*` codec, `RelayClient`, and the daemon's dial-out path.
async fn spawn_test_relay() -> u16 {
    use futures::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        // The daemon dials first (parks), the client second; upgrade each as it
        // arrives so the daemon's handshake completes before the client connects.
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

// A full session drives over the relay, plaintext: the daemon dials out and hosts
// the broker, a `RelayClient` attaches through the relay, a loop event reaches the
// client (daemon→client decode), a user message round-trips (client→daemon→echo),
// and a remote Quit *detaches* the controller while the daemon keeps running — a
// daemon ends only when its process stops, never because a controller left.
#[tokio::test]
async fn relay_round_trip_event_command_and_quit_detaches() {
    let port = spawn_test_relay().await;
    let url = format!("ws://127.0.0.1:{port}/test");

    let cipher = Cipher::generate();
    let bb = spawn_bare_broker(Vec::new());
    let daemon = tokio::spawn(run_relay_daemon(
        url.clone(),
        cipher.clone(),
        bb.handle.clone(),
    ));

    let client = RelayClient::new(url.clone(), cipher.clone());
    let mut a = client.attach().await.expect("relay attach");

    // Injected loop event streams live to the client through the WS codec.
    bb.agent_tx
        .send(AgentEvent::AssistantText { text: "hi".into() })
        .await
        .unwrap();
    expect_text(&mut a, "hi").await;

    // A user message goes client→daemon→broker; the broker echoes it back over the
    // relay, proving both directions of the command path through the codec.
    a.ui_tx
        .send(UiEvent::UserMessage {
            text: "drive".into(),
        })
        .await
        .unwrap();
    match next_event(&mut a).await {
        Some(ControllerEvent::UserMessage { text }) if text == "drive" => {}
        other => panic!("expected echoed UserMessage, got {other:?}"),
    }

    // Quit over the relay is a detach: the controller's stream closes, but the
    // daemon survives (it re-dials for the next controller rather than returning).
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

// Like `spawn_test_relay`, but records every Binary payload it forwards — so a test
// can inspect exactly what the relay sees and prove it is opaque ciphertext.
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

// The ciphertext-blind proof (8.2-e): drive a full session through the relay with a
// distinctive plaintext marker in both directions, then assert every byte the relay
// forwarded is opaque — no marker leaks, nothing deserializes as a protocol frame —
// while still decrypting under the key (proving it's our ciphertext, not noise).
#[tokio::test]
async fn relay_sees_only_ciphertext() {
    const MARKER: &str = "TOP-SECRET-PLAINTEXT-MARKER";

    let (port, captured) = spawn_capturing_relay().await;
    let url = format!("ws://127.0.0.1:{port}/test");
    let cipher = Cipher::generate();

    let bb = spawn_bare_broker(Vec::new());
    let daemon = tokio::spawn(run_relay_daemon(
        url.clone(),
        cipher.clone(),
        bb.handle.clone(),
    ));

    let client = RelayClient::new(url.clone(), cipher.clone());
    let mut a = client.attach().await.expect("relay attach");

    // Send the marker each way: a loop event out, a user message in (echoed back).
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
        Some(ControllerEvent::UserMessage { text }) if text == MARKER => {}
        other => panic!("expected echoed marker, got {other:?}"),
    }

    // The round-trip is done and every marker-bearing frame has been forwarded and
    // captured; a remote Quit no longer stops the daemon, so just abort the task.
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
