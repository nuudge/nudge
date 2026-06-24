// 8.4-a protocol kit: a pure-JVM Kotlin library that speaks nudge's relay
// wire protocol (pairing decode, secretbox E2E, frame serde, WS attach handshake).
// No Android dependency — it builds and tests with a plain JDK so the genuinely
// agent-coupled logic can be validated against the live relay before any UI exists.
// The Android :app module (8.4-b) will depend on this and swap lazysodium-java for
// lazysodium-android (same crypto_secretbox wire format).
plugins {
    // Pinned to the Kotlin version AGP 9.x bundles as built-in Kotlin, so the whole
    // build (this pure-JVM lib + the :app Android module) runs one Kotlin compiler.
    kotlin("jvm") version "2.2.10"
    kotlin("plugin.serialization") version "2.2.10"
}

repositories {
    mavenCentral()
}

// Pin to JDK 21 so the build is reproducible regardless of which JDK happens to
// run Gradle (Homebrew installs a newer JDK as a Gradle dependency that Kotlin
// can't target yet). Configures both the Kotlin and Java toolchains.
kotlin {
    jvmToolchain(21)
}

dependencies {
    implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.7.3")
    implementation("com.squareup.okhttp3:okhttp:4.12.0")
    // libsodium binding (desktop JVM). The wire format (XSalsa20-Poly1305 secretbox,
    // nonce|MAC|ciphertext) is identical to the Rust dryoc side and to lazysodium-android.
    implementation("com.goterl:lazysodium-java:5.1.4")
    implementation("net.java.dev.jna:jna:5.14.0")

    testImplementation(kotlin("test"))
}

tasks.test {
    useJUnitPlatform()
}

// Headless live smoke against a relay-paired daemon (8.4-a end-to-end proof). Not a
// unit test — it needs a running daemon + pairing code:
//   ./gradlew :protocol:smoke --args "nudge:<pairing-code>"
tasks.register<JavaExec>("smoke") {
    group = "verification"
    description = "Attach to a relay-paired daemon and run one turn. Pass the code via --args."
    mainClass.set("io.gitlab.hongtao1207.nudge.protocol.SmokeMainKt")
    classpath = sourceSets["main"].runtimeClasspath
}
