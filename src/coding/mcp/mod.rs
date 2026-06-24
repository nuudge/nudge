//! MCP (Model Context Protocol) client — Phase 7.
//!
//! Connects to external MCP servers — local **stdio** subprocesses or remote
//! **Streamable HTTP** endpoints (optionally with a static bearer token) — and
//! exposes their tools to the agent loop in the same shape as built-in tools.
//! To the model, an MCP tool is indistinguishable from a native one; only the
//! dispatch branch in `agent.rs` routes by name. Permission follows each tool's
//! `readOnlyHint` annotation.
//!
//! Servers come from two origins with different load policies:
//! - **User-specified** (`.mcp.json` in cwd) connect at startup and stay for the
//!   session — part of the stable, cached tool prefix.
//! - **Dormant** (the built-in [`catalog`]) are known but disconnected; the user
//!   loads/unloads them mid-session (`/mcp load <name>`), which is the only
//!   thing that changes the tool surface after startup.

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

/// Separator between the server namespace and the tool name. Double underscore
/// because the Anthropic API restricts tool names to `^[a-zA-Z0-9_-]{1,64}$`,
/// so `.` / `/` are not options.
const SEP: &str = "__";

/// Project-local config filename, read from the session cwd.
const CONFIG_FILE: &str = ".mcp.json";

/// Name of the foundational meta-tool that lets the *model* connect a dormant
/// server on demand (the model-driven counterpart to the user's `/mcp load`).
pub const LOAD_TOOL_NAME: &str = "load_tool";

/// A server to connect to, parsed from `.mcp.json` or the dormant [`catalog`].
pub struct ServerSpec {
    pub name: String,
    /// One-line capability blurb shown to the model in the `load_tool` schema so
    /// it can decide what to pull in. Only set for dormant-catalog entries;
    /// `None` for user-specified servers (those load at startup, not on request).
    pub description: Option<String>,
    pub transport: Transport,
}

/// How to reach a server.
pub enum Transport {
    /// Local subprocess; client launches it and talks over stdin/stdout.
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    /// Remote Streamable HTTP endpoint.
    Http { url: String, auth: HttpAuth },
}

/// How a remote HTTP server is authenticated.
pub enum HttpAuth {
    /// Optional static `Authorization` header value (e.g. `Bearer <token>`).
    Static(Option<String>),
    /// OAuth 2.0 authorization-code flow (PKCE). Empty `scopes` lets the server
    /// select them. Tokens are obtained interactively once, then persisted.
    /// `client_id` present ⇒ pre-registered client (e.g. Google, which doesn't
    /// support dynamic registration); absent ⇒ dynamic client registration.
    OAuth {
        scopes: Vec<String>,
        client_id: Option<String>,
        client_secret: Option<String>,
    },
}

/// `.mcp.json` shape — matches Claude Code / Claude Desktop's `mcpServers` block
/// for paste-compatibility. A `BTreeMap` gives deterministic (sorted) server
/// order, which keeps the merged `tools` array stable for prompt caching.
#[derive(Deserialize)]
struct ConfigFile {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: BTreeMap<String, ServerEntry>,
}

/// One entry. `command` ⇒ stdio; `url` ⇒ remote HTTP. Fields are optional so a
/// single shape covers both transports.
#[derive(Deserialize)]
struct ServerEntry {
    // stdio
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    // http
    url: Option<String>,
    /// Literal bearer token (convenient, but avoid committing secrets).
    token: Option<String>,
    /// Name of an env var holding the bearer token — preferred for secrets.
    token_env: Option<String>,
    /// Auth mode for an http server: `"oauth"` runs the OAuth flow; anything
    /// else (or absent) uses the static token above.
    auth: Option<String>,
    /// OAuth scopes to request (optional; empty lets the server choose).
    #[serde(default)]
    scopes: Vec<String>,
    /// Pre-registered OAuth client id (e.g. a Google Cloud web-app client).
    /// When set, skips dynamic client registration.
    client_id: Option<String>,
    /// Env var holding the pre-registered client secret (kept out of the file).
    client_secret_env: Option<String>,
}

/// Load server specs from a project-local `.mcp.json` in `cwd`. A missing file
/// is not an error — MCP is opt-in per project, so we return an empty list.
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
    // `url` wins if both are set; otherwise fall back to stdio `command`.
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

