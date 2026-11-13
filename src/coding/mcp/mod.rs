//! MCP client. User-specified servers (`.mcp.json`) connect at startup and stay
//! in the cached tool prefix; dormant catalog servers load/unload mid-session,
//! the only thing that changes the tool surface after startup.

mod catalog;
mod oauth;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Result, anyhow};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, Content, Tool},
    service::{RoleClient, RunningService},
    transport::{
        ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::core::AgentEvent;

/// The API restricts tool names to `^[a-zA-Z0-9_-]{1,64}$`, so no `.` or `/`.
const SEP: &str = "__";

const CONFIG_FILE: &str = ".mcp.json";

pub const LOAD_TOOL_NAME: &str = "load_tool";

pub struct ServerSpec {
    pub name: String,
    /// Shown to the model in the `load_tool` schema; only set for dormant entries.
    pub description: Option<String>,
    pub transport: Transport,
}

pub enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Http {
        url: String,
        auth: HttpAuth,
    },
}

pub enum HttpAuth {
    /// Full `Authorization` header value, if any.
    Static(Option<String>),
    /// Empty `scopes` lets the server choose. No `client_id` ⇒ dynamic client
    /// registration (Google and others require a pre-registered client).
    OAuth {
        scopes: Vec<String>,
        client_id: Option<String>,
        client_secret: Option<String>,
    },
}

/// Matches Claude Code's `mcpServers` block for paste-compatibility. BTreeMap
/// keeps server order, and so the cached tool array, deterministic.
#[derive(Deserialize)]
struct ConfigFile {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: BTreeMap<String, ServerEntry>,
}

/// `command` ⇒ stdio; `url` ⇒ remote HTTP.
#[derive(Deserialize)]
struct ServerEntry {
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    url: Option<String>,
    token: Option<String>,
    token_env: Option<String>,
    /// `"oauth"` runs the OAuth flow; anything else uses the static token.
    auth: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    client_id: Option<String>,
    client_secret_env: Option<String>,
}

pub fn load_config(cwd: &Path) -> Result<Vec<ServerSpec>> {
    let path = cwd.join(CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(anyhow!("reading {}: {e}", path.display())),
    };
    let cfg: ConfigFile =
        serde_json::from_str(&text).map_err(|e| anyhow!("parsing {}: {e}", path.display()))?;
    cfg.mcp_servers
        .into_iter()
        .map(|(name, e)| entry_to_spec(name, e))
        .collect()
}

fn entry_to_spec(name: String, e: ServerEntry) -> Result<ServerSpec> {
    // `url` wins if both are set.
    let transport = if let Some(url) = e.url {
        let auth = if e.auth.as_deref() == Some("oauth") {
            let client_secret =
                match &e.client_secret_env {
                    Some(var) => Some(std::env::var(var).map_err(|_| {
                        anyhow!("server '{name}': client_secret_env `{var}` not set")
                    })?),
                    None => None,
                };
            HttpAuth::OAuth {
                scopes: e.scopes,
                client_id: e.client_id,
                client_secret,
            }
        } else {
            let token = match &e.token_env {
                Some(var) => Some(
                    std::env::var(var)
                        .map_err(|_| anyhow!("server '{name}': token_env `{var}` not set"))?,
                ),
                None => e.token,
            };
            HttpAuth::Static(token.map(|t| format!("Bearer {t}")))
        };
        Transport::Http { url, auth }
    } else if let Some(command) = e.command {
        Transport::Stdio {
            command,
            args: e.args,
            env: e.env.into_iter().collect(),
        }
    } else {
        return Err(anyhow!(
            "server '{name}': set either `command` (stdio) or `url` (http)"
        ));
    };
    Ok(ServerSpec {
        name,
        description: None,
        transport,
    })
}

type Client = RunningService<RoleClient, ()>;

#[derive(Clone, Copy, PartialEq)]
enum Layer {
    UserSpecified,
    Dormant,
}

struct ServerConn {
    layer: Layer,
    client: Client,
    schemas: Vec<Value>,
}

struct Route {
    /// A name key, not an index, so unloading a server doesn't invalidate other routes.
    server: String,
    /// Un-namespaced tool name on the server.
    tool: String,
    read_only: bool,
}

