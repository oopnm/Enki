package io.github.oopnm.enki.link

import java.io.ByteArrayOutputStream
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class MediaTest {
    private fun frame(kind: Kind, tag: Int, pts: Long = tag.toLong()) =
        EncodedFrame(kind, byteArrayOf(tag.toByte()), pts)

    @Test
    fun writerEmitsPreambleHeaderAndPayload() {
        val out = ByteArrayOutputStream()
        val w = MediaFrameWriter(out)
        w.writePreamble()
        w.write(EncodedFrame(Kind.VIDEO_KEY, byteArrayOf(1, 2, 3), 33_333, FLAG_DISCONTINUITY))
        val bytes = out.toByteArray()
        assertEquals(Channel.MEDIA, decodePreamble(bytes.copyOfRange(0, 8)))
        val h = FrameHeader.decode(bytes.copyOfRange(8, 24))
        assertEquals(FrameHeader(0, Kind.VIDEO_KEY, FLAG_DISCONTINUITY, 3, 33_333), h)
        assertArrayEquals(byteArrayOf(1, 2, 3), bytes.copyOfRange(24, 27))
        assertEquals(27, bytes.size)
    }

    @Test
    fun preambleValidation() {
        assertEquals(Channel.CONTROL, decodePreamble(encodePreamble(Channel.CONTROL)))
        val badMagic = encodePreamble(Channel.MEDIA).also { it[0] = 'X'.code.toByte() }
        assertTrue(runCatching { decodePreamble(badMagic) }.exceptionOrNull() is ProtocolException)
        val badVersion = encodePreamble(Channel.MEDIA).also { it[4] = 9 }
        assertTrue(runCatching { decodePreamble(badVersion) }.exceptionOrNull() is ProtocolException)
        val badChannel = encodePreamble(Channel.MEDIA).also { it[5] = 7 }
        assertTrue(runCatching { decodePreamble(badChannel) }.exceptionOrNull() is ProtocolException)
    }

    @Test
    fun controlMessageIsLengthPrefixed() {
        val b = encodeControlMessage("""{"type":"hello"}""")
        assertEquals(15, b.size - 4)
        assertArrayEquals(byteArrayOf(0, 0, 0, 15), b.copyOfRange(0, 4))
    }

    @Test
    fun queueKeepsOrder() {
        val q = FrameQueue(8)
        q.offerKey(frame(Kind.VIDEO_CONFIG, 0), frame(Kind.VIDEO_KEY, 1))
        assertFalse(q.offerDelta(frame(Kind.VIDEO_DELTA, 2)))
        q.close()
        assertEquals(Kind.VIDEO_CONFIG, q.take()!!.kind)
        assertEquals(Kind.VIDEO_KEY, q.take()!!.kind)
        assertEquals(Kind.VIDEO_DELTA, q.take()!!.kind)
        assertNull(q.take())
    }

    @Test
    fun overflowDropsDeltasUntilKeyThenFlagsDiscontinuity() {
        val q = FrameQueue(4)
        q.offerKey(frame(Kind.VIDEO_CONFIG, 0), frame(Kind.VIDEO_KEY, 1))
        assertFalse(q.offerDelta(frame(Kind.VIDEO_DELTA, 2)))
        assertFalse(q.offerDelta(frame(Kind.VIDEO_DELTA, 3)))
        // queue full: first drop asks for a key frame, later drops stay quiet
        assertTrue(q.offerDelta(frame(Kind.VIDEO_DELTA, 4)))
        assertFalse(q.offerDelta(frame(Kind.VIDEO_DELTA, 5)))
        // the next key frame replaces the stale queue
        q.offerKey(frame(Kind.VIDEO_CONFIG, 6), frame(Kind.VIDEO_KEY, 7))
        q.close()
        val config = q.take()!!
        assertEquals(Kind.VIDEO_CONFIG, config.kind)
        assertEquals(FLAG_DISCONTINUITY, config.flags)
        val key = q.take()!!
        assertEquals(Kind.VIDEO_KEY, key.kind)
        assertEquals(0, key.flags)
        assertNull(q.take())
    }

    @Test
    fun keyWithoutConfigCarriesDiscontinuityFlag() {
        val q = FrameQueue(2)
        q.offerKey(null, frame(Kind.VIDEO_KEY, 1))
        q.offerKey(null, frame(Kind.VIDEO_KEY, 2)) // 1 + 2 > 2 -> gap
        q.close()
        assertEquals(FLAG_DISCONTINUITY, q.take()!!.flags)
        assertNull(q.take())
    }

    @Test
    fun takeUnblocksOnClose() {
        val q = FrameQueue(4)
        var result: EncodedFrame? = frame(Kind.VIDEO_KEY, 0)
        val t = Thread { result = q.take() }
        t.start()
        Thread.sleep(50)
        q.close()
        t.join(2000)
        assertNull(result)
    }
}
