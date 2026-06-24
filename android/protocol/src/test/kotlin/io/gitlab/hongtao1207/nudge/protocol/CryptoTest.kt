package io.gitlab.hongtao1207.nudge.protocol

import java.security.SecureRandom
import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse

class CryptoTest {
    private fun randomKey() = ByteArray(32).also { SecureRandom().nextBytes(it) }

    @Test
    fun sealOpenRoundTrip() {
        val c = Cipher(randomKey())
        val pt = "a representative protocol frame".encodeToByteArray()
        val sealed = c.seal(pt)
        assertFalse(sealed.contentEquals(pt), "output must not be plaintext")
        assertContentEquals(pt, c.open(sealed))
    }

    @Test
    fun freshNonceEachSeal() {
        val c = Cipher(randomKey())
        val a = c.seal("same".encodeToByteArray())
        val b = c.seal("same".encodeToByteArray())
        assertFalse(a.contentEquals(b), "same plaintext must produce different ciphertext")
    }

    @Test
    fun tamperedFrameFails() {
        val c = Cipher(randomKey())
        val sealed = c.seal("hello".encodeToByteArray())
        val last = sealed.size - 1
        sealed[last] = (sealed[last].toInt() xor 0xff).toByte()
        assertFailsWith<IllegalStateException> { c.open(sealed) }
    }

    @Test
    fun wrongKeyFails() {
        val sealed = Cipher(randomKey()).seal("hello".encodeToByteArray())
        assertFailsWith<IllegalStateException> { Cipher(randomKey()).open(sealed) }
    }
}
