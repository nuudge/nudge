package io.gitlab.hongtao1207.nudge.protocol

import java.util.Base64
import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertEquals

class PairingTest {
    @Test
    fun decodeRoundTrip() {
        // Craft a code exactly the way the Rust daemon mints one: the compact
        // binary blob [id: 16 bytes][key: 32 bytes][relay UTF-8], base64url no-pad
        // under the `nudge:` scheme.
        val id = ByteArray(16) { (it + 1).toByte() } // 0102…10
        val key = ByteArray(32) { it.toByte() }
        val relay = "wss://relay.example.com"
        val blob = id + key + relay.encodeToByteArray()
        val code = "nudge:" +
            Base64.getUrlEncoder().withoutPadding().encodeToString(blob)

        val expectedId = id.joinToString("") { "%02x".format(it.toInt() and 0xFF) }
        val p = Pairing.decode(code)
        assertEquals(relay, p.relayBase)
        assertEquals(expectedId, p.rendezvousId)
        assertEquals("$relay/$expectedId", p.dialUrl())
        assertEquals("$relay/$expectedId/client", p.clientDialUrl())

        // The key survived decoding: a frame sealed under the raw key opens under
        // the decoded cipher — matching keys on both ends is the whole point.
        val sealed = Cipher(key).seal("frame".encodeToByteArray())
        assertContentEquals("frame".encodeToByteArray(), p.cipher.open(sealed))
    }
}
