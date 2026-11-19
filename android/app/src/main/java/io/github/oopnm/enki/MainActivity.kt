package io.github.oopnm.enki

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.view.Gravity
import android.view.View
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.SeekBar
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

    /** Covers the whole screen in black while "black screen" is on and the phone is streaming. */
    private lateinit var blackout: View

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
        blackout = View(this).apply {
            setBackgroundColor(0xFF000000.toInt())
            isClickable = true // swallow touches so nothing behind it is pressed by accident
            visibility = View.GONE
            setOnLongClickListener {
                EnkiSettings.setByUser("phone.black_screen", false) // a long press brings the screen back
                true
            }
        }
        setContentView(
            FrameLayout(this).apply {
                addView(ScrollView(this@MainActivity).apply { addView(column) })
                addView(blackout, FrameLayout.LayoutParams(-1, -1))
            },
        )
        // "Start streaming when the app opens": only if the camera permission is already granted.
        if (EnkiSettings.getBool("phone.auto_start") && !EnkiState.running &&
            checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED
        ) {
            StreamService.start(this)
        }
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
        applyScreenSettings()
    }

    /** Keep-screen-on and black-screen only matter while streaming; both are synced settings. */
    private fun applyScreenSettings() {
        val streaming = EnkiState.running
        val keepOn = streaming && EnkiSettings.getBool("phone.keep_screen_on")
        val black = streaming && EnkiSettings.getBool("phone.black_screen")
        if (keepOn || black) window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        else window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        window.attributes = window.attributes.apply {
            screenBrightness = if (black) 0.01f else WindowManager.LayoutParams.BRIGHTNESS_OVERRIDE_NONE
        }
        blackout.visibility = if (black) View.VISIBLE else View.GONE
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
        applyScreenSettings()
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
            is SettingKind.Whole -> return sliderRow(def, label, kind.min.toDouble(), kind.max.toDouble(), kind.step.toDouble(), 0)
            is SettingKind.Decimal -> return sliderRow(def, label, kind.min, kind.max, (kind.max - kind.min) / 100.0, 2)
            else -> {} // text has no control on the phone
        }
        return row
    }

    /**
     * A label with its value and a slider under it. The value shows while dragging; the change is sent when the
     * finger lifts, so the PC and the camera get one change instead of a flood.
     */
    private fun sliderRow(def: SettingDef, label: TextView, min: Double, max: Double, step: Double, digits: Int): View {
        val column = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        val value = TextView(this).apply { textSize = 14f }
        val top = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            (label.parent as? LinearLayout)?.removeView(label)
            addView(label)
            addView(value)
        }
        fun show(v: Double) { value.text = if (digits == 0) v.toLong().toString() else "%.${digits}f".format(v) }
        val steps = Math.round((max - min) / step).toInt()
        val current = (EnkiSettings.get(def.key) as? Number)?.toDouble() ?: min
        show(current)
        val bar = SeekBar(this).apply {
            this.max = steps
            progress = Math.round((current - min) / step).toInt().coerceIn(0, steps)
            setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
                private fun at(p: Int) = (min + p * step).coerceIn(min, max)

                override fun onProgressChanged(b: SeekBar, p: Int, fromUser: Boolean) {
                    if (fromUser) show(at(p))
                }

                override fun onStartTrackingTouch(b: SeekBar) {}

                override fun onStopTrackingTouch(b: SeekBar) {
                    if (rendering) return
                    val v = at(b.progress)
                    val sent: Any = if (digits == 0) Math.round(v) else Math.round(v * 100) / 100.0
                    commit(def, sent) {
                        val now = (EnkiSettings.get(def.key) as? Number)?.toDouble() ?: min
                        b.progress = Math.round((now - min) / step).toInt().coerceIn(0, steps)
                        show(now)
                    }
                }
            })
        }
        column.addView(top)
        column.addView(bar)
        return column
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
