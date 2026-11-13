use anyhow::{Context, Result};
use futures::StreamExt;
use futures::stream::{SplitSink, SplitStream};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use super::encryption::Cipher;
use super::wire::{
    ClientFrame, FrameReader, FrameWriter, LineReader, LineWriter, ServerFrame, WsReader, WsWriter,
    keepalive_timer,
};
use crate::core::{
    BrokerHandle, ClientIdentity, ClientProfile, Controller, ControllerEvent, PeerRegistration,
    SessionHandle, UiEvent,
};

// Matches the in-process host's CHANNEL_CAPACITY so backpressure behaves the same
// across transports.
const CHANNEL_CAPACITY: usize = 64;

// A non-peer target never sends `ReverseAttach`; this bound turns that into a clean
// failure instead of a hang.
const REVERSE_ATTACH_TIMEOUT: Duration = Duration::from_secs(5);

// Several relay pair timeouts' worth of failed redials: past a sleep's network
// recovery, so the likelier cause is a restarted daemon (see `redial`).
const RESTART_HINT_AFTER: Duration = Duration::from_secs(120);

// Bridges a daemon's Unix socket to the in-memory `Controller` channels, so the TUI
// runs unchanged against a remote daemon. Dropping the `Controller` tears the
// connection down.
pub struct SocketClient {
    socket_path: PathBuf,
}

impl SocketClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    pub(crate) async fn connect(
        &self,
        after_seq: Option<u64>,
        who: ClientIdentity,
    ) -> Result<Option<Controller>> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| format!("connecting to daemon at {}", self.socket_path.display()))?;
        let (read_half, write_half) = stream.into_split();
        let mut reader = LineReader(BufReader::new(read_half));
        let mut writer = LineWriter(write_half);

        writer
            .send(&ClientFrame::Attach {
                after_seq,
                who,
                reverse_offer: false,
            })
            .await?;
        let handshake: Option<ServerFrame> = reader.recv().await?;
        match handshake {
            Some(ServerFrame::Attached) => {}
            Some(ServerFrame::Busy) => return Ok(None),
            other => anyhow::bail!("unexpected handshake response: {other:?}"),
        }

        let (event_tx, event_rx) = mpsc::unbounded_channel::<ControllerEvent>();
        let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>(CHANNEL_CAPACITY);

        tokio::spawn(pump_reads(reader, event_tx));
        tokio::spawn(pump_writes(writer, ui_rx));

        Ok(Some(Controller {
            events: event_rx,
            ui_tx,
        }))
    }

    pub async fn dial_peer(
        &self,
        self_broker: BrokerHandle,
        registrar: mpsc::UnboundedSender<PeerRegistration>,
        who: ClientIdentity,
    ) -> Result<()> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| format!("connecting to session at {}", self.socket_path.display()))?;
        let (read_half, write_half) = stream.into_split();
        peer_handshake(
            LineReader(BufReader::new(read_half)),
            LineWriter(write_half),
            self_broker,
            registrar,
            who,
            true,
        )
        .await
    }
}

impl SessionHandle for SocketClient {
    // Silent on failure: a foreground reattach runs under the TUI, where a stderr
    // write would corrupt the screen. The first connect reports errors via `connect`.
    async fn attach(&self, who: ClientIdentity) -> Option<Controller> {
        self.connect(None, who).await.ok().flatten()
    }

    // The connection lives in the bridge tasks; dropping the `Controller` ends it.
    fn detach(&self) {}
}

// Unlike a Unix socket, a relayed connection can die under a live TUI (a laptop
// sleeping), so it is supervised and redialed (see `stay_attached`).
#[derive(Clone)]
pub struct RelayClient {
    relay_url: String,
    cipher: Cipher,
}

type RelayWs = WebSocketStream<MaybeTlsStream<TcpStream>>;
type RelayReader = WsReader<SplitStream<RelayWs>>;
type RelayWriter = WsWriter<SplitSink<RelayWs, Message>>;

impl RelayClient {
    pub fn new(relay_url: String, cipher: Cipher) -> Self {
        Self { relay_url, cipher }
    }

