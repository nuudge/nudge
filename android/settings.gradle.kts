// Multi-module so :protocol stays pure JVM, buildable and testable without the Android SDK.
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

include(":protocol")
include(":app")
