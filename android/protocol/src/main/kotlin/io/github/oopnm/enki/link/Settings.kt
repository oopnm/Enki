package io.github.oopnm.enki.link

/**
 * Settings sync, phone side. This mirrors desktop/crates/enki-settings (schema check, hybrid logical clock, the
 * per-key sync rules); both are tested against protocol/vectors/settings.txt so they decide alike.
 *
 * Values are plain Kotlin: Boolean, Long, Double or String.
 */
enum class Scope { SYNCED, LOCAL }

enum class Apply { LIVE, RESTART, PASSIVE }

sealed class SettingKind {
    object Bool : SettingKind()
    data class Choice(val values: List<String>) : SettingKind()
    data class Whole(val min: Long, val max: Long, val step: Long) : SettingKind()
    data class Decimal(val min: Double, val max: Double) : SettingKind()
    data class Text(val maxLen: Int) : SettingKind()
}

/** Codes of `setting_reject`. */
enum class RejectCode(val wire: String) {
    UNKNOWN_KEY("unknown_key"), BAD_VALUE("bad_value"), LOCAL_ONLY("local_only"),
    UNSUPPORTED("unsupported"), APPLY_FAILED("apply_failed"), CLOCK_SKEW("clock_skew");

    companion object {
        fun parse(code: String): RejectCode = entries.firstOrNull { it.wire == code } ?: APPLY_FAILED
    }
}

data class Reject(val code: RejectCode, val message: String)

data class SettingDef(
    val key: String,
    val group: String,
    val kind: SettingKind,
    val default: Any,
    val scope: Scope,
    val apply: Apply,
    /** Whether the phone shows a control for it (desktop-only settings are not shown). */
    val onPhone: Boolean,
) {
    /** The value in its canonical form (numbers widened, so equal values compare equal), or a [Reject]. */
    fun check(value: Any?): Pair<Any?, Reject?> {
        fun bad(why: String) = null to Reject(RejectCode.BAD_VALUE, "$key: $why")
        return when (val k = kind) {
            SettingKind.Bool -> if (value is Boolean) value to null else bad("must be true or false")
            is SettingKind.Choice ->
                if (value is String && value in k.values) value to null else bad("must be one of ${k.values.joinToString(", ")}")
            is SettingKind.Whole -> {
                val v = (value as? Number)?.takeIf { it.toDouble() == it.toLong().toDouble() }?.toLong()
                    ?: return bad("must be a whole number")
                when {
                    v < k.min || v > k.max -> bad("must be between ${k.min} and ${k.max}")
                    k.step > 1 && (v - k.min) % k.step != 0L -> bad("must be ${k.min} plus a multiple of ${k.step}")
                    else -> v to null
                }
            }
            is SettingKind.Decimal -> {
                val v = (value as? Number)?.toDouble()?.takeIf { it.isFinite() } ?: return bad("must be a number")
                if (v < k.min || v > k.max) bad("must be between ${k.min} and ${k.max}") else v to null
            }
            is SettingKind.Text ->
                if (value is String && value.codePointCount(0, value.length) <= k.maxLen && value.none { it.isISOControl() }) value to null
                else bad("must be text of at most ${k.maxLen} characters")
        }
    }
}

/** Hybrid logical clock value: wall-clock milliseconds, then a counter. */
data class Hlc(val wall: Long, val counter: Int) : Comparable<Hlc> {
    override fun compareTo(other: Hlc): Int =
        if (wall != other.wall) wall.compareTo(other.wall) else counter.compareTo(other.counter)
}

/** One setting with the stamp of the write that produced it. The newest `(hlc, device)` wins on both sides. */
data class SettingEntry(val key: String, val value: Any, val hlc: Hlc, val device: String) {
    val stamp: Pair<Hlc, String> get() = hlc to device
}

private fun newer(a: SettingEntry, b: SettingEntry): Boolean =
    if (a.hlc != b.hlc) a.hlc > b.hlc else a.device > b.device

private fun sameStamp(a: SettingEntry, b: SettingEntry) = a.hlc == b.hlc && a.device == b.device

class HlcClock(private val now: () -> Long = { System.currentTimeMillis() }) {
    var last = Hlc(0, 0)
        private set

    fun advanceTo(seen: Hlc) {
        if (seen > last) last = seen
    }

    fun tick(): Hlc {
        val t = now()
        last = if (t > last.wall) Hlc(t, 0) else Hlc(last.wall, last.counter + 1)
        return last
    }

    /** False when the remote stamp is further ahead than [MAX_DRIFT_MS] (the clock is not touched then). */
    fun observe(remote: Hlc): Boolean {
        val t = now()
        if (remote.wall > t + MAX_DRIFT_MS) return false
        val wall = maxOf(last.wall, remote.wall, t)
        val counter = when {
            wall == last.wall && wall == remote.wall -> maxOf(last.counter, remote.counter) + 1
            wall == last.wall -> last.counter + 1
            wall == remote.wall -> remote.counter + 1
            else -> 0
        }
        last = Hlc(wall, counter)
        return true
    }

    companion object {
        const val MAX_DRIFT_MS = 24L * 60 * 60 * 1000
    }
}

sealed class Applied {
    data class Changed(val key: String, val value: Any) : Applied()
    object Unchanged : Applied()
    /** Older than what we hold: answer with this entry so the sender catches up. */
    data class Stale(val current: SettingEntry) : Applied()
}

data class Merge(
    val changed: List<Pair<String, Any>>,
    val sendBack: List<SettingEntry>,
    val ignored: List<Pair<String, Reject>>,
)

data class RolledBack(val key: String, val value: Any, val reject: Reject)

