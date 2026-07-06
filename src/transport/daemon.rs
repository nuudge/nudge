use anyhow::{Context, Result};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;

use super::encryption::Cipher;
use super::wire::{
    ClientFrame, FrameReader, FrameWriter, LineReader, LineWriter, ServerFrame, WsReader, WsWriter,
};
use crate::core::{BrokerHandle, HandoffStatus, SessionHandle, UiEvent};

// How one client connection ended — decides what the accept loop does next.
enum ConnOutcome {
    // The client left (clean detach, dropped socket, or transport error) but the
    // session lives on, headless. The daemon loops back and accepts the next one.
    Disconnected,
    // A client quit the session (or the loop ended on its own). The daemon stops
    // and the caller shuts the host down.
    SessionEnded,
}

// Bind the listener without clobbering a live daemon. A leftover socket file from
// a crashed daemon (nothing listening behind it) is removed and rebound; a socket
// that still accepts connections means a daemon already owns this path, so we
// refuse rather than steal its address.
pub fn bind_listener(path: &Path) -> Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                anyhow::bail!("a daemon is already running at {}", path.display());
            }
            // Stale socket file → remove and rebind once.
            std::fs::remove_file(path)
                .with_context(|| format!("removing stale socket {}", path.display()))?;
            UnixListener::bind(path).with_context(|| format!("binding socket {}", path.display()))
        }
        Err(e) => Err(e).with_context(|| format!("binding socket {}", path.display())),
    }
}

// Accept one client at a time and bridge it to the broker. Serial by design: the
// broker is multi-attach, but this standalone `--daemon --socket` debug host only
// needs one client at a time, so the accept loop stays simple. (True concurrent
// clients — remote watch-mode — would spawn `handle_conn` per connection.) Only this
// debug host reaches here (the local session's handoff is relay-only), so it logs
// freely — there's no TUI to corrupt.
async fn serve(listener: &UnixListener, broker: &BrokerHandle) {
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("[daemon] accept error: {e:#}");
                continue;
            }
        };
        // Wrap the accepted byte stream in the line codec, then hand the framed
        // halves to the transport-agnostic `handle_conn`. The relay path wraps a WS
        // connection in its own codec and calls the same function.
        let (read_half, write_half) = stream.into_split();
        let reader = LineReader(BufReader::new(read_half));
        let writer = LineWriter(write_half);
        match handle_conn(reader, writer, broker).await {
            ConnOutcome::Disconnected => {
                eprintln!("[daemon] client disconnected; session still running");
            }
            // The controller's stream closed, which means the host itself is
            // shutting down.
            ConnOutcome::SessionEnded => {
                eprintln!("[daemon] session ended; shutting down");
                break;
            }
        }
    }
}

// Standalone headless daemon (the `--daemon --socket` debug host). No TUI, so it
// logs freely. A controller's Quit no longer ends the session (it's a detach); the
// daemon runs until its process is stopped (the caller turns that into a graceful
// host shutdown), so this owns socket-file cleanup.
pub async fn run_daemon(listener: UnixListener, path: PathBuf, broker: BrokerHandle) -> Result<()> {
    eprintln!("[daemon] listening on {}", path.display());
    serve(&listener, &broker).await;
    // The listener doesn't unlink the socket file on drop; do it so the next
    // daemon binds cleanly.
    let _ = std::fs::remove_file(&path);
    Ok(())
}

// Headless daemon hosting the session over the relay (the `--daemon --relay`
// process). Unlike the Unix daemon it does not listen — it dials OUT to the relay
// (the only direction that crosses NAT) as this session's single host, parking one
// spare connection and opening one more per attached client (see `relay_dial_loop`).
// A controller's Quit is just a detach; the session ends only when this daemon
// process is stopped, which the caller turns into a graceful host shutdown.
pub async fn run_relay_daemon(
    relay_url: String,
    cipher: Cipher,
    broker: BrokerHandle,
) -> Result<()> {
    eprintln!("[daemon] hosting over relay at {relay_url}");
    relay_dial_loop(relay_url, cipher, broker, None).await;
    Ok(())
}

