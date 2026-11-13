use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

use crate::core::{self, ClientIdentity, PeerRegistration, SessionHandle, SessionHost};
use crate::transport::{self, PairingScope, PeerAccept};
use crate::tui;

const HANDOFF_STATUS_CAP: usize = 8;

// The relay handoff hook is wired here, not in `core`, to keep `core` below the
// transport layer.
pub(super) async fn run(
    mut host: SessionHost,
    mut ui_cfg: tui::UiConfig,
    who: ClientIdentity,
    relay: Option<String>,
    peer_identity: ClientIdentity,
    peer_registrar: mpsc::UnboundedSender<PeerRegistration>,
) -> Result<()> {
    let handoff_rx = if let Some(base) = relay {
        // Pairings are regenerated each launch (no persistence); each scope gets its
        // own room + key so rights follow the room. Legs dial only once backgrounded.
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
        let agent_accept = PeerAccept {
            identity: peer_identity,
            registrar: peer_registrar,
        };
        let broker = host.broker_handle();
        let (status_tx, status_rx) = mpsc::channel::<core::HandoffStatus>(HANDOFF_STATUS_CAP);
        // Dedupes re-dials while one is live; a failed dial clears it so the next
        // /background retries.
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
                // All legs dial the same relay, so they share one status channel.
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
    // The loop outlives the front-end, so end the session explicitly.
    let _ = host.shutdown().await;
    tui_result
}
