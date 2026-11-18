package io.github.oopnm.enki.link

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class SettingsEngineTest {
    private var phoneTime = 1_000_000L
    private var pcTime = 1_000_000L
    private val phone = SettingsEngine("phone-0001", HlcClock { phoneTime })
    private val pc = SettingsEngine("pc-0001", HlcClock { pcTime })

    @Test
    fun aLocalChangeIsSentOnceAndNotEchoed() {
        val sent = phone.setLocal("camera.facing", "front")
        assertNotNull(sent)
        assertEquals("front", phone.get("camera.facing"))
        assertTrue(phone.hasPending())
        assertTrue(pc.applyRemote(sent!!) is Applied.Changed)
        assertFalse("applying a remote change creates nothing to send back", pc.hasPending())
        assertEquals(Applied.Unchanged, pc.applyRemote(sent))
        assertTrue(phone.acked(sent.key, sent.hlc, sent.device))
        assertFalse(phone.hasPending())
        assertNull("same value again is not a change", phone.setLocal("camera.facing", "front"))
    }

    @Test
    fun bothSidesPickTheSameWinnerOnATie() {
        val fromPc = pc.setLocal("video.rotate", "90")!!
        val fromPhone = phone.setLocal("video.rotate", "270")!!
        assertEquals(fromPc.hlc, fromPhone.hlc)
        val onPhone = phone.applyRemote(fromPc)
        val onPc = pc.applyRemote(fromPhone)
        assertEquals(pc.get("video.rotate"), phone.get("video.rotate"))
        assertEquals("270", pc.get("video.rotate")) // "phone-0001" > "pc-0001"
        assertTrue(onPhone is Applied.Stale)
        assertTrue(onPc is Applied.Changed)
    }

    @Test
    fun aRefusedChangeRollsBackToTheLastAgreedValue() {
        val a = phone.setLocal("video.rotate", "90")!!
        phone.acked(a.key, a.hlc, a.device)
        val b = phone.setLocal("video.rotate", "180")!!
        val c = phone.setLocal("video.rotate", "270")!!
        val reject = Reject(RejectCode.APPLY_FAILED, "encoder restart failed")
        assertNull("answer to an older change: nothing to do", phone.rejected(b.key, b.hlc, b.device, reject))
        assertEquals("270", phone.get("video.rotate"))
        val rolled = phone.rejected(c.key, c.hlc, c.device, reject)
        assertEquals("90", rolled!!.value)
    }

    @Test
    fun connectingSendsOnlyTheDifferencesAndConverges() {
        pc.setLocal("video.rotate", "90")
        pcTime = 1_000_100
        pc.setLocal("video.mirror", "on")
        phoneTime = 1_000_050
        phone.setLocal("camera.facing", "front")
        phoneTime = 1_000_200
        phone.setLocal("video.mirror", "off")

        val merge = pc.mergeSnapshot(phone.snapshot())
        assertEquals(setOf("camera.facing", "video.mirror"), merge.changed.map { it.first }.toSet())
        assertEquals(listOf("video.rotate"), merge.sendBack.map { it.key })
        phone.mergeSnapshot(merge.sendBack)
        for (key in listOf("camera.facing", "video.rotate", "video.mirror")) {
            assertEquals("$key converged", pc.get(key), phone.get(key))
        }
        assertEquals("off", pc.get("video.mirror"))
    }

    @Test
    fun localSettingsNeverTravel() {
        assertNull(phone.setLocal("ui.language", "ar"))
        assertTrue(phone.snapshot().isEmpty())
        assertFalse(phone.hasPending())
        val forged = SettingEntry("ui.language", "zh", Hlc(1_000_001, 0), "pc-0001")
        try {
            phone.applyRemote(forged)
            throw AssertionError("must be refused")
        } catch (e: RejectedException) {
            assertEquals(RejectCode.LOCAL_ONLY, e.reject.code)
        }
    }

    @Test
    fun badInputIsRefusedWithACode() {
        try {
            phone.setLocal("camera.facing", "up")
            throw AssertionError("must be refused")
        } catch (e: RejectedException) {
            assertEquals(RejectCode.BAD_VALUE, e.reject.code)
        }
        assertNull(phone.export().firstOrNull { it.key == "camera.facing" })
    }

    @Test
    fun saveAndLoadKeepTheClockAheadOfSavedStamps() {
        val saved = phone.setLocal("video.rotate", "180")!!
        val fresh = SettingsEngine("phone-0001", HlcClock { 5L }) // system clock set far back
        assertEquals(0, fresh.load(phone.export(), phone.clockLast))
        assertEquals("180", fresh.get("video.rotate"))
        val next = fresh.setLocal("video.rotate", "0")!!
        assertTrue(next.hlc > saved.hlc)
    }
}
