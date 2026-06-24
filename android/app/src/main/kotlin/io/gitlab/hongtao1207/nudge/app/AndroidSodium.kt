package io.gitlab.hongtao1207.nudge.app

import com.goterl.lazysodium.LazySodiumAndroid
import com.goterl.lazysodium.SodiumAndroid
import com.goterl.lazysodium.interfaces.SecretBox

// The Android libsodium binding, injected into Pairing.decode so :protocol's Cipher
// never touches its JVM default (LazySodiumJava, which isn't on the APK classpath).
// Same crypto_secretbox wire format as the Rust daemon and the desktop smoke test.
object AndroidSodium {
    val secretBox: SecretBox.Native by lazy { LazySodiumAndroid(SodiumAndroid()) }
}
