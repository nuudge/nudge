use anyhow::Result;
use serde_json::Value;
use std::future::Future;
use tokio::sync::mpsc;

use crate::core::events::{AgentEvent, McpServerInfo, ModelInfo, UiEvent};
use crate::core::host::BrokerHandle;
use crate::core::identity::ClientIdentity;
use crate::core::peer::{PeerDialer, PeerFactory, PeerRegistration, PeerSet};
use crate::llm::SystemBlock;

use super::command::McpCommand;

pub struct AgentConfig {
    pub model: String,
    pub max_tokens: u32,
    pub max_iterations: usize,
    // "summarized" shows thinking text; "omitted" sends only the signature (faster TTFT,
    // same cost).
    pub thinking_display: String,
    pub models: Vec<ModelInfo>,
}

// Keeps the loop unaware of tools, MCP, AGENTS.md and cwd concerns, so this module
// doesn't depend on the layer above it.
pub trait Backend {
    // Rebuilt each turn so volatile context (env, git, dir listing) stays fresh.
    fn system_blocks(&self) -> Vec<SystemBlock>;
    // The usize is the stable/dynamic cache-boundary index (None = no caching).
    fn tool_schemas(&self) -> (Vec<Value>, Option<usize>);
    fn web_search(&self) -> bool {
        false
    }
    fn tool_summary(&self, name: &str, input: &Value) -> String;
    fn requires_permission(&self, name: &str) -> bool;
    fn permission_summary(&self, name: &str, input: &Value) -> String;
    fn execute(
        &mut self,
        name: &str,
        input: &Value,
        notify: &mpsc::Sender<AgentEvent>,
    ) -> impl Future<Output = Result<String>> + Send;
    fn handle_mcp(
        &mut self,
        cmd: &McpCommand,
        notify: &mpsc::Sender<AgentEvent>,
    ) -> impl Future<Output = String> + Send;
    fn mcp_catalog(&self) -> Vec<McpServerInfo>;
    // Re-read each turn boundary so a mid-session `git checkout` reaches the header.
    fn git_branch(&self) -> Option<String> {
        None
    }
}

// The broker stamps the identity from its registry (a client never claims its own), so
// the loop can trust it. `None` marks broker-internal sends such as the final Quit.
pub type LoopInput = (Option<ClientIdentity>, UiEvent);

// `peers` are agents this loop is a client of; `peer_register_rx` delivers ones added at
// runtime (e.g. a spawned child).
pub struct AgentIo {
    pub ui_rx: mpsc::Receiver<LoopInput>,
    pub agent_tx: mpsc::Sender<AgentEvent>,
    pub peers: PeerSet,
    pub peer_register_rx: Option<mpsc::UnboundedReceiver<PeerRegistration>>,
    // None = this agent may not spawn.
    pub peer_factory: Option<PeerFactory>,
    // None = this session may not dial peers (e.g. a spawned child). The dial runs
    // spawned so the loop never blocks; the edge returns via `peer_register_rx`.
    pub peer_dialer: Option<PeerDialer>,
    // This agent's own broker, so a spawned or dialed peer can attach back.
    pub self_handle: BrokerHandle,
    // Sticky: set once two distinct driving senders have connected.
    pub multi_driver: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
