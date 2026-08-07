use crate::core::events::CommandInfo;

// The server-side parse of a `/…` command line. Internal — never crosses the wire
// (the wire carries `UiEvent::Command { line }`). The loop executes each variant
// against its own state (model / session / MCP registry), so the grammar lives in
// exactly one place regardless of which front-end typed the line.
pub(super) enum Command {
    // Switch the API model at the next turn boundary.
    SetModel(String),
    // Rename the session: Some = verbatim, None = let the loop derive one.
    Rename(Option<String>),
    Mcp(McpCommand),
    // List the session's held peer edges (read-only).
    Peers,
    // Dial a remote peer over the relay using a pasted pairing code (human-only).
    ConnectPeer(String),
    // `/model` with no argument: not an error, but the pickerless path needs a hint.
    ModelUsage,
    // Anything unrecognized, echoed back so the client sees why nothing happened.
    Unknown(String),
}

// The MCP subgrammar, handed to the backend (which owns the registry).
pub enum McpCommand {
    List,
    Load(String),
    Unload(String),
    Usage,
}

pub(super) fn parse(line: &str) -> Command {
    let line = line.trim();
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("/model") => match parts.next() {
            Some(id) => Command::SetModel(id.to_string()),
            None => Command::ModelUsage,
        },
        Some("/session-rename") => {
            let arg = line["/session-rename".len()..].trim();
            Command::Rename((!arg.is_empty()).then(|| arg.to_string()))
        }
        Some("/mcp") => Command::Mcp(match (parts.next(), parts.next()) {
            (None, _) => McpCommand::List,
            (Some("load"), Some(name)) => McpCommand::Load(name.to_string()),
            (Some("unload"), Some(name)) => McpCommand::Unload(name.to_string()),
            _ => McpCommand::Usage,
        }),
        Some("/peers") => Command::Peers,
        Some("/connect-peer") => {
            let arg = line["/connect-peer".len()..].trim();
            Command::ConnectPeer(arg.to_string())
        }
        _ => Command::Unknown(line.to_string()),
    }
}

// The daemon's command list, advertised in Capabilities so clients render menus
// (and know what to send) without a compiled-in copy of the grammar.
pub fn command_catalog() -> Vec<CommandInfo> {
    vec![
        CommandInfo {
            name: "/model".into(),
            usage: "/model <id> — switch the API model".into(),
        },
        CommandInfo {
            name: "/session-rename".into(),
            usage: "/session-rename [name] — rename the session (bare = derive one)".into(),
        },
        CommandInfo {
            name: "/mcp".into(),
            usage: "/mcp | /mcp load <name> | /mcp unload <name>".into(),
        },
        CommandInfo {
            name: "/peers".into(),
            usage: "/peers — list held peer agents".into(),
        },
        CommandInfo {
            name: "/connect-peer".into(),
            usage: "/connect-peer <pairing-code> — dial a remote peer agent over the relay".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    // `/connect-peer` takes the rest of the line as the pairing code; bare = empty
    // (dispatch turns that into a usage hint).
    #[test]
    fn parses_connect_peer_with_and_without_code() {
        match parse("/connect-peer nudge:abc123") {
            Command::ConnectPeer(code) => assert_eq!(code, "nudge:abc123"),
            _ => panic!("expected ConnectPeer with the code"),
        }
        match parse("/connect-peer") {
            Command::ConnectPeer(code) => assert!(code.is_empty()),
            _ => panic!("expected ConnectPeer with an empty code"),
        }
    }

    // The command is advertised so front-ends render it (and know it exists).
    #[test]
    fn catalog_advertises_connect_peer() {
        assert!(command_catalog().iter().any(|c| c.name == "/connect-peer"));
    }
}
