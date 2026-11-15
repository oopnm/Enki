package io.github.oopnm.enki

import android.Manifest
import android.app.Activity
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.view.Gravity
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import io.github.oopnm.enki.link.Link

/** Phase 1: start/stop the camera server and show its state. The PC side connects through `adb forward`. */
class MainActivity : Activity() {
    private lateinit var statusView: TextView
    private lateinit var toggle: Button

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val pad = (24 * resources.displayMetrics.density).toInt()
        val title = TextView(this).apply {
            text = "Enki"
            textSize = 28f
            gravity = Gravity.CENTER
        }
        statusView = TextView(this).apply {
            textSize = 16f
            gravity = Gravity.CENTER
            setPadding(0, pad, 0, pad)
        }
        toggle = Button(this).apply { setOnClickListener { onToggle() } }
        val hint = TextView(this).apply {
            textSize = 14f
            gravity = Gravity.CENTER
            text = "Connect the phone by USB, then run on the PC:\n" +
                "adb forward tcp:${Link.DEFAULT_PORT} tcp:${Link.DEFAULT_PORT}\n\n" +
                "Enki Link v${Link.VERSION} · Android ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT})"
        }
        setContentView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER
            setPadding(pad, pad, pad, pad)
            addView(title)
            addView(statusView)
            addView(toggle)
            addView(hint)
        })
    }

    override fun onStart() {
        super.onStart()
        EnkiState.listener = { runOnUiThread { render() } }
        render()
    }

    override fun onStop() {
        EnkiState.listener = null
        super.onStop()
    }

    private fun render() {
        statusView.text = EnkiState.status
        toggle.text = if (EnkiState.running) "Stop" else "Start"
    }

    private fun onToggle() {
        if (EnkiState.running) {
            StreamService.stop(this)
            return
        }
        val missing = requiredPermissions().filter { checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }
        if (missing.isEmpty()) StreamService.start(this) else requestPermissions(missing.toTypedArray(), REQUEST_PERMISSIONS)
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode != REQUEST_PERMISSIONS) return
        // The notification permission is optional: the service runs without it, the notification just stays hidden.
        if (checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
            StreamService.start(this)
        } else {
            statusView.text = "Camera permission is needed to stream"
        }
    }

    private fun requiredPermissions(): List<String> = buildList {
        add(Manifest.permission.CAMERA)
        if (Build.VERSION.SDK_INT >= 33) add(Manifest.permission.POST_NOTIFICATIONS)
    }

    private companion object {
        const val REQUEST_PERMISSIONS = 1
    }
}
