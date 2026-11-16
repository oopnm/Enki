package io.github.oopnm.enki.link

/**
 * Turns raw device-orientation angles into the quarter turns the picture needs.
 * Pure functions, so they are unit-tested on the JVM (no Android classes involved).
 */

/**
 * Quantizes an `OrientationEventListener` angle (0..359, clockwise from natural portrait) to 0/90/180/270.
 * The current bucket is kept until the phone is more than 60 degrees away from its centre (45 + 15 of
 * hysteresis), so a phone resting near a boundary does not flicker between two values.
 */
fun deviceBucket(degrees: Int, current: Int): Int {
    val d = Math.abs(degrees - current) % 360
    if (minOf(d, 360 - d) <= 60) return current
    return (((degrees % 360) + 45) / 90 * 90) % 360
}

/**
 * Clockwise rotation (0/90/180/270) to apply to the camera picture so it is upright, from the camera's
 * fixed sensor orientation and the device orientation. Same formula as Android's JPEG_ORIENTATION.
 */
fun imageRotation(sensorOrientation: Int, frontFacing: Boolean, deviceOrientation: Int): Int =
    if (frontFacing) (sensorOrientation - deviceOrientation + 360) % 360
    else (sensorOrientation + deviceOrientation) % 360
