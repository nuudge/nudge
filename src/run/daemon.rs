use anyhow::{Context, Result};
use std::path::Path;

use crate::core::SessionHost;
use crate::transport::{self, PairingScope};

// The caller serves `socket`; with `socket_only` (`--socket`) there are no relay legs.
pub(super) async fn run(
    host: SessionHost,
    socket: &Path,
    socket_only: bool,
    relay: Option<String>,
    watch: bool,
    peer_accept: Option<transport::PeerAccept>,
) -> Result<()> {
    let broker = host.broker_handle();
    eprintln!("[daemon] listening on {}", socket.display());
    // A controller leaving never ends the daemon; only a process signal does.
    let run = async move {
        if socket_only {
            broker.closed().await;
            eprintln!("[daemon] session ended; shutting down");
            Ok(())
        } else {
            // Dial OUT to the relay: the only direction that crosses NAT. Each scope
            // (full, watch, agent) gets its own room + key.
            let base = relay.context(
                "set NUDGE_RELAY to host over a relay, or pass --socket <path> for a local debug daemon",
            )?;
            let full = transport::Pairing::generate(base.clone());
            print_pairing(&full, "full-access")?;
            let mut legs = vec![transport::RelayLeg {
                dial_url: full.host_dial_url(),
                cipher: full.cipher.clone(),
                profile: full.scope.profile(),
                peer_accept: None,
            }];
            if watch {
                let watch_pairing =
                    transport::Pairing::generate_scoped(base.clone(), PairingScope::WatchOnly);
                print_pairing(&watch_pairing, "watch-only")?;
                legs.push(transport::RelayLeg {
                    dial_url: watch_pairing.host_dial_url(),
                    cipher: watch_pairing.cipher.clone(),
                    profile: watch_pairing.scope.profile(),
                    peer_accept: None,
                });
            }
            if let Some(peer_accept) = peer_accept {
                let agent_pairing = transport::Pairing::generate_scoped(base, PairingScope::Agent);
                print_pairing(&agent_pairing, "agent-peer")?;
                legs.push(transport::RelayLeg {
                    dial_url: agent_pairing.host_dial_url(),
                    cipher: agent_pairing.cipher.clone(),
                    profile: agent_pairing.scope.profile(),
                    peer_accept: Some(peer_accept),
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

// The code carries the E2E key, so it goes to the operator's terminal only.
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
