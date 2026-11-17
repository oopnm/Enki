package io.github.oopnm.enki.link

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class FrameQueueDropTest {
    private fun delta(pts: Long) = EncodedFrame(Kind.VIDEO_DELTA, byteArrayOf(1), pts)

    private fun key(pts: Long) = EncodedFrame(Kind.VIDEO_KEY, byteArrayOf(2), pts)

    @Test
    fun framesDroppedWhileTheReaderIsSlowAreCounted() {
        val q = FrameQueue(3)
        assertEquals(0, q.droppedFrames())
        assertFalse(q.offerDelta(delta(1)))
        assertFalse(q.offerDelta(delta(2)))
        assertFalse(q.offerDelta(delta(3)))
        assertTrue("queue full: caller must ask for a key frame", q.offerDelta(delta(4)))
        assertEquals(1, q.droppedFrames())
        assertFalse(q.offerDelta(delta(5)))
        assertFalse(q.offerDelta(delta(6)))
        assertEquals(3, q.droppedFrames())
        q.offerKey(null, key(7)) // clears the 3 stale frames still queued
        assertEquals(6, q.droppedFrames())
    }

    @Test
    fun aKeyFrameThatFitsDropsNothing() {
        val q = FrameQueue(4)
        q.offerDelta(delta(1))
        q.offerKey(null, key(2))
        assertEquals(0, q.droppedFrames())
    }
}