    // Split from the trait method so the first connect, before the TUI owns the
    // screen, can surface its error.
    pub(crate) async fn connect(
        &self,
        after_seq: Option<u64>,
        who: ClientIdentity,
    ) -> Result<Option<Controller>> {
        let Some(conn) = self.open(after_seq, who.clone()).await? else {
            return Ok(None);
        };
        let (event_tx, event_rx) = mpsc::unbounded_channel::<ControllerEvent>();
        let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>(CHANNEL_CAPACITY);
        tokio::spawn(stay_attached(
            self.clone(),
            who,
            after_seq,
            conn,
            event_tx,
            ui_rx,
        ));

        Ok(Some(Controller {
            events: event_rx,
            ui_tx,
        }))
    }

    // `None` = reachable, but the session is gone (Busy).
    async fn open(
        &self,
        after_seq: Option<u64>,
        who: ClientIdentity,
    ) -> Result<Option<(RelayReader, RelayWriter)>> {
        let (ws, _resp) = connect_async(self.relay_url.as_str())
            .await
            .with_context(|| format!("dialing relay at {}", self.relay_url))?;
        let (sink, stream) = ws.split();
        let mut writer = WsWriter::new(sink, self.cipher.clone());
        let mut reader = WsReader::new(stream, self.cipher.clone());

        writer
            .send(&ClientFrame::Attach {
                after_seq,
                who,
                reverse_offer: false,
            })
            .await?;
        let handshake: Option<ServerFrame> = reader.recv().await?;
        match handshake {
            Some(ServerFrame::Attached) => Ok(Some((reader, writer))),
            Some(ServerFrame::Busy) => Ok(None),
            other => anyhow::bail!("unexpected handshake response: {other:?}"),
        }
    }

    // Both directed edges of the mutual peer pair ride this one socket; `self_broker`
    // is the dialer's own broker, exposed to the far side.
    pub async fn dial_peer(
        &self,
        self_broker: BrokerHandle,
        registrar: mpsc::UnboundedSender<PeerRegistration>,
        who: ClientIdentity,
    ) -> Result<()> {
        let (ws, _resp) = connect_async(self.relay_url.as_str())
            .await
            .with_context(|| format!("dialing peer relay at {}", self.relay_url))?;
        let (sink, stream) = ws.split();
        peer_handshake(
            WsReader::new(stream, self.cipher.clone()),
            WsWriter::new(sink, self.cipher.clone()),
            self_broker,
            registrar,
            who,
            false,
        )
        .await
    }
}

impl SessionHandle for RelayClient {
    // Silent on failure, same as `SocketClient::attach`.
    async fn attach(&self, who: ClientIdentity) -> Option<Controller> {
        self.connect(None, who).await.ok().flatten()
    }

    // Dropping the `Controller` detaches and stops any redial in `stay_attached`.
    fn detach(&self) {}
}

// Returns once the pump is spawned so the caller's loop never blocks on I/O.
// `trust_grant` honours the target's `supervised` grant; only a same-user socket target
// is trusted, so a remote target can't make itself supervised.
async fn peer_handshake<R, W>(
    mut reader: R,
    mut writer: W,
    self_broker: BrokerHandle,
    registrar: mpsc::UnboundedSender<PeerRegistration>,
    who: ClientIdentity,
    trust_grant: bool,
) -> Result<()>
where
    R: FrameReader<ServerFrame> + 'static,
    W: FrameWriter<ClientFrame> + 'static,
{
    writer
        .send(&ClientFrame::Attach {
            after_seq: None,
            who,
            reverse_offer: true,
        })
        .await?;
    match reader.recv().await? {
        Some(ServerFrame::Attached) => {}
        Some(ServerFrame::Busy) => anyhow::bail!("the peer session has ended"),
        other => anyhow::bail!("unexpected handshake response: {other:?}"),
    }
    let (peer_who, supervised) = match timeout(REVERSE_ATTACH_TIMEOUT, reader.recv()).await {
        Ok(Ok(Some(ServerFrame::ReverseAttach { who, supervised }))) => {
            (who, trust_grant && supervised)
        }
        _ => anyhow::bail!(
            "the far side offered no return edge — the target must be a `--daemon --peer` session"
        ),
    };

    let (fwd_ev_tx, fwd_ev_rx) = mpsc::unbounded_channel::<ControllerEvent>();
    let (fwd_ui_tx, fwd_ui_rx) = mpsc::channel::<UiEvent>(CHANNEL_CAPACITY);
    let forward = Controller {
        events: fwd_ev_rx,
        ui_tx: fwd_ui_tx,
    };
    if registrar
        .send(PeerRegistration {
            supervised,
            ..PeerRegistration::new(forward, peer_who.clone())
        })
        .is_err()
    {
        anyhow::bail!("the dialer's loop is gone");
    }

    // The reverse grant is implied by the user running /connect-peer. A supervised
    // target gets no command scope back, like a spawned child's return edge.
    let reverse = if supervised {
        ClientProfile::agent_peer()
    } else {
        ClientProfile::agent()
    };
    let ac = match self_broker.attach_as(peer_who, reverse).await {
        Some(c) => c,
        None => anyhow::bail!("the dialer's broker is gone"),
    };

    tokio::spawn(dial_pump(reader, writer, fwd_ev_tx, fwd_ui_rx, ac));
    Ok(())
}

