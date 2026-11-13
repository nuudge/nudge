use anyhow::{Context, Result};
use futures::StreamExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;

use super::encryption::Cipher;
use super::wire::{
    ClientFrame, FrameReader, FrameWriter, LineReader, LineWriter, ServerFrame, WsReader, WsWriter,
    keepalive_timer,
};
use crate::core::{
    BrokerHandle, ClientIdentity, ClientProfile, Controller, ControllerEvent, HandoffStatus,
    PeerRegistration, SessionHandle, UiEvent,
};

// A stale socket file (nothing listening) is removed and rebound; a live one means
// another host owns the path, so refuse rather than steal it.
pub fn bind_listener(path: &Path) -> Result<UnixListener> {
    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                anyhow::bail!("a session is already listening at {}", path.display());
            }
            std::fs::remove_file(path)
                .with_context(|| format!("removing stale socket {}", path.display()))?;
            UnixListener::bind(path)
                .with_context(|| format!("binding socket {}", path.display()))?
        }
        Err(e) => return Err(e).with_context(|| format!("binding socket {}", path.display())),
    };
    // Whoever can connect gets full rights, so the trust boundary is the OS user.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting socket {}", path.display()))?;
    Ok(listener)
}

// Runs until aborted; the caller owns the socket file. Shares the process with the
// local TUI, so it must never write stderr.
pub async fn serve_socket(listener: UnixListener, broker: BrokerHandle, peer_accept: PeerAccept) {
    loop {
        let Ok((stream, _addr)) = listener.accept().await else {
            // Errors like EMFILE persist; without a pause this would spin a core.
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let broker = broker.clone();
        let peer_accept = peer_accept.clone();
        tokio::spawn(async move {
            let (read_half, write_half) = stream.into_split();
            let mut reader = LineReader(BufReader::new(read_half));
            let writer = LineWriter(write_half);
            let Some((after_seq, who, reverse_offer)) = recv_attach(&mut reader).await else {
                return;
            };
            let grant = if reverse_offer {
                Grant::Peer
            } else {
                Grant::Kind
            };
            bridge(
                reader,
                writer,
                &broker,
                after_seq,
                who,
                grant,
                reverse_offer,
                Some(peer_accept),
            )
            .await;
        });
    }
}

enum Grant {
    // From the client's claimed kind.
    Kind,
    Fixed(ClientProfile),
    // `supervisor` or `agent_peer`, decided by the target's broker.
    Peer,
}

// Full and watch-only are separate rooms, so the profile follows the room and a
// client can't move between them. `peer_accept` is set only on an agent-scope leg.
pub struct RelayLeg {
    pub dial_url: String,
    pub cipher: Cipher,
    pub profile: ClientProfile,
    pub peer_accept: Option<PeerAccept>,
}

// What an agent-scope leg needs to answer a reverse-edge offer. `identity` is
// announced to the dialer, never derived by the other side.
#[derive(Clone)]
pub struct PeerAccept {
    pub identity: ClientIdentity,
    pub registrar: mpsc::UnboundedSender<PeerRegistration>,
}

// Dials OUT to the relay (the only direction that crosses NAT) rather than
// listening. Each leg parks its own spare so every code pairs immediately.
pub async fn run_relay_daemon(legs: Vec<RelayLeg>, broker: BrokerHandle) -> Result<()> {
    let mut tasks = Vec::new();
    for leg in legs {
        eprintln!("[daemon] hosting over relay at {}", leg.dial_url);
        let broker = broker.clone();
        tasks.push(tokio::spawn(async move {
            relay_dial_loop(
                leg.dial_url,
                leg.cipher,
                broker,
                None,
                leg.profile,
                leg.peer_accept,
            )
            .await;
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
    Ok(())
}

// Shares the process with the local TUI, so it must never write stderr; progress
// goes over `status` instead.
pub async fn serve_relay_handoff(
    relay_url: String,
    cipher: Cipher,
    broker: BrokerHandle,
    status: mpsc::Sender<HandoffStatus>,
    profile: ClientProfile,
    peer_accept: Option<PeerAccept>,
) {
    relay_dial_loop(
        relay_url,
        cipher,
        broker,
        Some(status),
        profile,
        peer_accept,
    )
    .await;
}

// Keeps one spare host connection parked on the relay; on pairing it hands the
// connection to a `bridge` task and re-dials at once, so a second client never waits.
//
// `status: None` is the standalone daemon (logs, retries forever); `Some` is the
// co-located handoff (silent, no retry). Connected is reported once, not per re-dial,
// so the pair screen doesn't flicker as spares rotate.
async fn relay_dial_loop(
    relay_url: String,
    cipher: Cipher,
    broker: BrokerHandle,
    status: Option<mpsc::Sender<HandoffStatus>>,
    profile: ClientProfile,
    peer_accept: Option<PeerAccept>,
) {
    let mut announced = false;
    loop {
        if !broker.is_alive() {
            break;
        }
        if !announced && let Some(s) = &status {
            let _ = s.send(HandoffStatus::Connecting).await;
        }
        match connect_async(relay_url.as_str()).await {
            Ok((ws, _resp)) => {
                if !announced {
                    if let Some(s) = &status {
                        let _ = s.send(HandoffStatus::Connected).await;
                    }
                    announced = true;
                }
                let (sink, stream) = ws.split();
                let mut reader = WsReader::new(stream, cipher.clone());
                let mut writer = WsWriter::new(sink, cipher.clone());
                // No frame arrives while parked, so the first one (the Attach) is the
                // pairing signal; a spare that dies first yields None and is re-dialed.
                let mut ping = keepalive_timer();
                let attach = loop {
                    tokio::select! {
                        attach = recv_attach(&mut reader) => break attach,
                        _ = ping.tick() => {
                            if FrameWriter::<ServerFrame>::keepalive(&mut writer).await.is_err() {
                                break None;
                            }
                        }
                    }
                };
                if let Some((after_seq, who, reverse_offer)) = attach {
                    let broker = broker.clone();
                    // Rights come from the leg the daemon minted, never from the client.
                    let profile = profile.clone();
                    let peer_accept = peer_accept.clone();
                    tokio::spawn(async move {
                        bridge(
                            reader,
                            writer,
                            &broker,
                            after_seq,
                            who,
                            Grant::Fixed(profile),
                            reverse_offer,
                            peer_accept,
                        )
                        .await;
                    });
                }
            }
            Err(e) => match &status {
                // Live bridges keep running; the next /background re-arms.
                Some(s) => {
                    let _ = s.send(HandoffStatus::Failed(format!("{e}"))).await;
                    break;
                }
                None => {
                    eprintln!("[daemon] relay dial failed: {e:#}; retrying in 2s");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            },
        }
    }
}

// `None` on anything but an Attach: the client never bound, so there's nothing to detach.
async fn recv_attach<R>(reader: &mut R) -> Option<(Option<u64>, ClientIdentity, bool)>
where
    R: FrameReader<ClientFrame>,
{
    match reader.recv().await {
        Ok(Some(ClientFrame::Attach {
            after_seq,
            who,
            reverse_offer,
        })) => Some((after_seq, who, reverse_offer)),
        _ => None,
    }
}

// Reverse-edge frames are produced only when the dialer offered and this leg has
// `peer_accept`; otherwise it is forward-only.
#[allow(clippy::too_many_arguments)]
async fn bridge<R, W>(
    mut reader: R,
    mut writer: W,
    broker: &BrokerHandle,
    after_seq: Option<u64>,
    who: ClientIdentity,
    grant: Grant,
    reverse_offer: bool,
    peer_accept: Option<PeerAccept>,
) where
    R: FrameReader<ClientFrame>,
    W: FrameWriter<ServerFrame>,
{
    let (attached, supervised) = match grant {
        Grant::Kind => (broker.attach(who.clone()).await, false),
        Grant::Fixed(profile) => (broker.attach_as(who.clone(), profile).await, false),
        Grant::Peer => match broker.attach_peer(who.clone()).await {
            Some((controller, supervised)) => (Some(controller), supervised),
            None => (None, false),
        },
    };
    let controller = match attached {
        Some(c) => c,
        None => {
            // Attach fails only if the broker is gone (host shut down since accept).
            let _ = writer.send(&ServerFrame::Busy).await;
            return;
        }
    };
    if writer.send(&ServerFrame::Attached).await.is_err() {
        // Dropping `controller` detaches it; the session lives on.
        return;
    }

    let mut rev_ev_tx: Option<mpsc::UnboundedSender<ControllerEvent>> = None;
    let mut rev_ui_rx: Option<mpsc::Receiver<UiEvent>> = None;
    if reverse_offer && let Some(pa) = peer_accept {
        // `ReverseAttach` must be the first frame after `Attached`: the dialer reads
        // exactly one frame for it and fails closed. Don't start the event pump before this.
        if writer
            .send(&ServerFrame::ReverseAttach {
                who: pa.identity.clone(),
                supervised,
            })
            .await
            .is_err()
        {
            return;
        }
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<ControllerEvent>();
        let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>(64);
        // The target never supervises its dialer; named by what the dialer announced.
        let _ = pa.registrar.send(PeerRegistration::new(
            Controller {
                events: ev_rx,
                ui_tx,
            },
            who,
        ));
        rev_ev_tx = Some(ev_tx);
        rev_ui_rx = Some(ui_rx);
    }

    let mut events = controller.events;
    let ui_tx = controller.ui_tx;
    // The broker replays its whole buffer from index 0 on every attach, so the
    // receive index is a stable resume cursor across reconnects.
    let mut seq: u64 = 0;
    let mut ping = keepalive_timer();

    loop {
        tokio::select! {
            _ = ping.tick() => {
                if writer.keepalive().await.is_err() {
                    return;
                }
            }
            maybe_ev = events.recv() => {
                match maybe_ev {
                    Some(event) => {
                        let this_seq = seq;
                        seq += 1;
                        if let Some(a) = after_seq
                            && this_seq <= a {
                                continue;
                            }
                        if writer
                            .send(&ServerFrame::Event { seq: this_seq, event })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    None => return,
                }
            }
            frame = reader.recv() => {
                match frame {
                    Ok(Some(ClientFrame::Command(ui))) => {
                        // Quit detaches only this controller: a daemon ends only when its
                        // process stops, and one controller leaving must not strand others.
                        if matches!(ui, UiEvent::Quit) || ui_tx.send(ui).await.is_err() {
                            return;
                        }
                    }
                    Ok(Some(ClientFrame::ReverseEvent { event, .. })) => {
                        if let Some(tx) = &rev_ev_tx {
                            let _ = tx.send(event);
                        }
                    }
                    Ok(Some(ClientFrame::Detach)) | Ok(None) | Err(_) => return,
                    // A second Attach mid-session is a protocol error; ignore it.
                    Ok(Some(ClientFrame::Attach { .. })) => {}
                }
            }
            // A closed channel (the PeerSet reaped the return peer) quiets only this arm.
            rev_ui = recv_opt_ui(&mut rev_ui_rx) => {
                match rev_ui {
                    Some(ui) => {
                        if writer.send(&ServerFrame::ReverseCommand(ui)).await.is_err() {
                            return;
                        }
                    }
                    None => rev_ui_rx = None,
                }
            }
        }
    }
}

// Pends forever without a reverse edge, keeping the select arm quiet.
async fn recv_opt_ui(rx: &mut Option<mpsc::Receiver<UiEvent>>) -> Option<UiEvent> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}
