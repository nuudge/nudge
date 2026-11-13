use tokio::sync::mpsc;

use super::types::AgentConfig;
use crate::core::events::AgentEvent;
use crate::core::session::Session;

// Dedup keeps identical events from flooding the replay buffer each turn. Fields are
// passed owned: borrowing `&Backend`/`&Session` across the await would demand `Sync`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn emit_session_info_if_changed(
    tx: &mpsc::Sender<AgentEvent>,
    model: &str,
    branch: Option<String>,
    session_id: String,
    cwd: String,
    session_name: Option<String>,
    auto: bool,
    last: &mut (String, Option<String>, Option<String>, bool),
) {
    let current = (model.to_string(), branch, session_name, auto);
    if current == *last {
        return;
    }
    *last = current.clone();
    let _ = tx
        .send(AgentEvent::SessionInfo {
            model: current.0,
            cwd,
            git_branch: current.1,
            session_id,
            session_name: current.2,
            auto: current.3,
        })
        .await;
}

// The Notice matters: the TUI can't know a daemon-derived name synchronously.
pub(super) async fn finalize_rename(
    name: String,
    branch: Option<String>,
    cfg: &AgentConfig,
    session: &mut Session,
    tx: &mpsc::Sender<AgentEvent>,
    last_ctx: &mut (String, Option<String>, Option<String>, bool),
) {
    match session.set_name(name.clone(), branch.clone()) {
        Ok(()) => {
            let _ = tx
                .send(AgentEvent::Notice {
                    text: format!("session renamed to '{name}'"),
                })
                .await;
            emit_session_info_if_changed(
                tx,
                &cfg.model,
                branch,
                session.id.clone(),
                session.cwd_display(),
                session.name.clone(),
                session.auto,
                last_ctx,
            )
            .await;
        }
        Err(e) => {
            let _ = tx
                .send(AgentEvent::Error {
                    message: format!("rename failed: {e:#}"),
                })
                .await;
        }
    }
}
