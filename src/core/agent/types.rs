use anyhow::Result;
use serde_json::Value;
use std::future::Future;
use tokio::sync::mpsc;

use crate::core::events::{AgentEvent, UiEvent};
use crate::core::host::BrokerHandle;
use crate::core::identity::ClientIdentity;
use crate::core::peer::{PeerFactory, PeerRegistration, PeerSet};
use crate::llm::SystemBlock;

pub struct AgentConfig {
    pub model: String,
    pub max_tokens: u32,
    pub max_iterations: usize,
    // "summarized" (default) shows thinking text; "omitted" sends signature only
    // (faster TTFT). Cost is identical — this only changes wire-level visibility.
    pub thinking_display: String,
}

// Everything the loop needs from the concrete agent (tools, prompt/context,
// control handling). The coding agent implements this; the loop stays unaware
// of tool implementations, MCP, CLAUDE.md, or any cwd-specific concern — that
// is what keeps this module from depending on the layer above it.
pub trait Backend {
    // Rebuilt each turn so volatile context (env, git, dir listing) stays fresh.
    fn system_blocks(&self) -> Vec<SystemBlock>;
    // Tool schemas, plus the stable/dynamic cache-boundary index (None = no caching).
    fn tool_schemas(&self) -> (Vec<Value>, Option<usize>);
    fn tool_summary(&self, name: &str, input: &Value) -> String;
    fn requires_permission(&self, name: &str) -> bool;
    fn permission_summary(&self, name: &str, input: &Value) -> String;
    // May emit events (e.g. an OAuth URL) through `notify`.
    fn execute(
        &mut self,
        name: &str,
        input: &Value,
        notify: &mpsc::Sender<AgentEvent>,
    ) -> impl Future<Output = Result<String>> + Send;
    // A control event the loop doesn't own (e.g. MCP load/unload/list); true if consumed.
    fn handle_control(
        &mut self,
        ev: &UiEvent,
        notify: &mpsc::Sender<AgentEvent>,
    ) -> impl Future<Output = bool> + Send;
    // Re-read each turn boundary so a mid-session `git checkout` reaches the header.
    fn git_branch(&self) -> Option<String> {
        None
    }
}

// One inbound event plus the identity of the client that sent it. The broker stamps
// the identity from its registry (the attach handshake) — a client never claims its
// own — so the loop can trust it when attributing a peer's message in the transcript.
// `None` marks broker-internal sends (e.g. the final Quit), which need no sender.
pub type LoopInput = (Option<ClientIdentity>, UiEvent);

// The loop's I/O, bundled so the signature stays readable as it grows. `ui_rx` /
// `agent_tx` are the inbound-drive / outbound-event halves (who drives me, what I
// emit); `peers` is the set of agents I'm a *client* of (whom I drive/observe), and
// `peer_register_rx` delivers peers handed to me at runtime (e.g. a spawned child).
// A top-level session has an empty `PeerSet` and a registrar that never fires, so the
// two peer-related select arms are inert and behavior is byte-for-byte unchanged.
pub struct AgentIo {
    pub ui_rx: mpsc::Receiver<LoopInput>,
    pub agent_tx: mpsc::Sender<AgentEvent>,
    pub peers: PeerSet,
    pub peer_register_rx: Option<mpsc::UnboundedReceiver<PeerRegistration>>,
    // Executor behind the model-facing Spawn tool (None = this agent may not spawn);
    // `self_handle` reaches this agent's OWN broker, handed to the factory so a
    // spawned child can attach back — the return edge.
    pub peer_factory: Option<PeerFactory>,
    pub self_handle: BrokerHandle,
}
