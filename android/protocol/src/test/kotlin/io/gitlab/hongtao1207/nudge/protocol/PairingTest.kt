package io.gitlab.hongtao1207.nudge.protocol

import java.util.Base64
import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith

class PairingTest {
    // Craft a code exactly the way the Rust daemon mints one: the compact binary blob
    // [scope: 1 byte][id: 16 bytes][key: 32 bytes][relay UTF-8], base64url no-pad under
    // the `nudge:` scheme.
    private fun code(scopeByte: Byte, id: ByteArray, key: ByteArray, relay: String): String {
        val blob = byteArrayOf(scopeByte) + id + key + relay.encodeToByteArray()
        return "nudge:" + Base64.getUrlEncoder().withoutPadding().encodeToString(blob)
    }

    @Test
    fun decodeRoundTrip() {
        val id = ByteArray(16) { (it + 1).toByte() } // 0102…10
        val key = ByteArray(32) { it.toByte() }
        val relay = "wss://relay.example.com"

        val expectedId = id.joinToString("") { "%02x".format(it.toInt() and 0xFF) }
        val p = Pairing.decode(code(PairingScope.Full.byte, id, key, relay))
        assertEquals(relay, p.relayBase)
        assertEquals(expectedId, p.rendezvousId)
        assertEquals(PairingScope.Full, p.scope)
        assertEquals("$relay/$expectedId", p.dialUrl())
        assertEquals("$relay/$expectedId/client", p.clientDialUrl())

        // The key survived decoding: a frame sealed under the raw key opens under
        // the decoded cipher — matching keys on both ends is the whole point.
        val sealed = Cipher(key).seal("frame".encodeToByteArray())
        assertContentEquals("frame".encodeToByteArray(), p.cipher.open(sealed))
    }

    @Test
    fun decodesTheLeadingScopeByteForBothScopes() {
        val id = ByteArray(16) { (it + 1).toByte() }
        val key = ByteArray(32) { it.toByte() }
        val relay = "wss://r"
        // The leading byte is the only difference between a full and a watch-only code
        // in this crafted pair; decoding must surface each scope.
        assertEquals(PairingScope.Full, Pairing.decode(code(0, id, key, relay)).scope)
        assertEquals(PairingScope.WatchOnly, Pairing.decode(code(1, id, key, relay)).scope)
    }

    @Test
    fun rejectsAnUnknownScopeByte() {
        val id = ByteArray(16) { (it + 1).toByte() }
        val key = ByteArray(32) { it.toByte() }
        assertFailsWith<IllegalArgumentException> {
            Pairing.decode(code(9, id, key, "wss://r"))
        }
    }
}
