package io.github.oopnm.enki

import android.content.Context
import android.graphics.SurfaceTexture
import android.hardware.camera2.CameraAccessException
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.media.MediaCodec
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import io.github.oopnm.enki.link.CameraCaps
import io.github.oopnm.enki.link.Capabilities

/** Asks Camera2 and MediaCodec what this phone can do, for the `capabilities` message. Never throws. */
object CapabilitiesReader {
    fun read(context: Context): Capabilities {
        val cameras = try {
            val manager = context.getSystemService(Context.CAMERA_SERVICE) as CameraManager
            manager.cameraIdList.mapNotNull { id -> runCatching { camera(manager, id) }.getOrNull() }
        } catch (e: CameraAccessException) {
            emptyList()
        } catch (e: RuntimeException) {
            emptyList()
        }
        return Capabilities(cameras, codecs())
    }

    private fun camera(manager: CameraManager, id: String): CameraCaps? {
        val c = manager.getCameraCharacteristics(id)
        val map = c.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP) ?: return null
        val facing = when (c.get(CameraCharacteristics.LENS_FACING)) {
            CameraCharacteristics.LENS_FACING_FRONT -> "front"
            CameraCharacteristics.LENS_FACING_BACK -> "back"
            else -> "external"
        }
        val sizes = (map.getOutputSizes(MediaCodec::class.java) ?: map.getOutputSizes(SurfaceTexture::class.java) ?: emptyArray())
            .filter { it.width <= MAX_WIDTH && it.height <= MAX_HEIGHT }
            .sortedByDescending { it.width.toLong() * it.height }
            .map { it.width to it.height }
            .distinct()
            .take(MAX_SIZES)
        val fps = c.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES)
            ?.map { it.lower to it.upper }?.distinct() ?: emptyList()
        val zoomMax: Float? = if (Build.VERSION.SDK_INT >= 30) {
            c.get(CameraCharacteristics.CONTROL_ZOOM_RATIO_RANGE)?.upper
        } else {
            c.get(CameraCharacteristics.SCALER_AVAILABLE_MAX_DIGITAL_ZOOM)
        }
        val zoom = zoomMax ?: 1.0f
        val af = c.get(CameraCharacteristics.CONTROL_AF_AVAILABLE_MODES)?.toList() ?: emptyList()
        val focus = buildList {
            if (CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_VIDEO in af || CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_PICTURE in af) add("continuous")
            if (CaptureRequest.CONTROL_AF_MODE_AUTO in af) add("auto")
            if (CaptureRequest.CONTROL_AF_MODE_OFF in af && (c.get(CameraCharacteristics.LENS_INFO_MINIMUM_FOCUS_DISTANCE) ?: 0f) > 0f) add("manual")
        }
        val ev = c.get(CameraCharacteristics.CONTROL_AE_COMPENSATION_RANGE)
        return CameraCaps(
            id = id,
            facing = facing,
            sensorOrientation = c.get(CameraCharacteristics.SENSOR_ORIENTATION) ?: 0,
            sizes = sizes,
            fpsRanges = fps,
            zoomMax = zoom,
            flash = c.get(CameraCharacteristics.FLASH_INFO_AVAILABLE) == true,
            focusModes = focus,
            evMin = ev?.lower ?: 0,
            evMax = ev?.upper ?: 0,
            evStep = c.get(CameraCharacteristics.CONTROL_AE_COMPENSATION_STEP)?.toFloat() ?: 0f,
        )
    }

    /** Video encoders on this phone, as the names the protocol uses. */
    private fun codecs(): List<String> = try {
        val types = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
            .filter { it.isEncoder }
            .flatMap { it.supportedTypes.toList() }
        buildList {
            if (MediaFormat.MIMETYPE_VIDEO_AVC in types) add("h264")
            if (MediaFormat.MIMETYPE_VIDEO_HEVC in types) add("hevc")
        }
    } catch (e: RuntimeException) {
        listOf("h264")
    }

    private const val MAX_WIDTH = 3840
    private const val MAX_HEIGHT = 2160
    private const val MAX_SIZES = 12
}
