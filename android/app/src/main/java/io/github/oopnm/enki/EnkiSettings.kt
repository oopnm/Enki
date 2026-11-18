package io.github.oopnm.enki

import android.content.Context
import android.content.SharedPreferences
import io.github.oopnm.enki.link.Applied
import io.github.oopnm.enki.link.Hlc
import io.github.oopnm.enki.link.HlcClock
import io.github.oopnm.enki.link.Merge
import io.github.oopnm.enki.link.Reject
import io.github.oopnm.enki.link.RejectCode
import io.github.oopnm.enki.link.RejectedException
import io.github.oopnm.enki.link.RolledBack
import io.github.oopnm.enki.link.SettingEntry
import io.github.oopnm.enki.link.SettingsEngine
import java.security.SecureRandom
import java.util.concurrent.CopyOnWriteArrayList
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject

/**
 * The phone's settings: one [SettingsEngine] stored in SharedPreferences, used by the screen and by the link to the
 * PC. A change made on the screen is pushed to the PC by [LinkServer] (through [userChangeListener]); a change from the
 * PC arrives through [applyRemote] and refreshes the screen (through [changeListeners]).
 */
object EnkiSettings {
    private const val PREFS = "enki_settings"
    private const val KEY_DEVICE_ID = "device_id"
    private const val KEY_CLOCK = "clock"
    private const val KEY_ENTRIES = "entries"

    private lateinit var context: Context
    private lateinit var prefs: SharedPreferences
    private lateinit var engine: SettingsEngine

    @Volatile private var ready = false
    lateinit var deviceId: String
        private set

    /** Called after any change, from any thread, so a screen can redraw. */
    val changeListeners = CopyOnWriteArrayList<() -> Unit>()

    /**
     * Called (on the caller's thread) when the user changes a setting on the phone: the entry to send to the PC (null
     * for a local setting) and the new value. Set by [LinkServer] while it runs.
     */
    @Volatile var userChangeListener: ((SettingEntry?, String, Any) -> Unit)? = null

    @Synchronized
    fun init(appContext: Context) {
        if (ready) return
        context = appContext.applicationContext
        prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        deviceId = prefs.getString(KEY_DEVICE_ID, null) ?: newId().also { prefs.edit().putString(KEY_DEVICE_ID, it).apply() }
        engine = SettingsEngine(deviceId, HlcClock())
        val (entries, clock) = readSaved()
        engine.load(entries, clock)
        ready = true
    }

    private fun newId(): String {
        val bytes = ByteArray(16).also { SecureRandom().nextBytes(it) }
        return bytes.joinToString("") { "%02x".format(it) }
    }

    fun get(key: String): Any? = engine.get(key)

    fun getString(key: String): String = engine.getString(key)

    fun getBool(key: String): Boolean = engine.getBool(key)

    fun snapshot(): List<SettingEntry> = engine.snapshot()

    /** The phone's own limits on top of the schema: a phone without a front camera cannot switch to it. */
    private fun unsupported(key: String, value: Any?): Reject? {
        if (key == "camera.facing" && (value == "front" || value == "back") &&
            CameraStreamer.sensorOrientation(context, value as String) == null
        ) {
            return Reject(RejectCode.UNSUPPORTED, "this phone has no $value camera")
        }
        return null
    }

    /** A change made on the phone's screen. Returns why it was refused, or null. */
    fun setByUser(key: String, value: Any): Reject? {
        unsupported(key, value)?.let { return it }
        val before = engine.get(key)
        val entry = try {
            engine.setLocal(key, value)
        } catch (e: RejectedException) {
            return e.reject
        }
        val changed = engine.get(key) != before
        persist()
        if (changed) {
            userChangeListener?.invoke(entry, key, value)
            notifyChanged()
        }
        return null
    }

    /** A change from the PC. Throws [RejectedException] when it must be refused (the caller sends `setting_reject`). */
    fun applyRemote(entry: SettingEntry): Applied {
        unsupported(entry.key, entry.value)?.let { throw RejectedException(it) }
        val result = engine.applyRemote(entry)
        if (result !is Applied.Stale) {
            persist()
            notifyChanged()
        }
        return result
    }

    fun mergeSnapshot(remote: List<SettingEntry>): Merge {
        val merge = engine.mergeSnapshot(remote)
        persist()
        notifyChanged()
        return merge
    }

    fun acked(key: String, hlc: Hlc, device: String) = engine.acked(key, hlc, device)

    fun rejected(key: String, hlc: Hlc, device: String, reject: Reject): RolledBack? {
        val rolled = engine.rejected(key, hlc, device, reject)
        if (rolled != null) {
            persist()
            notifyChanged()
        }
        return rolled
    }

    private fun notifyChanged() {
        changeListeners.forEach { runCatching { it() } }
    }

    @Synchronized
    private fun persist() {
        val array = JSONArray()
        for (e in engine.export()) {
            array.put(
                JSONObject().put("key", e.key).put("value", e.value).put("wall", e.hlc.wall)
                    .put("counter", e.hlc.counter).put("device", e.device),
            )
        }
        val clock = engine.clockLast
        prefs.edit().putString(KEY_ENTRIES, array.toString()).putString(KEY_CLOCK, "${clock.wall}.${clock.counter}").apply()
    }

    private fun readSaved(): Pair<List<SettingEntry>, Hlc?> {
        val entries = ArrayList<SettingEntry>()
        try {
            val array = JSONArray(prefs.getString(KEY_ENTRIES, "[]"))
            for (i in 0 until array.length()) {
                val o = array.getJSONObject(i)
                val raw = o.get("value")
                val value: Any = if (raw is Int) raw.toLong() else raw
                entries.add(SettingEntry(o.getString("key"), value, Hlc(o.getLong("wall"), o.getInt("counter")), o.getString("device")))
            }
        } catch (e: JSONException) {
            entries.clear() // a damaged file starts over with the defaults
        }
        val clock = prefs.getString(KEY_CLOCK, null)?.let {
            val wall = it.substringBefore('.').toLongOrNull()
            val counter = it.substringAfter('.', "0").toIntOrNull()
            if (wall != null && counter != null) Hlc(wall, counter) else null
        }
        return entries to clock
    }
}
