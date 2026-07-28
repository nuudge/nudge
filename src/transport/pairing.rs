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
// binary blob, `[scope: 1 byte][id: 16 bytes][key: 32 bytes][relay URL: UTF-8]`,
// base64url'd once. We deliberately avoid JSON (keys + braces), hex (2× the id), and
// double-base64 (the key inside JSON, then the JSON re-encoded): every saved character
// shrinks the QR, and the 32-byte E2E key already dominates its size (≈25 rows). The key
// is the full 32 bytes (the QR carries the entropy), so "derive the key" is identity for
// now; a short typeable code would slot a KDF in here instead.
//
// The leading scope byte is a *client-side display hint only* (full vs watch-only) — the
// daemon never trusts it. A remote client's rights come from WHICH pairing (room + key)
// its connection reached, which the daemon minted and holds; flipping this byte in a
// stolen watch-only code changes nothing, because it can't move the connection to the
// full room (a different id + key it was never given). See `daemon.rs` accept path.

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use dryoc::rng::copy_randombytes;
use qrcode::render::unicode;
use qrcode::{EcLevel, QrCode};

use super::encryption::Cipher;
use crate::core::ClientProfile;

const KEY_BYTES: usize = 32;

const SCHEME: &str = "nudge:";
// 128 bits of rendezvous id: unguessable, so an unpaired device can't stumble onto
// the relay room. Rendered hex for a clean single URL-path segment.
const RENDEZVOUS_ID_BYTES: usize = 16;

// The rights a pairing grants, minted into the code by the daemon and resolved back to a
// ClientProfile on the accept path. `Full` = a full human front-end (your own phone);
// `WatchOnly` = a restricted spectator (a teammate's code). The authority is the room the
// pairing dials, not this tag — see the module comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingScope {
    Full,
    WatchOnly,
}

impl PairingScope {
    fn byte(self) -> u8 {
        match self {
            PairingScope::Full => 0,
            PairingScope::WatchOnly => 1,
        }
    }

    fn from_byte(b: u8) -> Result<Self> {
        match b {
            0 => Ok(PairingScope::Full),
            1 => Ok(PairingScope::WatchOnly),
            other => anyhow::bail!("unknown pairing scope byte: {other}"),
        }
    }

    // The daemon-side rights this scope grants. Used at mint time to tag the parked host
    // spare, so a client arriving through this pairing's room is attached with exactly
    // this profile — the client never presents or influences it.
    pub fn profile(self) -> ClientProfile {
        match self {
            PairingScope::Full => ClientProfile::human(),
            PairingScope::WatchOnly => ClientProfile::watch_only(),
        }
    }
}

// Everything a device needs to join a session: where the relay is, which room to
// meet in, and the key to decrypt the conversation.
pub struct Pairing {
    pub relay_base: String,
    pub rendezvous_id: String,
    pub cipher: Cipher,
    // The rights this code grants (a display hint for the client; the daemon's authority
    // is the room, not this field). Defaults to `Full` via `generate`.
    pub scope: PairingScope,
}

impl Pairing {
    // Mint a fresh full-rights pairing for a daemon: random room id + random E2E key,
    // against the given relay base URL (scheme + host[:port], no path).
    pub fn generate(relay_base: String) -> Self {
        Self::generate_scoped(relay_base, PairingScope::Full)
    }

