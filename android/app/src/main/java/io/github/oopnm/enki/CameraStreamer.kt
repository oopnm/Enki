package io.github.oopnm.enki

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.SurfaceTexture
import android.hardware.camera2.CameraAccessException
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CameraMetadata
import android.hardware.camera2.CaptureRequest
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.graphics.Rect
import android.os.HandlerThread
import android.util.Range
import android.util.Size
import android.view.Surface
import io.github.oopnm.enki.link.BitrateController
import io.github.oopnm.enki.link.EncodedFrame
import io.github.oopnm.enki.link.FrameQueue
import io.github.oopnm.enki.link.Kind
import kotlin.math.abs

/**
 * Camera2 -> MediaCodec (H.264, surface input) -> [FrameQueue].
 *
 * One instance streams one session: call [start], then [stop] (idempotent) when the connection ends.
 * [onError] is called at most once, from an arbitrary thread, if the camera or encoder fails.
 */
class CameraStreamer(
    private val context: Context,
    private val queue: FrameQueue,
    /** "back" or "front". */
    private val facing: String,
    private val onError: (String) -> Unit,
    /** Called with the encoder's target bit rate in kbit/s when streaming starts and whenever it changes. */
    private val onBitrate: (Int) -> Unit = {},
    private val video: VideoConfig = VideoConfig.DEFAULT,
    /** Called with "h264" or "hevc" just before the first frame of this encoder is produced. */
    private val onCodec: (String) -> Unit = {},
    /** Called once, when the first key frame of this stream came out of the encoder (the settings work). */
    private val onStarted: () -> Unit = {},
    /** "chosen upper fps|every range the camera offers", e.g. "30|15-30,30-30"; for the PC's log. */
    private val onFpsInfo: (String) -> Unit = {},
    initialLens: LensControls = LensControls.read(),
    initialBitrate: Int = video.autoBitrate(),
) {
    private val lock = Any()
    private var stopped = false
    private var failed = false

    @Volatile private var cameraThread: HandlerThread? = null
    @Volatile private var camera: CameraDevice? = null
    @Volatile private var session: CameraCaptureSession? = null
    @Volatile private var encoder: MediaCodec? = null
    @Volatile private var inputSurface: Surface? = null
    @Volatile private var drainThread: Thread? = null
    @Volatile private var adapterThread: Thread? = null
    @Volatile private var started = false

    /** Camera controls that can change while streaming; see [applyLens]. */
    @Volatile private var lens: LensControls = initialLens

    /** The bit rate the user asked for (bit/s); the adapter works down from it when the link is slow. */
    @Volatile var bitrateTarget: Int = initialBitrate

    /** An upper limit on the bit rate (bit/s), e.g. while the phone is hot; Int.MAX_VALUE = none. */
    @Volatile var bitrateCeiling: Int = Int.MAX_VALUE

    @Volatile private var activeCharacteristics: CameraCharacteristics? = null
    @Volatile private var activeFps: Range<Int>? = null
    @Volatile private var activeHandler: Handler? = null
    @Volatile private var activeDevice: CameraDevice? = null

    @SuppressLint("MissingPermission") // checked by the caller (MainActivity requests it before starting the service)
    fun start() {
        val manager = context.getSystemService(Context.CAMERA_SERVICE) as CameraManager
        val cameraId = pickCamera(manager) ?: return fail("no $facing camera on this phone")
        val characteristics = manager.getCameraCharacteristics(cameraId)
        val map = characteristics.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
            ?: return fail("camera has no stream configuration")
        val size = chooseSize(map.getOutputSizes(MediaCodec::class.java) ?: map.getOutputSizes(SurfaceTexture::class.java))
            ?: return fail("camera reports no output sizes")
        val fpsRange = chooseFpsRange(characteristics.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES))
        activeCharacteristics = characteristics
        activeFps = fpsRange
        onFpsInfo("${fpsRange?.upper ?: 0}|" + (characteristics.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES)?.joinToString(",") { "${it.lower}-${it.upper}" } ?: ""))

        try {
            startEncoder(size, fpsRange?.upper ?: video.fps)
        } catch (e: Exception) {
            return fail("encoder failed: ${e.message}")
        }
        if (synchronized(lock) { stopped }) { // stop() ran while the encoder was starting
            releaseResources()
            return
        }

        val thread = HandlerThread("enki-camera").also { it.start() }
        cameraThread = thread
        val handler = Handler(thread.looper)
        activeHandler = handler
        try {
            manager.openCamera(cameraId, object : CameraDevice.StateCallback() {
                override fun onOpened(device: CameraDevice) {
                    synchronized(lock) {
                        if (stopped) { device.close(); return }
                        camera = device
                    }
                    activeDevice = device
                    createSession(device, handler)
                }

                override fun onDisconnected(device: CameraDevice) {
                    device.close()
                    fail("camera disconnected")
                }

                override fun onError(device: CameraDevice, error: Int) {
                    device.close()
                    fail(if (error == ERROR_CAMERA_IN_USE) "camera in use by another app" else "camera error $error")
                }
            }, handler)
        } catch (e: CameraAccessException) {
            fail("cannot open camera: ${e.message}")
        } catch (e: SecurityException) {
            fail("camera permission missing")
        }
    }

    fun stop() {
        val first = synchronized(lock) { if (stopped) false else { stopped = true; true } }
        if (first) releaseResources()
    }

    /** Safe to call more than once; every step tolerates an already-released object. */
    private fun releaseResources() {
        adapterThread?.interrupt()
        val thread = drainThread
        runCatching { session?.stopRepeating() }
        runCatching { session?.close() }
        runCatching { camera?.close() }
        // The drain loop ends once the encoder is stopped; join before releasing it.
        runCatching { encoder?.stop() }
        thread?.takeIf { it !== Thread.currentThread() }?.join(1000)
        runCatching { encoder?.release() }
        runCatching { inputSurface?.release() }
        cameraThread?.quitSafely()
        session = null; camera = null; encoder = null; inputSurface = null
    }

    private fun createSession(device: CameraDevice, handler: Handler) {
        val surface = inputSurface ?: return fail("no encoder surface")
        try {
            @Suppress("DEPRECATION") // the SessionConfiguration variant needs API 28; minSdk is 24
            device.createCaptureSession(listOf(surface), object : CameraCaptureSession.StateCallback() {
                override fun onConfigured(s: CameraCaptureSession) {
                    synchronized(lock) {
                        if (stopped) { s.close(); return }
                        session = s
                    }
                    try {
                        val current = lens
                        s.setRepeatingRequest(buildRequest(device, surface, current, false), null, handler)
                        if (current.focusMode == "once") s.capture(buildRequest(device, surface, current, true), null, handler)
                    } catch (e: Exception) {
                        fail("cannot start capture: ${e.message}")
                    }
                }

                override fun onConfigureFailed(s: CameraCaptureSession) = fail("camera session configuration failed")
            }, handler)
        } catch (e: Exception) {
            fail("cannot create camera session: ${e.message}")
        }
    }

    /** Applies new camera controls to the running stream (no restart). Quietly does nothing when not streaming yet. */
    fun applyLens(next: LensControls) {
        val previous = lens
        lens = next
        val s = session ?: return
        val device = activeDevice ?: return
        val surface = inputSurface ?: return
        try {
            s.setRepeatingRequest(buildRequest(device, surface, next, false), null, activeHandler)
            if (next.focusMode == "once" && previous.focusMode != "once") {
                s.capture(buildRequest(device, surface, next, true), null, activeHandler)
            }
        } catch (e: Exception) {
            // A camera that is closing or a control the hardware refuses must not end the stream.
        }
    }

    /**
     * The capture request for [c]. Every control is checked against what this camera reports it supports, because
     * Camera2 silently ignores or throws on unsupported values and phones differ a lot.
     */
    private fun buildRequest(device: CameraDevice, surface: Surface, c: LensControls, triggerAf: Boolean): CaptureRequest {
        val chars = activeCharacteristics
        val builder = device.createCaptureRequest(CameraDevice.TEMPLATE_RECORD)
        builder.addTarget(surface)
        builder.set(CaptureRequest.CONTROL_MODE, CaptureRequest.CONTROL_MODE_AUTO)
        activeFps?.let { builder.set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, it) }
        if (chars == null) return builder.build()

        // Focus
        val afModes = chars.get(CameraCharacteristics.CONTROL_AF_AVAILABLE_MODES) ?: IntArray(0)
        val minFocus = chars.get(CameraCharacteristics.LENS_INFO_MINIMUM_FOCUS_DISTANCE) ?: 0f
        when {
            c.focusMode == "manual" && minFocus > 0f -> {
                builder.set(CaptureRequest.CONTROL_AF_MODE, CaptureRequest.CONTROL_AF_MODE_OFF)
                builder.set(CaptureRequest.LENS_FOCUS_DISTANCE, c.focusDistance.coerceIn(0f, 1f) * minFocus)
            }
            c.focusMode == "once" && afModes.contains(CaptureRequest.CONTROL_AF_MODE_AUTO) -> {
                builder.set(CaptureRequest.CONTROL_AF_MODE, CaptureRequest.CONTROL_AF_MODE_AUTO)
                if (triggerAf) builder.set(CaptureRequest.CONTROL_AF_TRIGGER, CaptureRequest.CONTROL_AF_TRIGGER_START)
            }
            afModes.contains(CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_VIDEO) ->
                builder.set(CaptureRequest.CONTROL_AF_MODE, CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_VIDEO)
        }

        // Exposure: manual ISO / shutter need the MANUAL_SENSOR capability, otherwise only the compensation works.
        val manualSensor = chars.get(CameraCharacteristics.REQUEST_AVAILABLE_CAPABILITIES)
            ?.contains(CameraMetadata.REQUEST_AVAILABLE_CAPABILITIES_MANUAL_SENSOR) == true
        val isoRange = chars.get(CameraCharacteristics.SENSOR_INFO_SENSITIVITY_RANGE)
        val timeRange = chars.get(CameraCharacteristics.SENSOR_INFO_EXPOSURE_TIME_RANGE)
        if (manualSensor && isoRange != null && timeRange != null && (c.iso != "auto" || c.shutter != "auto")) {
            builder.set(CaptureRequest.CONTROL_AE_MODE, CaptureRequest.CONTROL_AE_MODE_OFF)
            val iso = c.iso.toIntOrNull() ?: 400
            val nanos = (c.shutter.toLongOrNull()?.let { 1_000_000_000L / it }) ?: 16_666_666L
            builder.set(CaptureRequest.SENSOR_SENSITIVITY, iso.coerceIn(isoRange.lower, isoRange.upper))
            builder.set(CaptureRequest.SENSOR_EXPOSURE_TIME, nanos.coerceIn(timeRange.lower, timeRange.upper))
        } else {
            chars.get(CameraCharacteristics.CONTROL_AE_COMPENSATION_RANGE)?.let {
                builder.set(CaptureRequest.CONTROL_AE_EXPOSURE_COMPENSATION, c.exposureComp.coerceIn(it.lower, it.upper))
            }
        }

        // White balance
        val awb = when (c.whiteBalance) {
            "incandescent" -> CaptureRequest.CONTROL_AWB_MODE_INCANDESCENT
            "fluorescent" -> CaptureRequest.CONTROL_AWB_MODE_FLUORESCENT
            "daylight" -> CaptureRequest.CONTROL_AWB_MODE_DAYLIGHT
            "cloudy" -> CaptureRequest.CONTROL_AWB_MODE_CLOUDY_DAYLIGHT
            else -> null
        }
        if (awb != null && chars.get(CameraCharacteristics.CONTROL_AWB_AVAILABLE_MODES)?.contains(awb) == true) {
            builder.set(CaptureRequest.CONTROL_AWB_MODE, awb)
        }

        // Torch
        if (c.torch && chars.get(CameraCharacteristics.FLASH_INFO_AVAILABLE) == true) {
            builder.set(CaptureRequest.FLASH_MODE, CaptureRequest.FLASH_MODE_TORCH)
        }

        // Stabilization
        if (c.stabilization && chars.get(CameraCharacteristics.CONTROL_AVAILABLE_VIDEO_STABILIZATION_MODES)
                ?.contains(CaptureRequest.CONTROL_VIDEO_STABILIZATION_MODE_ON) == true
        ) {
            builder.set(CaptureRequest.CONTROL_VIDEO_STABILIZATION_MODE, CaptureRequest.CONTROL_VIDEO_STABILIZATION_MODE_ON)
        }

        // Noise reduction
        val nr = when (c.noiseReduction) {
            "off" -> CaptureRequest.NOISE_REDUCTION_MODE_OFF
            "fast" -> CaptureRequest.NOISE_REDUCTION_MODE_FAST
            "quality" -> CaptureRequest.NOISE_REDUCTION_MODE_HIGH_QUALITY
            else -> null
        }
        if (nr != null && chars.get(CameraCharacteristics.NOISE_REDUCTION_AVAILABLE_NOISE_REDUCTION_MODES)?.contains(nr) == true) {
            builder.set(CaptureRequest.NOISE_REDUCTION_MODE, nr)
        }

        // Anti-flicker
        val band = when (c.antiflicker) {
            "off" -> CaptureRequest.CONTROL_AE_ANTIBANDING_MODE_OFF
            "50" -> CaptureRequest.CONTROL_AE_ANTIBANDING_MODE_50HZ
            "60" -> CaptureRequest.CONTROL_AE_ANTIBANDING_MODE_60HZ
            else -> CaptureRequest.CONTROL_AE_ANTIBANDING_MODE_AUTO
        }
        if (chars.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_ANTIBANDING_MODES)?.contains(band) == true) {
            builder.set(CaptureRequest.CONTROL_AE_ANTIBANDING_MODE, band)
        }

        // Zoom: the zoom ratio on Android 11+, a centred crop of the sensor before that.
        if (Build.VERSION.SDK_INT >= 30) {
            val range = chars.get(CameraCharacteristics.CONTROL_ZOOM_RATIO_RANGE)
            if (range != null) builder.set(CaptureRequest.CONTROL_ZOOM_RATIO, c.zoom.coerceIn(range.lower, range.upper))
        } else {
            val active = chars.get(CameraCharacteristics.SENSOR_INFO_ACTIVE_ARRAY_SIZE)
            val max = chars.get(CameraCharacteristics.SCALER_AVAILABLE_MAX_DIGITAL_ZOOM) ?: 1f
            if (active != null) {
                val z = c.zoom.coerceIn(1f, max)
                val w = (active.width() / z).toInt()
                val h = (active.height() / z).toInt()
                val left = active.left + (active.width() - w) / 2
                val top = active.top + (active.height() - h) / 2
                builder.set(CaptureRequest.SCALER_CROP_REGION, Rect(left, top, left + w, top + h))
            }
        }
        return builder.build()
    }

    private fun hasEncoder(mime: String): Boolean =
        MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.any { info ->
            info.isEncoder && info.supportedTypes.any { it.equals(mime, ignoreCase = true) }
        }

    private fun startEncoder(size: Size, fps: Int) {
        if (video.hevc && !hasEncoder(MediaFormat.MIMETYPE_VIDEO_HEVC)) {
            throw IllegalStateException("this phone cannot encode H.265")
        }
        // Low-latency hints first; some encoders reject them, so fall back to the plain configuration.
        val codec = try {
            configureEncoder(size, fps, lowLatency = true)
        } catch (e: Exception) {
            configureEncoder(size, fps, lowLatency = false)
        }
        encoder = codec
        onCodec(if (video.hevc) "hevc" else "h264")
        drainThread = Thread({ drain(codec) }, "enki-encoder").also { it.start() }
        adapterThread = Thread({ adaptBitrate(codec) }, "enki-bitrate").also { it.isDaemon = true; it.start() }
    }

    /**
     * Once a second, lowers the encoder's bit rate if the send queue dropped frames (the PC or the cable cannot
     * keep up) and raises it again after a calm period; see [BitrateController].
     */
    private fun adaptBitrate(codec: MediaCodec) {
        var base = bitrateTarget
        var controller = BitrateController(base)
        var applied = minOf(controller.current, bitrateCeiling)
        onBitrate(applied / 1000)
        var lastDropped = queue.droppedFrames()
        while (!Thread.currentThread().isInterrupted) {
            try {
                Thread.sleep(1000)
            } catch (e: InterruptedException) {
                return
            }
            val dropped = queue.droppedFrames()
            if (bitrateTarget != base) { // the user changed the bit rate: start from the new target
                base = bitrateTarget
                controller = BitrateController(base, minOf(500_000, base))
            }
            controller.onSecond(dropped - lastDropped)
            lastDropped = dropped
            val effective = minOf(controller.current, bitrateCeiling)
            if (effective != applied) {
                applied = effective
                runCatching {
                    codec.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_VIDEO_BITRATE, effective) })
                }
                onBitrate(effective / 1000)
            }
        }
    }

    private fun configureEncoder(size: Size, fps: Int, lowLatency: Boolean): MediaCodec {
        val mime = if (video.hevc) MediaFormat.MIMETYPE_VIDEO_HEVC else MediaFormat.MIMETYPE_VIDEO_AVC
        val format = MediaFormat.createVideoFormat(mime, size.width, size.height).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, minOf(bitrateTarget, bitrateCeiling))
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 1)
            setInteger(MediaFormat.KEY_BITRATE_MODE, MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR)
            if (lowLatency) {
                setInteger(MediaFormat.KEY_PRIORITY, 0) // real-time
                if (!video.hevc) setInteger(MediaFormat.KEY_PROFILE, MediaCodecInfo.CodecProfileLevel.AVCProfileBaseline) // no B-frames
                if (Build.VERSION.SDK_INT >= 29) setInteger(MediaFormat.KEY_MAX_B_FRAMES, 0)
                if (Build.VERSION.SDK_INT >= 30) setInteger(MediaFormat.KEY_LATENCY, 1)
            }
        }
        val codec = MediaCodec.createEncoderByType(mime)
        try {
            codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            inputSurface = codec.createInputSurface()
            codec.start()
        } catch (e: Exception) {
            codec.release()
            throw e
        }
        return codec
    }

    /** Reads encoded buffers until the encoder is stopped or fails. */
    private fun drain(codec: MediaCodec) {
        val info = MediaCodec.BufferInfo()
        var config: ByteArray? = null
        var basePts = -1L
        try {
            while (true) {
                val index = codec.dequeueOutputBuffer(info, 10_000)
                if (index < 0) continue // try-again / format-changed / buffers-changed: nothing to do
                val buffer = codec.getOutputBuffer(index)
                if (buffer != null && info.size > 0) {
                    buffer.position(info.offset)
                    buffer.limit(info.offset + info.size)
                    val data = ByteArray(info.size).also { buffer.get(it) }
                    if (basePts < 0) basePts = info.presentationTimeUs
                    val pts = (info.presentationTimeUs - basePts).coerceAtLeast(0)
                    when {
                        info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0 -> config = data
                        info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0 -> {
                            queue.offerKey(
                            config?.let { EncodedFrame(Kind.VIDEO_CONFIG, it, pts) },
                            EncodedFrame(Kind.VIDEO_KEY, data, pts),
                            )
                            if (!started) { started = true; onStarted() }
                        }
                        else -> if (queue.offerDelta(EncodedFrame(Kind.VIDEO_DELTA, data, pts))) requestKeyFrame(codec)
                    }
                }
                codec.releaseOutputBuffer(index, false)
                if (info.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) break
            }
        } catch (e: IllegalStateException) {
            // Thrown when stop() releases the codec under us; only an error if we were not asked to stop.
            val wasStopped = synchronized(lock) { stopped }
            if (!wasStopped) fail("encoder stopped: ${e.message}")
        }
    }

    private fun requestKeyFrame(codec: MediaCodec) {
        runCatching {
            codec.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) })
        }
    }

    private fun fail(message: String) {
        val first = synchronized(lock) { (!failed && !stopped).also { if (it) failed = true } }
        if (first) onError(message)
    }

    private fun pickCamera(manager: CameraManager): String? {
        val want = if (facing == "front") CameraCharacteristics.LENS_FACING_FRONT else CameraCharacteristics.LENS_FACING_BACK
        val match = manager.cameraIdList.firstOrNull {
            manager.getCameraCharacteristics(it).get(CameraCharacteristics.LENS_FACING) == want
        }
        // A phone without a back camera (rare) still streams something; a missing front camera is an error.
        return match ?: if (facing == "front") null else manager.cameraIdList.firstOrNull()
    }

    /** The 16:9 size whose height is closest to the wanted one (an exact match when the camera has it). */
    private fun chooseSize(sizes: Array<Size>?): Size? {
        if (sizes.isNullOrEmpty()) return null
        val fit = sizes.filter { it.width <= 1920 && it.height <= 1080 }
        return fit.filter { it.width * 9 == it.height * 16 }.minByOrNull { abs(it.height - video.height) }
            ?: fit.maxByOrNull { it.width * it.height }
            ?: sizes.minByOrNull { it.width * it.height }
    }

    private fun chooseFpsRange(ranges: Array<Range<Int>>?): Range<Int>? {
        if (ranges.isNullOrEmpty()) return null
        return ranges.filter { it.upper == video.fps }.maxByOrNull { it.lower }
            ?: ranges.minByOrNull { abs(it.upper - video.fps) }
    }

    companion object {
        /** The camera's fixed sensor orientation (degrees), used to work out the picture rotation. */
        fun sensorOrientation(context: Context, facing: String): Int? = try {
            val manager = context.getSystemService(Context.CAMERA_SERVICE) as CameraManager
            val want = if (facing == "front") CameraCharacteristics.LENS_FACING_FRONT else CameraCharacteristics.LENS_FACING_BACK
            val id = manager.cameraIdList.firstOrNull {
                manager.getCameraCharacteristics(it).get(CameraCharacteristics.LENS_FACING) == want
            } ?: if (facing == "front") null else manager.cameraIdList.firstOrNull()
            id?.let { manager.getCameraCharacteristics(it).get(CameraCharacteristics.SENSOR_ORIENTATION) }
        } catch (e: CameraAccessException) {
            null
        }

        private const val ERROR_CAMERA_IN_USE = CameraDevice.StateCallback.ERROR_CAMERA_IN_USE
    }
}
