// QR-based device pairing for the relayed path (Phase 8.3). Replaces the bare
// pre-shared key file with a single self-contained code the daemon shows and a
// device scans. The code carries everything a device needs to join: the relay
// base URL, a fresh random rendezvous id (the "room number"), and the E2E key.
// Scanning the code *is* the pairing act — it transfers the key that keeps the
// relay ciphertext-blind. "Refuses unpaired devices" then falls out of E2E with
// no extra gate: without the code a device can't find the room (the id is a
// 128-bit secret) and couldn't decrypt it if it did (no key).
//
// The code is `nudge:<base64url(json)>` — an opaque token under a scheme the
// Android client (8.4) can claim via an intent filter. JSON keeps it debuggable
// (the roadmap's "JSON first" default), and base64url avoids any path/query
// escaping. The key is the full 32 bytes (the QR carries the entropy), so
// "derive the key" is identity for now; a short typeable code would slot a KDF in
// here instead.

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use dryoc::rng::copy_randombytes;
use qrcode::QrCode;
use qrcode::render::unicode;
use serde::{Deserialize, Serialize};

use super::encryption::Cipher;

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

// The wire form of a pairing code: base64url-JSON inside the `nudge:` token.
#[derive(Serialize, Deserialize)]
struct Payload {
    relay: String,
    id: String,
    k: String,
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

    // The full WebSocket URL both peers dial: base + the room id as the path.
    pub fn dial_url(&self) -> String {
        format!(
            "{}/{}",
            self.relay_base.trim_end_matches('/'),
            self.rendezvous_id
        )
    }

    // Encode to the scannable pairing code.
    pub fn encode(&self) -> String {
        let payload = Payload {
            relay: self.relay_base.clone(),
            id: self.rendezvous_id.clone(),
            k: URL_SAFE_NO_PAD.encode(self.cipher.key_bytes()),
        };
        let json = serde_json::to_vec(&payload).expect("Payload always serializes");
        format!("{SCHEME}{}", URL_SAFE_NO_PAD.encode(json))
    }

    // Decode a scanned/pasted pairing code back into a Pairing.
    pub fn decode(code: &str) -> Result<Self> {
        let b64 = code.trim().strip_prefix(SCHEME).with_context(|| {
            format!("not a nudge pairing code (missing '{SCHEME}' prefix)")
        })?;
        let json = URL_SAFE_NO_PAD
            .decode(b64)
            .context("pairing code is not valid base64url")?;
        let payload: Payload =
            serde_json::from_slice(&json).context("pairing code payload is not valid JSON")?;
        let key = URL_SAFE_NO_PAD
            .decode(&payload.k)
            .context("pairing code key is not valid base64url")?;
        Ok(Self {
            relay_base: payload.relay,
            rendezvous_id: payload.id,
            cipher: Cipher::from_bytes(&key)?,
        })
    }

    // Render the pairing code as a terminal QR (two pixel rows per text line).
    pub fn render_qr(&self) -> Result<String> {
        let code = QrCode::new(self.encode()).context("building QR code")?;
        Ok(code.render::<unicode::Dense1x2>().quiet_zone(true).build())
    }
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
    }
}
