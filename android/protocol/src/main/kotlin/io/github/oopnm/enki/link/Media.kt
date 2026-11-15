package io.github.oopnm.enki.link

import java.io.OutputStream
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.locks.ReentrantLock
import kotlin.concurrent.withLock

/** One media payload ready to be sent as an Enki Link frame. */
class EncodedFrame(val kind: Kind, val data: ByteArray, val ptsUs: Long, val flags: Int = 0) {
    fun withFlags(extra: Int) = EncodedFrame(kind, data, ptsUs, flags or extra)
}

/** Writes the preamble and media frames (16-byte header + payload) to a stream. Use from one thread only. */
class MediaFrameWriter(private val out: OutputStream, private val streamId: Int = 0) {
    fun writePreamble() {
        out.write(encodePreamble(Channel.MEDIA))
        out.flush()
    }

    fun write(frame: EncodedFrame) {
        if (frame.data.size > Link.MAX_PAYLOAD) throw ProtocolException("payload too large: ${frame.data.size}")
        out.write(FrameHeader(streamId, frame.kind, frame.flags, frame.data.size.toLong(), frame.ptsUs).encode())
        out.write(frame.data)
        out.flush()
    }
}

/** A control message on the wire: u32 big-endian length followed by UTF-8 JSON. */
fun encodeControlMessage(json: String): ByteArray {
    val body = json.toByteArray(Charsets.UTF_8)
    if (body.size > Link.MAX_CONTROL) throw ProtocolException("control message too large: ${body.size}")
    return ByteBuffer.allocate(4 + body.size).order(ByteOrder.BIG_ENDIAN).putInt(body.size).put(body).array()
}

/**
 * Bounded hand-off between the encoder thread (producer) and the network thread (consumer).
 *
 * Latency matters more than completeness for a webcam, so when the consumer falls behind we never block
 * the encoder: delta frames are dropped until the next key frame, which clears whatever was still queued
 * and is sent with the discontinuity flag. Each key frame is preceded by the video config (SPS/PPS).
 */
class FrameQueue(private val capacity: Int) {
    private val items = ArrayDeque<EncodedFrame>()
    private val lock = ReentrantLock()
    private val notEmpty = lock.newCondition()
    private var waitingForKey = false
    private var closed = false

    init {
        require(capacity >= 2) { "capacity must hold a config + key frame" }
    }

    /**
     * Queues a delta frame. Returns true exactly when this call started dropping frames, i.e. the caller
     * should ask the encoder for a new key frame.
     */
    fun offerDelta(frame: EncodedFrame): Boolean {
        lock.withLock {
            if (closed || waitingForKey) return false
            if (items.size >= capacity) {
                waitingForKey = true
                return true
            }
            items.addLast(frame)
            notEmpty.signal()
            return false
        }
    }

    /** Queues [config] (may be null) followed by the key frame, discarding stale data if the queue is behind. */
    fun offerKey(config: EncodedFrame?, key: EncodedFrame) {
        lock.withLock {
            if (closed) return
            val gap = waitingForKey || items.size + 2 > capacity
            if (gap) items.clear()
            val flag = if (gap) FLAG_DISCONTINUITY else 0
            if (config != null) {
                items.addLast(config.withFlags(flag))
                items.addLast(key)
            } else {
                items.addLast(key.withFlags(flag))
            }
            waitingForKey = false
            notEmpty.signal()
        }
    }

    /** Blocks until a frame is available. Returns null once the queue is closed and empty. */
    fun take(): EncodedFrame? {
        lock.withLock {
            while (items.isEmpty()) {
                if (closed) return null
                notEmpty.await()
            }
            return items.removeFirst()
        }
    }

    fun close() {
        lock.withLock {
            closed = true
            notEmpty.signalAll()
        }
    }
}
