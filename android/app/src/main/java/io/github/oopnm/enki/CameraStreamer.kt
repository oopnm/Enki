package io.github.oopnm.enki

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.SurfaceTexture
import android.hardware.camera2.CameraAccessException
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.util.Range
import android.util.Size
import android.view.Surface
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
    private val onError: (String) -> Unit,
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

    @SuppressLint("MissingPermission") // checked by the caller (MainActivity requests it before starting the service)
    fun start() {
        val manager = context.getSystemService(Context.CAMERA_SERVICE) as CameraManager
        val cameraId = pickBackCamera(manager) ?: return fail("no camera found")
        val characteristics = manager.getCameraCharacteristics(cameraId)
        val map = characteristics.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
            ?: return fail("camera has no stream configuration")
        val size = chooseSize(map.getOutputSizes(MediaCodec::class.java) ?: map.getOutputSizes(SurfaceTexture::class.java))
            ?: return fail("camera reports no output sizes")
        val fpsRange = chooseFpsRange(characteristics.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES))

        val afVideo = characteristics.get(CameraCharacteristics.CONTROL_AF_AVAILABLE_MODES)
            ?.contains(CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_VIDEO) == true

        try {
            startEncoder(size, fpsRange?.upper ?: TARGET_FPS)
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
        try {
            manager.openCamera(cameraId, object : CameraDevice.StateCallback() {
                override fun onOpened(device: CameraDevice) {
                    synchronized(lock) {
                        if (stopped) { device.close(); return }
                        camera = device
                    }
                    createSession(device, handler, fpsRange, afVideo)
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

    private fun createSession(device: CameraDevice, handler: Handler, fpsRange: Range<Int>?, afVideo: Boolean) {
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
                        val request = device.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
                            addTarget(surface)
                            set(CaptureRequest.CONTROL_MODE, CaptureRequest.CONTROL_MODE_AUTO)
                            if (afVideo) set(CaptureRequest.CONTROL_AF_MODE, CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_VIDEO)
                            if (fpsRange != null) set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, fpsRange)
                        }.build()
                        s.setRepeatingRequest(request, null, handler)
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

    private fun startEncoder(size: Size, fps: Int) {
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, size.width, size.height).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, BITRATE)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 1)
            setInteger(MediaFormat.KEY_BITRATE_MODE, MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR)
        }
        val codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        try {
            codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            inputSurface = codec.createInputSurface()
            codec.start()
        } catch (e: Exception) {
            codec.release()
            throw e
        }
        encoder = codec
        drainThread = Thread({ drain(codec) }, "enki-encoder").also { it.start() }
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
                        info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0 -> queue.offerKey(
                            config?.let { EncodedFrame(Kind.VIDEO_CONFIG, it, pts) },
                            EncodedFrame(Kind.VIDEO_KEY, data, pts),
                        )
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

    private fun pickBackCamera(manager: CameraManager): String? =
        manager.cameraIdList.firstOrNull {
            manager.getCameraCharacteristics(it).get(CameraCharacteristics.LENS_FACING) == CameraCharacteristics.LENS_FACING_BACK
        } ?: manager.cameraIdList.firstOrNull()

    private fun chooseSize(sizes: Array<Size>?): Size? {
        if (sizes.isNullOrEmpty()) return null
        val fit = sizes.filter { it.width <= 1920 && it.height <= 1080 }
        return fit.firstOrNull { it.width == 1280 && it.height == 720 }
            ?: fit.filter { it.width * 9 == it.height * 16 }.minByOrNull { abs(it.width - 1280) }
            ?: fit.maxByOrNull { it.width * it.height }
            ?: sizes.minByOrNull { it.width * it.height }
    }

    private fun chooseFpsRange(ranges: Array<Range<Int>>?): Range<Int>? {
        if (ranges.isNullOrEmpty()) return null
        return ranges.filter { it.upper == TARGET_FPS }.maxByOrNull { it.lower } ?: ranges.maxByOrNull { it.upper }
    }

    private companion object {
        const val TARGET_FPS = 30
        const val BITRATE = 4_000_000
        const val ERROR_CAMERA_IN_USE = CameraDevice.StateCallback.ERROR_CAMERA_IN_USE
    }
}
