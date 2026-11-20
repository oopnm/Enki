package io.github.oopnm.enki

import android.graphics.SurfaceTexture

/**
 * Connects the preview on the phone's screen (owned by [MainActivity]) with the camera session (owned by the
 * streaming service). The screen offers a [texture] while it is visible; the camera session draws into it.
 */
object PreviewBridge {
    /** The texture of the preview view while it exists, else null. */
    @Volatile var texture: SurfaceTexture? = null

    /** Clockwise degrees that make the picture upright for the current camera and the way the phone is held. */
    @Volatile var rotation: Int = 0

    /** True for the front camera: its preview is mirrored like a mirror. */
    @Volatile var mirror: Boolean = false

    /**
     * Called when [texture] changed. With `waitForCamera` the call returns only after the camera stopped using the
     * old texture, which must happen before the system releases it.
     */
    @Volatile var onTexture: ((waitForCamera: Boolean) -> Unit)? = null

    /** Called (on any thread) when [rotation] or [mirror] changed. */
    @Volatile var onLook: (() -> Unit)? = null
}
