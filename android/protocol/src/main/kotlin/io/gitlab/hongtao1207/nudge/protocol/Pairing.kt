package io.gitlab.hongtao1207.nudge.protocol

import com.goterl.lazysodium.interfaces.SecretBox
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import java.util.Base64

// The Kotlin peer of Rust's transport::pairing. A scanned QR carries
// `nudge:<base64url(json)>` where json = {relay, id, k}; decoding it yields
// everything a device needs to join: relay base URL, rendezvous room id, and the
// 32-byte E2E key. Scanning *is* the pairing act — there is no key exchange beyond
// the code, which is why an unpaired device can neither find the room nor decrypt it.
class Pairing(
    val relayBase: String,
    val rendezvousId: String,
    val cipher: Cipher,
) {
    // The wss URL both peers dial: relay base + room id as the single path segment.
    fun dialUrl(): String = "${relayBase.trimEnd('/')}/$rendezvousId"

    @Serializable
    private data class Payload(val relay: String, val id: String, val k: String)

    companion object {
        private const val SCHEME = "nudge:"
        private val json = Json { ignoreUnknownKeys = true }

        fun decode(code: String): Pairing = decode(code, Cipher.defaultSodium)

        // Android passes its own LazySodiumAndroid here — the JVM default
        // (LazySodiumAndroid's class isn't on the APK classpath) is never touched.
        fun decode(code: String, sodium: SecretBox.Native): Pairing {
            val trimmed = code.trim()
            require(trimmed.startsWith(SCHEME)) {
                "not a nudge pairing code (missing '$SCHEME' prefix)"
            }
            val payloadJson = Base64.getUrlDecoder()
                .decode(trimmed.removePrefix(SCHEME))
                .decodeToString()
            val payload = json.decodeFromString<Payload>(payloadJson)
            val key = Base64.getUrlDecoder().decode(payload.k)
            return Pairing(payload.relay, payload.id, Cipher(key, sodium))
        }
    }
}
