package io.github.oopnm.enki

import android.content.Context
import io.github.oopnm.enki.link.Channel
import io.github.oopnm.enki.link.FrameQueue
import io.github.oopnm.enki.link.Link
import io.github.oopnm.enki.link.MediaFrameWriter
import io.github.oopnm.enki.link.ProtocolException
import io.github.oopnm.enki.link.decodePreamble
import io.github.oopnm.enki.link.encodeControlMessage
import io.github.oopnm.enki.link.encodePreamble
import java.io.BufferedInputStream
import java.io.BufferedOutputStream
import java.io.DataInputStream
import java.io.IOException
import java.io.OutputStream
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.Collections
import java.util.concurrent.atomic.AtomicBoolean
import org.json.JSONException
import org.json.JSONObject

/**
 * Enki Link server (phone side). Listens on loopback only, so the PC reaches it through `adb forward`.
 *
 * The PC opens a connection and sends its 8-byte preamble; we validate it and answer with our own, then
 * serve that channel. The camera runs only while a media connection is open.
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
    private var facing = "back"
    private val mediaLock = Any()
    private var activeMedia: MediaSession? = null

    /** Owns the camera pipeline of one media connection and can swap it for the other camera while streaming. */
    private inner class MediaSession(private val queue: FrameQueue, private val onError: (String) -> Unit) {
        private var streamer: CameraStreamer? = null
        private var closed = false

        fun start() {
            synchronized(mediaLock) {
                if (!closed) streamer = CameraStreamer(context, queue, facing, onError).also { it.start() }
            }
        }

        fun switchCamera() {
            synchronized(mediaLock) {
                if (closed) return
                streamer?.stop()
                streamer = CameraStreamer(context, queue, facing, onError).also { it.start() }
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
        Thread({ acceptLoop(s) }, "enki-accept").start()
        status(WAITING)
    }

    fun stop() {
        if (!stopped.compareAndSet(false, true)) return
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
            socket.soTimeout = 0
            when (channel) {
                Channel.MEDIA -> serveMedia(out)
                Channel.CONTROL -> serveControl(input, out)
            }
        } catch (e: ProtocolException) {
            status("Rejected a connection: ${e.message}")
        } catch (e: IOException) {
            // The peer went away or the handshake timed out; nothing else to do.
        } finally {
            clients.remove(socket)
            runCatching { socket.close() }
        }
    }

    private fun serveMedia(out: OutputStream) {
        if (!mediaBusy.compareAndSet(false, true)) {
            status("A second media connection was refused (one stream at a time)")
            return
        }
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
            mediaBusy.set(false)
            val error = cameraError
            if (!stopped.get()) status(if (error != null) "Camera stopped: $error" else WAITING)
        }
    }

    /** Says hello, then handles `set camera.facing`; other messages are read and ignored (see docs/protocol.md). */
    private fun serveControl(input: DataInputStream, out: OutputStream) {
        val hello = JSONObject()
            .put("type", "hello")
            .put("version", Link.VERSION)
            .put("app", "enki-android")
            .put("app_version", appVersion)
        out.write(encodeControlMessage(hello.toString()))
        out.flush()
        while (true) {
            val len = input.readInt()
            if (len < 0 || len > Link.MAX_CONTROL) throw ProtocolException("bad control message length $len")
            val body = ByteArray(len).also { input.readFully(it) }
            val message = try { JSONObject(String(body, Charsets.UTF_8)) } catch (e: JSONException) { continue }
            if (message.optString("type") == "set" && message.optString("key") == "camera.facing") {
                val value = message.optString("value")
                if (value == "front" || value == "back") {
                    setFacing(value)
                    reply(out, JSONObject().put("type", "state").put("key", "camera.facing").put("value", value))
                } else {
                    reply(out, JSONObject().put("type", "error").put("code", "bad_value")
                        .put("message", "camera.facing must be \"front\" or \"back\""))
                }
            }
        }
    }

    private fun reply(out: OutputStream, json: JSONObject) {
        out.write(encodeControlMessage(json.toString()))
        out.flush()
    }

    companion object {
        const val WAITING = "Ready. Waiting for the PC to connect"
        const val STREAMING = "Streaming camera to the PC"
        private const val QUEUE_FRAMES = 30
        private const val HANDSHAKE_TIMEOUT_MS = 5_000
    }
}
