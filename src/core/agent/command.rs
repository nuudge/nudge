use crate::core::events::CommandInfo;

// Internal; the wire carries only the raw line, so the grammar lives in one place.
pub(super) enum Command {
    SetModel(String),
    // None = let the loop derive a name.
    Rename(Option<String>),
    Mcp(McpCommand),
    Peers,
    // Human-only.
    ConnectPeer(String),
    Auto,
    ModelUsage,
    Unknown(String),
}

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
        Some("/auto") => Command::Auto,
        _ => Command::Unknown(line.to_string()),
    }
}

// Auto mode pre-approves future Spawn prompts, so the broker only lets a client that
// may answer permission prompts issue it.
pub(crate) fn pre_approves_permissions(line: &str) -> bool {
    matches!(parse(line), Command::Auto)
}

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
            usage: "/connect-peer <session id|name|pairing-code> — make another session a peer"
                .into(),
        },
        CommandInfo {
            name: "/auto".into(),
            usage: "/auto — toggle auto mode (delegate work to a supervised subagent)".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn catalog_advertises_connect_peer() {
        assert!(command_catalog().iter().any(|c| c.name == "/connect-peer"));
    }

    #[test]
    fn only_auto_pre_approves_permissions() {
        assert!(matches!(parse("/auto"), Command::Auto));
        assert!(pre_approves_permissions(" /auto "));
        assert!(!pre_approves_permissions("/peers"));
        assert!(!pre_approves_permissions("/automate"));
        assert!(command_catalog().iter().any(|c| c.name == "/auto"));
    }
}
