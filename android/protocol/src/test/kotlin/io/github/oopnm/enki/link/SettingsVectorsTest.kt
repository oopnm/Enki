package io.github.oopnm.enki.link

import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/** Runs protocol/vectors/settings.txt, the same decisions the Rust engine is tested against. */
class SettingsVectorsTest {
    /** `key=value@wall.counter#device`, value as `"text"`, true/false or a number. */
    private fun parseEntry(text: String): SettingEntry {
        val device = text.substringAfterLast('#')
        val head = text.substringBeforeLast('#')
        val stamp = head.substringAfterLast('@')
        val keyValue = head.substringBeforeLast('@')
        val key = keyValue.substringBefore('=')
        val raw = keyValue.substringAfter('=')
        val value: Any = when {
            raw.startsWith("\"") -> raw.trim('"')
            raw == "true" -> true
            raw == "false" -> false
            raw.contains('.') -> raw.toDouble()
            else -> raw.toLong()
        }
        return SettingEntry(
            key, value,
            Hlc(stamp.substringBefore('.').toLong(), stamp.substringAfter('.').toInt()),
            device,
        )
    }

    @Test
    fun theSharedDecisionVectorsHold() {
        val lines = File("../../protocol/vectors/settings.txt").readLines()
            .filter { it.isNotBlank() && !it.startsWith("#") }
        assertTrue("the vector file was read", lines.size >= 10)
        for (line in lines) {
            val parts = line.split("|")
            val (name, current, incoming, expected) = parts
            val engine = SettingsEngine("x", HlcClock { 1_000_000L })
            if (current != "-") assertEquals("$name: current loads", 0, engine.load(listOf(parseEntry(current))))
            val got = try {
                when (engine.applyRemote(parseEntry(incoming))) {
                    is Applied.Changed -> "changed"
                    Applied.Unchanged -> "unchanged"
                    is Applied.Stale -> "stale"
                }
            } catch (e: RejectedException) {
                "reject:${e.reject.code.wire}"
            }
            assertEquals(name, expected, got)
        }
    }
}
