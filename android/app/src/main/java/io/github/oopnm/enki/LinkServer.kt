package io.github.oopnm.enki

import android.content.Context
import android.os.Build
import android.util.Log
import io.github.oopnm.enki.link.Applied
import io.github.oopnm.enki.link.Auth
import io.github.oopnm.enki.link.AuthStatus
import io.github.oopnm.enki.link.Capabilities
import io.github.oopnm.enki.link.ControlMessages
import io.github.oopnm.enki.link.Channel
import io.github.oopnm.enki.link.FrameQueue
import io.github.oopnm.enki.link.Hlc
import io.github.oopnm.enki.link.Link
import io.github.oopnm.enki.link.MediaFrameWriter
import io.github.oopnm.enki.link.ProtocolException
import io.github.oopnm.enki.link.Reject
import io.github.oopnm.enki.link.RejectCode
import io.github.oopnm.enki.link.RejectedException
import io.github.oopnm.enki.link.SettingEntry
import io.github.oopnm.enki.link.UnsupportedVersionException
import io.github.oopnm.enki.link.decodePreamble
import io.github.oopnm.enki.link.encodeControlMessage
import io.github.oopnm.enki.link.encodePreamble
import io.github.oopnm.enki.link.imageRotation
import java.io.BufferedInputStream
import java.io.BufferedOutputStream
import java.io.DataInputStream
import java.io.IOException
import java.io.OutputStream
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.SocketTimeoutException
import java.net.Socket
import java.util.Collections
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.RejectedExecutionException
import java.util.concurrent.atomic.AtomicBoolean
import org.json.JSONException
import org.json.JSONObject

/**
 * Enki Link server (phone side). Listens on loopback only, so the PC reaches it through `adb forward`.
 *
 * The PC opens a connection and sends its 8-byte preamble; we validate it and answer with our own. Then we
 * send a random challenge and serve the channel only if the PC answers it with the session token it gave
 * us through adb ([SessionToken]); any other app on the phone can reach this port but cannot answer.
 * The camera runs only while a media connection is open.
 */
