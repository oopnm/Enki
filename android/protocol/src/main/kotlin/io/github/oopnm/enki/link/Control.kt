package io.github.oopnm.enki.link

/** What one phone camera can do (mirrors `CameraCaps` in desktop/crates/enki-protocol). */
data class CameraCaps(
    val id: String,
    /** "back", "front" or "external". */
    val facing: String,
    val sensorOrientation: Int,
    /** Video sizes as (width, height), largest first. */
    val sizes: List<Pair<Int, Int>>,
    /** Frame-rate ranges as (min, max). */
    val fpsRanges: List<Pair<Int, Int>>,
    val zoomMax: Float,
    val flash: Boolean,
    /** Subset of "continuous", "auto", "manual". */
    val focusModes: List<String>,
    val evMin: Int,
    val evMax: Int,
    /** Size of one exposure-compensation step in EV. */
    val evStep: Float,
)

data class Capabilities(val cameras: List<CameraCaps>, /** "h264", "hevc". */ val codecs: List<String>)

/**
 * Control messages the phone sends, as JSON text (mirrors `control::Message` on the PC; shared test vectors:
 * protocol/vectors/control.txt). Incoming messages are parsed by the app with org.json.
 */
object ControlMessages {
    fun hello(appVersion: String, device: String, androidSdk: Int): String =
        """{"type":"hello","version":${Link.VERSION},"app":"enki-android","app_version":${quote(appVersion)},"device":${quote(device)},"android":$androidSdk}"""

    fun pong(seq: Long): String = """{"type":"pong","seq":$seq}"""

    fun stateInt(key: String, value: Int): String = """{"type":"state","key":${quote(key)},"value":$value}"""

    fun stateString(key: String, value: String): String =
        """{"type":"state","key":${quote(key)},"value":${quote(value)}}"""

    fun error(code: String, message: String): String =
        """{"type":"error","code":${quote(code)},"message":${quote(message)}}"""

    fun capabilities(c: Capabilities): String = buildString {
        append("""{"type":"capabilities","cameras":[""")
        c.cameras.forEachIndexed { i, cam ->
            if (i > 0) append(',')
            append("""{"id":${quote(cam.id)},"facing":${quote(cam.facing)},"sensor_orientation":${cam.sensorOrientation},""")
            append(""""sizes":${pairs(cam.sizes)},"fps_ranges":${pairs(cam.fpsRanges)},""")
            append(""""zoom_max":${number(cam.zoomMax)},"flash":${cam.flash},"focus_modes":${strings(cam.focusModes)},""")
            append(""""ev_range":[${cam.evMin},${cam.evMax}],"ev_step":${number(cam.evStep)}}""")
        }
        append("""],"codecs":${strings(c.codecs)}}""")
    }

    private fun pairs(list: List<Pair<Int, Int>>) = list.joinToString(",", "[", "]") { "[${it.first},${it.second}]" }

    private fun strings(list: List<String>) = list.joinToString(",", "[", "]") { quote(it) }

    /** JSON has no NaN or infinity. */
    private fun number(v: Float) = if (v.isFinite()) v.toString() else "0.0"

    /** A JSON string literal. */
    fun quote(s: String): String = buildString {
        append('"')
        for (ch in s) {
            when {
                ch == '"' -> append("\\\"")
                ch == '\\' -> append("\\\\")
                ch == '\n' -> append("\\n")
                ch == '\r' -> append("\\r")
                ch == '\t' -> append("\\t")
                ch < ' ' -> append("\\u%04x".format(ch.code))
                else -> append(ch)
            }
        }
        append('"')
    }
}
