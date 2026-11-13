// Unpaired devices are refused with no extra gate: without the code a device can't
// find the room (128-bit secret id) and couldn't decrypt it if it did (no key).
//
// Payload is compact binary, not JSON/hex, because every byte grows the QR:
// `[scope: 1][id: 16][key: 32][relay URL: UTF-8]`.
//
// The scope byte is a client-side display hint the daemon never trusts: rights come
// from which room + key the connection reached, so flipping it can't upgrade a code.

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
const RENDEZVOUS_ID_BYTES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingScope {
    Full,
    WatchOnly,
    Agent,
}

impl PairingScope {
    fn byte(self) -> u8 {
        match self {
            PairingScope::Full => 0,
            PairingScope::WatchOnly => 1,
            PairingScope::Agent => 2,
        }
    }

    fn from_byte(b: u8) -> Result<Self> {
        match b {
            0 => Ok(PairingScope::Full),
            1 => Ok(PairingScope::WatchOnly),
            2 => Ok(PairingScope::Agent),
            other => anyhow::bail!("unknown pairing scope byte: {other}"),
        }
    }

    // Applied daemon-side at mint time to the parked spare; the client never presents it.
    pub fn profile(self) -> ClientProfile {
        match self {
            PairingScope::Full => ClientProfile::human(),
            PairingScope::WatchOnly => ClientProfile::watch_only(),
            PairingScope::Agent => ClientProfile::agent(),
        }
    }
}

pub struct Pairing {
    pub relay_base: String,
    pub rendezvous_id: String,
    pub cipher: Cipher,
    pub scope: PairingScope,
}

impl Pairing {
    // `relay_base` is scheme + host[:port], no path.
    pub fn generate(relay_base: String) -> Self {
        Self::generate_scoped(relay_base, PairingScope::Full)
    }

    // Each scope gets its own room + key; that separation is what makes scope
    // daemon-authoritative.
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

    // The relay tells host from client by the trailing role segment, since it can't
    // read the encrypted attach frame.
    pub fn dial_url(&self) -> String {
        format!(
            "{}/{}",
            self.relay_base.trim_end_matches('/'),
            self.rendezvous_id
        )
    }

    pub fn host_dial_url(&self) -> String {
        format!("{}/host", self.dial_url())
    }

    pub fn client_dial_url(&self) -> String {
        format!("{}/client", self.dial_url())
    }

    pub fn encode(&self) -> String {
        let mut bytes = vec![self.scope.byte()];
        bytes.extend_from_slice(&hex_to_bytes(&self.rendezvous_id));
        bytes.extend_from_slice(self.cipher.key_bytes());
        bytes.extend_from_slice(self.relay_base.as_bytes());
        format!("{SCHEME}{}", URL_SAFE_NO_PAD.encode(&bytes))
    }

    // The relay URL is variable-length, so it goes last and needs no length prefix.
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

    // EcLevel::L: an on-screen QR is pixel-perfect, so 7% recovery is ample and keeps it small.
    pub fn render_qr(&self) -> Result<String> {
        let code = QrCode::with_error_correction_level(self.encode().as_bytes(), EcLevel::L)
            .context("building QR code")?;
        Ok(code.render::<unicode::Dense1x2>().quiet_zone(true).build())
    }
}

// Input is always `generate`'s hex, so a bad pair yields a zero byte rather than failing.
fn hex_to_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2).unwrap_or("0"), 16).unwrap_or(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip() {
        for scope in [
            PairingScope::Full,
            PairingScope::WatchOnly,
            PairingScope::Agent,
        ] {
            let p = Pairing::generate_scoped("wss://relay.example.com".into(), scope);
            let restored = Pairing::decode(&p.encode()).unwrap();
            assert_eq!(restored.relay_base, p.relay_base);
            assert_eq!(restored.rendezvous_id, p.rendezvous_id);
            assert_eq!(restored.scope, scope);
            let sealed = p.cipher.seal(b"frame");
            assert_eq!(restored.cipher.open(&sealed).unwrap(), b"frame");
        }
    }

    #[test]
    fn generate_defaults_to_full_scope() {
        let p = Pairing::generate("wss://r".into());
        assert_eq!(p.scope, PairingScope::Full);
    }

    #[test]
    fn flipping_the_scope_byte_cannot_move_the_room_or_upgrade_rights() {
        let watch = Pairing::generate_scoped("wss://r".into(), PairingScope::WatchOnly);

        let mut raw = URL_SAFE_NO_PAD
            .decode(watch.encode().strip_prefix(SCHEME).unwrap())
            .unwrap();
        raw[0] = PairingScope::Full.byte();
        let tampered = format!("{SCHEME}{}", URL_SAFE_NO_PAD.encode(&raw));

        let decoded = Pairing::decode(&tampered).unwrap();
        assert_eq!(decoded.scope, PairingScope::Full);
        // Same room and key, so it still reaches only the watch-only spare.
        assert_eq!(decoded.rendezvous_id, watch.rendezvous_id);
        let sealed = watch.cipher.seal(b"x");
        assert_eq!(decoded.cipher.open(&sealed).unwrap(), b"x");
        assert_eq!(watch.scope.profile(), ClientProfile::watch_only());
        assert_ne!(watch.scope.profile(), ClientProfile::human());
    }

    #[test]
    fn scope_resolves_to_the_expected_profile() {
        assert_eq!(PairingScope::Full.profile(), ClientProfile::human());
        assert_eq!(
            PairingScope::WatchOnly.profile(),
            ClientProfile::watch_only()
        );
        assert_eq!(PairingScope::Agent.profile(), ClientProfile::agent());
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
        assert_eq!(p.host_dial_url(), "wss://r.example.com/abc123/host");
        assert_eq!(p.client_dial_url(), "wss://r.example.com/abc123/client");
    }
}
