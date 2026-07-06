// A tiny, ciphertext-blind WebSocket relay (Phase 8.2-b): the publicly-reachable
// box both peers dial OUT to — the only direction that crosses NAT/CGNAT. Its whole
// job is to pair connections that name the same rendezvous id (the URL path is
// `<room_id>/<role>`) and copy WebSocket messages between a pair verbatim. It never
// inspects a payload — so once the app-layer E2E lands (8.2-d) it forwards only
// ciphertext, with no change here. The one thing it reads is the plaintext `role`
// segment (`host` or `client`): the relay can't open the encrypted attach frame, so
// the role is the only signal telling the session host apart from a front-end.
// Sharing no code with the agent, it lives as a standalone binary that can lift into
// its own repo to deploy.
//
// Room model: one session's daemon is the single *host* (it opens one connection per
// client — a byte-blind pipe joins exactly two sockets, so it can't fan one host
// socket to many). A room holds two queues of parked waiters, `host_conns` and
// `client_conns`; an arriver pairs with a waiter in the *opposite* queue (so two
// clients never pair with each other) or parks in its own. A client parks bounded
// (a lonely client — no host during a re-dial gap, or a hostless room — is closed on
// timeout); a host spare parks indefinitely, since waiting for the next client is its
// whole job. The paired-with party owns the bidirectional pump. TLS and per-hop
// keepalive are deploy concerns (8.3); locally this is plain ws:// on a loopback port.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

// How long a first arriver waits for its partner before the relay closes it, so
// unpaired connections can't accumulate in the rendezvous map.
const DEFAULT_PAIR_TIMEOUT: Duration = Duration::from_secs(30);

/// A ciphertext-blind WebSocket rendezvous for nudge.
#[derive(Parser)]
#[command(name = "relay", version)]
struct Cli {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:9000", value_name = "addr")]
    listen: String,

    /// Seconds an unpaired connection waits for its partner before being closed.
    #[arg(long, default_value_t = DEFAULT_PAIR_TIMEOUT.as_secs(), value_name = "secs")]
    pair_timeout: u64,
}

type Ws = WebSocketStream<TcpStream>;

// Which side of a session a connection dialed as. The only plaintext the relay reads
// (from the path's trailing segment) — it can't see the encrypted attach frame.
#[derive(Clone, Copy)]
enum Role {
    Host,
    Client,
}

// One rendezvous room: parked waiters split by role. Each entry is `(token, sender)`;
// the token lets a timed-out or dropped waiter evict only *its own* entry, never a
// later waiter that pushed onto the same queue.
#[derive(Default)]
struct Room {
    host_conns: VecDeque<(u64, oneshot::Sender<Ws>)>,
    client_conns: VecDeque<(u64, oneshot::Sender<Ws>)>,
}

impl Room {
    // A host spare is parked right now. Derived from the queue (no separate field to
    // keep in sync); reads false in the brief re-dial gap after a spare is consumed
    // and before the daemon parks the next one — so it gates teardown, not admission.
    fn host_present(&self) -> bool {
        !self.host_conns.is_empty()
    }

    // Nothing left to rendezvous: no host spare parked and no client waiting. The room
    // is host-anchored, so this is the one place it gets torn down.
    fn removable(&self) -> bool {
        !self.host_present() && self.client_conns.is_empty()
    }
}

// Rendezvous map: room id → its parked waiters. A room is created on first arrival
// and removed once both its queues drain.
type Rendezvous = Arc<Mutex<HashMap<String, Room>>>;

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(0);

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let listener = TcpListener::bind(&cli.listen)
        .await
        .with_context(|| format!("binding {}", cli.listen))?;
    eprintln!("[relay] listening on {}", cli.listen);
    run(listener, Duration::from_secs(cli.pair_timeout)).await
}

// Accept forever, handling each connection in its own task. Never returns under
// normal operation (the bind is the only fallible step, done by the caller).
async fn run(listener: TcpListener, pair_timeout: Duration) -> Result<()> {
    let rendezvous: Rendezvous = Arc::new(Mutex::new(HashMap::new()));
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("[relay] accept error: {e:#}");
                continue;
            }
        };
        let rv = rendezvous.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, rv, pair_timeout).await {
                eprintln!("[relay] {peer} ended: {e:#}");
            }
        });
    }
}

// The handshake callback's `Err` type (`ErrorResponse`, a full HTTP response) is
// fixed by tungstenite's `Callback` trait, not ours to shrink — and we only ever
// return `Ok`, so the large-error lint doesn't apply here.
#[allow(clippy::result_large_err)]
async fn handle_conn(
    stream: TcpStream,
    rendezvous: Rendezvous,
    pair_timeout: Duration,
) -> Result<()> {
    // Capture the request path during the WS handshake — the only plaintext the relay
    // ever sees. It is `<room_id>/<role>`: the room id is the secret rendezvous number
    // (not content — the E2E key never comes here), the role is `host` or `client`.
    let mut path = String::new();
    let ws = accept_hdr_async(stream, |req: &Request, resp: Response| {
        path = req.uri().path().trim_start_matches('/').to_string();
        Ok(resp)
    })
    .await
    .context("websocket handshake failed")?;

    // Split off the trailing role segment; whatever precedes it is the room id.
    let (id, role) = match path.rsplit_once('/') {
        Some((id, "host")) if !id.is_empty() => (id.to_string(), Role::Host),
        Some((id, "client")) if !id.is_empty() => (id.to_string(), Role::Client),
        _ => anyhow::bail!("client did not name a rendezvous id and role (path '{path}')"),
    };

    park_or_pair(ws, id, role, rendezvous, pair_timeout).await
}

