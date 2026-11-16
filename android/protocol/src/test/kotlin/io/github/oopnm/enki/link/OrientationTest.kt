package io.github.oopnm.enki.link

import org.junit.Assert.assertEquals
import org.junit.Test

class OrientationTest {
    @Test
    fun bucketsHaveHysteresis() {
        assertEquals(0, deviceBucket(50, 0))      // inside the 60 degree band: stay
        assertEquals(90, deviceBucket(70, 0))     // beyond it: move
        assertEquals(0, deviceBucket(340, 0))     // wraps around
        assertEquals(0, deviceBucket(359, 0))
        assertEquals(270, deviceBucket(300, 0))
        assertEquals(90, deviceBucket(100, 90))
        assertEquals(180, deviceBucket(175, 90))
        assertEquals(0, deviceBucket(10, 270))    // 270 -> 0 across the wrap
    }

    @Test
    fun backCameraRotation() {
        // typical back sensor orientation is 90
        assertEquals(90, imageRotation(90, false, 0))
        assertEquals(180, imageRotation(90, false, 90))
        assertEquals(270, imageRotation(90, false, 180))
        assertEquals(0, imageRotation(90, false, 270))
    }

    @Test
    fun frontCameraRotation() {
        // typical front sensor orientation is 270
        assertEquals(270, imageRotation(270, true, 0))
        assertEquals(180, imageRotation(270, true, 90))
        assertEquals(90, imageRotation(270, true, 180))
        assertEquals(0, imageRotation(270, true, 270))
    }

    @Test
    fun bothCamerasAgreeInLandscapeOnTypicalSensors() {
        // This is what was measured on the phone: held landscape, both cameras need 180 degrees.
        assertEquals(imageRotation(90, false, 90), imageRotation(270, true, 90))
    }
}
