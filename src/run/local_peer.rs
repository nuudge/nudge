use anyhow::{Context, Result};
use std::path::Path;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::coding;
use crate::core::store::SessionRow;
use crate::core::{BrokerHandle, ClientIdentity, PeerRegistration};
use crate::transport::SocketClient;

// Makes a session on this machine a peer of this one over its socket; the edge
// arrives through `registrar`.
pub(super) async fn connect_local(
    reference: &str,
    self_id: &str,
    self_broker: BrokerHandle,
    registrar: mpsc::UnboundedSender<PeerRegistration>,
    who: ClientIdentity,
) -> Result<()> {
    let target = check_target(coding::find_session(reference)?, reference, self_id)?;
    let socket = super::session_socket_path(&target.id)?;
    dial(&target, &socket, self_broker, registrar, who).await
}

fn check_target(found: Option<SessionRow>, reference: &str, self_id: &str) -> Result<SessionRow> {
    let target = found.with_context(|| format!("no session matches {reference:?}"))?;
    anyhow::ensure!(target.id != self_id, "that is this session");
    Ok(target)
}

async fn dial(
    target: &SessionRow,
    socket: &Path,
    self_broker: BrokerHandle,
    registrar: mpsc::UnboundedSender<PeerRegistration>,
    who: ClientIdentity,
) -> Result<()> {
    if UnixStream::connect(socket).await.is_err() {
        let label = match &target.name {
            Some(name) => name.clone(),
            None => target.id.chars().take(8).collect(),
        };
        anyhow::bail!("session {label} isn't running");
    }
    SocketClient::new(socket.to_path_buf())
        .dial_peer(self_broker, registrar, who)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ClientKind;
    use crate::core::host::spawn_bare_broker;
    use crate::core::store::SessionStore;
    use crate::transport::{PeerAccept, bind_listener, serve_socket};
    use std::time::Duration;
    use tokio::time::timeout;

    fn row(store: &SessionStore, id: &str, name: Option<&str>) -> SessionRow {
        store.insert_session(id, "/proj", None, None).unwrap();
        if let Some(name) = name {
            store.set_name(id, name, None).unwrap();
        }
        store.session(id).unwrap().unwrap()
    }

    fn agent(name: &str) -> ClientIdentity {
        ClientIdentity {
            kind: ClientKind::Agent,
            name: name.into(),
            session_id: None,
            task: None,
        }
    }

    // Short: macOS caps socket paths at 104 bytes.
    fn temp_socket() -> std::path::PathBuf {
        let token = &uuid::Uuid::new_v4().simple().to_string()[..12];
        std::env::temp_dir().join(format!("lp-{token}.sock"))
    }

    #[test]
    fn refuses_its_own_session_and_unknown_references() {
        let store = SessionStore::open_in_memory().unwrap();
        let own = row(&store, "self-id", None);
        let err = check_target(Some(own), "self-id", "self-id").unwrap_err();
        assert!(err.to_string().contains("this session"), "{err}");
        let err = check_target(None, "ghost", "self-id").unwrap_err();
        assert!(err.to_string().contains("no session matches"), "{err}");
    }

    #[tokio::test]
    async fn a_dead_socket_is_not_running() {
        let store = SessionStore::open_in_memory().unwrap();
        let target = row(&store, "0123456789abcdef", Some("worker"));
        let bb = spawn_bare_broker(Vec::new());
        let (registrar, _rx) = mpsc::unbounded_channel();
        let err = dial(
            &target,
            &temp_socket(),
            bb.handle.clone(),
            registrar,
            agent("a"),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "session worker isn't running");
        bb.task.abort();
    }

    // The target has no human attached, so the step-2 grant makes the dialer its supervisor.
    #[tokio::test]
    async fn a_live_unattended_session_is_dialed_and_supervised() {
        let store = SessionStore::open_in_memory().unwrap();
        let target = row(&store, "target-id", None);
        let path = temp_socket();
        let bb_target = spawn_bare_broker(Vec::new());
        let (target_reg_tx, _target_reg_rx) = mpsc::unbounded_channel();
        let serve = tokio::spawn(serve_socket(
            bind_listener(&path).unwrap(),
            bb_target.handle.clone(),
            PeerAccept {
                identity: agent("target"),
                registrar: target_reg_tx,
            },
        ));

        let bb_self = spawn_bare_broker(Vec::new());
        let (registrar, mut reg_rx) = mpsc::unbounded_channel();
        dial(
            &target,
            &path,
            bb_self.handle.clone(),
            registrar,
            agent("self"),
        )
        .await
        .expect("dial a live session");
        let edge = timeout(Duration::from_secs(5), reg_rx.recv())
            .await
            .expect("registration timed out")
            .expect("an edge was registered");
        assert_eq!(edge.who.name, "target");
        assert!(edge.supervised);

        serve.abort();
        bb_target.task.abort();
        bb_self.task.abort();
        let _ = std::fs::remove_file(&path);
    }
}
