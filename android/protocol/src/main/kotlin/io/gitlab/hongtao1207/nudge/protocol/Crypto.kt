package io.gitlab.hongtao1207.nudge.protocol

import com.goterl.lazysodium.LazySodiumJava
import com.goterl.lazysodium.SodiumJava
import com.goterl.lazysodium.interfaces.SecretBox
import java.security.SecureRandom

// Application-layer E2E — the Kotlin peer of Rust's transport::encryption. Same
// primitive (XSalsa20-Poly1305 secretbox) and the same per-frame wire layout,
// nonce(24) ‖ MAC(16) ‖ ciphertext, so a frame sealed by the Rust daemon opens
// here and vice versa. The crypto binding is injected as a SecretBox.Native so the
// Android module can pass a LazySodiumAndroid instead — the wire format is identical.
class Cipher(
    private val key: ByteArray,
    private val sodium: SecretBox.Native = defaultSodium,
) {
    init {
        require(key.size == SecretBox.KEYBYTES) {
            "relay key must be exactly ${SecretBox.KEYBYTES} bytes, found ${key.size}"
        }
    }

    // Fresh random nonce prepended to the secretbox output.
    fun seal(plaintext: ByteArray): ByteArray {
        val nonce = ByteArray(SecretBox.NONCEBYTES).also(rng::nextBytes)
        val cipher = ByteArray(SecretBox.MACBYTES + plaintext.size)
        check(sodium.cryptoSecretBoxEasy(cipher, plaintext, plaintext.size.toLong(), nonce, key)) {
            "secretbox seal failed"
        }
        return nonce + cipher
    }

    // Split off the nonce, then authenticate-and-decrypt. A wrong key or any
    // tampering fails the Poly1305 tag and throws rather than returning garbage.
    fun open(framed: ByteArray): ByteArray {
        val overhead = SecretBox.NONCEBYTES + SecretBox.MACBYTES
        require(framed.size >= overhead) { "sealed frame too short to contain a nonce + MAC" }
        val nonce = framed.copyOfRange(0, SecretBox.NONCEBYTES)
        val cipher = framed.copyOfRange(SecretBox.NONCEBYTES, framed.size)
        val message = ByteArray(cipher.size - SecretBox.MACBYTES)
        check(sodium.cryptoSecretBoxOpenEasy(message, cipher, cipher.size.toLong(), nonce, key)) {
            "frame failed to authenticate (wrong key or tampered)"
        }
        return message
    }

    companion object {
        private val rng = SecureRandom()
        val defaultSodium: SecretBox.Native by lazy { LazySodiumJava(SodiumJava()) }
    }
}