pub struct McpRegistry {
    servers: BTreeMap<String, ServerConn>,
    /// Keyed by namespaced tool name.
    routes: HashMap<String, Route>,
    dormant: BTreeMap<String, ServerSpec>,
    /// Emitted by the caller before the TUI starts.
    pub connect_log: Vec<String>,
}

impl McpRegistry {
    /// A failed connection is logged and skipped so the agent still runs.
    pub async fn bootstrap(user_specs: &[ServerSpec]) -> Self {
        let mut reg = Self {
            servers: BTreeMap::new(),
            routes: HashMap::new(),
            dormant: catalog::dormant_catalog()
                .into_iter()
                .map(|s| (s.name.clone(), s))
                .collect(),
            connect_log: Vec::new(),
        };
        for spec in user_specs {
            match connect_one(spec, None).await {
                Ok((client, tools)) => {
                    let n = reg.register(&spec.name, Layer::UserSpecified, client, &tools);
                    reg.connect_log
                        .push(format!("[mcp] connected: {} ({n} tools)", spec.name));
                }
                Err(e) => {
                    reg.connect_log
                        .push(format!("[mcp] FAILED to connect {}: {e:#}", spec.name));
                }
            }
        }
        reg
    }

    fn register(&mut self, name: &str, layer: Layer, client: Client, tools: &[Tool]) -> usize {
        let prefix = sanitize(name);
        let mut schemas = Vec::with_capacity(tools.len());
        for tool in tools {
            let orig = tool.name.to_string();
            let namespaced = format!("{prefix}{SEP}{}", sanitize(&orig));
            let read_only = tool
                .annotations
                .as_ref()
                .and_then(|a| a.read_only_hint)
                .unwrap_or(false);
            self.routes.insert(
                namespaced.clone(),
                Route {
                    server: name.to_string(),
                    tool: orig,
                    read_only,
                },
            );
            schemas.push(tool_to_schema(&namespaced, tool));
        }
        self.servers.insert(
            name.to_string(),
            ServerConn {
                layer,
                client,
                schemas,
            },
        );
        tools.len()
    }

    /// `notify` carries an OAuth authorize URL out to the TUI.
    pub async fn load(
        &mut self,
        name: &str,
        notify: Option<&mpsc::Sender<AgentEvent>>,
    ) -> Result<usize> {
        if self.servers.contains_key(name) {
            return Err(anyhow!("server '{name}' is already loaded"));
        }
        // Scope the catalog borrow so it ends before the `&mut self` register.
        let (client, tools) = {
            let spec = self
                .dormant
                .get(name)
                .ok_or_else(|| anyhow!("no dormant server named '{name}'"))?;
            connect_one(spec, notify).await?
        };
        Ok(self.register(name, Layer::Dormant, client, &tools))
    }

    pub fn unload(&mut self, name: &str) -> Result<()> {
        match self.servers.get(name) {
            None => return Err(anyhow!("server '{name}' is not loaded")),
            Some(conn) if conn.layer != Layer::Dormant => {
                return Err(anyhow!(
                    "'{name}' is a user-specified server (from .mcp.json) and can't be unloaded"
                ));
            }
            Some(_) => {}
        }
        self.servers.remove(name); // Drop closes the transport / kills the child.
        self.routes.retain(|_, r| r.server != name);
        Ok(())
    }

