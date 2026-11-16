pub mod agent;
pub mod events;
pub mod host;
pub mod identity;
pub mod peer;
pub mod profile;
pub mod reference;
pub mod session;
pub mod store;

pub use agent::{AgentConfig, Backend};
pub use events::{AgentEvent, ControllerEvent, McpServerInfo, ModelInfo, SessionChoice, UiEvent};
pub use host::{BrokerHandle, Controller, HandoffStatus, SessionHandle, SessionHost};
pub use identity::{ClientIdentity, ClientKind};
pub use peer::{PeerDialer, PeerFactory, PeerRegistration, PeerWiring, SpawnRequest};
pub use profile::ClientProfile;

// The stored state can't say this: 'ended' is written only on graceful teardown, so a
// crashed session would read 'running' forever.
pub fn is_listening(socket: &std::path::Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liveness_is_whether_the_socket_answers() {
        // Short: macOS caps socket paths at 104 bytes.
        let token = &uuid::Uuid::new_v4().simple().to_string()[..12];
        let socket = std::env::temp_dir().join(format!("live-{token}.sock"));
        assert!(!is_listening(&socket), "no file");

        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(is_listening(&socket), "bound");

        drop(listener);
        // A child forked by a concurrent test holds the listener until its exec closes it.
        let stale = (0..200).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            !is_listening(&socket)
        });
        assert!(stale, "stale file left by a crash");
        std::fs::remove_file(&socket).ok();
    }
}
