use anyhow::{Context, Result};
use std::future::Future;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};
use tokio::net::UnixStream;

use crate::coding;
use crate::core::store::SessionRow;
use crate::core::{BrokerHandle, ClientIdentity, PeerRegistration, SpawnRequest, UiEvent};
use crate::transport::SocketClient;

// Covers the MCP bootstrap, which runs before the host binds its socket.
const START_TIMEOUT: Duration = Duration::from_secs(30);
const LOG_TAIL_LINES: usize = 10;
const LOG_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

// Makes a session on this machine a peer of this one over its socket, starting it
// headless first if it has ended.
pub(super) async fn connect_local(
    reference: &str,
    target: SessionRow,
    self_id: &str,
    self_broker: BrokerHandle,
    who: ClientIdentity,
) -> Result<PeerRegistration> {
    check_target(&target, reference, self_id)?;
    let socket = coding::session_socket_path(&target.id)?;
    let client = SocketClient::new(socket.clone());
    ensure_running_then(
        &socket,
        || start_host(&target, &socket),
        client.dial_peer(self_broker, who),
    )
    .await
}

// A child is a normal session in its own process, supervised by whoever claims it; it
// outlives this session and is revived by starting or reconnecting to it the same way.
pub(super) async fn spawn_child(
    request: SpawnRequest,
    parent_id: String,
    self_broker: BrokerHandle,
    who: ClientIdentity,
) -> Result<PeerRegistration> {
    let (row, task) = match request {
        SpawnRequest::New { task } => (coding::new_child(&parent_id, &task)?, Some(task)),
        SpawnRequest::Revive { child, task } => (coding::find_child(&parent_id, &child)?, task),
    };
    let socket = coding::session_socket_path(&row.id)?;
    let client = SocketClient::new(socket.clone());
    claim(
        &row,
        &socket,
        || start_host(&row, &socket),
        client.dial_peer(self_broker, who),
        task,
    )
    .await
}

// The target grants supervision only when nothing else answers its prompts, so a refused
// grant means the child is taken; the dropped edge closes its connection.
async fn claim<S, SF>(
    row: &SessionRow,
    socket: &Path,
    start: S,
    dial: impl Future<Output = Result<PeerRegistration>>,
    task: Option<String>,
) -> Result<PeerRegistration>
where
    S: FnOnce() -> SF,
    SF: Future<Output = Result<()>>,
{
    let mut reg = ensure_running_then(socket, start, dial).await?;
    anyhow::ensure!(
        reg.supervised,
        "{} is already taken: someone else answers its prompts; use /connect-peer to reach it",
        label(row)
    );
    reg.who.task = row.spawn_task.clone();
    if let Some(text) = task {
        reg.controller
            .ui_tx
            .send(UiEvent::UserMessage { text })
            .await
            .context("the child's connection closed")?;
    }
    Ok(reg)
}

fn check_target(target: &SessionRow, reference: &str, self_id: &str) -> Result<()> {
    anyhow::ensure!(target.id != self_id, "{reference} is this session itself");
    Ok(())
}

// The lock is held through `dial`, so the dialer that started the host becomes its
// supervisor; a racing dialer waits, then finds it live and gets a plain peer edge.
async fn ensure_running_then<T, S, SF>(
    socket: &Path,
    start: S,
    dial: impl Future<Output = Result<T>>,
) -> Result<T>
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

pub(super) async fn accepts(socket: &Path) -> bool {
    UnixStream::connect(socket).await.is_ok()
}

