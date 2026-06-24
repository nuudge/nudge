// Application-layer end-to-end encryption for the relayed path (Phase 8.2-d). Each
// protocol frame is sealed under a pre-shared symmetric key before it becomes a
// WebSocket message and opened on the far side, so the relay only ever forwards
// opaque ciphertext — the "ciphertext-blind relay" guarantee. The local Unix-socket
// path is trusted and stays plaintext; encryption applies only to the WS codec.
//
// Primitive: XSalsa20-Poly1305 (libsodium `crypto_secretbox`), via dryoc's
// libsodium-compatible classic API. The wire layout per frame is
// `nonce(24) ‖ MAC(16) ‖ ciphertext` — i.e. a fresh random nonce prepended to the
// `crypto_secretbox_easy` output — so the Android lazysodium client can interop in
// 8.4. The key is provided either as a raw key file (`--key`) or carried in a QR
// pairing code (see `pairing`); both produce the same `Cipher`.

use std::path::Path;

use anyhow::{Context, Result};
use dryoc::classic::crypto_secretbox::{
    Key, Nonce, crypto_secretbox_easy, crypto_secretbox_keygen, crypto_secretbox_open_easy,
};
use dryoc::constants::{
    CRYPTO_SECRETBOX_KEYBYTES, CRYPTO_SECRETBOX_MACBYTES, CRYPTO_SECRETBOX_NONCEBYTES,
};
use dryoc::rng::copy_randombytes;

// Holds the shared secret. Intentionally not `Debug` so the key can't leak into a
// log line. `Clone` (the key is `Copy`) so the reader and writer halves each carry
// one without sharing mutable state.
#[derive(Clone)]
pub struct Cipher {
    key: Key,
}

impl Cipher {
    pub fn generate() -> Self {
        Self {
            key: crypto_secretbox_keygen(),
        }
    }

    // Rebuild a cipher from raw key bytes carried in a pairing code. Errors on a
    // wrong-sized key so a malformed code fails loudly at pairing time, not later
    // as an opaque authentication failure on every frame.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let key: Key = bytes.try_into().map_err(|_| {
            anyhow::anyhow!(
                "relay key must be exactly {CRYPTO_SECRETBOX_KEYBYTES} bytes, found {}",
                bytes.len()
            )
        })?;
        Ok(Self { key })
    }

    // The raw key bytes, for embedding in a pairing code. Crate-internal so the
    // secret has no public accessor to leak through.
    pub(crate) fn key_bytes(&self) -> &[u8; CRYPTO_SECRETBOX_KEYBYTES] {
        &self.key
    }

    // Load a raw 32-byte key file (written by `save`). Errors clearly on a
    // wrong-sized file so a truncated/garbage key surfaces at startup.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("reading relay key {}", path.display()))?;
        let key: Key = bytes.as_slice().try_into().map_err(|_| {
            anyhow::anyhow!(
                "relay key {} must be exactly {CRYPTO_SECRETBOX_KEYBYTES} bytes, found {}",
                path.display(),
                bytes.len()
            )
        })?;
        Ok(Self { key })
    }

    // Write the raw key with owner-only (0600) permissions — it is a secret.
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, self.key)
            .with_context(|| format!("writing relay key {}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
        Ok(())
    }

    // Seal one frame: fresh nonce ‖ secretbox. The buffers are sized exactly, so
    // the encrypt step cannot fail (it only errors on a wrong-sized output buffer).
    pub fn seal(&self, plaintext: &[u8]) -> Vec<u8> {
        let mut nonce: Nonce = [0u8; CRYPTO_SECRETBOX_NONCEBYTES];
        copy_randombytes(&mut nonce);
        let mut out =
            vec![0u8; CRYPTO_SECRETBOX_NONCEBYTES + CRYPTO_SECRETBOX_MACBYTES + plaintext.len()];
        out[..CRYPTO_SECRETBOX_NONCEBYTES].copy_from_slice(&nonce);
        crypto_secretbox_easy(
            &mut out[CRYPTO_SECRETBOX_NONCEBYTES..],
            plaintext,
            &nonce,
            &self.key,
        )
        .expect("secretbox_easy cannot fail with correctly sized buffers");
        out
    }

    // Open one sealed frame. A wrong key or any tampering fails the Poly1305 tag,
    // so a bad frame is an authentication error, not silent garbage.
    pub fn open(&self, framed: &[u8]) -> Result<Vec<u8>> {
        if framed.len() < CRYPTO_SECRETBOX_NONCEBYTES + CRYPTO_SECRETBOX_MACBYTES {
            anyhow::bail!("sealed frame too short to contain a nonce + MAC");
        }
        let (nonce_bytes, ciphertext) = framed.split_at(CRYPTO_SECRETBOX_NONCEBYTES);
        let nonce: Nonce = nonce_bytes.try_into().expect("split at nonce length");
        let mut plaintext = vec![0u8; ciphertext.len() - CRYPTO_SECRETBOX_MACBYTES];
        crypto_secretbox_open_easy(&mut plaintext, ciphertext, &nonce, &self.key)
            .map_err(|_| anyhow::anyhow!("frame failed to authenticate (wrong key or tampered)"))?;
        Ok(plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_round_trip() {
        let cipher = Cipher::generate();
        let plaintext = b"a representative protocol frame";
        let sealed = cipher.seal(plaintext);
        assert_ne!(&sealed[..], &plaintext[..], "output must not be plaintext");
        assert_eq!(cipher.open(&sealed).unwrap(), plaintext);
    }

    #[test]
    fn fresh_nonce_each_seal() {
        let cipher = Cipher::generate();
        // Same plaintext, different ciphertext — proves the nonce is random per call.
        assert_ne!(cipher.seal(b"same"), cipher.seal(b"same"));
    }

    #[test]
    fn tampered_frame_fails_to_open() {
        let cipher = Cipher::generate();
        let mut sealed = cipher.seal(b"hello");
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert!(cipher.open(&sealed).is_err());
    }

    #[test]
    fn wrong_key_fails_to_open() {
        let sealed = Cipher::generate().seal(b"hello");
        assert!(Cipher::generate().open(&sealed).is_err());
    }
}