    // Mint a fresh pairing with an explicit scope. A watch-only pairing gets its OWN
    // random room + key (distinct from the full one), which is what makes the scope
    // daemon-authoritative: a watch-only holder can't reach the full room.
    pub fn generate_scoped(relay_base: String, scope: PairingScope) -> Self {
        let mut raw = [0u8; RENDEZVOUS_ID_BYTES];
        copy_randombytes(&mut raw);
        let rendezvous_id = raw.iter().map(|b| format!("{b:02x}")).collect();
        Self {
            relay_base,
            rendezvous_id,
            cipher: Cipher::generate(),
            scope,
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

    // Encode to the scannable pairing code: `nudge:<base64url([scope][id][key][relay])>`.
    pub fn encode(&self) -> String {
        let mut bytes = vec![self.scope.byte()];
        bytes.extend_from_slice(&hex_to_bytes(&self.rendezvous_id));
        bytes.extend_from_slice(self.cipher.key_bytes());
        bytes.extend_from_slice(self.relay_base.as_bytes());
        format!("{SCHEME}{}", URL_SAFE_NO_PAD.encode(&bytes))
    }

    // Decode a scanned/pasted pairing code back into a Pairing. Layout is fixed: a
    // 1-byte scope tag, a 16-byte rendezvous id, a 32-byte key, then the relay URL as
    // UTF-8 (variable, so it goes last — no length prefix needed).
    pub fn decode(code: &str) -> Result<Self> {
        let b64 = code
            .trim()
            .strip_prefix(SCHEME)
            .with_context(|| format!("not a nudge pairing code (missing '{SCHEME}' prefix)"))?;
        let bytes = URL_SAFE_NO_PAD
            .decode(b64)
            .context("pairing code is not valid base64url")?;
        if bytes.len() < 1 + RENDEZVOUS_ID_BYTES + KEY_BYTES {
            anyhow::bail!("pairing code too short ({} bytes)", bytes.len());
        }
        let (scope_byte, rest) = bytes.split_at(1);
        let scope = PairingScope::from_byte(scope_byte[0])?;
        let (id_bytes, rest) = rest.split_at(RENDEZVOUS_ID_BYTES);
        let (key, relay) = rest.split_at(KEY_BYTES);
        let rendezvous_id = id_bytes.iter().map(|b| format!("{b:02x}")).collect();
        let relay_base =
            String::from_utf8(relay.to_vec()).context("pairing code relay URL is not UTF-8")?;
        Ok(Self {
            relay_base,
            rendezvous_id,
            cipher: Cipher::from_bytes(key)?,
            scope,
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
        for scope in [PairingScope::Full, PairingScope::WatchOnly] {
            let p = Pairing::generate_scoped("wss://relay.example.com".into(), scope);
            let restored = Pairing::decode(&p.encode()).unwrap();
            assert_eq!(restored.relay_base, p.relay_base);
            assert_eq!(restored.rendezvous_id, p.rendezvous_id);
            assert_eq!(restored.scope, scope);
            let sealed = p.cipher.seal(b"frame");
            assert_eq!(restored.cipher.open(&sealed).unwrap(), b"frame");
        }
    }

    // `generate` mints a full-rights code (the default handoff / your own phone).
    #[test]
    fn generate_defaults_to_full_scope() {
        let p = Pairing::generate("wss://r".into());
        assert_eq!(p.scope, PairingScope::Full);
    }

    // The security property: the scope byte is a client-side hint the daemon never
    // trusts. A watch-only holder who flips the byte to Full and re-encodes cannot gain
    // full rights, because the byte doesn't move the connection to the full room — the
    // room id and key (the real authority the daemon minted) are untouched by the flip.
    #[test]
    fn flipping_the_scope_byte_cannot_move_the_room_or_upgrade_rights() {
        let watch = Pairing::generate_scoped("wss://r".into(), PairingScope::WatchOnly);

        // Decode → flip the leading scope byte to Full → re-encode (what an attacker does).
        let mut raw = URL_SAFE_NO_PAD
            .decode(watch.encode().strip_prefix(SCHEME).unwrap())
            .unwrap();
        raw[0] = PairingScope::Full.byte();
        let tampered = format!("{SCHEME}{}", URL_SAFE_NO_PAD.encode(&raw));

        let decoded = Pairing::decode(&tampered).unwrap();
        // The hint now reads Full…
        assert_eq!(decoded.scope, PairingScope::Full);
        // …but the room and key are unchanged, so it still dials the SAME (watch) room,
        // where the daemon parked only a watch-only spare. Authority = the room.
        assert_eq!(decoded.rendezvous_id, watch.rendezvous_id);
        let sealed = watch.cipher.seal(b"x");
        assert_eq!(decoded.cipher.open(&sealed).unwrap(), b"x");
        // And the daemon resolves the profile from the scope IT minted for that room,
        // never from the client's byte: the watch room stays watch_only.
        assert_eq!(watch.scope.profile(), ClientProfile::watch_only());
        assert_ne!(watch.scope.profile(), ClientProfile::human());
    }

    // The daemon-side scope→profile mapping (used at mint time to tag each parked spare).
    #[test]
    fn scope_resolves_to_the_expected_profile() {
        assert_eq!(PairingScope::Full.profile(), ClientProfile::human());
        assert_eq!(
            PairingScope::WatchOnly.profile(),
            ClientProfile::watch_only()
        );
    }

    #[test]
    fn dial_url_joins_base_and_room() {
        let p = Pairing {
            relay_base: "wss://r.example.com/".into(),
            rendezvous_id: "abc123".into(),
            cipher: Cipher::generate(),
            scope: PairingScope::Full,
        };
        assert_eq!(p.dial_url(), "wss://r.example.com/abc123");
        // Host and client dial the same room, distinguished only by the role segment.
        assert_eq!(p.host_dial_url(), "wss://r.example.com/abc123/host");
        assert_eq!(p.client_dial_url(), "wss://r.example.com/abc123/client");
    }
}
