// A ciphertext-blind WebSocket relay both peers dial OUT to (the only direction that
// crosses NAT). It pairs connections on path `<room_id>/<role>` and copies messages
// verbatim; the plaintext role is its only way to tell host from client.
//
// One session's daemon is the single host and opens one connection per client, since
// a byte-blind pipe can't fan one socket to many. An arriver pairs only with the
// opposite role. Clients park bounded by the pair timeout; host spares park forever.
//
// Liveness is per hop: the relay pings each leg itself and never forwards Ping/Pong,
// since a relayed pong would make a dead far end look alive.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, oneshot};
use tokio::time::{Instant, interval_at, sleep, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

// Bounds unpaired client connections so they can't accumulate in the rendezvous map.
const DEFAULT_PAIR_TIMEOUT: Duration = Duration::from_secs(30);

// Must stay well above the endpoints' 20s ping interval, or live-but-quiet legs get
// cut. The relay pings at a third of it, so a client that only pongs stays alive.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

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

#[derive(Clone, Copy)]
enum Role {
    Host,
    Client,
}

impl Role {
    fn opposite(self) -> Role {
        match self {
            Role::Host => Role::Client,
            Role::Client => Role::Host,
        }
    }
}

// The token lets a timed-out waiter evict only its own entry, never a later waiter
// on the same queue.
#[derive(Default)]
struct Room {
    host_conns: VecDeque<(u64, oneshot::Sender<Ws>)>,
    client_conns: VecDeque<(u64, oneshot::Sender<Ws>)>,
}

impl Room {
    // Reads false in the re-dial gap between spares, so it gates teardown, not admission.
    fn host_present(&self) -> bool {
        !self.host_conns.is_empty()
    }

    fn removable(&self) -> bool {
        !self.host_present() && self.client_conns.is_empty()
    }
}

type Rendezvous = Arc<Mutex<HashMap<String, Room>>>;

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(0);

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let listener = TcpListener::bind(&cli.listen)
        .await
        .with_context(|| format!("binding {}", cli.listen))?;
    eprintln!("[relay] listening on {}", cli.listen);
    run(
        listener,
        Duration::from_secs(cli.pair_timeout),
        IDLE_TIMEOUT,
    )
    .await
}

async fn run(listener: TcpListener, pair_timeout: Duration, idle_timeout: Duration) -> Result<()> {
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
            if let Err(e) = handle_conn(stream, rv, pair_timeout, idle_timeout).await {
                eprintln!("[relay] {peer} ended: {e:#}");
            }
        });
    }
}

// The callback's `Err` type is fixed by tungstenite's `Callback` trait, and we only
// ever return `Ok`.
#[allow(clippy::result_large_err)]
async fn handle_conn(
    stream: TcpStream,
    rendezvous: Rendezvous,
    pair_timeout: Duration,
    idle_timeout: Duration,
) -> Result<()> {
    let mut path = String::new();
    let ws = accept_hdr_async(stream, |req: &Request, resp: Response| {
        path = req.uri().path().trim_start_matches('/').to_string();
        Ok(resp)
    })
    .await
    .context("websocket handshake failed")?;

    let (id, role) = match path.rsplit_once('/') {
        Some((id, "host")) if !id.is_empty() => (id.to_string(), Role::Host),
        Some((id, "client")) if !id.is_empty() => (id.to_string(), Role::Client),
        _ => anyhow::bail!("client did not name a rendezvous id and role (path '{path}')"),
    };

    park_or_pair(ws, id, role, rendezvous, pair_timeout, idle_timeout).await
}

