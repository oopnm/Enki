package io.github.oopnm.enki

import android.graphics.SurfaceTexture

/**
 * Connects the preview on the phone's screen (owned by [MainActivity]) with the camera session (owned by the
 * streaming service). The screen offers a [texture] while it is visible; the camera session draws into it.
 */
object PreviewBridge {
    /** The texture of the preview view while it exists, else null. */
    @Volatile var texture: SurfaceTexture? = null

    /**
     * Called when [texture] changed. With `waitForCamera` the call returns only after the camera stopped using the
     * old texture, which must happen before the system releases it.
     */
    @Volatile var onTexture: ((waitForCamera: Boolean) -> Unit)? = null
}
