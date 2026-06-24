use anyhow::{Context, Result};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio_tungstenite::connect_async;

use super::encryption::Cipher;
use super::wire::{
    ClientFrame, FrameReader, FrameWriter, LineReader, LineWriter, ServerFrame, WsReader, WsWriter,
};
use crate::core::{BrokerHandle, SessionHandle, UiEvent};

// How one client connection ended — decides what the accept loop does next.
enum ConnOutcome {
    // The client left (clean detach, dropped socket, or transport error) but the
    // session lives on, headless. The daemon loops back and accepts the next one.
    Disconnected,
    // A client quit the session (or the loop ended on its own). The daemon stops
    // and the caller shuts the host down.
    SessionEnded,
}

// Where the accept loop runs relative to the session owner. This now governs only
// stderr behaviour: a standalone process has no TUI and logs freely; a co-located
// loop shares the process with the owner's TUI, so it must stay SILENT — any stderr
// write corrupts the alternate screen and desyncs ratatui's diff. (Quit semantics
// are no longer mode-dependent: a controller's Quit is always just a detach — see
// `handle_conn`. A daemon ends only when its own process is stopped.)
enum ServeMode {
    // The accept loop is the whole process (the `--daemon` server): no local TUI.
    Standalone,
    // The accept loop is bolted onto an interactive process that already has a
    // local owner-TUI (spawned on /background). Clients arriving here are guests.
    CoLocated,
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

// Accept one client at a time and bridge it to the broker. Serial by design:
// handling connections one at a time makes a foreground reattach race-free — the
// previous connection's `detach` completes (on `handle_conn` returning) before the
// next connection's `attach` runs, so the second never sees a stale single-attach
// lock.
async fn serve(listener: &UnixListener, broker: &BrokerHandle, mode: ServeMode) {
    // A `CoLocated` loop shares the screen with the owner's TUI, so it must stay
    // silent (any stderr write would corrupt it); a `Standalone` daemon logs freely.
    let log = matches!(mode, ServeMode::Standalone);
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                if log {
                    eprintln!("[daemon] accept error: {e:#}");
                }
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
                if log {
                    eprintln!("[daemon] client disconnected; session still running");
                }
            }
            ConnOutcome::SessionEnded => {
                // A local-TUI force-takeover (reclaim) closes the booted client's
                // stream and surfaces here as SessionEnded, but the broker is still
                // alive — keep accepting so control can hand back. Only a real host
                // shutdown (broker gone) ends the loop. Standalone --daemon never
                // force-takes-over, so there this is always a genuine shutdown.
                if broker.is_alive() {
                    if log {
                        eprintln!("[daemon] controller reclaimed locally; still serving");
                    }
                    continue;
                }
                if log {
                    eprintln!("[daemon] session ended; shutting down");
                }
                break;
            }
        }
    }
}

// Standalone headless daemon (the `--daemon` process). No TUI, so it logs freely.
// A controller's Quit no longer ends the session (it's a detach); the daemon runs
// until its process is stopped (the caller turns that into a graceful host
// shutdown), so this owns socket-file cleanup.
pub async fn run_daemon(listener: UnixListener, path: PathBuf, broker: BrokerHandle) -> Result<()> {
    eprintln!("[daemon] listening on {}", path.display());
    serve(&listener, &broker, ServeMode::Standalone).await;
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
    relay_dial_loop(relay_url, cipher, broker, true).await;
    Ok(())
}

// Co-located relay handoff: spawned on a normal in-process session's first
// /background when --relay is armed (the relay counterpart of `serve_handoff`).
// Runs SILENTLY — it shares the process with the local TUI, so any stderr write
// would corrupt the alternate screen. Connection status is invisible by design;
// the phone's own UI shows it. The session is never ended by this loop — only by
// the local owner quitting (which tears the process down).
pub async fn serve_relay_handoff(relay_url: String, cipher: Cipher, broker: BrokerHandle) {
    relay_dial_loop(relay_url, cipher, broker, false).await;
}