// The party that parks owns the pump; the second arriver hands its socket over.
//
// A parked socket is read while it waits, to answer pings, notice EOF, and hold early
// frames (e.g. a client's Attach). Dead or silent waiters are evicted so nobody pairs
// with a corpse.
async fn park_or_pair(
    mut ws: Ws,
    id: String,
    mut role: Role,
    rendezvous: Rendezvous,
    pair_timeout: Duration,
    idle_timeout: Duration,
) -> Result<()> {
    loop {
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        let mut rx = {
            let mut map = rendezvous.lock().await;
            let room = map.entry(id.clone()).or_default();
            let opposite = match role {
                Role::Host => &mut room.client_conns,
                Role::Client => &mut room.host_conns,
            };
            if let Some((_partner_token, partner_tx)) = opposite.pop_front() {
                if room.removable() {
                    map.remove(&id);
                }
                return partner_tx
                    .send(ws)
                    .map_err(|_| anyhow::anyhow!("partner for '{id}' vanished before pairing"));
            }
            let (tx, rx) = oneshot::channel::<Ws>();
            match role {
                Role::Host => room.host_conns.push_back((token, tx)),
                Role::Client => room.client_conns.push_back((token, tx)),
            }
            rx
        };

        let mut early = Vec::new();
        let mut ping = interval_at(Instant::now() + idle_timeout / 3, idle_timeout / 3);
        let idle = sleep(idle_timeout);
        tokio::pin!(idle);
        let lonely = async move {
            match role {
                Role::Host => std::future::pending().await,
                Role::Client => sleep(pair_timeout).await,
            }
        };
        tokio::pin!(lonely);
        let paired = loop {
            tokio::select! {
                partner = &mut rx => break partner.map_err(|_| "its queue entry vanished"),
                msg = ws.next() => match msg {
                    Some(Ok(m)) => {
                        idle.as_mut().reset(Instant::now() + idle_timeout);
                        match m {
                            Message::Ping(_) | Message::Pong(_) => {}
                            Message::Close(_) => break Err("it closed while parked"),
                            m => early.push(m),
                        }
                    }
                    _ => break Err("it dropped while parked"),
                },
                _ = ping.tick() => {
                    if !send_within(&mut ws, Message::Ping(Vec::new()), idle_timeout).await {
                        break Err("a ping to it failed");
                    }
                }
                _ = &mut idle => break Err("it went silent while parked"),
                _ = &mut lonely => break Err("no partner connected in time"),
            }
        };
        match paired {
            Ok(partner) => return pipe(ws, early, partner, idle_timeout).await,
            Err(why) => {
                evict(&rendezvous, &id, role, token).await;
                // Handoffs happen under the rendezvous lock, so after `evict` no partner
                // is in flight: one already landed in `rx`, or none ever will. Re-pair it.
                match rx.try_recv() {
                    Ok(orphan) => {
                        ws = orphan;
                        role = role.opposite();
                    }
                    Err(_) => anyhow::bail!("unpaired waiter for '{id}' dropped: {why}"),
                }
            }
        }
    }
}

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

// `early` is what the parked side `a` sent before `b` arrived. However the pipe ends,
// both legs get a Close so the surviving end learns of it cleanly.
async fn pipe(a: Ws, early: Vec<Message>, b: Ws, idle_timeout: Duration) -> Result<()> {
    let (mut a_tx, mut a_rx) = a.split();
    let (mut b_tx, mut b_rx) = b.split();
    for m in early {
        if !send_within(&mut b_tx, m, idle_timeout).await {
            return Ok(());
        }
    }
    let mut ping = interval_at(Instant::now() + idle_timeout / 3, idle_timeout / 3);
    let a_idle = sleep(idle_timeout);
    let b_idle = sleep(idle_timeout);
    tokio::pin!(a_idle, b_idle);
    loop {
        tokio::select! {
            msg = a_rx.next() => {
                a_idle.as_mut().reset(Instant::now() + idle_timeout);
                if !forward(msg, &mut b_tx, idle_timeout).await {
                    break;
                }
            }
            msg = b_rx.next() => {
                b_idle.as_mut().reset(Instant::now() + idle_timeout);
                if !forward(msg, &mut a_tx, idle_timeout).await {
                    break;
                }
            }
            _ = ping.tick() => {
                if !send_within(&mut a_tx, Message::Ping(Vec::new()), idle_timeout).await
                    || !send_within(&mut b_tx, Message::Ping(Vec::new()), idle_timeout).await
                {
                    break;
                }
            }
            _ = &mut a_idle => break,
            _ = &mut b_idle => break,
        }
    }
    let _ = timeout(
        idle_timeout,
        futures::future::join(a_tx.close(), b_tx.close()),
    )
    .await;
    Ok(())
}

