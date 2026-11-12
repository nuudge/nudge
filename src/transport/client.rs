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

// Channel depth between the bridge tasks and the TUI. Matches the in-process
// host's CHANNEL_CAPACITY so backpressure behaves the same across transports.
const CHANNEL_CAPACITY: usize = 64;

// How long a peer dial waits, after `Attached`, for the acceptor's `ReverseAttach`.
// A real `--daemon --peer` sends it as the first post-Attached frame; a non-peer
// target never does, so this bound turns "no return edge" into a clean failure
// instead of a hang.
const REVERSE_ATTACH_TIMEOUT: Duration = Duration::from_secs(5);

// The remote counterpart of `SessionHost`: it owns no loop, only the path to a
// daemon's Unix socket. Each `attach` opens a fresh connection, performs the
// handshake, and spawns two bridge tasks that translate between socket frames and
// the in-memory `Controller` channels the TUI already speaks — so the TUI runs
// unchanged whether it drives a local host or a remote daemon. A connection lives
// exactly as long as one foreground attachment: detaching or quitting drops the
// `Controller`, which tears the connection down.
pub struct SocketClient {
    socket_path: PathBuf,
}

impl SocketClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    // Open + handshake on a fresh connection. `after_seq` is the resume cursor
    // (None = full replay). Split from the trait method so a future auto-reconnect
    // path can request replay-from-cursor without a new public API.
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
            Some(ServerFrame::Busy) => return Ok(None), // session gone (broker down)
            // The daemon must answer the handshake with Attached or Busy; anything
            // else (including EOF) is a protocol fault.
            other => anyhow::bail!("unexpected handshake response: {other:?}"),
        }

        let (event_tx, event_rx) = mpsc::unbounded_channel::<ControllerEvent>();
        let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>(CHANNEL_CAPACITY);

        // Reader bridge: inbound Event frames → the controller's event stream.
        tokio::spawn(pump_reads(reader, event_tx));
        // Writer bridge: the controller's UiEvents → outbound Command frames.
        tokio::spawn(pump_writes(writer, ui_rx));

        Ok(Some(Controller {
            events: event_rx,
            ui_tx,
        }))
    }
}

impl SessionHandle for SocketClient {
    // The TUI always attaches with a full replay (`after_seq = None`), matching the
    // in-process behaviour where a foreground reattach replays the whole buffer.
    // Seq-aware resume is wired through `connect` but unused until drops are real
    // (relay / cellular in a later phase).
    //
    // Silent on failure (Err → None): a foreground reattach runs this *under the
    // TUI*, where any stderr write would corrupt the alternate screen and desync
    // ratatui's diff (the stderr-under-TUI rule). The one place that needs the
    // error's cause — the very first connect, before the TUI owns the screen — calls
    // `connect` directly (see `run_connect`) and reports it there.
    async fn attach(&self, who: ClientIdentity) -> Option<Controller> {
        self.connect(None, who).await.ok().flatten()
    }

    // No-op: the connection lives in the bridge tasks, not here. The TUI drops the
    // `Controller` immediately after calling this (run_loop: detach → events = None,
    // ui_tx = None), which closes `ui_rx` (the writer sends an explicit Detach frame
    // and drops the write half) and the event receiver (the reader winds down on the
    // next send or on socket EOF). The daemon reads that as a pause-in-place detach.
    fn detach(&self) {}
}

