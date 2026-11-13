package io.gitlab.hongtao1207.nudge.protocol

import com.goterl.lazysodium.interfaces.SecretBox
import java.util.Base64

// Mirrors Rust's transport::pairing. Scanning the code is the whole pairing act: no key
// exchange beyond it, so an unpaired device can neither find the room nor decrypt it.
class Pairing(
    val relayBase: String,
    val rendezvousId: String,
    val cipher: Cipher,
    val scope: PairingScope,
) {
    // The relay can't read the encrypted attach frame, so it pairs host and client by
    // a trailing role segment.
    fun dialUrl(): String = "${relayBase.trimEnd('/')}/$rendezvousId"

    fun clientDialUrl(): String = "${dialUrl()}/client"

    companion object {
        private const val SCHEME = "nudge:"
        private const val ID_BYTES = 16
        private const val KEY_BYTES = 32

        fun decode(code: String): Pairing = decode(code, Cipher.defaultSodium)

        // Android passes LazySodiumAndroid so the JVM default binding is never loaded.
        fun decode(code: String, sodium: SecretBox.Native): Pairing {
            val trimmed = code.trim()
            require(trimmed.startsWith(SCHEME)) {
                "not a nudge pairing code (missing '$SCHEME' prefix)"
            }
            val bytes = Base64.getUrlDecoder().decode(trimmed.removePrefix(SCHEME))
            require(bytes.size >= 1 + ID_BYTES + KEY_BYTES) {
                "pairing code too short (${bytes.size} bytes)"
            }
            // Layout: scope(1) ‖ id(16, rendered hex to match the daemon's URL path) ‖ key(32) ‖ relay URL.
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

// Display hint only: the daemon grants rights by which room + key the client reached,
// never by this byte, so flipping it grants nothing.
enum class PairingScope(val byte: Byte) {
    Full(0),
    WatchOnly(1);

    companion object {
        fun fromByte(b: Byte): PairingScope =
            entries.firstOrNull { it.byte == b }
                ?: throw IllegalArgumentException("unknown pairing scope byte: $b")
    }
}
