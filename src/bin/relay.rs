// A tiny, ciphertext-blind WebSocket relay (Phase 8.2-b): the publicly-reachable
// box both peers dial OUT to — the only direction that crosses NAT/CGNAT. Its
// whole job is to pair two connections that name the same rendezvous id (taken
// from the URL path) and copy WebSocket messages between them verbatim. It never
// inspects a payload, knows nothing of the agent's wire protocol, and has no
// notion of "host" vs "controller" — so once the app-layer E2E lands (8.2-d) it
// forwards only ciphertext, with no change here. Sharing no code with the agent,
// it lives as a standalone binary that can lift into its own repo to deploy.
//
// Pairing handoff: the first arriver for an id parks a oneshot sender in the
// rendezvous map and waits (bounded); the second arriver takes that sender, hands
// its socket over, and exits, leaving the first task to own the bidirectional
// pump. TLS and per-hop keepalive are deploy concerns (8.3); locally this is plain
// ws:// on a loopback port.

use std::collections::HashMap;
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
// Rendezvous map: id → (token, sender). The token lets a timed-out waiter evict
// only *its own* entry, never a later waiter that reused the same id.
type Rendezvous = Arc<Mutex<HashMap<String, (u64, oneshot::Sender<Ws>)>>>;

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
    // Capture the rendezvous id from the upgrade request path during the WS
    // handshake. The path is the only plaintext the relay ever sees — a room
    // number, not secret content (the pairing key that seeds E2E never comes here).
    let mut id = String::new();
    let ws = accept_hdr_async(stream, |req: &Request, resp: Response| {
        id = req.uri().path().trim_start_matches('/').to_string();
        Ok(resp)
    })
    .await
    .context("websocket handshake failed")?;

    if id.is_empty() {
        anyhow::bail!("client did not name a rendezvous id (empty path)");
    }

    // Already a peer waiting on this id? Then we're the second arriver: take their
    // sender and hand our socket over — they own the bidirectional pump.
    let waiting = rendezvous.lock().await.remove(&id);
    if let Some((_token, partner_tx)) = waiting {
        return partner_tx
            .send(ws)
            .map_err(|_| anyhow::anyhow!("partner for '{id}' vanished before pairing"));
    }

    // First arriver: register a waiter and block (bounded) for a partner. We do
    // NOT read this socket while parked — anything the peer sends early just sits
    // in the TCP buffer until the pump starts, so no message is lost.
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel::<Ws>();
    rendezvous.lock().await.insert(id.clone(), (token, tx));

    match tokio::time::timeout(pair_timeout, rx).await {
        Ok(Ok(partner)) => pipe(ws, partner).await,
        _ => {
            // Timed out (or the sender was dropped): evict our entry, but only if
            // it's still ours — a later waiter may have reused the id meanwhile.
            let mut map = rendezvous.lock().await;
            if matches!(map.get(&id), Some((t, _)) if *t == token) {
                map.remove(&id);
            }
            anyhow::bail!("no partner connected for '{id}' within {pair_timeout:?}")
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

    // Two clients on the same id are paired; opaque bytes flow both directions.
    #[tokio::test]
    async fn pairs_and_pipes_both_directions() {
        let port = start_relay(DEFAULT_PAIR_TIMEOUT).await;
        let mut a = dial(port, "room1").await;
        let mut b = dial(port, "room1").await;

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
        let mut a1 = dial(port, "room1").await;
        let mut a2 = dial(port, "room1").await; // pairs with a1
        let mut b = dial(port, "room2").await; // lonely, different id

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

    // An unpaired waiter is closed once the pairing timeout elapses.
    #[tokio::test]
    async fn lonely_waiter_is_dropped_after_timeout() {
        let port = start_relay(Duration::from_millis(300)).await;
        let mut a = dial(port, "solo").await;

        let res = timeout(Duration::from_secs(2), a.next())
            .await
            .expect("relay never closed the lonely waiter");
        match res {
            None => {} // stream ended
            Some(Ok(m)) => assert!(m.is_close(), "expected close, got {m:?}"),
            Some(Err(_)) => {} // reset is acceptable too
        }
    }
}