/** Thrown by [SettingsEngine.setLocal] for a value the schema refuses. */
class RejectedException(val reject: Reject) : Exception(reject.message)

class SettingsEngine(
    private val deviceId: String,
    private val clock: HlcClock = HlcClock(),
    private val defs: List<SettingDef> = SettingsSchema.defs,
) {
    private class Pending(var latest: SettingEntry, val baseline: SettingEntry?)

    private val entries = LinkedHashMap<String, SettingEntry>()
    private val pending = HashMap<String, Pending>()

    val clockLast: Hlc get() = clock.last

    private fun def(key: String) = defs.firstOrNull { it.key == key }

    @Synchronized
    fun get(key: String): Any? = def(key)?.let { entries[key]?.value ?: it.default }

    fun getString(key: String): String = get(key) as? String ?: ""

    fun getBool(key: String): Boolean = get(key) as? Boolean ?: false

    @Synchronized
    fun hasPending(): Boolean = pending.isNotEmpty()

    /** A change made on this device. Returns the entry to send to the PC, or null when nothing needs sending. */
    @Synchronized
    fun setLocal(key: String, value: Any): SettingEntry? {
        val d = def(key) ?: throw RejectedException(Reject(RejectCode.UNKNOWN_KEY, "$key is not a setting"))
        val (checked, reject) = d.check(value)
        if (reject != null) throw RejectedException(reject)
        if (get(key) == checked) return null
        val entry = SettingEntry(key, checked!!, clock.tick(), deviceId)
        val previous = entries.put(key, entry)
        if (d.scope == Scope.LOCAL) return null
        val p = pending[key]
        if (p != null) p.latest = entry else pending[key] = Pending(entry, previous)
        return entry
    }

    /** A change that arrived from the PC. Throws [RejectedException] for what must be refused. */
    @Synchronized
    fun applyRemote(incoming: SettingEntry): Applied {
        val d = def(incoming.key)
            ?: throw RejectedException(Reject(RejectCode.UNKNOWN_KEY, "${incoming.key} is not a setting here"))
        if (d.scope == Scope.LOCAL) {
            throw RejectedException(Reject(RejectCode.LOCAL_ONLY, "${incoming.key} belongs to one device and is not synced"))
        }
        val (checked, reject) = d.check(incoming.value)
        if (reject != null) throw RejectedException(reject)
        if (!clock.observe(incoming.hlc)) {
            throw RejectedException(Reject(RejectCode.CLOCK_SKEW, "the sender's clock is too far ahead of this device's"))
        }
        val value: Any = checked!!
        val entry = incoming.copy(value = value)
        val oldValue = get(incoming.key)
        val current = entries[incoming.key]
        if (current != null) {
            if (sameStamp(current, entry)) return Applied.Unchanged
            if (newer(current, entry)) return Applied.Stale(current)
        }
        val p = pending[incoming.key]
        if (p != null && newer(entry, p.latest)) pending.remove(incoming.key)
        entries[incoming.key] = entry
        return if (oldValue == value) Applied.Unchanged else Applied.Changed(incoming.key, value)
    }

    @Synchronized
    fun snapshot(): List<SettingEntry> =
        entries.values.filter { def(it.key)?.scope == Scope.SYNCED }

    @Synchronized
    fun mergeSnapshot(remote: List<SettingEntry>): Merge {
        pending.clear() // the exchange decides every key anew
        val changed = ArrayList<Pair<String, Any>>()
        val sendBack = ArrayList<SettingEntry>()
        val ignored = ArrayList<Pair<String, Reject>>()
        val seen = HashSet<String>()
        for (incoming in remote) {
            seen.add(incoming.key)
            try {
                when (val r = applyRemote(incoming)) {
                    is Applied.Changed -> changed.add(r.key to r.value)
                    is Applied.Stale -> sendBack.add(r.current)
                    Applied.Unchanged -> {}
                }
            } catch (e: RejectedException) {
                ignored.add(incoming.key to e.reject)
            }
        }
        snapshot().filter { it.key !in seen }.forEach { sendBack.add(it) }
        return Merge(changed, sendBack, ignored)
    }

    @Synchronized
    fun acked(key: String, hlc: Hlc, device: String): Boolean {
        val p = pending[key] ?: return false
        if (p.latest.hlc == hlc && p.latest.device == device) {
            pending.remove(key)
            return true
        }
        return false
    }

    /** Returns what to show again after the PC refused our change, or null when we have moved on since. */
    @Synchronized
    fun rejected(key: String, hlc: Hlc, device: String, reject: Reject): RolledBack? {
        val p = pending[key] ?: return null
        if (!(p.latest.hlc == hlc && p.latest.device == device)) return null
        pending.remove(key)
        if (p.baseline != null) entries[key] = p.baseline else entries.remove(key)
        return RolledBack(key, get(key) ?: return null, reject)
    }

    @Synchronized
    fun export(): List<SettingEntry> = entries.values.toList()

    /** Loads saved entries, skipping what the schema no longer allows; returns how many were skipped. */
    @Synchronized
    fun load(saved: List<SettingEntry>, savedClock: Hlc? = null): Int {
        var skipped = 0
        for (e in saved) {
            val d = def(e.key)
            val (checked, reject) = d?.check(e.value) ?: (null to Reject(RejectCode.UNKNOWN_KEY, e.key))
            if (reject != null) {
                skipped++
                continue
            }
            if (e.device == deviceId) clock.advanceTo(e.hlc)
            entries[e.key] = e.copy(value = checked!!)
        }
        savedClock?.let { clock.advanceTo(it) }
        return skipped
    }
}
