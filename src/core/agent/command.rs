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
    ]
}