// `false` ends the pipe; a Close is forwarded first.
async fn forward(
    msg: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    to: &mut SplitSink<Ws, Message>,
    idle_timeout: Duration,
) -> bool {
    match msg {
        Some(Ok(Message::Ping(_) | Message::Pong(_))) => true,
        Some(Ok(m)) => {
            let closing = m.is_close();
            send_within(to, m, idle_timeout).await && !closing
        }
        _ => false,
    }
}

// A send to a half-open peer can block once its TCP buffer fills, which would also
// stall every deadline in the caller's select loop — so bound it like the idle wait.
async fn send_within<S>(sink: &mut S, m: Message, limit: Duration) -> bool
where
    S: futures::Sink<Message> + Unpin,
{
    matches!(timeout(limit, sink.send(m)).await, Ok(Ok(())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;
    use tokio_tungstenite::MaybeTlsStream;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    async fn start_relay(pair_timeout: Duration) -> u16 {
        start_relay_idle(pair_timeout, IDLE_TIMEOUT).await
    }

    async fn start_relay_idle(pair_timeout: Duration, idle_timeout: Duration) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(run(listener, pair_timeout, idle_timeout));
        port
    }

    async fn dial(port: u16, id: &str) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
        let url = format!("ws://127.0.0.1:{port}/{id}");
        connect_async(url).await.expect("dial relay").0
    }

    fn bin(bytes: &[u8]) -> Message {
        Message::Binary(bytes.to_vec())
    }

    // Reading past the relay's pings also answers them, keeping this leg alive.
    async fn next_data(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> Option<Message> {
        timeout(Duration::from_secs(2), async {
            loop {
                match ws.next().await {
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(m)) => return Some(m),
                    _ => return None,
                }
            }
        })
        .await
        .expect("recv timed out")
    }

    async fn recv(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> Vec<u8> {
        next_data(ws)
            .await
            .expect("stream ended")
            .into_data()
            .to_vec()
    }

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

    #[tokio::test]
    async fn different_ids_are_isolated() {
        let port = start_relay(Duration::from_secs(5)).await;
        let mut a1 = dial(port, "room1/host").await;
        let mut a2 = dial(port, "room1/client").await;
        let mut b = dial(port, "room2/client").await;

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

    #[tokio::test]
    async fn lonely_waiter_is_dropped_after_timeout() {
        let port = start_relay(Duration::from_millis(300)).await;
        let mut a = dial(port, "solo/client").await;

        let res = timeout(Duration::from_secs(2), a.next())
            .await
            .expect("relay never closed the lonely waiter");
        match res {
            None => {}
            Some(Ok(m)) => assert!(m.is_close(), "expected close, got {m:?}"),
            Some(Err(_)) => {}
        }
    }

    #[tokio::test]
    async fn host_serves_two_sequential_clients() {
        let port = start_relay(DEFAULT_PAIR_TIMEOUT).await;

        let mut host1 = dial(port, "room/host").await;
        let mut a = dial(port, "room/client").await;
        a.send(bin(b"to-host-1")).await.unwrap();
        assert_eq!(recv(&mut host1).await, b"to-host-1");

        let mut host2 = dial(port, "room/host").await;
        let mut b = dial(port, "room/client").await;
        b.send(bin(b"to-host-2")).await.unwrap();
        assert_eq!(recv(&mut host2).await, b"to-host-2");
    }

    // The daemon's re-dial gap: a client may land while no host spare is parked.
    #[tokio::test]
    async fn client_waits_for_host_then_pairs() {
        let port = start_relay(DEFAULT_PAIR_TIMEOUT).await;

        let mut c = dial(port, "room/client").await;
        let mut h = dial(port, "room/host").await;
        h.send(bin(b"hello-client")).await.unwrap();
        assert_eq!(recv(&mut c).await, b"hello-client");
    }

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

    #[tokio::test]
    async fn parked_host_is_not_timed_out() {
        let port = start_relay(Duration::from_millis(200)).await;
        let mut h = dial(port, "room/host").await;

        assert!(
            timeout(Duration::from_millis(500), h.next()).await.is_err(),
            "a parked host must not be closed by the pair timeout"
        );

        let mut c = dial(port, "room/client").await;
        c.send(bin(b"late")).await.unwrap();
        assert_eq!(recv(&mut h).await, b"late");
    }

    #[tokio::test]
    async fn dead_parked_host_is_evicted_not_paired() {
        let port = start_relay(DEFAULT_PAIR_TIMEOUT).await;
        let corpse = dial(port, "room/host").await;
        drop(corpse);
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut live = dial(port, "room/host").await;
        let mut c = dial(port, "room/client").await;
        c.send(bin(b"to-live")).await.unwrap();
        assert_eq!(recv(&mut live).await, b"to-live");
    }

    // Half-open (e.g. its laptop slept): the relay's own pings go unanswered.
    #[tokio::test]
    async fn silent_parked_host_is_evicted() {
        let port = start_relay_idle(DEFAULT_PAIR_TIMEOUT, Duration::from_millis(300)).await;
        let _silent = dial(port, "room/host").await; // held open, never read
        tokio::time::sleep(Duration::from_millis(600)).await;

        let mut live = dial(port, "room/host").await;
        let mut c = dial(port, "room/client").await;
        c.send(bin(b"to-live")).await.unwrap();
        assert_eq!(recv(&mut live).await, b"to-live");
    }

    #[tokio::test]
    async fn early_frames_from_parked_client_survive_pairing() {
        let port = start_relay(DEFAULT_PAIR_TIMEOUT).await;
        let mut c = dial(port, "room/client").await;
        c.send(bin(b"attach")).await.unwrap();
        c.send(bin(b"second")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut h = dial(port, "room/host").await;
        assert_eq!(recv(&mut h).await, b"attach");
        assert_eq!(recv(&mut h).await, b"second");
    }

    #[tokio::test]
    async fn ping_is_answered_per_hop_not_forwarded() {
        let port = start_relay(DEFAULT_PAIR_TIMEOUT).await;
        let mut a = dial(port, "room/host").await;
        let mut b = dial(port, "room/client").await;

        a.send(Message::Ping(b"probe".to_vec())).await.unwrap();
        let pong = timeout(Duration::from_secs(2), a.next())
            .await
            .expect("no pong from the relay hop")
            .unwrap()
            .unwrap();
        assert_eq!(pong, Message::Pong(b"probe".to_vec()));
        assert!(
            timeout(Duration::from_millis(300), b.next()).await.is_err(),
            "the ping crossed the pair"
        );

        a.send(bin(b"still-piped")).await.unwrap();
        assert_eq!(recv(&mut b).await, b"still-piped");
    }

    // A client that never pings but keeps reading answers the relay's pings, so it
    // outlives the idle deadline both parked and as a quiet pair.
    #[tokio::test]
    async fn reading_client_that_never_pings_stays_connected() {
        let idle = Duration::from_millis(300);
        let port = start_relay_idle(DEFAULT_PAIR_TIMEOUT, idle).await;
        let mut h = dial(port, "room/host").await;
        assert!(
            timeout(idle * 4, next_data(&mut h)).await.is_err(),
            "a parked reader was closed despite answering pings"
        );

        let mut c = dial(port, "room/client").await;
        c.send(bin(b"paired")).await.unwrap();
        assert_eq!(recv(&mut h).await, b"paired");
        let (h_quiet, c_quiet) = tokio::join!(
            timeout(idle * 4, next_data(&mut h)),
            timeout(idle * 4, next_data(&mut c)),
        );
        assert!(
            h_quiet.is_err() && c_quiet.is_err(),
            "a quiet pair of readers was closed despite answering pings"
        );

        h.send(bin(b"h2c")).await.unwrap();
        assert_eq!(recv(&mut c).await, b"h2c");
        c.send(bin(b"c2h")).await.unwrap();
        assert_eq!(recv(&mut h).await, b"c2h");
    }

    #[tokio::test]
    async fn silent_leg_closes_pair_with_close_to_survivor() {
        let port = start_relay_idle(DEFAULT_PAIR_TIMEOUT, Duration::from_millis(300)).await;
        let mut h = dial(port, "room/host").await;
        let mut c = dial(port, "room/client").await;
        c.send(bin(b"hi")).await.unwrap();
        assert_eq!(recv(&mut h).await, b"hi");

        // `c` is never read again, so it stops answering the relay's pings.
        match next_data(&mut h).await {
            Some(m) => assert!(m.is_close(), "expected Close, got {m:?}"),
            None => panic!("survivor saw the socket drop without a Close"),
        }
    }
}