async fn pump_reads<R: FrameReader<ServerFrame> + 'static>(
    mut reader: R,
    event_tx: mpsc::UnboundedSender<ControllerEvent>,
) {
    loop {
        match reader.recv().await {
            Ok(Some(ServerFrame::Event { event, .. })) => {
                if event_tx.send(event).is_err() {
                    break;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
}

// Sends an explicit Detach when the TUI lets go, so the daemon pauses in place
// rather than inferring it from EOF.
async fn pump_writes<W: FrameWriter<ClientFrame> + 'static>(
    mut writer: W,
    mut ui_rx: mpsc::Receiver<UiEvent>,
) {
    while let Some(ev) = ui_rx.recv().await {
        if writer.send(&ClientFrame::Command(ev)).await.is_err() {
            return;
        }
    }
    let _ = writer.send(&ClientFrame::Detach).await;
}

// Redials the same pairing whenever the socket dies under a still-attached TUI,
// resuming after `last_seq` so only the gap replays.
async fn stay_attached(
    client: RelayClient,
    who: ClientIdentity,
    mut last_seq: Option<u64>,
    (mut reader, mut writer): (RelayReader, RelayWriter),
    event_tx: mpsc::UnboundedSender<ControllerEvent>,
    mut ui_rx: mpsc::Receiver<UiEvent>,
) {
    let mut unsent = None;
    while pump_relay(
        &mut reader,
        &mut writer,
        &event_tx,
        &mut ui_rx,
        &mut last_seq,
        &mut unsent,
    )
    .await
    {
        // Input typed during the redial queues in `ui_rx` and flushes in order after.
        let _ = event_tx.send(ControllerEvent::Notice {
            text: "relay connection lost — reconnecting…".into(),
        });
        match redial(&client, &who, last_seq, &event_tx, RESTART_HINT_AFTER).await {
            Some(conn) => (reader, writer) = conn,
            None => return,
        }
        let _ = event_tx.send(ControllerEvent::Notice {
            text: "reconnected".into(),
        });
    }
}

// `true` = the connection dropped (worth a redial); `false` = the TUI let go. A
// command whose send failed is kept in `unsent` for the next connection.
async fn pump_relay(
    reader: &mut RelayReader,
    writer: &mut RelayWriter,
    event_tx: &mpsc::UnboundedSender<ControllerEvent>,
    ui_rx: &mut mpsc::Receiver<UiEvent>,
    last_seq: &mut Option<u64>,
    unsent: &mut Option<ClientFrame>,
) -> bool {
    if let Some(frame) = unsent.take()
        && writer.send(&frame).await.is_err()
    {
        *unsent = Some(frame);
        return true;
    }
    let mut ping = keepalive_timer();
    loop {
        tokio::select! {
            frame = reader.recv() => match frame {
                Ok(Some(ServerFrame::Event { seq, event })) => {
                    // The resume cursor only advances past what the TUI actually got.
                    if event_tx.send(event).is_err() {
                        break;
                    }
                    *last_seq = Some(seq);
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return true,
            },
            ui = ui_rx.recv() => match ui {
                Some(ui) => {
                    // The daemon answers a Quit by closing; that close is no drop.
                    let quitting = matches!(ui, UiEvent::Quit);
                    let frame = ClientFrame::Command(ui);
                    let sent = writer.send(&frame).await.is_ok();
                    if quitting {
                        return false;
                    }
                    if !sent {
                        *unsent = Some(frame);
                        return true;
                    }
                }
                None => break,
            },
            _ = ping.tick() => {
                if FrameWriter::<ClientFrame>::keepalive(writer).await.is_err() {
                    return true;
                }
            }
        }
    }
    let _ = writer.send(&ClientFrame::Detach).await;
    false
}

// Backoff 1s, 2s, 4s, then every 8s, forever while the TUI stays attached: giving up
// would strand a TUI whose network is slow to return after a wake.
//
// A restarted daemon mints a new room, so every attempt then waits out the relay's
// pair timeout silently; after `restart_hint_after`, tell the user once.
async fn redial(
    client: &RelayClient,
    who: &ClientIdentity,
    last_seq: Option<u64>,
    event_tx: &mpsc::UnboundedSender<ControllerEvent>,
    restart_hint_after: Duration,
) -> Option<(RelayReader, RelayWriter)> {
    let started = tokio::time::Instant::now();
    let mut hinted = false;
    let mut attempt: u32 = 0;
    loop {
        let backoff = Duration::from_secs(1 << attempt.min(3));
        attempt += 1;
        let dialed = tokio::select! {
            _ = event_tx.closed() => return None,
            dialed = async {
                tokio::time::sleep(backoff).await;
                client.open(last_seq, who.clone()).await
            } => dialed,
        };
        match dialed {
            Ok(Some(conn)) => return Some(conn),
            Ok(None) => return None,
            Err(_) => {}
        }
        if !hinted && started.elapsed() >= restart_hint_after {
            hinted = true;
            let _ = event_tx.send(ControllerEvent::Notice {
                text: "still reconnecting — if the session was restarted, its pairing code \
                       changed; quit and reconnect with the new code"
                    .into(),
            });
        }
    }
}

// Ends when the socket drops or either half is torn down: dropping `fwd_ev_tx` lets
// the PeerSet reap the forward peer, and dropping `ac` detaches the reverse controller.
async fn dial_pump<R, W>(
    mut reader: R,
    mut writer: W,
    fwd_ev_tx: mpsc::UnboundedSender<ControllerEvent>,
    mut fwd_ui_rx: mpsc::Receiver<UiEvent>,
    mut ac: Controller,
) where
    R: FrameReader<ServerFrame> + 'static,
    W: FrameWriter<ClientFrame> + 'static,
{
    let mut rev_seq: u64 = 0;
    let mut ping = keepalive_timer();
    loop {
        tokio::select! {
            _ = ping.tick() => if writer.keepalive().await.is_err() { break },
            ui = fwd_ui_rx.recv() => match ui {
                Some(ui) => if writer.send(&ClientFrame::Command(ui)).await.is_err() { break },
                None => break,
            },
            ev = ac.events.recv() => match ev {
                Some(event) => {
                    let seq = rev_seq;
                    rev_seq += 1;
                    if writer.send(&ClientFrame::ReverseEvent { seq, event }).await.is_err() { break }
                }
                None => break,
            },
            frame = reader.recv() => match frame {
                Ok(Some(ServerFrame::Event { event, .. })) => { let _ = fwd_ev_tx.send(event); }
                Ok(Some(ServerFrame::ReverseCommand(ui))) => { let _ = ac.ui_tx.send(ui).await; }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn redial_hints_once_at_a_stale_code() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let client = RelayClient::new(
            format!("ws://127.0.0.1:{port}/r/client"),
            Cipher::generate(),
        );
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<ControllerEvent>();
        let redialing = tokio::spawn(async move {
            redial(
                &client,
                &ClientIdentity::human("a"),
                None,
                &event_tx,
                Duration::ZERO,
            )
            .await;
        });

        match timeout(Duration::from_secs(3), event_rx.recv()).await {
            Ok(Some(ControllerEvent::Notice { text })) => assert!(text.contains("pairing code")),
            other => panic!("expected the stale-code hint, got {other:?}"),
        }
        assert!(
            timeout(Duration::from_secs(3), event_rx.recv())
                .await
                .is_err(),
            "the hint repeated on a later failed attempt"
        );
        assert!(
            !redialing.is_finished(),
            "redial gave up instead of retrying"
        );
        redialing.abort();
    }
}