// `drop` runs before the fields drop, so the path is unlinked while `_file` still holds
// the lock; a waiter on the old inode then sees it's stale (see `lock`).
struct RunLock {
    _file: std::fs::File,
    path: PathBuf,
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn lock(path: PathBuf) -> Result<RunLock> {
    tokio::task::spawn_blocking(move || {
        loop {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
                .with_context(|| format!("opening {}", path.display()))?;
            file.lock()
                .with_context(|| format!("locking {}", path.display()))?;
            // A holder unlinks the file before releasing it, so a lock won on an inode
            // no longer at the path excludes no one; retry on the current file.
            let held = file.metadata()?;
            if let Ok(current) = std::fs::metadata(&path)
                && (held.dev(), held.ino()) == (current.dev(), current.ino())
            {
                return Ok(RunLock { _file: file, path });
            }
        }
    })
    .await
    .context("lock task panicked")?
}

// Best-effort: housekeeping must never stop a session from starting.
fn prune_logs(dir: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "log")
            && let Ok(modified) = entry.metadata().and_then(|m| m.modified())
            && now
                .duration_since(modified)
                .is_ok_and(|age| age > LOG_RETENTION)
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

async fn start_host(target: &SessionRow, socket: &Path) -> Result<()> {
    let label = label(target);
    if let Some(dir) = socket.parent() {
        prune_logs(dir, SystemTime::now());
    }
    let log_path = socket.with_extension("log");
    let log = open_run_log(&log_path, chrono::Local::now())?;
    let mut child =
        tokio::process::Command::new(std::env::current_exe().context("locating the nudge binary")?)
            .args([
                "--daemon",
                "--local",
                "--needs-answerer",
                "--resume",
                &target.id,
            ])
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

// The full id, not a prefix, so the label also works as a `--resume` argument.
pub(super) fn label(target: &SessionRow) -> String {
    target.name.clone().unwrap_or_else(|| target.id.clone())
}

const RUN_SEPARATOR: &str = "--- started ";

// Appended, so a revived session's log still shows how its previous run ended.
fn open_run_log(path: &Path, now: chrono::DateTime<chrono::Local>) -> Result<std::fs::File> {
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(log, "{RUN_SEPARATOR}{} ---", now.to_rfc3339())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(log)
}

// Only the current run: earlier runs' lines would misreport why this one failed.
fn log_tail(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let run_start = lines
        .iter()
        .rposition(|l| l.starts_with(RUN_SEPARATOR))
        .map_or(0, |i| i + 1);
    let run = &lines[run_start..];
    run[run.len().saturating_sub(LOG_TAIL_LINES)..].join("\n")
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
    use tokio::sync::mpsc;
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
    fn refuses_its_own_session() {
        let store = SessionStore::open_in_memory().unwrap();
        store
            .insert_session("self-id", "/proj", None, None)
            .unwrap();
        let own = store.session("self-id").unwrap().unwrap();
        let err = check_target(&own, "self-id", "self-id").unwrap_err();
        assert!(err.to_string().contains("this session"), "{err}");
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
        assert!(!socket.with_extension("lock").exists());

        let _ = std::fs::remove_file(&socket);
    }

    #[tokio::test]
    async fn a_waiter_on_an_unlinked_lock_retries_on_the_fresh_file() {
        let path = temp_socket().with_extension("lock");
        let first = lock(path.clone()).await.unwrap();
        let waiter = tokio::spawn(lock(path.clone()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !waiter.is_finished(),
            "the waiter must block on the held lock"
        );

        drop(first);
        let second = timeout(Duration::from_secs(5), waiter)
            .await
            .expect("the waiter never acquired")
            .unwrap()
            .unwrap();
        let at_path = std::fs::File::open(&path).expect("the waiter recreated the file");
        assert!(
            at_path.try_lock().is_err(),
            "the waiter must hold the file now at the path, not the unlinked one"
        );

        drop(second);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn concurrent_lockers_never_overlap() {
        let path = temp_socket().with_extension("lock");
        let inside = Arc::new(AtomicUsize::new(0));
        let lockers: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                let inside = inside.clone();
                tokio::spawn(async move {
                    for _ in 0..20 {
                        let _lock = lock(path.clone()).await.unwrap();
                        assert_eq!(
                            inside.fetch_add(1, Ordering::SeqCst),
                            0,
                            "overlapping holders"
                        );
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        inside.fetch_sub(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for locker in lockers {
            locker.await.unwrap();
        }
        assert!(!path.exists());
    }

    #[test]
    fn a_restart_appends_to_the_log_and_the_tail_is_the_current_run() {
        let path =
            std::env::temp_dir().join(format!("lp-log-{}.log", uuid::Uuid::new_v4().simple()));
        let mut first = open_run_log(&path, chrono::Local::now()).unwrap();
        writeln!(
            first,
            "[daemon] no one left to answer its prompts; stopping"
        )
        .unwrap();
        drop(first);
        let mut second = open_run_log(&path, chrono::Local::now()).unwrap();
        writeln!(second, "error: bad config").unwrap();
        drop(second);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("stopping"), "the earlier run is kept: {text}");
        assert_eq!(text.matches(RUN_SEPARATOR).count(), 2, "{text}");
        assert_eq!(log_tail(&path), "error: bad config");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn prune_logs_removes_only_old_logs() {
        let dir = std::env::temp_dir().join(format!("lp-prune-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&dir).unwrap();
        let now = SystemTime::now();
        let old = now - LOG_RETENTION - Duration::from_secs(60);
        for (name, modified) in [
            ("old.log", old),
            ("new.log", now),
            ("old.lock", old),
            ("old.sock", old),
        ] {
            std::fs::File::create(dir.join(name))
                .unwrap()
                .set_modified(modified)
                .unwrap();
        }

        prune_logs(&dir, now);
        let mut left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["new.log", "old.lock", "old.sock"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn child_row() -> SessionRow {
        let store = SessionStore::open_in_memory().unwrap();
        store.insert_session("parent", "/proj", None, None).unwrap();
        store
            .insert_session("child", "/proj", Some("parent"), Some("count files"))
            .unwrap();
        store.session("child").unwrap().unwrap()
    }

    // The receiver holds the child's back edge; dropping it would close the connection.
    fn serve_child(
        socket: &Path,
        broker: BrokerHandle,
    ) -> (
        tokio::task::JoinHandle<()>,
        mpsc::UnboundedReceiver<PeerRegistration>,
    ) {
        let (registrar, back) = mpsc::unbounded_channel();
        let serve = tokio::spawn(serve_socket(
            bind_listener(socket).unwrap(),
            broker,
            PeerAccept {
                identity: agent("child-x"),
                registrar,
            },
        ));
        (serve, back)
    }

    #[tokio::test]
    async fn an_ended_child_is_started_then_claimed_and_given_its_task() {
        let socket = temp_socket();
        let mut child = spawn_bare_broker(Vec::new());
        let bb_self = spawn_bare_broker(Vec::new());
        let served = std::sync::Mutex::new(None);
        let starts = AtomicUsize::new(0);
        let start = || {
            starts.fetch_add(1, Ordering::SeqCst);
            *served.lock().unwrap() = Some(serve_child(&socket, child.handle.clone()));
            async { Ok(()) }
        };

        let reg = claim(
            &child_row(),
            &socket,
            start,
            SocketClient::new(socket.clone()).dial_peer(bb_self.handle.clone(), agent("parent")),
            Some("count files".into()),
        )
        .await
        .expect("claim the started child");
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(reg.supervised);
        assert_eq!(reg.who.name, "child-x");
        assert_eq!(reg.who.task.as_deref(), Some("count files"));
        match timeout(Duration::from_secs(5), child.loop_ui_rx.recv()).await {
            Ok(Some((_, UiEvent::UserMessage { text }))) => assert_eq!(text, "count files"),
            other => panic!("expected the task as the child's first message, got {other:?}"),
        }

        let (serve, _back) = served.lock().unwrap().take().unwrap();
        serve.abort();
        child.task.abort();
        bb_self.task.abort();
        let _ = std::fs::remove_file(&socket);
    }

    #[tokio::test]
    async fn a_live_child_with_no_answerer_is_reclaimed_without_starting_it() {
        let socket = temp_socket();
        let mut child = spawn_bare_broker(Vec::new());
        let (serve, _back) = serve_child(&socket, child.handle.clone());
        let bb_self = spawn_bare_broker(Vec::new());

        let reg = claim(
            &child_row(),
            &socket,
            || async { anyhow::bail!("a live child must not be started") },
            SocketClient::new(socket.clone()).dial_peer(bb_self.handle.clone(), agent("parent")),
            None,
        )
        .await
        .expect("reclaim the live child");
        assert!(reg.supervised);
        assert!(
            timeout(Duration::from_millis(200), child.loop_ui_rx.recv())
                .await
                .is_err(),
            "a revive with no task sends nothing"
        );

        serve.abort();
        child.task.abort();
        bb_self.task.abort();
        let _ = std::fs::remove_file(&socket);
    }

    #[tokio::test]
    async fn a_child_someone_else_answers_is_taken_and_the_edge_dropped() {
        let socket = temp_socket();
        let child = spawn_bare_broker(Vec::new());
        let _human = child
            .handle
            .attach_as(
                ClientIdentity::human("h"),
                crate::core::ClientProfile::human(),
            )
            .await
            .unwrap();
        let (serve, mut back) = serve_child(&socket, child.handle.clone());
        let bb_self = spawn_bare_broker(Vec::new());

        let err = claim(
            &child_row(),
            &socket,
            || async { anyhow::bail!("a live child must not be started") },
            SocketClient::new(socket.clone()).dial_peer(bb_self.handle.clone(), agent("parent")),
            Some("ignored".into()),
        )
        .await
        .err()
        .expect("a refused grant must fail the claim");
        assert!(err.to_string().contains("already taken"), "{err}");
        assert!(err.to_string().contains("/connect-peer"), "{err}");

        let mut edge = timeout(Duration::from_secs(5), back.recv())
            .await
            .expect("the child registered its back edge")
            .unwrap();
        assert!(
            timeout(Duration::from_secs(5), edge.controller.events.recv())
                .await
                .expect("the dropped edge must close the connection")
                .is_none()
        );

        serve.abort();
        child.task.abort();
        bb_self.task.abort();
        let _ = std::fs::remove_file(&socket);
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
        let client = SocketClient::new(path.clone());
        let edge = ensure_running_then(
            &path,
            || async { anyhow::bail!("a live session must not be started") },
            client.dial_peer(bb_self.handle.clone(), agent("self")),
        )
        .await
        .expect("dial a live session");
        assert_eq!(edge.who.name, "target");
        assert!(edge.supervised);

        serve.abort();
        bb_target.task.abort();
        bb_self.task.abort();
        let _ = std::fs::remove_file(&path);
    }
}
