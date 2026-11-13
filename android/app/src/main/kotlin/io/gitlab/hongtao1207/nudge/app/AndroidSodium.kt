package io.gitlab.hongtao1207.nudge.app

import com.goterl.lazysodium.LazySodiumAndroid
import com.goterl.lazysodium.SodiumAndroid
import com.goterl.lazysodium.interfaces.SecretBox

// Injected into Pairing.decode so :protocol's Cipher never touches its JVM default
// (LazySodiumJava), which isn't on the APK classpath.
object AndroidSodium {
    val secretBox: SecretBox.Native by lazy { LazySodiumAndroid(SodiumAndroid()) }
}
