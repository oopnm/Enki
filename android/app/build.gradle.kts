plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "io.github.oopnm.enki"
    compileSdk = 34

    defaultConfig {
        applicationId = "io.github.oopnm.enki"
        minSdk = 24          // Android 7.0
        targetSdk = 34
        versionCode = 1
        versionName = "0.1.0"
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
}

dependencies {
    implementation(project(":protocol"))
}
