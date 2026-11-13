// Pure JVM, no Android dependency, so it builds and tests with a plain JDK.
plugins {
    // Must match AGP 9.x's built-in Kotlin so the whole build runs one compiler.
    kotlin("jvm") version "2.2.10"
    kotlin("plugin.serialization") version "2.2.10"
}

repositories {
    mavenCentral()
}

// Homebrew's Gradle pulls a newer JDK than Kotlin can target yet.
kotlin {
    jvmToolchain(21)
}

dependencies {
    implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.7.3")
    implementation("com.squareup.okhttp3:okhttp:4.12.0")
    // Same secretbox wire format (nonce|MAC|ciphertext) as Rust dryoc and lazysodium-android.
    implementation("com.goterl:lazysodium-java:5.1.4")
    implementation("net.java.dev.jna:jna:5.14.0")

    testImplementation(kotlin("test"))
}

tasks.test {
    useJUnitPlatform()
}

// Needs a running daemon: ./gradlew :protocol:smoke --args "nudge:<pairing-code>"
tasks.register<JavaExec>("smoke") {
    group = "verification"
    description = "Attach to a relay-paired daemon and run one turn. Pass the code via --args."
    mainClass.set("io.gitlab.hongtao1207.nudge.protocol.SmokeMainKt")
    classpath = sourceSets["main"].runtimeClasspath
}
