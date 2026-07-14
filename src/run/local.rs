use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

use crate::core::{self, ClientIdentity, SessionHandle, SessionHost};
use crate::transport;
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
pub(super) async fn run(
    mut host: SessionHost,
    mut ui_cfg: tui::UiConfig,
    who: ClientIdentity,
    relay: Option<String>,
) -> Result<()> {
    let handoff_rx = if let Some(base) = relay {
        // Regenerate the pairing each launch (no persistence): the device
        // re-scans after a restart. The QR/code go to the TUI, which surfaces
        // them on /background; the dial-out only opens once backgrounded.
        let pairing = transport::Pairing::generate(base);
        ui_cfg.pairing_qr = Some(pairing.render_qr()?);
        ui_cfg.pairing_code = Some(pairing.encode());
        let dial_url = pairing.host_dial_url();
        let cipher = pairing.cipher;
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
            let dial_url = dial_url.clone();
            let cipher = cipher.clone();
            let broker = broker.clone();
            let status_tx = status_tx.clone();
            tokio::spawn(async move {
                transport::serve_relay_handoff(dial_url, cipher, broker, status_tx).await;
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
