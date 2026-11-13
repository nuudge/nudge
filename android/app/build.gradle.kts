// AGP 9.x ships built-in Kotlin (2.2.10), so no kotlin-android plugin; the Compose
// compiler plugin version must match that built-in Kotlin.
import java.util.Properties

plugins {
    id("com.android.application") version "9.2.0"
    id("org.jetbrains.kotlin.plugin.compose") version "2.2.10"
}

// Untracked keystore.properties at the android/ root; absent it, release builds stay unsigned.
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
        // CI injects these from the release tag.
        versionCode = (System.getenv("VERSION_CODE") ?: "1").toInt()
        versionName = System.getenv("VERSION_NAME") ?: "0.1"
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
    // Swap the desktop libsodium binding (JNA-loaded, no Android .so) for the Android one.
    implementation(project(":protocol")) {
        exclude(group = "com.goterl", module = "lazysodium-java")
        exclude(group = "net.java.dev.jna", module = "jna")
    }
    implementation("com.goterl:lazysodium-android:5.1.0@aar")
    implementation("net.java.dev.jna:jna:5.14.0@aar")

    val composeBom = platform("androidx.compose:compose-bom:2024.10.01")
    implementation(composeBom)
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.7")
    implementation("androidx.core:core-ktx:1.13.1")

    // Scans inside Play Services, so no CAMERA permission; non-GMS devices fall back to paste.
    implementation("com.google.android.gms:play-services-code-scanner:16.1.0")

    implementation("io.noties.markwon:core:4.6.2")
    implementation("io.noties.markwon:ext-tables:4.6.2")
    implementation("io.noties.markwon:ext-strikethrough:4.6.2")
    implementation("io.noties.markwon:ext-tasklist:4.6.2")
    implementation("io.noties.markwon:linkify:4.6.2")

    debugImplementation("androidx.compose.ui:ui-tooling")
}
