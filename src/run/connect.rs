use anyhow::{Result, bail};

use crate::cli::Cli;
use crate::run::local_identity;
use crate::transport;
use crate::tui;

pub async fn run(cli: Cli) -> Result<()> {
    // Placeholders: the daemon's SessionInfo event, replayed first on attach, fills
    // in the real session id / model / cwd / branch.
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
        // Filled from the daemon's Capabilities event so a guest shows the daemon's models.
        models: Vec::new(),
    };
    // `connect` rather than the silent `SessionHandle::attach`: this runs before the
    // TUI owns the screen, so a connection failure can surface its cause here.
    if let Some(code) = cli.pair_code {
        let pairing = transport::Pairing::decode(&code)?;
        let client = transport::RelayClient::new(pairing.client_dial_url(), pairing.cipher);
        let controller = match client.connect(None, who.clone()).await? {
            Some(c) => c,
            None => bail!("could not attach: the relay session has ended"),
        };
        tui::run(&client, ui_cfg, who, controller, None).await
    } else if let Some(path) = cli.socket {
        let client = transport::SocketClient::new(path);
        let controller = match client.connect(None, who.clone()).await? {
            Some(c) => c,
            None => bail!("could not attach: the daemon has no live session"),
        };
        tui::run(&client, ui_cfg, who, controller, None).await
    } else {
        bail!("--connect needs --pair-code <code> (relay) or --socket <path> (local debug daemon)")
    }
}
