package io.gitlab.hongtao1207.nudge.protocol

import com.goterl.lazysodium.interfaces.SecretBox
import java.util.Base64

// The Kotlin peer of Rust's transport::pairing. A scanned QR carries
// `nudge:<base64url(payload)>` where payload is a compact binary blob,
// `[scope: 1 byte][id: 16 bytes][key: 32 bytes][relay URL: UTF-8]`; decoding it yields
// everything a device needs to join: relay base URL, rendezvous room id, and the 32-byte
// E2E key. Scanning *is* the pairing act — there is no key exchange beyond the code, which
// is why an unpaired device can neither find the room nor decrypt it.
//
// The leading scope byte is a display hint only (full vs watch-only); the daemon assigns
// the client's actual rights from WHICH pairing (room + key) it connected through, never
// from this byte, so flipping it grants nothing.
class Pairing(
    val relayBase: String,
    val rendezvousId: String,
    val cipher: Cipher,
    val scope: PairingScope,
) {
    // The room URL both peers build on: relay base + room id as a path segment. The
    // relay pairs a host with a client by a trailing role segment (it can't read the
    // encrypted attach frame), so a front-end dials `clientDialUrl()`.
    fun dialUrl(): String = "${relayBase.trimEnd('/')}/$rendezvousId"

    // The URL a front-end (this app) dials — the room URL plus the `client` role.
    fun clientDialUrl(): String = "${dialUrl()}/client"

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
            require(bytes.size >= 1 + ID_BYTES + KEY_BYTES) {
                "pairing code too short (${bytes.size} bytes)"
            }
            // Layout: 1-byte scope tag, 16-byte rendezvous id (rendered hex to match the
            // URL path the daemon dials), 32-byte key, then the relay URL (UTF-8, last).
            val scope = PairingScope.fromByte(bytes[0])
            val id = bytes.copyOfRange(1, 1 + ID_BYTES)
                .joinToString("") { "%02x".format(it.toInt() and 0xFF) }
            val key = bytes.copyOfRange(1 + ID_BYTES, 1 + ID_BYTES + KEY_BYTES)
            val relayStart = 1 + ID_BYTES + KEY_BYTES
            val relay = String(bytes, relayStart, bytes.size - relayStart, Charsets.UTF_8)
            return Pairing(relay, id, Cipher(key, sodium), scope)
        }
    }
}

// Mirrors Rust's transport::pairing::PairingScope — the rights a pairing grants (a
// display hint; the daemon's authority is the room the connection reached, not this tag).
enum class PairingScope(val byte: Byte) {
    Full(0),
    WatchOnly(1);

    companion object {
        fun fromByte(b: Byte): PairingScope =
            entries.firstOrNull { it.byte == b }
                ?: throw IllegalArgumentException("unknown pairing scope byte: $b")
    }
}
