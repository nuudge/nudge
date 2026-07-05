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
// (the only direction that crosses NAT) and is paired there with a controller.
// Each pairing is one connection: when the controller leaves, the relay tears our
// side down too, so we re-dial to be available for the next one. A controller's Quit
// is just a detach (we re-dial); the session ends only when this daemon process is
// stopped, which the caller turns into a graceful host shutdown.
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

// Dial OUT to the relay (the only direction that crosses NAT), pair with a
// controller, and bridge it to the broker; re-dial when the controller leaves so
// the next one can attach. `status` distinguishes the two callers: `None` is the
// standalone `--daemon` (logs to stderr, retries forever on an unreachable relay);
// `Some(tx)` is the co-located in-process handoff (silent, reports progress to the
// TUI, and does NOT retry a failed dial — the user re-/background to try again).
async fn relay_dial_loop(
    relay_url: String,
    cipher: Cipher,
    broker: BrokerHandle,
    status: Option<mpsc::Sender<HandoffStatus>>,
) {
    loop {
        if let Some(s) = &status {
            let _ = s.send(HandoffStatus::Connecting).await;
        }
        match connect_async(relay_url.as_str()).await {
            Ok((ws, _resp)) => {
                if let Some(s) = &status {
                    let _ = s.send(HandoffStatus::Connected).await;
                }
                let (sink, stream) = ws.split();
                let reader = WsReader::new(stream, cipher.clone());
                let writer = WsWriter::new(sink, cipher.clone());
                match handle_conn(reader, writer, &broker).await {
                    // Controller left (Quit, clean detach, dropped socket, or the
                    // relay timed out an unpaired wait): re-dial for the next one.
                    ConnOutcome::Disconnected => continue,
                    // The controller's stream closed, which now happens only when the
                    // broker itself is gone — foregrounding on the laptop no longer
                    // boots a paired phone; they coexist. The is_alive check stays as
                    // a cheap guard: if the broker somehow outlives the stream, re-dial
                    // rather than exit.
                    ConnOutcome::SessionEnded => {
                        if broker.is_alive() {
                            continue;
                        }
                        break;
                    }
                }
            }
            Err(e) => match &status {
                // Co-located: surface the failure to the TUI and stop. Handoff is
                // unavailable until the next /background fires a fresh dial.
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

async fn handle_conn<R, W>(mut reader: R, mut writer: W, broker: &BrokerHandle) -> ConnOutcome
where
    R: FrameReader<ClientFrame>,
    W: FrameWriter<ServerFrame>,
{
    // Handshake: the first frame must be Attach; it carries the resume cursor and the
    // attaching client's identity, which we forward to the broker.
    let (after_seq, who) = match reader.recv().await {
        Ok(Some(ClientFrame::Attach { after_seq, who })) => (after_seq, who),
        // Anything else first (or immediate EOF/error) → the client never bound;
        // nothing to detach, the session is unaffected.
        _ => return ConnOutcome::Disconnected,
    };

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
