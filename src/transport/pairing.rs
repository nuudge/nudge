// QR-based device pairing for the relayed path (Phase 8.3). Replaces the bare
// pre-shared key file with a single self-contained code the daemon shows and a
// device scans. The code carries everything a device needs to join: the relay
// base URL, a fresh random rendezvous id (the "room number"), and the E2E key.
// Scanning the code *is* the pairing act — it transfers the key that keeps the
// relay ciphertext-blind. "Refuses unpaired devices" then falls out of E2E with
// no extra gate: without the code a device can't find the room (the id is a
// 128-bit secret) and couldn't decrypt it if it did (no key).
//
// The code is `nudge:<base64url(payload)>` — an opaque token under a scheme the
// Android client (8.4) can claim via an intent filter. The payload is a compact
// binary blob, `[id: 16 bytes][key: 32 bytes][relay URL: UTF-8]`, base64url'd once.
// We deliberately avoid JSON (keys + braces), hex (2× the id), and double-base64
// (the key inside JSON, then the JSON re-encoded): every saved character shrinks
// the QR, and the 32-byte E2E key already dominates its size (≈25 rows). The key is
// the full 32 bytes (the QR carries the entropy), so "derive the key" is identity
// for now; a short typeable code would slot a KDF in here instead.

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use dryoc::rng::copy_randombytes;
use qrcode::render::unicode;
use qrcode::{EcLevel, QrCode};

use super::encryption::Cipher;

const KEY_BYTES: usize = 32;

const SCHEME: &str = "nudge:";
// 128 bits of rendezvous id: unguessable, so an unpaired device can't stumble onto
// the relay room. Rendered hex for a clean single URL-path segment.
const RENDEZVOUS_ID_BYTES: usize = 16;

// Everything a device needs to join a session: where the relay is, which room to
// meet in, and the key to decrypt the conversation.
pub struct Pairing {
    pub relay_base: String,
    pub rendezvous_id: String,
    pub cipher: Cipher,
}

impl Pairing {
    // Mint a fresh pairing for a daemon: random room id + random E2E key, against
    // the given relay base URL (scheme + host[:port], no path).
    pub fn generate(relay_base: String) -> Self {
        let mut raw = [0u8; RENDEZVOUS_ID_BYTES];
        copy_randombytes(&mut raw);
        let rendezvous_id = raw.iter().map(|b| format!("{b:02x}")).collect();
        Self {
            relay_base,
            rendezvous_id,
            cipher: Cipher::generate(),
        }
    }

    // The room URL both peers build on: relay base + the room id as the path. The
    // role segment (below) is appended to it — the relay pairs a host with a client by
    // that trailing segment, since it can't read the encrypted attach frame to tell
    // the two apart.
    pub fn dial_url(&self) -> String {
        format!(
            "{}/{}",
            self.relay_base.trim_end_matches('/'),
            self.rendezvous_id
        )
    }

    // The daemon (session host) dials this; the relay parks it as a host spare.
    pub fn host_dial_url(&self) -> String {
        format!("{}/host", self.dial_url())
    }

    // A front-end (`RelayClient`) dials this; the relay pairs it with a host spare.
    pub fn client_dial_url(&self) -> String {
        format!("{}/client", self.dial_url())
    }

    // Encode to the scannable pairing code: `nudge:<base64url([id][key][relay])>`.
    pub fn encode(&self) -> String {
        let mut bytes = hex_to_bytes(&self.rendezvous_id);
        bytes.extend_from_slice(self.cipher.key_bytes());
        bytes.extend_from_slice(self.relay_base.as_bytes());
        format!("{SCHEME}{}", URL_SAFE_NO_PAD.encode(&bytes))
    }

    // Decode a scanned/pasted pairing code back into a Pairing. Layout is fixed:
    // 16-byte rendezvous id, 32-byte key, then the relay URL as UTF-8 (variable,
    // so it goes last — no length prefix needed).
    pub fn decode(code: &str) -> Result<Self> {
        let b64 = code
            .trim()
            .strip_prefix(SCHEME)
            .with_context(|| format!("not a nudge pairing code (missing '{SCHEME}' prefix)"))?;
        let bytes = URL_SAFE_NO_PAD
            .decode(b64)
            .context("pairing code is not valid base64url")?;
        if bytes.len() < RENDEZVOUS_ID_BYTES + KEY_BYTES {
            anyhow::bail!("pairing code too short ({} bytes)", bytes.len());
        }
        let (id_bytes, rest) = bytes.split_at(RENDEZVOUS_ID_BYTES);
        let (key, relay) = rest.split_at(KEY_BYTES);
        let rendezvous_id = id_bytes.iter().map(|b| format!("{b:02x}")).collect();
        let relay_base =
            String::from_utf8(relay.to_vec()).context("pairing code relay URL is not UTF-8")?;
        Ok(Self {
            relay_base,
            rendezvous_id,
            cipher: Cipher::from_bytes(key)?,
        })
    }

    // Render the pairing code as a terminal QR (two pixel rows per text line). Uses
    // the lowest error-correction level: the on-screen QR is rendered pixel-perfect
    // (black-on-white in the TUI), so L's 7% recovery is ample and keeps it small.
    pub fn render_qr(&self) -> Result<String> {
        let code = QrCode::with_error_correction_level(self.encode().as_bytes(), EcLevel::L)
            .context("building QR code")?;
        Ok(code.render::<unicode::Dense1x2>().quiet_zone(true).build())
    }
}

// The rendezvous id is stored as a 32-char hex string (it doubles as the relay URL
// path segment), but the compact code carries its 16 raw bytes. `generate` always
// produces valid hex, so a bad pair just contributes a zero byte rather than failing.
fn hex_to_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2).unwrap_or("0"), 16).unwrap_or(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The daemon encodes; a client on another device decodes. A mismatch here would
    // silently break every pairing, so pin the round-trip — including that the key
    // survives, since matching keys on both ends is the whole point.
    #[test]
    fn encode_decode_round_trip() {
        let p = Pairing::generate("wss://relay.example.com".into());
        let restored = Pairing::decode(&p.encode()).unwrap();
        assert_eq!(restored.relay_base, p.relay_base);
        assert_eq!(restored.rendezvous_id, p.rendezvous_id);
        let sealed = p.cipher.seal(b"frame");
        assert_eq!(restored.cipher.open(&sealed).unwrap(), b"frame");
    }

    #[test]
    fn dial_url_joins_base_and_room() {
        let p = Pairing {
            relay_base: "wss://r.example.com/".into(),
            rendezvous_id: "abc123".into(),
            cipher: Cipher::generate(),
        };
        assert_eq!(p.dial_url(), "wss://r.example.com/abc123");
        // Host and client dial the same room, distinguished only by the role segment.
        assert_eq!(p.host_dial_url(), "wss://r.example.com/abc123/host");
        assert_eq!(p.client_dial_url(), "wss://r.example.com/abc123/client");
    }
}
