// Android client for nudge (Phase 8.4). Multi-module so the protocol kit
// (8.4-a) stays a pure-JVM library buildable/testable with just a JDK — no Android
// SDK — while the Compose app module (8.4-b) is added later on top of it.
pluginManagement {
    repositories {
        gradlePluginPortal()
        mavenCentral()
        google()
    }
}

dependencyResolutionManagement {
    repositories {
        mavenCentral()
        google()
    }
}

rootProject.name = "nudge-android"

// :protocol is pure JVM (8.4-a). :app is the Android/Compose client (8.4-b) and
// depends on :protocol, swapping lazysodium-java for lazysodium-android.
include(":protocol")
include(":app")
