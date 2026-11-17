package io.github.oopnm.enki.link

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class BitrateControllerTest {
    @Test
    fun dropsCutTheBitrateAndItNeverGoesBelowTheFloor() {
        val c = BitrateController(4_000_000, minBps = 1_000_000)
        assertEquals(2_800_000, c.onSecond(3))
        assertEquals(1_960_000, c.onSecond(1))
        assertEquals(1_372_000, c.onSecond(9))
        assertEquals(1_000_000, c.onSecond(1))
        assertNull("already at the floor", c.onSecond(5))
        assertEquals(1_000_000, c.current)
    }

    @Test
    fun itRecoversSlowlyAfterCalmSeconds() {
        val c = BitrateController(4_000_000, minBps = 1_000_000)
        c.onSecond(1) // 2.8 Mbit/s
        repeat(BitrateController.RECOVER_AFTER - 1) { assertNull(c.onSecond(0)) }
        assertEquals(3_220_000, c.onSecond(0)) // +15 %
        repeat(BitrateController.RECOVER_AFTER - 1) { assertNull(c.onSecond(0)) }
        assertEquals(3_703_000, c.onSecond(0))
    }

    @Test
    fun aDropRestartsTheCalmPeriod() {
        val c = BitrateController(4_000_000)
        c.onSecond(1)
        repeat(BitrateController.RECOVER_AFTER - 1) { c.onSecond(0) }
        c.onSecond(2) // drop just before recovery
        repeat(BitrateController.RECOVER_AFTER - 1) { assertNull(c.onSecond(0)) }
        assertTrue(c.onSecond(0)!! > c.current - 1)
    }

    @Test
    fun itNeverClimbsAboveTheTargetAndStaysQuietThere() {
        val c = BitrateController(4_000_000, minBps = 1_000_000)
        c.onSecond(1)
        var last = c.current
        repeat(200) {
            c.onSecond(0)?.let { last = it }
        }
        assertEquals(4_000_000, last)
        assertEquals(4_000_000, c.current)
        repeat(20) { assertNull(c.onSecond(0)) }
    }

    @Test
    fun recoveryAlwaysReachesTheTargetEvenFromTheFloor() {
        val c = BitrateController(1_000_000, minBps = 500_000)
        repeat(30) { c.onSecond(1) }
        assertEquals(500_000, c.current)
        repeat(1000) { c.onSecond(0) }
        assertEquals(1_000_000, c.current)
    }
}
