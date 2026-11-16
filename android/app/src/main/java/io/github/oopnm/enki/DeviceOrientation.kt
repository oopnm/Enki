package io.github.oopnm.enki

import android.content.Context
import android.hardware.SensorManager
import android.view.OrientationEventListener
import io.github.oopnm.enki.link.deviceBucket

/**
 * Follows how the phone is held, from the accelerometer (independent of the screen-rotation lock).
 * [bucket] is 0/90/180/270, clockwise from natural portrait; [onChange] is called when it changes.
 * The callback runs on the main thread, so it must not do network I/O.
 */
class DeviceOrientation(context: Context, private val onChange: () -> Unit) {
    @Volatile
    var bucket = 0
        private set

    private val listener = object : OrientationEventListener(context, SensorManager.SENSOR_DELAY_NORMAL) {
        override fun onOrientationChanged(degrees: Int) {
            if (degrees == ORIENTATION_UNKNOWN) return // lying flat: keep the last known value
            val next = deviceBucket(degrees, bucket)
            if (next != bucket) {
                bucket = next
                onChange()
            }
        }
    }

    fun enable() {
        if (listener.canDetectOrientation()) listener.enable()
    }

    fun disable() = listener.disable()
}