// Co-located relay handoff: spawned on a normal in-process session's /background to
// arm phone pairing. It shares the process with the local TUI, so it never writes
// stderr — instead it reports progress over `status`, which the TUI renders on the
// background pair screen. The session is never ended by this loop, only by the local
// owner quitting (which tears the process down).
pub async fn serve_relay_handoff(
    relay_url: String,
    cipher: Cipher,
    broker: BrokerHandle,
    status: mpsc::Sender<HandoffStatus>,
) {
    relay_dial_loop(relay_url, cipher, broker, Some(status)).await;
}

// Keep one spare host connection parked on the relay at all times. Each iteration
// dials OUT (the only direction that crosses NAT), parks as the room's host spare,
// and blocks until a client pairs with it — the first frame (its Attach) is that
// signal. On pairing we hand the connection to a `bridge` task and immediately loop
// to dial the next spare, so a second client never waits: the broker is multi-attach,
// so every bridge attaches alongside the others. A client leaving ends only its own
// bridge task. The loop (and the daemon) ends only when the broker is gone.
//
// `status` distinguishes the two callers: `None` is the standalone `--daemon` (logs
// to stderr, retries forever on an unreachable relay); `Some(tx)` is the co-located
// in-process handoff (silent, reports progress to the TUI, and does NOT retry a failed
// dial — the user re-/background to try again). Connecting/Connected are reported once
// on the first successful park, not on every re-dial, so the pair screen doesn't
// flicker as spares rotate.
async fn relay_dial_loop(
    relay_url: String,
    cipher: Cipher,
    broker: BrokerHandle,
    status: Option<mpsc::Sender<HandoffStatus>>,
) {
    let mut announced = false;
    loop {
        // Broker gone (host shut down) → stop parking spares; live bridge tasks wind
        // down on their own when their broker sends start failing.
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
                let writer = WsWriter::new(sink, cipher.clone());
                // Parked as a host spare: this await is the wait for a client to pair.
                // On pairing, serve the client on its own task, then loop to re-dial a
                // fresh spare so the next client pairs immediately. If the relay instead
                // dropped our spare before any client paired (relay restart, etc.),
                // recv_attach yields None and we just loop to re-dial.
                if let Some((after_seq, who)) = recv_attach(&mut reader).await {
                    let broker = broker.clone();
                    tokio::spawn(async move {
                        bridge(reader, writer, &broker, after_seq, who).await;
                    });
                }
            }
            Err(e) => match &status {
                // Co-located: surface the failure to the TUI and stop dialing new
                // spares. Live bridges keep running; the next /background re-arms.
                Some(s) => {
                    let _ = s.send(HandoffStatus::Failed(format!("{e}"))).await;
                    break;
                }
                // Standalone daemon: the session stays alive headless; back off and
                // retry rather than spinning on the dial.
                None => {
                    eprintln!("[daemon] relay dial failed: {e:#}; retrying in 2s");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            },
        }
    }
}

// Handshake read, split out so the relay dial loop can treat "the first frame
// arrived" as its pairing signal: while a host connection is parked on the relay no
// frame comes, so this pending `recv` *is* the parked-spare state. Resolves to the
// attach params once a client pairs and sends its Attach; `None` on anything else
// (wrong first frame, EOF, or error) — the client never bound, nothing to detach.
async fn recv_attach<R>(reader: &mut R) -> Option<(Option<u64>, crate::core::ClientIdentity)>
where
    R: FrameReader<ClientFrame>,
{
    match reader.recv().await {
        Ok(Some(ClientFrame::Attach { after_seq, who })) => Some((after_seq, who)),
        _ => None,
    }
}

