package io.gitlab.hongtao1207.nudge.protocol

import kotlin.test.Test
import kotlin.test.assertEquals

// Cross-language interop. This sealed frame was produced by the Rust crate
// (transport::encryption::Cipher::seal over a serde_json ServerFrame) under a known
// all-0x07 key. Proving the Kotlin client both opens AND parses it closes the one
// interop risk the Kotlin-only round-trips can't: that lazysodium and dryoc agree on
// the XSalsa20-Poly1305 secretbox wire format (nonce ‖ MAC ‖ ciphertext), and that
// kotlinx parses serde's external tagging byte-for-byte. Regenerate the vector with
// `cargo test print_interop_vector -- --nocapture` in the nudge crate.
class CryptoInteropTest {
    private fun hex(s: String) = ByteArray(s.length / 2) {
        s.substring(it * 2, it * 2 + 2).toInt(16).toByte()
    }

    @Test
    fun opensAndParsesRustSealedFrame() {
        val key = ByteArray(32) { 7.toByte() }
        val sealed = hex(
            "d91a9eb55fa515507928de17889ac48d14784b040cab44514b44c88ef026cdaa" +
                "bac53dbf1e23489cec7042655cffc77b5d05a3dc83bc361f6e026b36a96f0826" +
                "7013e4d1af01cd22c32b574993333a5d0302065476ddd6db25ae122f6cf1332b" +
                "e511d5b8100f47a4d12ab71502ff1772",
        )

        val plaintext = Cipher(key).open(sealed).decodeToString()
        assertEquals(
            """{"Event":{"seq":7,"event":{"AssistantText":{"text":"hello from rust"}}}}""",
            plaintext,
        )

        val frame = WireJson.decodeFromString(ServerFrame.serializer(), plaintext)
        assertEquals(ServerFrame.Event(7, ControllerEvent.AssistantText("hello from rust")), frame)
    }
}
