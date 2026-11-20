package io.github.oopnm.enki

/**
 * What needs the encoder (and so the camera session) to restart when it changes: the synced settings `video.resolution`,
 * `video.fps`, `video.codec`, and a manual bit rate that is only a live parameter.
 */
data class VideoConfig(
    /** Output height in pixels: 480, 720 or 1080. */
    val height: Int,
    val fps: Int,
    val hevc: Boolean,
) {
    /** The bit rate used when `video.bitrate_mode` is "auto": scaled from 720p30 H.264 at 4 Mbit/s. */
    fun autoBitrate(): Int {
        val pixels = height.toDouble() * height * 16 / 9 / (720.0 * 1280)
        val rate = 4_000_000 * pixels * (fps / 30.0).coerceIn(0.5, 2.0) * (if (hevc) 0.7 else 1.0)
        return rate.toInt().coerceIn(500_000, 20_000_000)
    }

    companion object {
        val DEFAULT = VideoConfig(720, 30, false)

        fun read(): VideoConfig = VideoConfig(
            height = when (EnkiSettings.getString("video.resolution")) { "480p" -> 480; "1080p" -> 1080; else -> 720 },
            fps = EnkiSettings.getString("video.fps").toIntOrNull() ?: 30,
            hevc = EnkiSettings.getString("video.codec") == "hevc",
        )
    }
}

/** The bit rate the user wants, in bit/s: the manual value or the automatic one for [video]. */
fun targetBitrate(video: VideoConfig): Int {
    if (EnkiSettings.getString("video.bitrate_mode") == "manual") {
        val kbps = (EnkiSettings.get("video.bitrate_kbps") as? Number)?.toInt() ?: 4000
        return (kbps * 1000).coerceIn(500_000, 20_000_000)
    }
    return video.autoBitrate()
}

/** The camera controls that change while streaming (everything in the settings group "camera" except the facing). */
data class LensControls(
    val zoom: Float,
    val focusMode: String,
    /** 0 = far (infinity), 1 = nearest the lens can focus. Used when [focusMode] is "manual". */
    val focusDistance: Float,
    /** In exposure steps of the camera. */
    val exposureComp: Int,
    /** "auto" or a number. */
    val iso: String,
    /** "auto" or N for 1/N second. */
    val shutter: String,
    val whiteBalance: String,
    val torch: Boolean,
    val stabilization: Boolean,
    val noiseReduction: String,
    val antiflicker: String,
) {
    companion object {
        fun read(): LensControls = LensControls(
            zoom = (EnkiSettings.get("camera.zoom") as? Number)?.toFloat() ?: 1f,
            focusMode = EnkiSettings.getString("camera.focus_mode").ifEmpty { "continuous" },
            focusDistance = (EnkiSettings.get("camera.focus_distance") as? Number)?.toFloat() ?: 0f,
            exposureComp = (EnkiSettings.get("camera.exposure_comp") as? Number)?.toInt() ?: 0,
            iso = EnkiSettings.getString("camera.iso").ifEmpty { "auto" },
            shutter = EnkiSettings.getString("camera.shutter").ifEmpty { "auto" },
            whiteBalance = EnkiSettings.getString("camera.white_balance").ifEmpty { "auto" },
            torch = EnkiSettings.getBool("camera.torch"),
            stabilization = EnkiSettings.getBool("camera.stabilization"),
            noiseReduction = EnkiSettings.getString("camera.noise_reduction").ifEmpty { "auto" },
            antiflicker = EnkiSettings.getString("camera.antiflicker").ifEmpty { "auto" },
        )
    }
}
