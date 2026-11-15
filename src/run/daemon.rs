use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;
use tokio::sync::watch;

use crate::core::SessionHost;
use crate::transport::{self, PairingScope};

// The caller serves `socket`; with no relay (`--local`) that is the only way in.
pub(super) async fn run(
    host: SessionHost,
    socket: &Path,
    relay: Option<String>,
    watch: bool,
    peer_accept: Option<transport::PeerAccept>,
    needs_answerer: bool,
) -> Result<()> {
    let broker = host.broker_handle();
    let answerers = if needs_answerer {
        eprintln!("[daemon] waiting for someone to answer its prompts");
        Some(
            broker
                .answerers()
                .await
                .context("the session broker is gone")?,
        )
    } else {
        None
    };
    eprintln!("[daemon] listening on {}", socket.display());
    // Only a process signal or, with `needs_answerer`, losing the last answerer ends the
    // daemon; an ordinary controller leaving never does.
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
    let unattended = async {
        match answerers {
            Some(a) => wait_for_exit(a, ANSWERER_GRACE).await,
            None => std::future::pending().await,
        }
    };
    let daemon_result = tokio::select! {
        r = run => r,
        _ = unattended => {
            eprintln!("[daemon] no one left to answer its prompts; stopping");
            Ok(())
        }
        _ = shutdown_signal() => {
            eprintln!("[daemon] shutdown signal received; stopping");
            Ok(())
        }
    };
    let _ = host.shutdown().await;
    daemon_result
}

// Long enough for the dialer that started this host (`--needs-answerer`) to finish
// its handshake.
const ANSWERER_GRACE: Duration = Duration::from_secs(30);

// An answerer is any controller that can answer permissions: a human TUI or a
// supervising agent. Resolves when none attached within `grace`, or once the last leaves.
async fn wait_for_exit(mut answerers: watch::Receiver<usize>, grace: Duration) {
    let attached = matches!(
        tokio::time::timeout(grace, answerers.wait_for(|n| *n > 0)).await,
        Ok(Ok(_))
    );
    if attached {
        let _ = answerers.wait_for(|n| *n == 0).await;
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
    use crate::core::{ClientIdentity, ClientKind, ClientProfile};

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
    async fn exits_after_the_grace_period_with_no_answerer() {
        let bb = spawn_bare_broker(Vec::new());
        let answerers = bb.handle.answerers().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), wait_for_exit(answerers, GRACE))
            .await
            .expect("must exit once the grace period passes");
        bb.task.abort();
    }

    #[tokio::test]
    async fn exits_when_the_supervisor_leaves_not_when_a_plain_peer_does() {
        let bb = spawn_bare_broker(Vec::new());
        let answerers = bb.handle.answerers().await.unwrap();
        let waiting = tokio::spawn(wait_for_exit(answerers, GRACE));

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
            .expect("must exit when the last answerer leaves")
            .unwrap();
        bb.task.abort();
    }

    #[tokio::test]
    async fn an_attached_human_keeps_it_up_before_and_after_the_supervisor() {
        let bb = spawn_bare_broker(Vec::new());
        let answerers = bb.handle.answerers().await.unwrap();
        let waiting = tokio::spawn(wait_for_exit(answerers, GRACE));

        let human = bb
            .handle
            .attach_as(ClientIdentity::human("h"), ClientProfile::human())
            .await
            .unwrap();
        tokio::time::sleep(GRACE * 2).await;
        assert!(
            !waiting.is_finished(),
            "a human counts within the grace period"
        );

        let (supervisor, granted) = bb.handle.attach_peer(agent("s")).await.unwrap();
        assert!(
            !granted,
            "the human already answers, so the agent is a plain peer"
        );
        drop(supervisor);
        tokio::time::sleep(GRACE).await;
        assert!(!waiting.is_finished(), "the human is still attached");

        drop(human);
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("must exit when the last answerer leaves")
            .unwrap();
        bb.task.abort();
    }
}
