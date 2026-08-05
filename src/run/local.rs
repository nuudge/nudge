use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

use crate::core::{self, ClientIdentity, PeerRegistration, SessionHandle, SessionHost};
use crate::transport::{self, PairingScope, PeerAccept};
use crate::tui;

// Buffer for relay-dial status updates flowing from the handoff task to the TUI.
// A handful of state flips at a time; the TUI drains it in its select loop.
const HANDOFF_STATUS_CAP: usize = 8;

// Local TUI driving the loop in-process. The TUI holds `&host` so it can
// re-attach after /background; it owns the initial foreground controller.
// When NUDGE_RELAY is set, /background fires the handoff hook to dial the
// relay (lazy) so a phone can attach; wiring it here (not in `core`) keeps
// `core` below the transport layer. The dial reports progress to the TUI over
// `status_rx`. With no relay configured, /background just pauses in place.
//
// `peer_identity` + `peer_registrar` arm the agent-peer leg (#61): once /background
// dials, an inbound `/connect-peer` dialer attaches through the agent room exactly
// like a teammate through the watch room, plus the return edge registers into this
// loop — so an armed interactive session is simultaneously human-driven and dialable.
pub(super) async fn run(
    mut host: SessionHost,
    mut ui_cfg: tui::UiConfig,
    who: ClientIdentity,
    relay: Option<String>,
    peer_identity: ClientIdentity,
    peer_registrar: mpsc::UnboundedSender<PeerRegistration>,
) -> Result<()> {
    let handoff_rx = if let Some(base) = relay {
        // Regenerate the pairings each launch (no persistence): the device re-scans
        // after a restart. Three scoped codes are minted — full-access (your own phone),
        // watch-only (a teammate who may observe but not drive), and agent-peer (a remote
        // nudge session that dials in with /connect-peer) — each its own room + key so
        // rights follow the room (same model as `--daemon --watch`/`--peer`). All go to
        // the TUI, which shows one at a time (cycle with `w`); every leg dials only once
        // backgrounded.
        let full = transport::Pairing::generate(base.clone());
        let watch = transport::Pairing::generate_scoped(base.clone(), PairingScope::WatchOnly);
        let agent = transport::Pairing::generate_scoped(base, PairingScope::Agent);
        ui_cfg.pairing_qr = Some(full.render_qr()?);
        ui_cfg.pairing_code = Some(full.encode());
        ui_cfg.pairing_qr_watch = Some(watch.render_qr()?);
        ui_cfg.pairing_code_watch = Some(watch.encode());
        ui_cfg.pairing_qr_agent = Some(agent.render_qr()?);
        ui_cfg.pairing_code_agent = Some(agent.encode());
        let full_url = full.host_dial_url();
        let full_cipher = full.cipher;
        let watch_url = watch.host_dial_url();
        let watch_cipher = watch.cipher;
        let agent_url = agent.host_dial_url();
        let agent_cipher = agent.cipher;
        // The agent leg carries the reverse-edge accept: this session's announced
        // identity plus the runtime registrar the return edge lands in (#52).
        let agent_accept = PeerAccept {
            identity: peer_identity,
            registrar: peer_registrar,
        };
        let broker = host.broker_handle();
        let (status_tx, status_rx) = mpsc::channel::<core::HandoffStatus>(HANDOFF_STATUS_CAP);
        // Dedupe re-dials: while one dial is live this is a no-op, so re-entering
        // /background does nothing; once a failed dial clears it, the next
        // /background fires a fresh one (the user's way to retry).
        let dialing = Arc::new(AtomicBool::new(false));
        host.set_handoff_hook(move || {
            if dialing.swap(true, Ordering::SeqCst) {
                return;
            }
            let dialing = dialing.clone();
            let full_url = full_url.clone();
            let full_cipher = full_cipher.clone();
            let watch_url = watch_url.clone();
            let watch_cipher = watch_cipher.clone();
            let agent_url = agent_url.clone();
            let agent_cipher = agent_cipher.clone();
            let agent_accept = agent_accept.clone();
            let broker = broker.clone();
            let status_tx = status_tx.clone();
            tokio::spawn(async move {
                // All legs dial the same relay, so they share one status channel:
                // status is relay reachability, not per-leg. Last-writer-wins is fine —
                // a leg failing after another connected would be a real divergence
                // (same relay) worth surfacing.
                tokio::join!(
                    transport::serve_relay_handoff(
                        full_url,
                        full_cipher,
                        broker.clone(),
                        status_tx.clone(),
                        core::ClientProfile::human(),
                        None,
                    ),
                    transport::serve_relay_handoff(
                        watch_url,
                        watch_cipher,
                        broker.clone(),
                        status_tx.clone(),
                        core::ClientProfile::watch_only(),
                        None,
                    ),
                    transport::serve_relay_handoff(
                        agent_url,
                        agent_cipher,
                        broker,
                        status_tx,
                        core::ClientProfile::agent(),
                        Some(agent_accept),
                    ),
                );
                dialing.store(false, Ordering::SeqCst);
            });
        });
        Some(status_rx)
    } else {
        None
    };

    let controller = host
        .attach(who.clone())
        .await
        .expect("initial attach on a fresh session cannot be busy");
    let tui_result = tui::run(&host, ui_cfg, who, controller, handoff_rx).await;
    // TUI exited → end the session explicitly (loop outlives the front-end).
    let _ = host.shutdown().await;
    tui_result
}
