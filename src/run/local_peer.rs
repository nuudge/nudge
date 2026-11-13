use anyhow::{Context, Result};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::coding;
use crate::core::store::SessionRow;
use crate::core::{BrokerHandle, ClientIdentity, PeerRegistration};
use crate::transport::SocketClient;

// Covers the MCP bootstrap, which runs before the host binds its socket.
const START_TIMEOUT: Duration = Duration::from_secs(30);
const LOG_TAIL_LINES: usize = 10;

// Makes a session on this machine a peer of this one over its socket, starting it
// headless first if it has ended. The edge arrives through `registrar`.
pub(super) async fn connect_local(
    reference: &str,
    self_id: &str,
    self_broker: BrokerHandle,
    registrar: mpsc::UnboundedSender<PeerRegistration>,
    who: ClientIdentity,
) -> Result<()> {
    let target = check_target(coding::find_session(reference)?, reference, self_id)?;
    let socket = super::session_socket_path(&target.id)?;
    let client = SocketClient::new(socket.clone());
    ensure_running_then(
        &socket,
        || start_host(&target, &socket),
        client.dial_peer(self_broker, registrar, who),
    )
    .await
}

fn check_target(found: Option<SessionRow>, reference: &str, self_id: &str) -> Result<SessionRow> {
    let target = found.with_context(|| format!("no session matches {reference:?}"))?;
    anyhow::ensure!(target.id != self_id, "that is this session");
    Ok(target)
}

// The lock is held through `dial`, so the dialer that started the host becomes its
// supervisor; a racing dialer waits, then finds it live and gets a plain peer edge.
async fn ensure_running_then<S, SF>(
    socket: &Path,
    start: S,
    dial: impl Future<Output = Result<()>>,
) -> Result<()>
where
    S: FnOnce() -> SF,
    SF: Future<Output = Result<()>>,
{
    if accepts(socket).await {
        return dial.await;
    }
    let _lock = lock(socket.with_extension("lock")).await?;
    if !accepts(socket).await {
        start().await?;
    }
    dial.await
}

async fn accepts(socket: &Path) -> bool {
    UnixStream::connect(socket).await.is_ok()
}

// Released when the returned file is dropped.
async fn lock(path: PathBuf) -> Result<std::fs::File> {
    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(file)
    })
    .await
    .context("lock task panicked")?
}

async fn start_host(target: &SessionRow, socket: &Path) -> Result<()> {
    let label = label(target);
    let log_path = socket.with_extension("log");
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("creating {}", log_path.display()))?;
    let mut child =
        tokio::process::Command::new(std::env::current_exe().context("locating the nudge binary")?)
            .args(["--peer-host", "--resume", &target.id])
            .current_dir(&target.cwd)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            // Its own process group, so terminal signals (a Ctrl-C here) don't reach it.
            .process_group(0)
            .spawn()
            .with_context(|| format!("starting session {label} in {}", target.cwd))?;

    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        if accepts(socket).await {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            anyhow::bail!(
                "session {label} exited ({status}) before it started listening:\n{}",
                log_tail(&log_path)
            );
        }
        if tokio::time::Instant::now() >= deadline {
            // Unbound means unsupervised, so nothing else would ever stop it.
            let _ = child.kill().await;
            anyhow::bail!(
                "session {label} didn't start within {}s:\n{}",
                START_TIMEOUT.as_secs(),
                log_tail(&log_path)
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn label(target: &SessionRow) -> String {
    match &target.name {
        Some(name) => name.clone(),
        None => target.id.chars().take(8).collect(),
    }
}

fn log_tail(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(LOG_TAIL_LINES)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ClientKind;
    use crate::core::host::spawn_bare_broker;
    use crate::core::store::SessionStore;
    use crate::transport::{PeerAccept, bind_listener, serve_socket};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::timeout;

    fn agent(name: &str) -> ClientIdentity {
        ClientIdentity {
            kind: ClientKind::Agent,
            name: name.into(),
            session_id: None,
            task: None,
        }
    }

    // Short: macOS caps socket paths at 104 bytes.
    fn temp_socket() -> PathBuf {
        let token = &uuid::Uuid::new_v4().simple().to_string()[..12];
        std::env::temp_dir().join(format!("lp-{token}.sock"))
    }

    #[test]
    fn refuses_its_own_session_and_unknown_references() {
        let store = SessionStore::open_in_memory().unwrap();
        store
            .insert_session("self-id", "/proj", None, None)
            .unwrap();
        let own = store.session("self-id").unwrap();
        let err = check_target(own, "self-id", "self-id").unwrap_err();
        assert!(err.to_string().contains("this session"), "{err}");
        let err = check_target(None, "ghost", "self-id").unwrap_err();
        assert!(err.to_string().contains("no session matches"), "{err}");
    }

    #[tokio::test]
    async fn racing_connects_to_a_dead_session_start_it_once() {
        let socket = temp_socket();
        let starts = Arc::new(AtomicUsize::new(0));
        let start = || {
            let starts = starts.clone();
            let socket = socket.clone();
            async move {
                starts.fetch_add(1, Ordering::SeqCst);
                // Slow enough that the other connect is already waiting on the lock.
                tokio::time::sleep(Duration::from_millis(200)).await;
                let listener = bind_listener(&socket)?;
                tokio::spawn(async move { while listener.accept().await.is_ok() {} });
                Ok(())
            }
        };

        let (a, b) = tokio::join!(
            ensure_running_then(&socket, start, async { Ok(()) }),
            ensure_running_then(&socket, start, async { Ok(()) }),
        );
        a.unwrap();
        b.unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);

        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_file(socket.with_extension("lock"));
    }

    // The target has no human attached, so the step-2 grant makes the dialer its supervisor.
    #[tokio::test]
    async fn a_live_unattended_session_is_dialed_and_supervised() {
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
        let client = SocketClient::new(path.clone());
        ensure_running_then(
            &path,
            || async { anyhow::bail!("a live session must not be started") },
            client.dial_peer(bb_self.handle.clone(), registrar, agent("self")),
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