class LinkServer(
    private val context: Context,
    private val appVersion: String,
    private val port: Int = Link.DEFAULT_PORT,
    private val status: (String) -> Unit,
) {
    private val stopped = AtomicBoolean(false)
    private val mediaBusy = AtomicBoolean(false)
    private val clients: MutableSet<Socket> = Collections.synchronizedSet(HashSet())
    private var server: ServerSocket? = null

    /** Which camera the next (or running) media session uses: "back" or "front". Guarded by [mediaLock]. */
    private var facing = EnkiSettings.getString("camera.facing").ifEmpty { "back" }
    private val mediaLock = Any()
    private var activeMedia: MediaSession? = null

    /** The media connection being served, so a dead control link can cut it (see [serveControl]). */
    @Volatile private var mediaSocket: Socket? = null

    /** Computed once: asking Camera2 about every camera takes a moment. */
    private val capabilitiesJson: String by lazy { ControlMessages.capabilities(CapabilitiesReader.read(context)) }

    // Control connections currently open, and a worker for network writes (never on the main thread).
    private val controls: MutableList<ControlOut> = Collections.synchronizedList(ArrayList())
    private val notifier: ExecutorService = Executors.newSingleThreadExecutor()
    private val deviceOrientation = DeviceOrientation(context) {
        try { notifier.execute { broadcastRotation() } } catch (e: RejectedExecutionException) { /* stopping */ }
    }

    /** Serializes writes to one control connection (replies and orientation updates come from different threads). */
    private class ControlOut(private val out: OutputStream) {
        @Synchronized
        fun send(json: String) {
            out.write(encodeControlMessage(json))
            out.flush()
        }

        fun send(json: JSONObject) = send(json.toString())
    }

    /** Clockwise rotation the PC should apply so the picture is upright, for the current camera and hold. */
    private fun rotationMessage(): JSONObject? {
        val front = synchronized(mediaLock) { facing } == "front"
        val sensor = CameraStreamer.sensorOrientation(context, if (front) "front" else "back") ?: return null
        val value = imageRotation(sensor, front, deviceOrientation.bucket)
        return JSONObject().put("type", "state").put("key", "image.rotation").put("value", value)
    }

    /** Sends a `state` message to every connected PC without blocking the caller (it may be the camera thread). */
    private fun broadcastState(json: String) {
        try {
            notifier.execute { synchronized(controls) { controls.toList() }.forEach { runCatching { it.send(json) } } }
        } catch (e: RejectedExecutionException) {
            // stopping
        }
    }

    private fun broadcastRotation() {
        val message = rotationMessage() ?: return
        synchronized(controls) { controls.toList() }.forEach { runCatching { it.send(message) } }
    }

    /** Owns the camera pipeline of one media connection and can swap it for the other camera while streaming. */
    private inner class MediaSession(private val queue: FrameQueue, private val onError: (String) -> Unit) {
        private fun newStreamer() = CameraStreamer(
            context, queue, facing, onError,
            onBitrate = { kbps -> broadcastState(ControlMessages.stateInt("stream.bitrate", kbps)) },
        )

        private var streamer: CameraStreamer? = null
        private var closed = false

        fun start() {
            synchronized(mediaLock) {
                if (!closed) streamer = newStreamer().also { it.start() }
            }
        }

        fun switchCamera() {
            synchronized(mediaLock) {
                if (closed) return
                streamer?.stop()
                streamer = newStreamer().also { it.start() }
            }
        }

        fun stop() {
            synchronized(mediaLock) {
                closed = true
                streamer?.stop()
                streamer = null
            }
        }
    }

    /** Returns true if the setting changed. A running stream switches camera right away. */
    private fun setFacing(value: String): Boolean = synchronized(mediaLock) {
        if (facing == value) return false
        facing = value
        activeMedia?.switchCamera()
        true
    }

    /** Binds the port and starts accepting. Throws [IOException] if the port is taken. */
    fun start() {
        val s = ServerSocket()
        try {
            s.reuseAddress = true
            s.bind(InetSocketAddress(InetAddress.getLoopbackAddress(), port))
        } catch (e: IOException) {
            s.close()
            throw e
        }
        server = s
        synchronized(mediaLock) { facing = EnkiSettings.getString("camera.facing").ifEmpty { "back" } }
        EnkiSettings.userChangeListener = { entry, key, value -> onUserChange(entry, key, value) }
        deviceOrientation.enable()
        Thread({ acceptLoop(s) }, "enki-accept").start()
        status(WAITING)
    }

    fun stop() {
        if (!stopped.compareAndSet(false, true)) return
        EnkiSettings.userChangeListener = null
        deviceOrientation.disable()
        SessionToken.clear()
        notifier.shutdownNow()
        runCatching { server?.close() }
        synchronized(clients) { clients.toList() }.forEach { runCatching { it.close() } }
    }

    private fun acceptLoop(s: ServerSocket) {
        while (!stopped.get()) {
            val socket = try { s.accept() } catch (e: IOException) { break }
            clients.add(socket)
            Thread({ handle(socket) }, "enki-client").start()
        }
    }

    private fun handle(socket: Socket) {
        try {
            socket.tcpNoDelay = true
            socket.soTimeout = HANDSHAKE_TIMEOUT_MS
            val input = DataInputStream(BufferedInputStream(socket.getInputStream()))
            val preamble = ByteArray(Link.PREAMBLE_LEN).also { input.readFully(it) }
            val channel = decodePreamble(preamble)
            val out = BufferedOutputStream(socket.getOutputStream(), 64 * 1024)
            out.write(encodePreamble(channel))
            out.flush()
            // Nothing is served, and the single media slot is not taken, before the PC proves it has the token.
            val verdict = Auth.serverHandshake(input, out, channel, SessionToken.get())
            if (verdict != AuthStatus.OK) {
                Log.w(TAG, "Refused a connection: $verdict")
                return
            }
            socket.soTimeout = 0
            when (channel) {
                Channel.MEDIA -> serveMedia(socket, out)
                Channel.CONTROL -> serveControl(socket, input, out)
            }
        } catch (e: UnsupportedVersionException) {
            // Answer with our own version first, so an older PC program can tell the user the versions differ.
            runCatching { socket.getOutputStream().apply { write(encodePreamble(Channel.CONTROL)); flush() } }
            Log.w(TAG, "Refused a connection: peer speaks protocol version ${e.peerVersion}, we speak ${Link.VERSION}")
        } catch (e: ProtocolException) {
            Log.w(TAG, "Refused a connection: ${e.message}")
        } catch (e: IOException) {
            // The peer went away or the handshake timed out; nothing else to do.
        } finally {
            clients.remove(socket)
            runCatching { socket.close() }
        }
    }

    private fun serveMedia(socket: Socket, out: OutputStream) {
        if (!mediaBusy.compareAndSet(false, true)) {
            status("A second media connection was refused (one stream at a time)")
            return
        }
        mediaSocket = socket
        val queue = FrameQueue(QUEUE_FRAMES)
        var cameraError: String? = null
        val session = MediaSession(queue) { message ->
            cameraError = message
            queue.close()
        }
        synchronized(mediaLock) { activeMedia = session }
        try {
            status(STREAMING)
            session.start()
            val writer = MediaFrameWriter(out)
            while (true) {
                val frame = queue.take() ?: break
                writer.write(frame)
            }
        } finally {
            synchronized(mediaLock) { if (activeMedia === session) activeMedia = null }
            session.stop()
            queue.close()
            mediaSocket = null
            mediaBusy.set(false)
            val error = cameraError
            if (!stopped.get()) status(if (error != null) "Camera stopped: $error" else WAITING)
        }
    }

    /**
     * Says hello and sends the phone's capabilities, answers `ping` with `pong`, handles `set camera.facing`
     * (confirmed with `state`, preceded by the current `image.rotation`) and pushes `image.rotation` whenever
     * the phone is turned. Other messages are ignored.
     *
     * The PC pings every second. If nothing arrives for [CONTROL_TIMEOUT_MS] the cable was pulled or the PC
     * program froze: TCP alone may not notice for minutes, so the camera is stopped by closing the media
     * connection, which saves the battery and lets the PC reconnect cleanly.
     */
    private fun serveControl(socket: Socket, input: DataInputStream, out: OutputStream) {
        val control = ControlOut(out)
        controls.add(control)
        try {
            control.send(
                ControlMessages.hello(
                    appVersion, Build.MODEL ?: "", Build.VERSION.SDK_INT, EnkiSettings.deviceId, listOf(FEATURE_SETTINGS),
                ),
            )
            control.send(runCatching { capabilitiesJson }.getOrElse { ControlMessages.capabilities(Capabilities(emptyList(), emptyList())) })
            // What this phone has set; the PC answers with only what it has newer or the phone lacks.
            control.send(ControlMessages.settingsSnapshot(false, EnkiSettings.snapshot()))
            socket.soTimeout = CONTROL_TIMEOUT_MS
            while (true) {
                val len = input.readInt()
                if (len < 0 || len > Link.MAX_CONTROL) throw ProtocolException("bad control message length $len")
                val body = ByteArray(len).also { input.readFully(it) }
                val message = try { JSONObject(String(body, Charsets.UTF_8)) } catch (e: JSONException) { continue }
                when (message.optString("type")) {
                    "ping" -> control.send(ControlMessages.pong(message.optLong("seq")))
                    "setting" -> parseEntry(message)?.let { handleSetting(control, it) }
                    "settings_snapshot" -> handleSnapshot(control, message)
                    "setting_ack" -> parseStamp(message)?.let { (hlc, device) ->
                        EnkiSettings.acked(message.optString("key"), hlc, device)
                    }
                    "setting_reject" -> parseStamp(message)?.let { (hlc, device) ->
                        val reject = Reject(RejectCode.parse(message.optString("code")), message.optString("message"))
                        val rolled = EnkiSettings.rejected(message.optString("key"), hlc, device, reject)
                        if (rolled != null) applyEffect(control, rolled.key, rolled.value)
                    }
                    "set" -> if (message.optString("key") == "camera.facing") {
                        val value = message.optString("value")
                        if (value == "front" || value == "back") {
                            setFacing(value)
                            // The rotation depends on the camera, so it goes out first; the PC waits for the facing reply.
                            rotationMessage()?.let { control.send(it) }
                            control.send(ControlMessages.stateString("camera.facing", value))
                        } else {
                            control.send(ControlMessages.error("bad_value", "camera.facing must be \"front\" or \"back\""))
                        }
                    }
                }
            }
        } catch (e: SocketTimeoutException) {
            Log.w(TAG, "No message from the PC for ${CONTROL_TIMEOUT_MS / 1000} s: stopping the camera")
            runCatching { mediaSocket?.close() }
        } finally {
            controls.remove(control)
        }
    }

    /** A setting changed on the phone's screen: tell every PC, and move the camera if that was the setting. */
    private fun onUserChange(entry: SettingEntry?, key: String, value: Any) {
        try {
            notifier.execute {
                if (entry != null) broadcastState(ControlMessages.setting(entry))
                if (key == "camera.facing" && value is String) {
                    setFacing(value)
                    broadcastRotation()
                }
            }
        } catch (e: RejectedExecutionException) {
            // stopping
        }
    }

    /** A change from the PC that was accepted (or rolled back): make it real. Only the camera needs the phone. */
    private fun applyEffect(control: ControlOut, key: String, value: Any) {
        if (key != "camera.facing" || value !is String) return
        setFacing(value)
        // The rotation depends on the camera, so it goes out first; the PC waits for the facing confirmation.
        rotationMessage()?.let { control.send(it) }
        control.send(ControlMessages.stateString("camera.facing", value))
    }

    private fun handleSetting(control: ControlOut, entry: SettingEntry) {
        try {
            when (val applied = EnkiSettings.applyRemote(entry)) {
                is Applied.Changed -> {
                    control.send(ControlMessages.settingAck(entry.key, entry.hlc, entry.device))
                    applyEffect(control, applied.key, applied.value)
                }
                Applied.Unchanged -> control.send(ControlMessages.settingAck(entry.key, entry.hlc, entry.device))
                is Applied.Stale -> control.send(ControlMessages.setting(applied.current))
            }
        } catch (e: RejectedException) {
            control.send(ControlMessages.settingReject(entry.key, entry.hlc, entry.device, e.reject.code.wire, e.reject.message))
        }
    }

    /** The PC's answer to our snapshot: applied, never answered. */
    private fun handleSnapshot(control: ControlOut, message: JSONObject) {
        if (!message.optBoolean("reply")) return
        val array = message.optJSONArray("settings") ?: return
        val entries = (0 until array.length()).mapNotNull { i -> array.optJSONObject(i)?.let { parseEntry(it) } }
        val merge = EnkiSettings.mergeSnapshot(entries)
        merge.changed.firstOrNull { it.first == "camera.facing" }?.let { applyEffect(control, it.first, it.second) }
    }

    private fun parseEntry(o: JSONObject): SettingEntry? = try {
        val hlc = o.getJSONArray("hlc")
        val raw = o.get("value")
        val value: Any = if (raw is Int) raw.toLong() else raw
        SettingEntry(o.getString("key"), value, Hlc(hlc.getLong(0), hlc.getInt(1)), o.getString("device"))
    } catch (e: JSONException) {
        null
    }

    private fun parseStamp(o: JSONObject): Pair<Hlc, String>? = try {
        val hlc = o.getJSONArray("hlc")
        Hlc(hlc.getLong(0), hlc.getInt(1)) to o.getString("device")
    } catch (e: JSONException) {
        null
    }

    companion object {
        /** Announced in `hello.features`: this app speaks settings sync. */
        const val FEATURE_SETTINGS = "settings"
        const val WAITING = "Ready. Waiting for the PC to connect"
        const val STREAMING = "Streaming camera to the PC"
        private const val QUEUE_FRAMES = 30
        private const val HANDSHAKE_TIMEOUT_MS = 5_000
        /** The PC pings every second; this is generous enough for a slow moment and short enough to save the battery. */
        private const val CONTROL_TIMEOUT_MS = 5_000
        private const val TAG = "EnkiLink"
    }
}
