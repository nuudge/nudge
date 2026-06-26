// 8.4-b Android client: a Compose chat UI on top of the :protocol kit (8.4-a).
// AGP 9.x ships built-in Kotlin (KGP 2.2.10), so no kotlin-android plugin is applied
// here; only the Compose compiler plugin (its version must match the built-in Kotlin).
import java.util.Properties

plugins {
    id("com.android.application") version "9.2.0"
    id("org.jetbrains.kotlin.plugin.compose") version "2.2.10"
}

// Release signing is driven by an untracked keystore.properties at the android/ root
// (storeFile, storePassword, keyAlias, keyPassword). Absent it, release builds stay
// unsigned — so debug builds and fresh checkouts work with no setup.
val keystorePropsFile = rootProject.file("keystore.properties")
val keystoreProps = Properties().apply {
    if (keystorePropsFile.exists()) keystorePropsFile.inputStream().use { load(it) }
}

android {
    namespace = "io.gitlab.hongtao1207.nudge.app"
    compileSdk = 36

    defaultConfig {
        applicationId = "io.gitlab.hongtao1207.nudge"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.1"
    }

    buildFeatures {
        compose = true
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_21
        targetCompatibility = JavaVersion.VERSION_21
    }

    // lazysodium-android ships native .so per ABI; this keeps the symbols intact.
    packaging {
        jniLibs.useLegacyPackaging = true
    }

    signingConfigs {
        if (keystorePropsFile.exists()) {
            create("release") {
                storeFile = rootProject.file(keystoreProps.getProperty("storeFile"))
                storePassword = keystoreProps.getProperty("storePassword")
                keyAlias = keystoreProps.getProperty("keyAlias")
                keyPassword = keystoreProps.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            if (keystorePropsFile.exists()) {
                signingConfig = signingConfigs.getByName("release")
            }
        }
    }
}

kotlin {
    jvmToolchain(21)
}

dependencies {
    // The protocol kit. Exclude its desktop libsodium binding (JNA-loaded, no Android
    // .so) and substitute the Android variant — identical crypto_secretbox wire format.
    implementation(project(":protocol")) {
        exclude(group = "com.goterl", module = "lazysodium-java")
        exclude(group = "net.java.dev.jna", module = "jna")
    }
    implementation("com.goterl:lazysodium-android:5.1.0@aar")
    implementation("net.java.dev.jna:jna:5.14.0@aar")

    // Compose. The BOM pins mutually-compatible Compose artifact versions; Android
    // Studio will offer newer BOMs on sync — bump freely, the compiler plugin is
    // decoupled from the runtime now.
    val composeBom = platform("androidx.compose:compose-bom:2024.10.01")
    implementation(composeBom)
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.7")
    implementation("androidx.core:core-ktx:1.13.1")

    // QR pairing (8.4-c). Google's code scanner (ML Kit under the hood) runs the camera
    // + scan UI inside a Play Services process, so the app needs no CAMERA permission and
    // no CameraX preview wiring. Requires Google Play Services on the device; the paste
    // field stays as the fallback for non-GMS devices / the bare emulator.
    implementation("com.google.android.gms:play-services-code-scanner:16.1.0")

    // Markdown rendering for assistant text (8.5). Markwon is the battle-tested Android
    // markdown lib; it renders into a TextView (wrapped in an AndroidView). core covers
    // CommonMark; the ext-* plugins add GFM tables, strikethrough, and task lists, and
    // linkify autolinks bare URLs.
    implementation("io.noties.markwon:core:4.6.2")
    implementation("io.noties.markwon:ext-tables:4.6.2")
    implementation("io.noties.markwon:ext-strikethrough:4.6.2")
    implementation("io.noties.markwon:ext-tasklist:4.6.2")
    implementation("io.noties.markwon:linkify:4.6.2")

    debugImplementation("androidx.compose.ui:ui-tooling")
}