// Pair with a waiter in the *opposite* queue if one exists (so two clients never pair
// with each other), else park in this role's own queue and wait for a partner. The
// party that parks owns the pump; the party that arrives second hands its socket over
// and exits. A client's wait is bounded by `pair_timeout`; a host spare parks with no
// timeout (waiting for the next client is its whole purpose). While parked we do NOT
// read the socket — anything the peer sends early just sits in the TCP buffer until
// the pump starts, so no message is lost.
async fn park_or_pair(
    ws: Ws,
    id: String,
    role: Role,
    rendezvous: Rendezvous,
    pair_timeout: Duration,
) -> Result<()> {
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let rx = {
        let mut map = rendezvous.lock().await;
        let room = map.entry(id.clone()).or_default();
        let opposite = match role {
            Role::Host => &mut room.client_conns,
            Role::Client => &mut room.host_conns,
        };
        if let Some((_partner_token, partner_tx)) = opposite.pop_front() {
            // A partner is parked: hand our socket over — it owns the pump — and exit.
            if room.removable() {
                map.remove(&id);
            }
            return partner_tx
                .send(ws)
                .map_err(|_| anyhow::anyhow!("partner for '{id}' vanished before pairing"));
        }
        // No partner: park in our own queue and wait.
        let (tx, rx) = oneshot::channel::<Ws>();
        match role {
            Role::Host => room.host_conns.push_back((token, tx)),
            Role::Client => room.client_conns.push_back((token, tx)),
        }
        rx
    };

    // Hosts wait indefinitely; clients are bounded so a lonely one can't accumulate.
    let paired = match role {
        Role::Host => rx.await.ok(),
        Role::Client => tokio::time::timeout(pair_timeout, rx)
            .await
            .ok()
            .and_then(|r| r.ok()),
    };
    match paired {
        Some(partner) => pipe(ws, partner).await,
        None => {
            evict(&rendezvous, &id, role, token).await;
            anyhow::bail!("no partner connected for '{id}' within {pair_timeout:?}")
        }
    }
}

// Remove our own parked entry (matched by token, so a later waiter on the same queue
// is untouched) after a timeout or a dropped sender, and drop the room if it's now
// empty. A no-op if the room or our entry is already gone.
async fn evict(rendezvous: &Rendezvous, id: &str, role: Role, token: u64) {
    let mut map = rendezvous.lock().await;
    if let Some(room) = map.get_mut(id) {
        let queue = match role {
            Role::Host => &mut room.host_conns,
            Role::Client => &mut room.client_conns,
        };
        queue.retain(|(t, _)| *t != token);
        if room.removable() {
            map.remove(id);
        }
    }
}