// The relayed counterpart of `SocketClient`: it owns no loop, only the relay URL
// (rendezvous path included) it dials OUT to. `attach` opens a fresh WebSocket
// through the relay, runs the same handshake as the Unix client — only the codec
// differs (`WsReader`/`WsWriter` instead of `LineReader`/`LineWriter`), so the TUI is
// unchanged across transports. Unlike a Unix socket, a relayed connection can die
// under a live TUI (a laptop sleeping), so the connection is supervised and redialed
// (see `stay_attached`). Every frame is end-to-end encrypted under `cipher` before it
// hits the relay (8.2-d).
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

    // Dial the relay + handshake, then keep the TUI attached across drops. Split from
    // the trait method (mirroring `SocketClient::connect`) so the first connect — run
    // before the TUI owns the screen — can surface its error cause via `run_connect`.
    pub(crate) async fn connect(
        &self,
        after_seq: Option<u64>,
        who: ClientIdentity,
    ) -> Result<Option<Controller>> {
        let Some(conn) = self.open(after_seq, who.clone()).await? else {
            return Ok(None); // session gone (broker down)
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

    // One connection: dial the relay and run the attach handshake. `None` = reachable,
    // but the session is gone (Busy).
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

    // Dial a peer session over the relay and establish the DUPLEX edge (#60): both
    // directed edges of the mutual pair ride this one socket. `self_broker` is the
    // dialer's OWN broker (so the far side can drive/observe it — the reverse edge),
    // `registrar` hands the forward peer to the dialer's live loop (#52), and `who` is
    // the dialer's announced identity. Non-blocking: it performs the handshake,
    // registers the forward peer, attaches the reverse controller, spawns the duplex
    // pump, and returns — the caller's loop is never held on network I/O. `Err` = the
    // far side is gone or offered no return edge (not a `--daemon --peer`).
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
        let mut writer = WsWriter::new(sink, self.cipher.clone());
        let mut reader = WsReader::new(stream, self.cipher.clone());

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
        // The acceptor announces itself in ReverseAttach as the first frame after
        // Attached. A non-peer target (a phone daemon) ignores the offer and streams
        // forward Events instead; bound the wait so we don't hang, then fail cleanly.
        let peer_who = match timeout(REVERSE_ATTACH_TIMEOUT, reader.recv()).await {
            Ok(Ok(Some(ServerFrame::ReverseAttach { who }))) => who,
            _ => anyhow::bail!(
                "the far side offered no return edge — the target must be a `--daemon --peer` session"
            ),
        };

        // Forward edge: the dialer holds the acceptor as a peer (drives/observes it).
        // The pump feeds inbound Events into `fwd_ev_tx` and drains outbound drives from
        // `fwd_ui_rx`; the Controller with the matching ends goes to the loop's PeerSet.
        let (fwd_ev_tx, fwd_ev_rx) = mpsc::unbounded_channel::<ControllerEvent>();
        let (fwd_ui_tx, fwd_ui_rx) = mpsc::channel::<UiEvent>(CHANNEL_CAPACITY);
        let forward = Controller {
            events: fwd_ev_rx,
            ui_tx: fwd_ui_tx,
        };
        if registrar
            .send(PeerRegistration::new(forward, peer_who.clone()))
            .is_err()
        {
            anyhow::bail!("the dialer's loop is gone");
        }

        // Reverse edge: attach a controller to the dialer's OWN broker as the acceptor
        // (provenance: the reverse grant is implied by running /connect-peer). Its
        // events go out as ReverseEvent; inbound ReverseCommands drive it.
        let ac = match self_broker
            .attach_as(peer_who, ClientProfile::agent())
            .await
        {
            Some(c) => c,
            None => anyhow::bail!("the dialer's broker is gone"),
        };

        tokio::spawn(dial_pump(reader, writer, fwd_ev_tx, fwd_ui_rx, ac));
        Ok(())
    }
}

impl SessionHandle for RelayClient {
    // Full-replay attach, silent on failure — same contract as `SocketClient`
    // (see its `attach` for why a foreground reattach must not write to stderr).
    async fn attach(&self, who: ClientIdentity) -> Option<Controller> {
        self.connect(None, who).await.ok().flatten()
    }

    // No-op: the connection lives in `stay_attached`. Dropping the `Controller` closes
    // `ui_rx` (it sends a Detach frame, then drops the socket) and the event receiver,
    // which the daemon reads as a pause-in-place detach — and which stops any redial.
    fn detach(&self) {}
}

// Forward controller events the daemon streams us into the TUI's event channel.
// Ends when the daemon closes the connection (clean EOF, or the host shutting down)
// or the TUI drops the receiver; dropping `event_tx` here closes the TUI's stream,
// which the run loop reads as "session/connection ended".
async fn pump_reads<R: FrameReader<ServerFrame> + 'static>(
    mut reader: R,
    event_tx: mpsc::UnboundedSender<ControllerEvent>,
) {
    loop {
        match reader.recv().await {
            Ok(Some(ServerFrame::Event { event, .. })) => {
                // (`seq` is the daemon's cursor bookkeeping; the client ignores it
                // until auto-reconnect needs to resume from a cursor.)
                if event_tx.send(event).is_err() {
                    break; // TUI dropped the stream (detach / quit)
                }
            }
            // Post-handshake Attached/Busy are unexpected; ignore defensively.
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break, // daemon closed the connection, or a fault
        }
    }
}

// Forward the TUI's UiEvents to the daemon as Command frames. When the TUI drops
// its `ui_tx` (detach / quit), `recv` returns None: send an explicit Detach so the
// daemon pauses in place rather than inferring it from EOF, then drop the write
// half (closing the connection).
async fn pump_writes<W: FrameWriter<ClientFrame> + 'static>(
    mut writer: W,
    mut ui_rx: mpsc::Receiver<UiEvent>,
) {
    while let Some(ev) = ui_rx.recv().await {
        if writer.send(&ClientFrame::Command(ev)).await.is_err() {
            return; // connection gone; the reader task will also wind down
        }
    }
    let _ = writer.send(&ClientFrame::Detach).await;
}