    pub fn status_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for (name, conn) in &self.servers {
            let origin = match conn.layer {
                Layer::UserSpecified => "user",
                Layer::Dormant => "dormant",
            };
            lines.push(format!(
                "  {name} — loaded ({origin}, {} tools)",
                conn.schemas.len()
            ));
        }
        for name in self.dormant.keys() {
            if !self.servers.contains_key(name) {
                lines.push(format!("  {name} — available (dormant)"));
            }
        }
        if lines.is_empty() {
            lines.push("  (no MCP servers)".into());
        }
        lines
    }

    /// `(name, description, loaded)`, in stable order so clients render a steady menu.
    pub fn catalog(&self) -> Vec<(String, Option<String>, bool)> {
        let mut out = Vec::new();
        for (name, conn) in &self.servers {
            if conn.layer == Layer::UserSpecified {
                out.push((name.clone(), None, true));
            }
        }
        for (name, spec) in &self.dormant {
            out.push((
                name.clone(),
                spec.description.clone(),
                self.servers.contains_key(name),
            ));
        }
        out
    }

    /// Byte-stable across the session (built from the fixed, sorted `dormant`
    /// map, not `servers`), so it stays in the cached tool prefix.
    pub fn load_tool_schema(&self) -> Option<Value> {
        if self.dormant.is_empty() {
            return None;
        }
        let mut listing = String::new();
        for (name, spec) in &self.dormant {
            let desc = spec.description.as_deref().unwrap_or("(no description)");
            listing.push_str(&format!("\n- {name}: {desc}"));
        }
        Some(json!({
            "name": LOAD_TOOL_NAME,
            "description": format!(
                "Connect a dormant MCP server so its tools become callable. Use \
                 this when the task needs a capability the current tools don't \
                 cover but one of the dormant servers below provides — loading is \
                 cheap and keeps unused tools out of context until needed. The \
                 server's tools appear in your tool list on your next turn; call \
                 them then. Loading may require a one-time interactive \
                 authorization. Available dormant servers:{listing}"
            ),
            "input_schema": {
                "type": "object",
                "properties": {
                    "server": {
                        "type": "string",
                        "description": "Name of a dormant server listed in this tool's description."
                    }
                },
                "required": ["server"]
            }
        }))
    }

    pub fn always_on_schemas(&self) -> Vec<Value> {
        self.schemas_for(Layer::UserSpecified)
    }

    pub fn dormant_schemas(&self) -> Vec<Value> {
        self.schemas_for(Layer::Dormant)
    }

    fn schemas_for(&self, layer: Layer) -> Vec<Value> {
        self.servers
            .values()
            .filter(|s| s.layer == layer)
            .flat_map(|s| s.schemas.iter().cloned())
            .collect()
    }

    pub fn is_mcp_tool(&self, name: &str) -> bool {
        self.routes.contains_key(name)
    }

    /// `readOnlyHint` comes from an untrusted server, which could under-report a
    /// destructive tool; unhinted tools gate.
    pub fn requires_permission(&self, name: &str) -> bool {
        self.routes.get(name).map(|r| !r.read_only).unwrap_or(true)
    }

    /// An `is_error` result returns `Err` so it flows through the tool_result error path.
    pub async fn call(&self, name: &str, input: &Value) -> Result<String> {
        let route = self
            .routes
            .get(name)
            .ok_or_else(|| anyhow!("unknown MCP tool: {name}"))?;
        let args = input.as_object().cloned().unwrap_or_default();
        let conn = self
            .servers
            .get(&route.server)
            .ok_or_else(|| anyhow!("MCP tool '{name}' routes to a disconnected server"))?;
        let result = conn
            .client
            .call_tool(CallToolRequestParams::new(route.tool.clone()).with_arguments(args))
            .await?;
        let text = flatten_content(&result.content);
        if result.is_error.unwrap_or(false) {
            Err(anyhow!(if text.is_empty() {
                "MCP tool returned an error".to_string()
            } else {
                text
            }))
        } else {
            Ok(text)
        }
    }
}

async fn connect_one(
    spec: &ServerSpec,
    notify: Option<&mpsc::Sender<AgentEvent>>,
) -> Result<(Client, Vec<Tool>)> {
    let client = match &spec.transport {
        Transport::Stdio { command, args, env } => {
            let cmd = Command::new(command).configure(|c| {
                for a in args {
                    c.arg(a);
                }
                for (k, v) in env {
                    c.env(k, v);
                }
            });
            ().serve(TokioChildProcess::new(cmd)?).await?
        }
        Transport::Http { url, auth } => match auth {
            HttpAuth::Static(auth_header) => {
                let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
                config.auth_header = auth_header.clone();
                ().serve(StreamableHttpClientTransport::from_config(config))
                    .await?
            }
            HttpAuth::OAuth {
                scopes,
                client_id,
                client_secret,
            } => {
                let transport = oauth::authorized_transport(
                    &spec.name,
                    url,
                    scopes,
                    client_id.as_deref(),
                    client_secret.as_deref(),
                    notify,
                )
                .await?;
                ().serve(transport).await?
            }
        },
    };
    let tools = client.list_all_tools().await?;
    Ok((client, tools))
}

fn tool_to_schema(namespaced: &str, tool: &Tool) -> Value {
    json!({
        "name": namespaced,
        "description": tool.description.as_deref().unwrap_or(""),
        "input_schema": Value::Object((*tool.input_schema).clone()),
    })
}

fn flatten_content(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
