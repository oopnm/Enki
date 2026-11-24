package io.github.oopnm.enki

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
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
import javax.net.ssl.KeyManagerFactory
import javax.net.ssl.SSLContext
import javax.net.ssl.SSLSocket
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

    /** The Wi-Fi listener (TLS) and its mDNS announcement; null while "Allow Wi-Fi connections" is off. Guarded by [wifiLock]. */
    private var wifiServer: ServerSocket? = null
    private var nsdListener: NsdManager.RegistrationListener? = null
    private val wifiLock = Any()

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

    /** Codec of the running encoder ("h264" or "hevc"), sent to a PC that connects while streaming. */
    @Volatile private var lastCodec: String? = null
    @Volatile private var lastFpsInfo: String? = null

    /** True while the phone is too hot and "lower quality when hot" is on. */
    @Volatile private var phoneHot = false

    private val phoneMonitor = PhoneMonitor(
        context,
        onBattery = { info -> broadcastBattery(info) },
        onHot = { hot -> phoneHot = hot; synchronized(mediaLock) { activeMedia?.refreshLive() } },
    )

    private fun broadcastBattery(info: BatteryInfo) {
        broadcastState(ControlMessages.stateInt("phone.battery", info.percent))
        broadcastState(ControlMessages.stateInt("phone.temperature", info.tempDc))
        broadcastState(ControlMessages.stateBool("phone.charging", info.charging))
    }

    /** Owns the camera pipeline of one media connection and can swap it for the other camera while streaming. */
    private inner class MediaSession(private val queue: FrameQueue, private val onError: (String) -> Unit) {
        /** The video settings of the running streamer, and the last ones that really produced a picture. */
        @Volatile private var currentVideo: VideoConfig? = null
        @Volatile private var lastGood: VideoConfig? = null

        private fun newStreamer(): CameraStreamer {
            val video = VideoConfig.read()
            currentVideo = video
            val target = targetBitrate(video)
            return CameraStreamer(
                context, queue, facing, { message -> onStreamError(message) },
                onBitrate = { kbps -> broadcastState(ControlMessages.stateInt("stream.bitrate", kbps)) },
                video = video,
                onCodec = { codec ->
                    lastCodec = codec
                    broadcastState(ControlMessages.stateString("stream.codec", codec))
                },
                onStarted = { lastGood = video },
                onFpsInfo = { info ->
                    lastFpsInfo = info
                    broadcastState(ControlMessages.stateString("stream.fps_info", info))
                },
                initialBitrate = target,
            ).also { it.bitrateCeiling = ceilingFor(target) }
        }

        private fun ceilingFor(target: Int): Int =
            if (phoneHot && EnkiSettings.getBool("phone.thermal_reduce")) maxOf(500_000, target / 2) else Int.MAX_VALUE

        /**
         * A new resolution, frame rate or codec that the phone cannot deliver goes back to the last working one,
         * through the normal settings path, so the PC and the screen show the value that is really in use.
         */
        private fun onStreamError(message: String) {
            val good = lastGood
            if (good != null && good != currentVideo) {
                Thread {
                    EnkiSettings.setByUser("video.resolution", "${good.height}p")
                    EnkiSettings.setByUser("video.fps", good.fps.toString())
                    EnkiSettings.setByUser("video.codec", if (good.hevc) "hevc" else "h264")
                }.start()
            } else {
                onError(message)
            }
        }

        /** Applies the camera controls and the bit rate to the running stream without restarting it. */
        /** Puts the phone-screen preview on or off while streaming (see [CameraStreamer.setPreview]). */
        fun setPreview(texture: android.graphics.SurfaceTexture?, wait: Boolean) {
            val s = synchronized(mediaLock) { streamer }
            s?.setPreview(texture, wait)
        }

        fun refreshLive() {
            synchronized(mediaLock) {
                val s = streamer ?: return
                val video = currentVideo ?: return
                val target = targetBitrate(video)
                s.bitrateTarget = target
                s.bitrateCeiling = ceilingFor(target)
                s.applyLens(LensControls.read())
            }
        }

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
                lastCodec = null
                lastFpsInfo = null
            }
        }
    }

    private val restartPending = AtomicBoolean(false)

    /**
     * A setting that changes the stream itself: resolution, frame rate and codec restart the encoder (several changes
     * made together restart it once); the camera controls and the bit rate are applied to the running stream.
     */
    private fun applyStreamSetting(key: String) {
        when (key) {
            "video.resolution", "video.fps", "video.codec" -> {
                if (!restartPending.compareAndSet(false, true)) return
                try {
                    notifier.execute {
                        Thread.sleep(150)
                        restartPending.set(false)
                        synchronized(mediaLock) { activeMedia?.switchCamera() }
                    }
                } catch (e: RejectedExecutionException) {
                    restartPending.set(false)
                }
            }
            "video.bitrate_mode", "video.bitrate_kbps", "phone.thermal_reduce" ->
                synchronized(mediaLock) { activeMedia?.refreshLive() }
            else -> if (key.startsWith("camera.") && key != "camera.facing") {
                synchronized(mediaLock) { activeMedia?.refreshLive() }
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
        phoneMonitor.enable()
        PreviewBridge.onTexture = { wait ->
            val media = synchronized(mediaLock) { activeMedia }
            media?.setPreview(PreviewBridge.texture, wait)
        }
        Thread({ acceptLoop(s) }, "enki-accept").start()
        if (EnkiSettings.getBool("wifi.enabled")) startWifi()
        status(WAITING)
    }

    fun stop() {
        if (!stopped.compareAndSet(false, true)) return
        EnkiSettings.userChangeListener = null
        deviceOrientation.disable()
        phoneMonitor.disable()
        PreviewBridge.onTexture = null
        SessionToken.clear()
        notifier.shutdownNow()
        stopWifi()
        runCatching { server?.close() }
        synchronized(clients) { clients.toList() }.forEach { runCatching { it.close() } }
    }

    // ---- Wi-Fi: TLS listener, mDNS announcement, pairing

    /** Opens the Wi-Fi listener (TLS with the phone's own certificate) and announces it on the network. Idempotent. */
    private fun startWifi() {
        synchronized(wifiLock) {
            if (wifiServer != null || stopped.get()) return
            try {
                val identity = WifiState.identity()
                val keys = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm())
                    .apply { init(identity.keyStore(), io.github.oopnm.enki.link.PhoneIdentity.PASSWORD) }
                val context = SSLContext.getInstance("TLS").apply { init(keys.keyManagers, null, null) }
                val s = context.serverSocketFactory.createServerSocket()
                s.reuseAddress = true
                s.bind(InetSocketAddress(Link.WIFI_PORT))
                wifiServer = s
                Thread({ wifiAcceptLoop(s, identity.fingerprint) }, "enki-wifi-accept").start()
                announce()
                Log.i(TAG, "Wi-Fi listener on port ${Link.WIFI_PORT}")
            } catch (e: Exception) {
                Log.w(TAG, "Cannot open the Wi-Fi listener: ${e.message}")
                status("Wi-Fi: ${e.message}")
            }
        }
    }

    private fun stopWifi() {
        synchronized(wifiLock) {
            val listener = nsdListener
            if (listener != null) {
                runCatching { (context.getSystemService(Context.NSD_SERVICE) as NsdManager).unregisterService(listener) }
                nsdListener = null
            }
            runCatching { wifiServer?.close() }
            wifiServer = null
        }
    }

    /** Announces `_enki._tcp` with the app's device id, so a paired PC finds the phone even when its address changed. */
    private fun announce() {
        val info = NsdServiceInfo().apply {
            serviceName = "Enki-" + EnkiSettings.deviceId.take(6)
            serviceType = "_enki._tcp."
            port = Link.WIFI_PORT
            setAttribute("id", EnkiSettings.deviceId)
        }
        val listener = object : NsdManager.RegistrationListener {
            override fun onServiceRegistered(serviceInfo: NsdServiceInfo) {}
            override fun onRegistrationFailed(serviceInfo: NsdServiceInfo, errorCode: Int) {
                Log.w(TAG, "mDNS registration failed: $errorCode")
            }
            override fun onServiceUnregistered(serviceInfo: NsdServiceInfo) {}
            override fun onUnregistrationFailed(serviceInfo: NsdServiceInfo, errorCode: Int) {}
        }
        nsdListener = listener
        runCatching {
            (context.getSystemService(Context.NSD_SERVICE) as NsdManager).registerService(info, NsdManager.PROTOCOL_DNS_SD, listener)
        }.onFailure { nsdListener = null; Log.w(TAG, "mDNS: ${it.message}") }
    }

    private fun wifiAcceptLoop(s: ServerSocket, fingerprint: ByteArray) {
        while (!stopped.get()) {
            val socket = try { s.accept() } catch (e: IOException) { break }
            clients.add(socket)
            Thread({ handleWifi(socket as SSLSocket, fingerprint) }, "enki-wifi-client").start()
        }
    }

    /**
     * A connection over Wi-Fi. Same steps as [handle], but inside TLS, and the challenge is answered with a paired
     * secret or with the pairing key of the code typed on this screen, bound to our certificate's fingerprint.
     * A paired computer gets the whole protocol; a computer that only knows the code may do one thing: pair.
     */
    private fun handleWifi(socket: SSLSocket, fingerprint: ByteArray) {
        try {
            socket.tcpNoDelay = true
            socket.soTimeout = HANDSHAKE_TIMEOUT_MS
            socket.startHandshake()
            val input = DataInputStream(BufferedInputStream(socket.getInputStream()))
            val preamble = ByteArray(Link.PREAMBLE_LEN).also { input.readFully(it) }
            val channel = decodePreamble(preamble)
            val out = BufferedOutputStream(socket.getOutputStream(), 64 * 1024)
            out.write(encodePreamble(channel))
            out.flush()
            val verdict = Auth.serverHandshakeTls(input, out, channel, fingerprint, { WifiState.secrets() }, { WifiState.offer() })
            when (verdict) {
                Auth.TlsVerdict.PAIRED -> {
                    socket.soTimeout = 0
                    when (channel) {
                        Channel.MEDIA -> serveMedia(socket, out)
                        Channel.CONTROL -> serveControl(socket, input, out, wifi = true)
                    }
                }
                Auth.TlsVerdict.OFFER -> if (channel == Channel.CONTROL) serveOffer(input, out)
                Auth.TlsVerdict.BAD_TOKEN -> {
                    Log.w(TAG, "Refused a Wi-Fi connection: wrong answer")
                    WifiState.offerFailed() // a wrong guess at the pairing code counts
                }
                Auth.TlsVerdict.NOT_PROVISIONED -> Log.w(TAG, "Refused a Wi-Fi connection: not paired")
            }
        } catch (e: ProtocolException) {
            Log.w(TAG, "Refused a Wi-Fi connection: ${e.message}")
        } catch (e: IOException) {
            // The peer went away, the handshake timed out, or it did not speak TLS.
        } finally {
            clients.remove(socket)
            runCatching { socket.close() }
        }
    }

    /** The one thing a computer that proved the pairing code may do: ask to pair. The code is spent by it. */
    private fun serveOffer(input: DataInputStream, out: OutputStream) {
        val len = input.readInt()
        if (len < 0 || len > Link.MAX_CONTROL) throw ProtocolException("bad control message length $len")
        val body = ByteArray(len).also { input.readFully(it) }
        val message = try { JSONObject(String(body, Charsets.UTF_8)) } catch (e: JSONException) { return }
        if (message.optString("type") != "pair") return
        WifiState.closeOffer()
        out.write(encodeControlMessage(pairReply(message)))
        out.flush()
    }

    /** Pairs the computer that sent [message] (`pair`): remembers its secret and answers with `paired`. */
    private fun pairReply(message: JSONObject): String {
        val id = message.optString("device").ifEmpty { Auth.toHex(io.github.oopnm.enki.link.Pairing.newSecret()).take(32) }
        val name = message.optString("name").ifEmpty { id.take(8) }
        val computer = WifiState.pair(id, name)
        // From now on a computer may connect over Wi-Fi, so the listener must be on.
        if (!EnkiSettings.getBool("wifi.enabled")) EnkiSettings.setByUser("wifi.enabled", true)
        return ControlMessages.paired(
            Auth.toHex(computer.secret),
            Auth.toHex(WifiState.identity().fingerprint),
            Build.MODEL ?: "Android",
            EnkiSettings.deviceId,
            Link.WIFI_PORT,
            WifiState.hosts(),
        )
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
                Channel.CONTROL -> serveControl(socket, input, out, wifi = false)
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
    private fun serveControl(socket: Socket, input: DataInputStream, out: OutputStream, wifi: Boolean) {
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
            lastCodec?.let { control.send(ControlMessages.stateString("stream.codec", it)) }
            lastFpsInfo?.let { control.send(ControlMessages.stateString("stream.fps_info", it)) }
            phoneMonitor.latest?.let {
                control.send(ControlMessages.stateInt("phone.battery", it.percent))
                control.send(ControlMessages.stateInt("phone.temperature", it.tempDc))
                control.send(ControlMessages.stateBool("phone.charging", it.charging))
            }
            socket.soTimeout = if (wifi) WIFI_CONTROL_TIMEOUT_MS else CONTROL_TIMEOUT_MS
            while (true) {
                val len = input.readInt()
                if (len < 0 || len > Link.MAX_CONTROL) throw ProtocolException("bad control message length $len")
                val body = ByteArray(len).also { input.readFully(it) }
                val message = try { JSONObject(String(body, Charsets.UTF_8)) } catch (e: JSONException) { continue }
                when (message.optString("type")) {
                    "ping" -> control.send(ControlMessages.pong(message.optLong("seq")))
                    // Only over the cable: adb is what lets the PC ask to be paired without a code.
                    "pair" -> if (!wifi) control.send(pairReply(message))
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
            Log.w(TAG, "No message from the PC for ${socket.soTimeout / 1000} s: stopping the camera")
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
                applyStreamSetting(key)
                if (key == "wifi.enabled") if (value == true) startWifi() else stopWifi()
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
        applyStreamSetting(key)
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
        merge.changed.forEach { applyEffect(control, it.first, it.second) }
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
        /** Wi-Fi stalls longer than a cable does; the PC waits 6 s for the phone and the phone 8 s for the PC. */
        private const val WIFI_CONTROL_TIMEOUT_MS = 8_000
        private const val TAG = "EnkiLink"
    }
}
