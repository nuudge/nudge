// Puts `core`'s session model onto a wire so a front-end in another process can
// drive it. Depends on `core`; `core` never depends on it.
pub mod client;
pub mod daemon;
pub mod encryption;
pub mod pairing;
pub mod wire;

pub use client::{RelayClient, SocketClient};
pub use daemon::{
    PeerAccept, RelayLeg, bind_listener, run_relay_daemon, serve_relay_handoff, serve_socket,
};
pub use pairing::{Pairing, PairingScope};

#[cfg(test)]
mod tests;
