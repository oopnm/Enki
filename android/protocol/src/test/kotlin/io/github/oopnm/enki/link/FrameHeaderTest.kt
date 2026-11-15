package io.github.oopnm.enki.link

import java.io.File
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class FrameHeaderTest {
    private fun unhex(s: String) = ByteArray(s.length / 2) { s.substring(it * 2, it * 2 + 2).toInt(16).toByte() }

    @Test
    fun sharedVectors() {
        // Gradle runs tests with the module directory as the working directory.
        val file = File("../../protocol/vectors/frame_header.txt")
        var n = 0
        file.readLines().filter { it.isNotBlank() && !it.startsWith("#") }.forEach { line ->
            val f = line.split("|")
            val h = FrameHeader(f[1].toInt(), Kind.from(f[2].toInt()), f[3].toInt(), f[4].toLong(), f[5].toLong())
            val bytes = unhex(f[6])
            assertArrayEquals(f[0], bytes, h.encode())
            assertEquals(f[0], h, FrameHeader.decode(bytes))
            n++
        }
        assertTrue(n >= 3)
    }

    @Test
    fun preambleBytes() {
        assertArrayEquals("ENKI".toByteArray() + byteArrayOf(0, 0, 0, 0), encodePreamble(Channel.MEDIA))
        assertEquals(1, encodePreamble(Channel.CONTROL)[5].toInt())
    }
}