/// All servers share the no-op `()` client handler, so the running-service type
/// is concrete — no `into_dyn()` needed for the single-handler case.
type Client = RunningService<RoleClient, ()>;

/// Which origin/load-policy a connected server belongs to. Only `Dormant`
/// servers can be unloaded; user-specified ones live for the whole session.
#[derive(Clone, Copy, PartialEq)]
enum Layer {
    UserSpecified,
    Dormant,
}

struct ServerConn {
    layer: Layer,
    client: Client,
    /// Pre-built, namespaced, Anthropic-shaped tool schemas for this server.
    schemas: Vec<Value>,
}

/// Where a namespaced tool lives and whether it's safe to auto-run.
struct Route {
    /// Key into `servers` — the server's (raw) name. A name key, not a Vec
    /// index, so unloading a server doesn't invalidate other tools' routes.
    server: String,
    /// Original (un-namespaced) tool name on the server.
    tool: String,
    /// From the server's `readOnlyHint` annotation — auto-allow if true.
    read_only: bool,
}

pub struct McpRegistry {
    /// server name -> connection
    servers: BTreeMap<String, ServerConn>,
    /// namespaced tool name -> route
    routes: HashMap<String, Route>,
    /// The built-in dormant catalog, keyed by server name — parsed but not
    /// connected until loaded on demand.
    dormant: BTreeMap<String, ServerSpec>,
    /// Human-readable connect outcomes, emitted by the caller before the TUI starts.
    pub connect_log: Vec<String>,
}

impl McpRegistry {
    /// Connect the user-specified servers (from `.mcp.json`) and stage the
    /// built-in dormant catalog (parsed, not connected). A failed connection is
    /// logged and skipped — the agent still runs with built-in tools (graceful
    /// degradation). Dormant servers connect later via [`load`](Self::load).
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

    /// Register an already-connected server: namespace its tools, build their
    /// schemas, and install routes. Returns the tool count. Pure mutation — no
    /// I/O — so it borrows `&mut self` without holding any spec borrow.
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

    /// Connect a dormant server by name. Errors if the name is unknown to the
    /// catalog or already connected. `notify` carries the OAuth authorize URL
    /// out to the TUI when an interactive auth flow is triggered mid-session.
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

    /// Disconnect a dormant server by name. Dropping the `ServerConn` closes the
    /// transport / kills the subprocess. Refuses to unload user-specified servers.
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

    /// One line per server: loaded user-specified, loaded dormant, then dormant
    /// servers still available to load. For the `/mcp` listing.
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

    /// Schema for the `load_tool` meta-tool. The dormant catalog (name +
    /// description) is baked into the tool's `description`, so the model knows
    /// what it can pull in without a separate discovery round-trip. The catalog
    /// is compile-time-fixed and `dormant` iterates in sorted order, so this
    /// schema is byte-stable across the session — it stays a foundational,
    /// cached tool even as servers load/unload (loading mutates `servers`, not
    /// `dormant`). Returns `None` when the catalog is empty (nothing to load).
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

    /// Schemas for always-on servers (user-specified) — the stable, cached
    /// prefix. Kept separate from [`dormant_schemas`](Self::dormant_schemas) so
    /// the agent can place a cache breakpoint at the boundary between them.
    pub fn always_on_schemas(&self) -> Vec<Value> {
        self.schemas_for(Layer::UserSpecified)
    }

    /// Schemas for currently-loaded dormant servers — the dynamic tail.
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

    /// Whether an MCP tool should prompt for permission. Tools the server marks
    /// `readOnlyHint` auto-allow; everything else (incl. unhinted tools) gates.
    /// NOTE: annotations come from the (untrusted) server — a hostile server
    /// could under-report a destructive tool as read-only. Per-server trust is
    /// the stronger control; this is the ergonomic default. See [[mcp]] tool-safety.
    pub fn requires_permission(&self, name: &str) -> bool {
        self.routes.get(name).map(|r| !r.read_only).unwrap_or(true)
    }

    /// Invoke a namespaced MCP tool. Flattens the result's text content to a
    /// String; a result flagged `is_error` is returned as `Err` so it flows
    /// through the existing tool_result error pipeline.
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
    // Both transports yield the same `RunningService<RoleClient, ()>`, so only
    // the transport construction differs — the handshake/discovery is identical.
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
        // MCP's `inputSchema` -> the API's `input_schema`; `title` is dropped.
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
