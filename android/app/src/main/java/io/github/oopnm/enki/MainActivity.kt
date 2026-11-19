package io.github.oopnm.enki

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.view.Gravity
import android.view.View
import android.widget.AdapterView
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Spinner
import android.widget.Switch
import android.widget.TextView
import android.widget.Toast
import io.github.oopnm.enki.link.Link
import io.github.oopnm.enki.link.SettingDef
import io.github.oopnm.enki.link.SettingKind
import io.github.oopnm.enki.link.SettingsSchema

/**
 * Start/stop the camera server, and the settings screen. The screen is built from the shared schema
 * (protocol/settings.json), so it shows the same settings as the desktop app, and a change made on either side
 * shows on the other.
 */
class MainActivity : Activity() {
    private lateinit var statusView: TextView
    private lateinit var toggle: Button
    private lateinit var settingsBox: LinearLayout

    /** True while the controls are being set from the stored values, so that does not count as a user change. */
    private var rendering = false
    private val redraw: () -> Unit = { runOnUiThread { renderSettings() } }

    override fun attachBaseContext(base: Context) {
        super.attachBaseContext(LocaleHelper.wrap(base))
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val pad = (24 * resources.displayMetrics.density).toInt()
        val title = TextView(this).apply {
            text = getString(R.string.app_name)
            textSize = 28f
            gravity = Gravity.CENTER
        }
        statusView = TextView(this).apply {
            textSize = 16f
            gravity = Gravity.CENTER
            setPadding(0, pad / 2, 0, pad / 2)
        }
        toggle = Button(this).apply { setOnClickListener { onToggle() } }
        settingsBox = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        val hint = TextView(this).apply {
            textSize = 14f
            gravity = Gravity.CENTER
            setPadding(0, pad, 0, 0)
            text = getString(R.string.hint_connect, Link.DEFAULT_PORT) +
                "\n\nEnki Link v${Link.VERSION} · Android ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT})"
        }
        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, pad, pad, pad)
            addView(title)
            addView(statusView)
            addView(toggle)
            addView(settingsBox)
            addView(hint)
        }
        setContentView(ScrollView(this).apply { addView(column) })
    }

    override fun onStart() {
        super.onStart()
        EnkiState.listener = { runOnUiThread { render() } }
        EnkiSettings.changeListeners.add(redraw)
        render()
        renderSettings()
    }

    override fun onStop() {
        EnkiState.listener = null
        EnkiSettings.changeListeners.remove(redraw)
        super.onStop()
    }

    private fun render() {
        statusView.text = EnkiState.status
        toggle.text = getString(if (EnkiState.running) R.string.stop else R.string.start)
    }

    // ---- settings screen, built from the shared schema ----

    private fun stringByName(name: String): String? {
        val id = resources.getIdentifier(name, "string", packageName)
        return if (id != 0) getString(id) else null
    }

    private fun resName(key: String) = key.replace('.', '_')

    private fun labelOf(def: SettingDef) = stringByName("setting_${resName(def.key)}") ?: def.key

    private fun valueLabel(def: SettingDef, value: String) = stringByName("setting_${resName(def.key)}__$value") ?: value

    private fun renderSettings() {
        rendering = true
        settingsBox.removeAllViews()
        val pad = (12 * resources.displayMetrics.density).toInt()
        for (group in SettingsSchema.defs.filter { it.onPhone }.groupBy { it.group }) {
            settingsBox.addView(
                TextView(this).apply {
                    text = stringByName("group_${group.key}") ?: group.key
                    textSize = 18f
                    setPadding(0, pad * 2, 0, pad / 2)
                },
            )
            for (def in group.value) settingsBox.addView(controlFor(def))
        }
        rendering = false
    }

    private fun controlFor(def: SettingDef): View {
        val row = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
        }
        val label = TextView(this).apply {
            text = labelOf(def)
            textSize = 16f
            layoutParams = LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f)
        }
        row.addView(label)
        when (val kind = def.kind) {
            SettingKind.Bool -> row.addView(
                Switch(this).apply {
                    isChecked = EnkiSettings.getBool(def.key)
                    setOnCheckedChangeListener { button, checked ->
                        if (!rendering) commit(def, checked) { button.isChecked = EnkiSettings.getBool(def.key) }
                    }
                },
            )
            is SettingKind.Choice -> {
                val names = kind.values.map { valueLabel(def, it) }
                row.addView(
                    Spinner(this).apply {
                        adapter = ArrayAdapter(this@MainActivity, android.R.layout.simple_spinner_dropdown_item, names)
                        setSelection(kind.values.indexOf(EnkiSettings.getString(def.key)).coerceAtLeast(0), false)
                        onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
                            override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) {
                                if (rendering) return
                                val chosen = kind.values[position]
                                if (chosen == EnkiSettings.getString(def.key)) return
                                commit(def, chosen) {
                                    setSelection(kind.values.indexOf(EnkiSettings.getString(def.key)).coerceAtLeast(0), false)
                                }
                            }

                            override fun onNothingSelected(parent: AdapterView<*>?) {}
                        }
                    },
                )
            }
            else -> {} // numbers and text have no control on the phone yet (phase 5)
        }
        return row
    }

    /** A change made on the screen. A refusal shows its reason and puts the control back. */
    private fun commit(def: SettingDef, value: Any, undo: () -> Unit) {
        val reject = EnkiSettings.setByUser(def.key, value)
        if (reject != null) {
            Toast.makeText(this, reject.message, Toast.LENGTH_LONG).show()
            rendering = true
            undo()
            rendering = false
            return
        }
        if (def.key == "ui.language") recreate() // the language applies to the whole screen
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
            statusView.text = getString(R.string.camera_permission_needed)
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