// Copy messages both ways until either side closes or errors. Every frame is
// forwarded verbatim — the relay never looks inside. A Close is forwarded, then
// the loop ends and both sockets drop.
async fn pipe(a: Ws, b: Ws) -> Result<()> {
    let (mut a_tx, mut a_rx) = a.split();
    let (mut b_tx, mut b_rx) = b.split();
    loop {
        tokio::select! {
            msg = a_rx.next() => match msg {
                Some(Ok(m)) => {
                    let closing = m.is_close();
                    if b_tx.send(m).await.is_err() || closing {
                        break;
                    }
                }
                _ => break, // a closed or errored
            },
            msg = b_rx.next() => match msg {
                Some(Ok(m)) => {
                    let closing = m.is_close();
                    if a_tx.send(m).await.is_err() || closing {
                        break;
                    }
                }
                _ => break, // b closed or errored
            },
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;
    use tokio_tungstenite::MaybeTlsStream;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    // Start a relay on an ephemeral loopback port and return it.
    async fn start_relay(pair_timeout: Duration) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(run(listener, pair_timeout));
        port
    }

    async fn dial(port: u16, id: &str) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
        let url = format!("ws://127.0.0.1:{port}/{id}");
        connect_async(url).await.expect("dial relay").0
    }

    fn bin(bytes: &[u8]) -> Message {
        Message::Binary(bytes.to_vec())
    }

    // Await one Binary message's payload, failing loudly instead of hanging.
    async fn recv(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> Vec<u8> {
        timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("recv timed out")
            .expect("stream ended")
            .expect("recv errored")
            .into_data()
            .to_vec()
    }

    // A host and a client on the same id are paired; opaque bytes flow both directions.
    #[tokio::test]
    async fn pairs_and_pipes_both_directions() {
        let port = start_relay(DEFAULT_PAIR_TIMEOUT).await;
        let mut a = dial(port, "room1/host").await;
        let mut b = dial(port, "room1/client").await;

        a.send(bin(b"hello-b")).await.unwrap();
        let got = timeout(Duration::from_secs(2), b.next())
            .await
            .expect("b receive timed out")
            .unwrap()
            .unwrap();
        assert_eq!(got.into_data().to_vec(), b"hello-b".to_vec());

        b.send(bin(b"hello-a")).await.unwrap();
        let got = timeout(Duration::from_secs(2), a.next())
            .await
            .expect("a receive timed out")
            .unwrap()
            .unwrap();
        assert_eq!(got.into_data().to_vec(), b"hello-a".to_vec());
    }

    // A message on one id never reaches a client on a different id.
    #[tokio::test]
    async fn different_ids_are_isolated() {
        let port = start_relay(Duration::from_secs(5)).await;
        let mut a1 = dial(port, "room1/host").await;
        let mut a2 = dial(port, "room1/client").await; // pairs with a1
        let mut b = dial(port, "room2/client").await; // lonely, different id

        a1.send(bin(b"secret")).await.unwrap();
        let got = timeout(Duration::from_secs(2), a2.next())
            .await
            .expect("a2 receive timed out")
            .unwrap()
            .unwrap();
        assert_eq!(got.into_data().to_vec(), b"secret".to_vec());

        assert!(
            timeout(Duration::from_millis(300), b.next()).await.is_err(),
            "a message crossed rendezvous ids"
        );
    }

    // An unpaired *client* is closed once the pairing timeout elapses (a host spare is
    // not — see `parked_host_is_not_timed_out`).
    #[tokio::test]
    async fn lonely_waiter_is_dropped_after_timeout() {
        let port = start_relay(Duration::from_millis(300)).await;
        let mut a = dial(port, "solo/client").await;

        let res = timeout(Duration::from_secs(2), a.next())
            .await
            .expect("relay never closed the lonely waiter");
        match res {
            None => {} // stream ended
            Some(Ok(m)) => assert!(m.is_close(), "expected close, got {m:?}"),
            Some(Err(_)) => {} // reset is acceptable too
        }
    }

    // The room survives a pairing: a host serves two clients in turn (the daemon parks
    // a fresh spare after each), proving the room isn't consumed like the old FIFO pair.
    #[tokio::test]
    async fn host_serves_two_sequential_clients() {
        let port = start_relay(DEFAULT_PAIR_TIMEOUT).await;

        let mut host1 = dial(port, "room/host").await;
        let mut a = dial(port, "room/client").await;
        a.send(bin(b"to-host-1")).await.unwrap();
        assert_eq!(recv(&mut host1).await, b"to-host-1");

        // The daemon parks another spare; a second client pairs with the same room.
        let mut host2 = dial(port, "room/host").await;
        let mut b = dial(port, "room/client").await;
        b.send(bin(b"to-host-2")).await.unwrap();
        assert_eq!(recv(&mut host2).await, b"to-host-2");
    }

    // A client arriving before any host parks (not fails); the host pairs with it.
    // This is the re-dial gap: a client can land while the host pool is momentarily
    // empty and must wait, not be rejected.
    #[tokio::test]
    async fn client_waits_for_host_then_pairs() {
        let port = start_relay(DEFAULT_PAIR_TIMEOUT).await;

        let mut c = dial(port, "room/client").await;
        let mut h = dial(port, "room/host").await;
        h.send(bin(b"hello-client")).await.unwrap();
        assert_eq!(recv(&mut c).await, b"hello-client");
    }

    // Two clients on the same id never pair with each other — both are lonely and get
    // closed on timeout (role-awareness: a client only pairs with a host).
    #[tokio::test]
    async fn two_clients_do_not_pair_with_each_other() {
        let port = start_relay(Duration::from_millis(300)).await;
        let mut c1 = dial(port, "room/client").await;
        let mut c2 = dial(port, "room/client").await;

        for c in [&mut c1, &mut c2] {
            let res = timeout(Duration::from_secs(2), c.next())
                .await
                .expect("a client that mispaired would stay open past the timeout");
            match res {
                None => {}
                Some(Ok(m)) => assert!(m.is_close(), "expected close, got {m:?}"),
                Some(Err(_)) => {}
            }
        }
    }

    // A parked host spare outlives the pair timeout (only clients are bounded), and
    // still pairs once a client finally arrives.
    #[tokio::test]
    async fn parked_host_is_not_timed_out() {
        let port = start_relay(Duration::from_millis(200)).await;
        let mut h = dial(port, "room/host").await;

        // No close within a window well past the client timeout.
        assert!(
            timeout(Duration::from_millis(500), h.next()).await.is_err(),
            "a parked host must not be closed by the pair timeout"
        );

        let mut c = dial(port, "room/client").await;
        c.send(bin(b"late")).await.unwrap();
        assert_eq!(recv(&mut h).await, b"late");
    }
}
