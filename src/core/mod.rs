pub mod agent;
pub mod events;
pub mod host;
pub mod identity;
pub mod peer;
pub mod profile;
pub mod session;

pub use agent::{AgentConfig, Backend};
pub use events::{AgentEvent, ControllerEvent, McpServerInfo, ModelInfo, UiEvent};
pub use host::{BrokerHandle, Controller, HandoffStatus, SessionHandle, SessionHost};
pub use identity::{ClientIdentity, ClientKind};
pub use peer::{PeerFactory, PeerRegistration, PeerWiring};
pub use profile::{ClientProfile, CommandScope};
