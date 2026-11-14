use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;
use tokio::sync::watch;

use crate::core::SessionHost;
use crate::transport::{self, PairingScope};

// The caller serves `socket`; with no relay that is the only way in.
pub(super) async fn run(
    host: SessionHost,
    socket: &Path,
    relay: Option<String>,
    watch: bool,
    peer_accept: Option<transport::PeerAccept>,
) -> Result<()> {
    let broker = host.broker_handle();
    eprintln!("[daemon] listening on {}", socket.display());
    // A controller leaving never ends the daemon; only a process signal does.
    let run = async move {
        let Some(base) = relay else {
            broker.closed().await;
            eprintln!("[daemon] session ended; shutting down");
            return Ok(());
        };
        // Dial OUT to the relay: the only direction that crosses NAT. Each scope
        // (full, watch, agent) gets its own room + key.
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

// Long enough for the dialer that spawned this host to finish its handshake.
const SUPERVISOR_GRACE: Duration = Duration::from_secs(30);

// Started by `connect_local` for an ended session; nobody else will ever quit it, so it
// ends with its supervision. Shutting down marks the session ended.
pub(super) async fn run_peer_host(host: SessionHost) -> Result<()> {
    let supervisors = host
        .broker_handle()
        .supervisors()
        .await
        .context("the session broker is gone")?;
    eprintln!("[peer-host] waiting for a supervisor");
    tokio::select! {
        _ = wait_for_exit(supervisors, SUPERVISOR_GRACE) => {
            eprintln!("[peer-host] no supervisor left; stopping");
        }
        _ = shutdown_signal() => eprintln!("[peer-host] shutdown signal received; stopping"),
    }
    host.shutdown().await
}

// Resolves when no supervisor attached within `grace`, or once the last one detaches.
async fn wait_for_exit(mut supervisors: watch::Receiver<usize>, grace: Duration) {
    let attached = matches!(
        tokio::time::timeout(grace, supervisors.wait_for(|n| *n > 0)).await,
        Ok(Ok(_))
    );
    if attached {
        let _ = supervisors.wait_for(|n| *n == 0).await;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::host::spawn_bare_broker;
    use crate::core::{ClientIdentity, ClientKind};

    fn agent(name: &str) -> ClientIdentity {
        ClientIdentity {
            kind: ClientKind::Agent,
            name: name.into(),
            session_id: None,
            task: None,
        }
    }

    const GRACE: Duration = Duration::from_millis(200);

    #[tokio::test]
    async fn exits_after_the_grace_period_with_no_supervisor() {
        let bb = spawn_bare_broker(Vec::new());
        let supervisors = bb.handle.supervisors().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), wait_for_exit(supervisors, GRACE))
            .await
            .expect("must exit once the grace period passes");
        bb.task.abort();
    }

    #[tokio::test]
    async fn exits_when_the_supervisor_leaves_not_when_a_plain_peer_does() {
        let bb = spawn_bare_broker(Vec::new());
        let supervisors = bb.handle.supervisors().await.unwrap();
        let waiting = tokio::spawn(wait_for_exit(supervisors, GRACE));

        let (supervisor, granted) = bb.handle.attach_peer(agent("s")).await.unwrap();
        assert!(granted);
        let (peer, granted) = bb.handle.attach_peer(agent("p")).await.unwrap();
        assert!(!granted);
        drop(peer);
        tokio::time::sleep(GRACE * 2).await;
        assert!(
            !waiting.is_finished(),
            "a held supervisor outlives the grace period and a plain peer's exit"
        );

        drop(supervisor);
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("must exit when the last supervisor leaves")
            .unwrap();
        bb.task.abort();
    }
}