// Dial OUT to the relay (the only direction that crosses NAT), pair with a
// controller, and bridge it to the broker; re-dial when the controller leaves so
// the next one can attach. `log` is off when co-located with a TUI (silent) and on
// for the standalone --daemon. Ends only when the broker is gone (host shutdown); a
// local force-takeover keeps it dialing so a device can attach again after reclaim.
async fn relay_dial_loop(relay_url: String, cipher: Cipher, broker: BrokerHandle, log: bool) {
    loop {
        match connect_async(relay_url.as_str()).await {
            Ok((ws, _resp)) => {
                let (sink, stream) = ws.split();
                let reader = WsReader::new(stream, cipher.clone());
                let writer = WsWriter::new(sink, cipher.clone());
                match handle_conn(reader, writer, &broker).await {
                    // Controller left (Quit, clean detach, dropped socket, or the
                    // relay timed out an unpaired wait): re-dial for the next one.
                    ConnOutcome::Disconnected => continue,
                    // Host shut down (broker gone → stop) OR a local force-takeover
                    // booted the controller (broker alive → re-dial so a device can
                    // attach again once control hands back).
                    ConnOutcome::SessionEnded => {
                        if broker.is_alive() {
                            continue;
                        }
                        break;
                    }
                }
            }
            // Relay unreachable (down / restarting): the session stays alive
            // headless; back off and retry rather than spinning on the dial.
            Err(e) => {
                if log {
                    eprintln!("[daemon] relay dial failed: {e:#}; retrying in 2s");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

// Co-located handoff accept loop (spawned on the local TUI's first /background).
// Runs SILENTLY (shares the process with the local TUI) and a guest Quit is only a
// detach — the launching process owns the session lifecycle and the socket file,
// so this neither ends the session on a guest Quit nor cleans up the socket.
pub async fn serve_handoff(listener: UnixListener, broker: BrokerHandle) {
    serve(&listener, &broker, ServeMode::CoLocated).await;
}

async fn handle_conn<R, W>(mut reader: R, mut writer: W, broker: &BrokerHandle) -> ConnOutcome
where
    R: FrameReader<ClientFrame>,
    W: FrameWriter<ServerFrame>,
{
    // Handshake: the first frame must be Attach; it carries the resume cursor.
    let after_seq = match reader.recv().await {
        Ok(Some(ClientFrame::Attach { after_seq })) => after_seq,
        // Anything else first (or immediate EOF/error) → the client never bound;
        // nothing to detach, the session is unaffected.
        _ => return ConnOutcome::Disconnected,
    };

    let controller = match broker.attach().await {
        Some(c) => c,
        None => {
            // Single-attach lock held elsewhere: tell the client and drop. We
            // never bound, so the current holder keeps running undisturbed.
            let _ = writer.send(&ServerFrame::Busy).await;
            return ConnOutcome::Disconnected;
        }
    };
    if writer.send(&ServerFrame::Attached).await.is_err() {
        broker.detach();
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
                            broker.detach();
                            return ConnOutcome::Disconnected;
                        }
                    }
                    // The controller stream closed: the host was shut down (the
                    // daemon process is stopping) or a force-takeover booted us.
                    // Either way this connection is done and the daemon stops.
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
                        // exactly like Detach, and is not forwarded to the loop.
                        if matches!(ui, UiEvent::Quit) {
                            broker.detach();
                            return ConnOutcome::Disconnected;
                        }
                        if ui_tx.send(ui).await.is_err() {
                            return ConnOutcome::SessionEnded; // broker gone
                        }
                    }
                    // Explicit detach or a clean EOF (client dropped the socket):
                    // pause in place so a later client reattaches and replays.
                    Ok(Some(ClientFrame::Detach)) | Ok(None) => {
                        broker.detach();
                        return ConnOutcome::Disconnected;
                    }
                    // A second Attach mid-session is a protocol error; ignore it.
                    Ok(Some(ClientFrame::Attach { .. })) => {}
                    // Transport / parse error → treat as a drop.
                    Err(_) => {
                        broker.detach();
                        return ConnOutcome::Disconnected;
                    }
                }
            }
        }
    }
}