// Keep a TUI attached over one relay connection after another — the phone's
// ChatViewModel pattern. When the socket dies under a still-attached TUI, redial the
// same pairing and resume after the last event it saw (`after_seq`), so only the gap
// replays and the transcript isn't duplicated. Ends, dropping `event_tx` (which the TUI
// reads as "session ended"), once the TUI lets go or the session is gone.
async fn stay_attached(
    client: RelayClient,
    who: ClientIdentity,
    mut last_seq: Option<u64>,
    (mut reader, mut writer): (RelayReader, RelayWriter),
    event_tx: mpsc::UnboundedSender<ControllerEvent>,
    mut ui_rx: mpsc::Receiver<UiEvent>,
) {
    while pump_relay(
        &mut reader,
        &mut writer,
        &event_tx,
        &mut ui_rx,
        &mut last_seq,
    )
    .await
    {
        let _ = event_tx.send(ControllerEvent::Notice {
            text: "relay connection lost — reconnecting…".into(),
        });
        match redial(&client, &who, last_seq, &event_tx).await {
            Some(conn) => (reader, writer) = conn,
            None => return,
        }
        let _ = event_tx.send(ControllerEvent::Notice {
            text: "reconnected".into(),
        });
    }
}

// Bridge the TUI's channels to one live connection. `true` = the connection dropped
// under the TUI (worth a redial); `false` = the TUI let go (detach / quit).
async fn pump_relay(
    reader: &mut RelayReader,
    writer: &mut RelayWriter,
    event_tx: &mpsc::UnboundedSender<ControllerEvent>,
    ui_rx: &mut mpsc::Receiver<UiEvent>,
    last_seq: &mut Option<u64>,
) -> bool {
    let mut ping = keepalive_timer();
    loop {
        tokio::select! {
            frame = reader.recv() => match frame {
                Ok(Some(ServerFrame::Event { seq, event })) => {
                    *last_seq = Some(seq);
                    if event_tx.send(event).is_err() {
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return true,
            },
            ui = ui_rx.recv() => match ui {
                Some(ui) => {
                    // The daemon answers a Quit by closing; that close is no drop.
                    let quitting = matches!(ui, UiEvent::Quit);
                    let sent = writer.send(&ClientFrame::Command(ui)).await.is_ok();
                    if quitting {
                        return false;
                    }
                    if !sent {
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

// Redial with backoff — 1s, 2s, 4s, then every 8s, the phone's schedule — for as long
// as the TUI stays attached: with no Reattach button to fall back on, giving up would
// strand a TUI whose network is slow to return after a wake. `None` = stop: the TUI
// let go, or the session is gone.
async fn redial(
    client: &RelayClient,
    who: &ClientIdentity,
    last_seq: Option<u64>,
    event_tx: &mpsc::UnboundedSender<ControllerEvent>,
) -> Option<(RelayReader, RelayWriter)> {
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
    }
}

// The dialer's half of the duplex peer edge (#60): both directed edges over one
// socket. Forward — the dialer holds the acceptor as a peer: inbound `Event`s feed the
// forward peer's stream (observe), outbound drives from `fwd_ui_rx` go out as `Command`
// (drive). Reverse — the acceptor drives/observes the dialer: `ac` (attached to the
// dialer's own broker) streams its events out as `ReverseEvent`, and inbound
// `ReverseCommand`s drive it. When the socket drops (or either side tears its half
// down) the loop ends: dropping `fwd_ev_tx` closes the forward peer's stream (the
// dialer's PeerSet reaps it), and dropping `ac` detaches the reverse controller.
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
                // Drive the acceptor (forward).
                Some(ui) => if writer.send(&ClientFrame::Command(ui)).await.is_err() { break },
                // The loop reaped the forward peer → tear the edge down.
                None => break,
            },
            ev = ac.events.recv() => match ev {
                // The dialer's own broker events → the acceptor observes them (reverse).
                Some(event) => {
                    let seq = rev_seq;
                    rev_seq += 1;
                    if writer.send(&ClientFrame::ReverseEvent { seq, event }).await.is_err() { break }
                }
                // The dialer's broker went away.
                None => break,
            },
            frame = reader.recv() => match frame {
                // The acceptor's events → the forward peer's observe stream.
                Ok(Some(ServerFrame::Event { event, .. })) => { let _ = fwd_ev_tx.send(event); }
                // The acceptor drives the dialer (reverse) → the dialer's own broker.
                Ok(Some(ServerFrame::ReverseCommand(ui))) => { let _ = ac.ui_tx.send(ui).await; }
                // ReverseAttach was consumed at the handshake; Attached/Busy are noise here.
                Ok(Some(_)) => {}
                // Socket closed or a fault → the edge is dropped.
                Ok(None) | Err(_) => break,
            },
        }
    }
}
