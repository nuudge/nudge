package io.gitlab.hongtao1207.nudge.protocol

import com.goterl.lazysodium.interfaces.SecretBox
import java.util.Base64

// The Kotlin peer of Rust's transport::pairing. A scanned QR carries
// `nudge:<base64url(payload)>` where payload is a compact binary blob,
// `[id: 16 bytes][key: 32 bytes][relay URL: UTF-8]`; decoding it yields everything a
// device needs to join: relay base URL, rendezvous room id, and the 32-byte E2E key.
// Scanning *is* the pairing act — there is no key exchange beyond the code, which is
// why an unpaired device can neither find the room nor decrypt it.
class Pairing(
    val relayBase: String,
    val rendezvousId: String,
    val cipher: Cipher,
) {
    // The wss URL both peers dial: relay base + room id as the single path segment.
    fun dialUrl(): String = "${relayBase.trimEnd('/')}/$rendezvousId"

    companion object {
        private const val SCHEME = "nudge:"
        private const val ID_BYTES = 16
        private const val KEY_BYTES = 32

        fun decode(code: String): Pairing = decode(code, Cipher.defaultSodium)

        // Android passes its own LazySodiumAndroid here — the JVM default
        // (LazySodiumAndroid's class isn't on the APK classpath) is never touched.
        fun decode(code: String, sodium: SecretBox.Native): Pairing {
            val trimmed = code.trim()
            require(trimmed.startsWith(SCHEME)) {
                "not a nudge pairing code (missing '$SCHEME' prefix)"
            }
            val bytes = Base64.getUrlDecoder().decode(trimmed.removePrefix(SCHEME))
            require(bytes.size >= ID_BYTES + KEY_BYTES) {
                "pairing code too short (${bytes.size} bytes)"
            }
            // Layout: 16-byte rendezvous id (rendered hex to match the URL path the
            // daemon dials), 32-byte key, then the relay URL (UTF-8, variable, last).
            val id = bytes.copyOfRange(0, ID_BYTES)
                .joinToString("") { "%02x".format(it.toInt() and 0xFF) }
            val key = bytes.copyOfRange(ID_BYTES, ID_BYTES + KEY_BYTES)
            val relay = String(bytes, ID_BYTES + KEY_BYTES, bytes.size - ID_BYTES - KEY_BYTES, Charsets.UTF_8)
            return Pairing(relay, id, Cipher(key, sodium))
        }
    }
}
