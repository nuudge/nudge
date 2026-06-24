package io.gitlab.hongtao1207.nudge.protocol

import java.util.Base64
import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertEquals

class PairingTest {
    @Test
    fun decodeRoundTrip() {
        // Craft a code exactly the way the Rust daemon mints one: base64url-JSON
        // ({relay, id, k}) under the `nudge:` scheme, key base64url no-pad.
        val key = ByteArray(32) { it.toByte() }
        val k = Base64.getUrlEncoder().withoutPadding().encodeToString(key)
        val payload = """{"relay":"wss://relay.example.com","id":"6f701d","k":"$k"}"""
        val code = "nudge:" +
            Base64.getUrlEncoder().withoutPadding().encodeToString(payload.encodeToByteArray())

        val p = Pairing.decode(code)
        assertEquals("wss://relay.example.com", p.relayBase)
        assertEquals("6f701d", p.rendezvousId)
        assertEquals("wss://relay.example.com/6f701d", p.dialUrl())

        // The key survived decoding: a frame sealed under the raw key opens under
        // the decoded cipher — matching keys on both ends is the whole point.
        val sealed = Cipher(key).seal("frame".encodeToByteArray())
        assertContentEquals("frame".encodeToByteArray(), p.cipher.open(sealed))
    }
}
