package io.github.oopnm.enki.link

import java.nio.ByteBuffer
import java.nio.ByteOrder

/** Enki Link wire format, version 0 (mirrors desktop/crates/enki-protocol). See docs/protocol.md. */
object Link {
    const val VERSION = 0
    const val PREAMBLE_LEN = 8
    const val FRAME_HEADER_LEN = 16
    const val MAX_PAYLOAD = 16 * 1024 * 1024L
    val MAGIC = byteArrayOf('E'.code.toByte(), 'N'.code.toByte(), 'K'.code.toByte(), 'I'.code.toByte())
}

enum class Channel(val code: Int) { MEDIA(0), CONTROL(1) }

enum class Kind(val code: Int) {
    VIDEO_CONFIG(0), VIDEO_KEY(1), VIDEO_DELTA(2), AUDIO_CONFIG(3), AUDIO_FRAME(4);

    companion object {
        fun from(code: Int): Kind = entries.firstOrNull { it.code == code }
            ?: throw ProtocolException("unknown frame kind $code")
    }
}

class ProtocolException(message: String) : Exception(message)

fun encodePreamble(channel: Channel): ByteArray {
    val b = ByteArray(Link.PREAMBLE_LEN)
    Link.MAGIC.copyInto(b)
    b[4] = Link.VERSION.toByte()
    b[5] = channel.code.toByte()
    return b
}

data class FrameHeader(val streamId: Int, val kind: Kind, val flags: Int, val len: Long, val ptsUs: Long) {
    fun encode(): ByteArray =
        ByteBuffer.allocate(Link.FRAME_HEADER_LEN).order(ByteOrder.BIG_ENDIAN)
            .put(streamId.toByte()).put(kind.code.toByte()).put(flags.toByte()).put(0)
            .putInt(len.toInt()).putLong(ptsUs).array()

    companion object {
        fun decode(b: ByteArray): FrameHeader {
            if (b.size != Link.FRAME_HEADER_LEN) throw ProtocolException("header must be 16 bytes")
            val buf = ByteBuffer.wrap(b).order(ByteOrder.BIG_ENDIAN)
            val stream = buf.get().toInt() and 0xFF
            val kind = Kind.from(buf.get().toInt() and 0xFF)
            val flags = buf.get().toInt() and 0xFF
            if (buf.get().toInt() != 0) throw ProtocolException("reserved byte must be zero")
            val len = buf.int.toLong() and 0xFFFFFFFFL
            if (len > Link.MAX_PAYLOAD) throw ProtocolException("payload too large: $len")
            return FrameHeader(stream, kind, flags, len, buf.long)
        }
    }
}
