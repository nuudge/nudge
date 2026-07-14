use anyhow::{Result, bail};

use crate::cli::Cli;
use crate::models::{DEFAULT_MODEL, MODELS, owned_models};
use crate::run::local_identity;
use crate::transport;
use crate::tui;

// Attach a TUI to a running daemon (Unix socket or relay). The client owns no loop
// and no session metadata, so the header starts as placeholders and is filled from
// the daemon's `SessionInfo` event — replayed first on attach.
pub async fn run(cli: Cli) -> Result<()> {
    // Placeholders only: the daemon's SessionInfo event (replayed first on attach)
    // overwrites session id / model / cwd / branch with the daemon's real context.
    let who = local_identity();
    let ui_cfg = tui::UiConfig {
        session_id: "(connecting…)".into(),
        session_name: None,
        model: DEFAULT_MODEL.into(),
        thinking_display: cli.thinking.as_display(),
        // A --connect client never hosts a relay, so it shows no pairing QR.
        pairing_qr: None,
        pairing_code: None,
        // A --connect client is a guest: it attaches to a daemon it doesn't host
        // (cosmetic badge only — clients coexist, none reclaims).
        is_owner: false,
        user_name: who.name.clone(),
        // A guest has no local API key to fetch a fresh list with; the daemon
        // (the owner process) is the one that talks to the provider.
        models: owned_models(MODELS),
    };
    // Use each client's `connect` rather than the silent `SessionHandle::attach` for
    // this first attach: it runs before the TUI owns the screen, so a connection
    // failure can (and should) surface its cause here. `Err` = transport failure
    // (no daemon / relay unreachable); `None` = reachable but the session is gone
    // (the daemon's broker has shut down).
    if let Some(code) = cli.pair_code {
        // A scanned pairing code is self-contained: it carries the relay URL, room
        // id, and E2E key, so it needs no other flags.
        let pairing = transport::Pairing::decode(&code)?;
        let client = transport::RelayClient::new(pairing.client_dial_url(), pairing.cipher);
        let controller = match client.connect(None, who.clone()).await? {
            Some(c) => c,
            None => bail!("could not attach: the relay session has ended"),
        };
        tui::run(&client, ui_cfg, who, controller, None).await
    } else if let Some(path) = cli.socket {
        // Local debug daemon over a Unix socket (--daemon --socket on the other end).
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
