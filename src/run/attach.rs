use anyhow::{Context, Result, bail};
use std::path::Path;

use super::local_peer::{accepts, label};
use crate::cli::Cli;
use crate::coding;
use crate::core::store::SessionRow;
use crate::core::{ClientIdentity, Controller};
use crate::run::local_identity;
use crate::transport::{self, SocketClient};
use crate::tui;

// `target` routes like /connect-peer: a pairing code attaches over the relay, anything
// else is a local session. An ended local session is not started.
pub async fn run(target: &str, cli: &Cli) -> Result<()> {
    // Placeholders: the host's SessionInfo event, replayed first on attach, fills in
    // the real session id / model / cwd / branch.
    let who = local_identity();
    let ui_cfg = tui::UiConfig {
        session_id: "(connecting…)".into(),
        session_name: None,
        auto: false,
        model: crate::config::default_model(),
        thinking_display: crate::config::resolve_thinking(cli.thinking.as_ref())?,
        pairing_qr: None,
        pairing_code: None,
        pairing_qr_watch: None,
        pairing_code_watch: None,
        pairing_qr_agent: None,
        pairing_code_agent: None,
        is_owner: false,
        user_name: who.name.clone(),
        // Filled from the host's Capabilities event so a guest shows the host's models.
        models: Vec::new(),
    };
    // `connect` rather than the silent `SessionHandle::attach`: this runs before the
    // TUI owns the screen, so a connection failure can surface its cause here.
    if transport::Pairing::is_code(target) {
        let pairing = transport::Pairing::decode(target)?;
        let client = transport::RelayClient::new(pairing.client_dial_url(), pairing.cipher);
        let controller = match client.connect(None, who.clone()).await? {
            Some(c) => c,
            None => bail!("could not attach: the relay session has ended"),
        };
        tui::run(&client, ui_cfg, who, controller, None).await
    } else {
        let row = coding::find_session(target)?
            .with_context(|| format!("no session matches {target:?}"))?;
        let socket = super::session_socket_path(&row.id)?;
        let client = SocketClient::new(socket.clone());
        let cwd = std::env::current_dir().context("could not determine cwd")?;
        let controller = attach_local(
            &client,
            &socket,
            &row,
            &cwd.display().to_string(),
            who.clone(),
        )
        .await?;
        tui::run(&client, ui_cfg, who, controller, None).await
    }
}

// `cwd` is where the user runs nudge; `--resume` opens a session in the current
// directory, so a session from elsewhere needs a `cd` first.
async fn attach_local(
    client: &SocketClient,
    socket: &Path,
    row: &SessionRow,
    cwd: &str,
    who: ClientIdentity,
) -> Result<Controller> {
    if !accepts(socket).await {
        let label = label(row);
        if row.cwd == cwd {
            bail!("session {label} isn't running (resume it with nudge --resume {label})");
        }
        bail!(
            "session {label} isn't running (resume it with: cd {} && nudge --resume {label})",
            row.cwd
        );
    }
    match client.connect(None, who).await? {
        Some(c) => Ok(c),
        None => bail!("could not attach: session {} has ended", label(row)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::store::SessionStore;

    #[tokio::test]
    async fn a_dead_local_session_is_not_running() {
        let store = SessionStore::open_in_memory().unwrap();
        store
            .insert_session("0123456789abcdef", "/proj", None, None)
            .unwrap();
        store.set_name("0123456789abcdef", "worker", None).unwrap();
        let row = store.session("0123456789abcdef").unwrap().unwrap();
        let token = &uuid::Uuid::new_v4().simple().to_string()[..12];
        let socket = std::env::temp_dir().join(format!("at-{token}.sock"));

        for (cwd, want) in [
            (
                "/proj",
                "session worker isn't running (resume it with nudge --resume worker)",
            ),
            (
                "/elsewhere",
                "session worker isn't running (resume it with: cd /proj && nudge --resume worker)",
            ),
        ] {
            let Err(err) = attach_local(
                &SocketClient::new(socket.clone()),
                &socket,
                &row,
                cwd,
                ClientIdentity::human("h"),
            )
            .await
            else {
                panic!("attaching to a dead session must fail");
            };
            assert_eq!(err.to_string(), want);
        }
    }

    #[tokio::test]
    async fn a_pairing_code_routes_to_the_relay() {
        use clap::Parser;
        let cli = Cli::parse_from(["nudge"]);
        let err = run("nudge:not*base64", &cli).await.unwrap_err();
        assert!(err.to_string().contains("base64"), "{err}");
    }
}
