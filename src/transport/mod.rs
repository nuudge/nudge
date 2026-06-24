// The transport layer, layered over `core`. It puts the in-memory session model
// (the broker's `Controller`/event stream from `core`) onto a wire so a front-end
// in another process can drive a session. `wire` holds the framed protocol plus a
// transport-agnostic frame seam (`FrameReader`/`FrameWriter`) with two codecs —
// the newline-JSON `Line*` codec for a local Unix socket and the `Ws*` codec for a
// relayed WebSocket. `daemon` is the server side (`--daemon` over a socket, the
// `/background` handoff loop, and the relay dial-out host) and `client` the
// `SocketClient` / `RelayClient` that reconstruct a `Controller`. `encryption`
// seals frames for the relayed path; `pairing` mints/encodes the QR code that
// carries the relay address, rendezvous id, and E2E key to a device. Depends on
// `core`; `core` never depends on it.
pub mod client;
pub mod daemon;
pub mod encryption;
pub mod pairing;
pub mod wire;

pub use client::{RelayClient, SocketClient};
pub use daemon::{bind_listener, run_daemon, run_relay_daemon, serve_handoff, serve_relay_handoff};
pub use encryption::Cipher;
pub use pairing::Pairing;

#[cfg(test)]
mod tests;
