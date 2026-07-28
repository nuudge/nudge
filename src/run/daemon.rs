use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::core::SessionHost;
use crate::transport::{self, PairingScope};

// Host the session headless: dial OUT to the relay by default, or bind a local Unix
// socket with --socket. Either way it runs until an explicit signal, then shuts down.
pub(super) async fn run(
    host: SessionHost,
    socket: Option<PathBuf>,
    relay: Option<String>,
    watch: bool,
) -> Result<()> {
    let broker = host.broker_handle();
    // The long-running daemon. A controller quitting/leaving never ends it, so
    // the only way out is an explicit process signal — we race the two and, on a
    // signal, fall through to a graceful host shutdown below.
    let run = async move {
        if let Some(path) = socket {
            // Local debug daemon: bind a Unix socket; clients attach with
            // --connect --socket <path>.
            let listener = transport::bind_listener(&path)?;
            transport::run_daemon(listener, path, broker).await
        } else {
            // Relay daemon: mint a fresh room + key, print a QR to pair a device,
            // and dial OUT to the relay (the only direction that crosses NAT). With
            // --watch, mint a SECOND watch-only pairing (its own room + key) and park a
            // spare on it too, so a restricted spectator can pair alongside.
            let base = relay.context(
                "set NUDGE_RELAY to host over a relay, or pass --socket <path> for a local debug daemon",
            )?;
            let full = transport::Pairing::generate(base.clone());
            print_pairing(&full, "full-access")?;
            let mut legs = vec![transport::RelayLeg {
                dial_url: full.host_dial_url(),
                cipher: full.cipher.clone(),
                profile: full.scope.profile(),
            }];
            if watch {
                let watch_pairing =
                    transport::Pairing::generate_scoped(base, PairingScope::WatchOnly);
                print_pairing(&watch_pairing, "watch-only")?;
                legs.push(transport::RelayLeg {
                    dial_url: watch_pairing.host_dial_url(),
                    cipher: watch_pairing.cipher.clone(),
                    profile: watch_pairing.scope.profile(),
                });
            }
            transport::run_relay_daemon(legs, broker).await
        }
    };
    let daemon_result = tokio::select! {
        r = run => r,
        _ = shutdown_signal() => {
            eprintln!("[daemon] shutdown signal received; stopping");
            Ok(())
        }
    };
    let _ = host.shutdown().await;
    daemon_result
}

// Resolve when the process is asked to stop (Ctrl-C or SIGTERM). A daemon is ended
// only by an explicit signal — never by a controller quitting — so this is the one
// thing that breaks the dial/accept loop and lets the session shut down gracefully.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            // Couldn't install the handler: never fire on SIGTERM, leaving Ctrl-C.
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

// Print the QR + paste-able code that pairs a device. The code is a secret (it
// carries the E2E key), so it goes to the operator's terminal only. `label` names the
// scope (full-access vs watch-only) so an operator minting both knows which is which.
fn print_pairing(pairing: &transport::Pairing, label: &str) -> Result<()> {
    println!(
        "Scan to pair a device ({label}) — this code carries the relay address, room id, and E2E key, so keep it secret:\n"
    );
    println!("{}", pairing.render_qr()?);
    println!(
        "Or paste this {label} pairing code on the other device:\n\n{}\n",
        pairing.encode()
    );
    Ok(())
}
