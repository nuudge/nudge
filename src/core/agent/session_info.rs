use tokio::sync::mpsc;

use super::types::AgentConfig;
use crate::core::events::AgentEvent;
use crate::core::session::Session;

// Emit SessionInfo only when the (model, branch, name) tuple changed, so headers track
// the daemon without an identical event flooding the replay buffer each turn. `branch`
// — and the session fields — are read by the caller *before* the await: holding
// `&Backend` across it would force a `B: Sync` bound on the whole loop, and holding
// `&Session` would demand `Sync` of the store connection.
pub(super) async fn emit_session_info_if_changed(
    tx: &mpsc::Sender<AgentEvent>,
    model: &str,
    branch: Option<String>,
    session_id: String,
    cwd: String,
    session_name: Option<String>,
    last: &mut (String, Option<String>, Option<String>),
) {
    let current = (model.to_string(), branch, session_name);
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
        })
        .await;
}

// Persist a resolved label and broadcast it: a Notice (so the user — and the TUI, which
// can't know a daemon-derived name synchronously — sees the final label) plus a re-emit
// of SessionInfo through the change-detecting helper.
pub(super) async fn finalize_rename(
    name: String,
    branch: Option<String>,
    cfg: &AgentConfig,
    session: &mut Session,
    tx: &mpsc::Sender<AgentEvent>,
    last_ctx: &mut (String, Option<String>, Option<String>),
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
