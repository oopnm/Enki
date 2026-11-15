package io.github.oopnm.enki

import android.app.Activity
import android.os.Build
import android.os.Bundle
import android.view.Gravity
import android.widget.TextView
import io.github.oopnm.enki.link.Link

/** Phase 0 placeholder: proves the APK builds, installs and opens on Android 7+. */
class MainActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val label = TextView(this).apply {
            gravity = Gravity.CENTER
            textSize = 20f
            text = "Enki\nEnki Link v${Link.VERSION}\nAndroid ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT})"
        }
        setContentView(label)
    }
}
