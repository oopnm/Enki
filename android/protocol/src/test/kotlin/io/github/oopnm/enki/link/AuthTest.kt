package io.github.oopnm.enki.link

import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.EOFException
import java.io.File
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AuthTest {
    private fun unhex(s: String) = ByteArray(s.length / 2) { s.substring(it * 2, it * 2 + 2).toInt(16).toByte() }

    private class Vector(val name: String, val token: ByteArray, val channel: Channel, val nonce: ByteArray, val mac: ByteArray)

    private fun vectors(): List<Vector> =
        // Gradle runs tests with the module directory as the working directory.
        File("../../protocol/vectors/auth.txt").readLines().filter { it.isNotBlank() && !it.startsWith("#") }.map { line ->
            val f = line.split("|")
            Vector(f[0], unhex(f[1]), Channel.from(f[2].toInt()), unhex(f[3]), unhex(f[4]))
        }

    @Test
    fun sharedVectors() {
        val all = vectors()
        assertTrue(all.size >= 3)
        all.forEach { v ->
            assertArrayEquals(v.name, v.mac, Auth.mac(v.token, v.channel, v.nonce))
            assertTrue(v.name, Auth.verify(v.token, v.channel, v.nonce, v.mac))
        }
    }

    @Test
    fun answerIsBoundToChannelTokenAndNonce() {
        val v = vectors().first()
        assertFalse(Auth.verify(v.token, Channel.MEDIA, v.nonce, v.mac))
        assertFalse(Auth.verify(ByteArray(32) { 9 }, v.channel, v.nonce, v.mac))
        assertFalse(Auth.verify(v.token, v.channel, ByteArray(16) { 9 }, v.mac))
    }

    private fun handshake(v: Vector, answer: ByteArray, token: ByteArray?): Pair<AuthStatus, ByteArray> {
        val out = ByteArrayOutputStream()
        val status = Auth.serverHandshake(ByteArrayInputStream(answer), out, v.channel, token) { v.nonce }
        return status to out.toByteArray()
    }

    @Test
    fun serverHandshakeAcceptsTheRightAnswer() {
        val v = vectors().first()
        val (status, sent) = handshake(v, v.mac, v.token)
        assertEquals(AuthStatus.OK, status)
        assertArrayEquals("challenge, then one verdict byte", v.nonce + byteArrayOf(0), sent)
    }

    @Test
    fun serverHandshakeRefusesWrongAndMissingTokens() {
        val v = vectors().first()
        val (bad, sentBad) = handshake(v, v.mac, ByteArray(32) { 1 })
        assertEquals(AuthStatus.BAD_TOKEN, bad)
        assertEquals(1, sentBad.last().toInt())
        val (none, sentNone) = handshake(v, v.mac, null)
        assertEquals(AuthStatus.NOT_PROVISIONED, none)
        assertEquals(2, sentNone.last().toInt())
        val (garbage, _) = handshake(v, ByteArray(32), v.token)
        assertEquals(AuthStatus.BAD_TOKEN, garbage)
    }

    @Test
    fun serverHandshakeFailsWhenThePeerHangsUp() {
        val v = vectors().first()
        val result = runCatching { handshake(v, v.mac.copyOf(10), v.token) }
        assertTrue(result.exceptionOrNull() is EOFException)
    }

    @Test
    fun challengesAreRandomByDefault() {
        val a = ByteArrayOutputStream().also { runCatching { Auth.serverHandshake(ByteArrayInputStream(ByteArray(0)), it, Channel.CONTROL, null) } }
        val b = ByteArrayOutputStream().also { runCatching { Auth.serverHandshake(ByteArrayInputStream(ByteArray(0)), it, Channel.CONTROL, null) } }
        assertEquals(Auth.NONCE_LEN, a.size())
        assertFalse(a.toByteArray().contentEquals(b.toByteArray()))
    }

    @Test
    fun tokenHexParsing() {
        val hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
        assertArrayEquals(ByteArray(32) { it.toByte() }, Auth.tokenFromHex(hex))
        assertEquals(hex, Auth.toHex(ByteArray(32) { it.toByte() }))
        assertNull(Auth.tokenFromHex(null))
        assertNull(Auth.tokenFromHex(hex.dropLast(2)))
        assertNull(Auth.tokenFromHex("zz" + hex.drop(2)))
    }

    @Test
    fun onlyAdbMayProvision() {
        assertTrue(Auth.isAdbUid(2000))
        assertTrue(Auth.isAdbUid(0))
        assertFalse(Auth.isAdbUid(10_123)) // an ordinary app
        assertFalse(Auth.isAdbUid(1000))   // system uid
    }

    @Test
    fun oldPeerVersionIsReportedWithItsNumber() {
        val old = encodePreamble(Channel.CONTROL).also { it[4] = 0 }
        val e = runCatching { decodePreamble(old) }.exceptionOrNull()
        assertTrue(e is UnsupportedVersionException)
        assertEquals(0, (e as UnsupportedVersionException).peerVersion)
    }
}