// Combined handshake + bridge for the serial Unix debug host: read the Attach, then
// serve. The relay path calls `recv_attach` and `bridge` separately so it can spawn
// each client's `bridge` on its own task and immediately re-dial a spare.
async fn handle_conn<R, W>(mut reader: R, writer: W, broker: &BrokerHandle) -> ConnOutcome
where
    R: FrameReader<ClientFrame>,
    W: FrameWriter<ServerFrame>,
{
    match recv_attach(&mut reader).await {
        Some((after_seq, who)) => bridge(reader, writer, broker, after_seq, who).await,
        None => ConnOutcome::Disconnected,
    }
}

// Bridge one attached client to the broker: register the controller, stream its
// replay+live events out, and forward its commands in, until it leaves. `after_seq`
// and `who` come from the already-read Attach frame.
async fn bridge<R, W>(
    mut reader: R,
    mut writer: W,
    broker: &BrokerHandle,
    after_seq: Option<u64>,
    who: crate::core::ClientIdentity,
) -> ConnOutcome
where
    R: FrameReader<ClientFrame>,
    W: FrameWriter<ServerFrame>,
{
    let controller = match broker.attach(who).await {
        Some(c) => c,
        None => {
            // There is no single-attach lock — attach fails only if the broker is
            // gone (the host shut down between our accept and this attach). Tell the
            // client (it treats Busy as "couldn't attach") and drop.
            let _ = writer.send(&ServerFrame::Busy).await;
            return ConnOutcome::Disconnected;
        }
    };
    if writer.send(&ServerFrame::Attached).await.is_err() {
        // Returning drops `controller`, which detaches it — the session lives on.
        return ConnOutcome::Disconnected;
    }

    let mut events = controller.events;
    let ui_tx = controller.ui_tx;
    // `seq` counts every event the controller stream yields, in receive order.
    // The broker replays its whole buffer from index 0 on every attach, so this
    // index is a stable cursor across reconnects: a client that saw through seq N
    // reattaches with `after_seq = Some(N)` and we skip everything <= N.
    let mut seq: u64 = 0;

    loop {
        tokio::select! {
            maybe_ev = events.recv() => {
                match maybe_ev {
                    Some(event) => {
                        let this_seq = seq;
                        seq += 1;
                        // Honour the resume cursor: drain replayed events the
                        // client already has without re-sending them.
                        if let Some(a) = after_seq
                            && this_seq <= a {
                                continue;
                            }
                        if writer
                            .send(&ServerFrame::Event { seq: this_seq, event })
                            .await
                            .is_err()
                        {
                            // Socket write failed → client gone; pause in place.
                            // Returning drops this controller, detaching it; the
                            // session lives on headless.
                            return ConnOutcome::Disconnected;
                        }
                    }
                    // The controller stream closed: the broker (and the session) shut
                    // down. Nothing else closes it now — a second client attaching no
                    // longer boots this one — so this connection is done and the
                    // daemon stops.
                    None => return ConnOutcome::SessionEnded,
                }
            }
            frame = reader.recv() => {
                match frame {
                    Ok(Some(ClientFrame::Command(ui))) => {
                        // A controller's Quit detaches *that controller*; it never
                        // ends the session. A daemon is a service — only stopping its
                        // process ends it — and a controller leaving (quit, detach, or
                        // drop) must never strand the others. So Quit is handled
                        // exactly like Detach (drop this controller and return), and is
                        // not forwarded to the loop.
                        if matches!(ui, UiEvent::Quit) {
                            return ConnOutcome::Disconnected;
                        }
                        if ui_tx.send(ui).await.is_err() {
                            return ConnOutcome::SessionEnded; // broker gone
                        }
                    }
                    // Explicit detach or a clean EOF (client dropped the socket):
                    // pause in place (dropping this controller detaches it) so a later
                    // client reattaches and replays.
                    Ok(Some(ClientFrame::Detach)) | Ok(None) => {
                        return ConnOutcome::Disconnected;
                    }
                    // A second Attach mid-session is a protocol error; ignore it.
                    Ok(Some(ClientFrame::Attach { .. })) => {}
                    // Transport / parse error → treat as a drop.
                    Err(_) => return ConnOutcome::Disconnected,
                }
            }
        }
    }
}
