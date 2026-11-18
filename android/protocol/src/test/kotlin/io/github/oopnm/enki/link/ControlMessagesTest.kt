package io.github.oopnm.enki.link

import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class ControlMessagesTest {
    /** protocol/vectors/control.txt: name|json (Gradle runs tests from the module directory). */
    private fun vector(name: String): String =
        File("../../protocol/vectors/control.txt").readLines()
            .filter { it.isNotBlank() && !it.startsWith("#") }
            .map { it.substringBefore("|") to it.substringAfter("|") }
            .first { it.first == name }.second

    @Test
    fun simpleMessagesMatchTheSharedVectors() {
        assertEquals(
            vector("hello"),
            ControlMessages.hello("0.2.0", "Pixel 6", 28, "0123456789abcdef0123456789abcdef", listOf("settings")),
        )
        assertEquals(vector("hello_legacy"), ControlMessages.hello("0.2.0", "Pixel 6", 28))
        assertEquals(vector("pong"), ControlMessages.pong(7))
        assertEquals(vector("state_rotation"), ControlMessages.stateInt("image.rotation", 180))
        assertEquals(vector("state_bitrate"), ControlMessages.stateInt("stream.bitrate", 3000))
        assertEquals(vector("error"), ControlMessages.error("bad_value", "camera.facing must be \"front\" or \"back\""))
    }

    private val phoneId = "0123456789abcdef0123456789abcdef"
    private val pcId = "fedcba9876543210fedcba9876543210"

    @Test
    fun settingsMessagesMatchTheSharedVectors() {
        val snapshot = listOf(
            SettingEntry("camera.facing", "front", Hlc(1_700_000_000_000, 2), phoneId),
            SettingEntry("video.auto_rotate", true, Hlc(1_700_000_000_500, 0), phoneId),
        )
        assertEquals(vector("settings_snapshot"), ControlMessages.settingsSnapshot(false, snapshot))
        val reply = listOf(SettingEntry("video.rotate", "90", Hlc(1_700_000_001_000, 0), pcId))
        assertEquals(vector("settings_snapshot_reply"), ControlMessages.settingsSnapshot(true, reply))
        assertEquals(vector("setting"), ControlMessages.setting(reply[0]))
        assertEquals(vector("setting_ack"), ControlMessages.settingAck("video.rotate", Hlc(1_700_000_001_000, 0), pcId))
        assertEquals(
            vector("setting_reject"),
            ControlMessages.settingReject(
                "camera.facing", Hlc(1_700_000_002_000, 1), pcId, "unsupported", "this phone has no front camera",
            ),
        )
    }

    @Test
    fun capabilitiesMatchTheSharedVector() {
        val caps = Capabilities(
            cameras = listOf(
                CameraCaps(
                    id = "0", facing = "back", sensorOrientation = 90,
                    sizes = listOf(1920 to 1080, 1280 to 720), fpsRanges = listOf(15 to 30, 30 to 30),
                    zoomMax = 8.0f, flash = true, focusModes = listOf("continuous", "auto"),
                    evMin = -4, evMax = 4, evStep = 0.5f,
                ),
            ),
            codecs = listOf("h264", "hevc"),
        )
        assertEquals(vector("capabilities"), ControlMessages.capabilities(caps))
    }

    @Test
    fun noCamerasStillMakesValidJson() {
        assertEquals("""{"type":"capabilities","cameras":[],"codecs":[]}""", ControlMessages.capabilities(Capabilities(emptyList(), emptyList())))
    }

    @Test
    fun stringsAreEscaped() {
        assertEquals(""""a\"b\\c\nd\u0001"""", ControlMessages.quote("a\"b\\c\nd\u0001"))
        val hello = ControlMessages.hello("1", "Model \"X\"", 30)
        assertTrue(hello.contains(""""device":"Model \"X\"""""))
    }

    @Test
    fun nonFiniteNumbersDoNotBreakTheJson() {
        val cam = CameraCaps("0", "back", 0, emptyList(), emptyList(), Float.NaN, false, emptyList(), 0, 0, Float.POSITIVE_INFINITY)
        val json = ControlMessages.capabilities(Capabilities(listOf(cam), emptyList()))
        assertTrue(json.contains(""""zoom_max":0.0""") && json.contains(""""ev_step":0.0"""))
    }
}
