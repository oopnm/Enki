package io.github.oopnm.enki.link

import java.io.InputStream
import java.io.OutputStream
import java.security.MessageDigest
import java.security.SecureRandom
import javax.crypto.Mac
import javax.crypto.spec.SecretKeySpec

/** The phone's verdict on a connection's answer to the challenge (one byte on the wire). */
enum class AuthStatus(val code: Int) {
    OK(0),
    /** The answer did not match the token. */
    BAD_TOKEN(1),
    /** The phone has no token yet (the PC has not provisioned it since the app started). */
    NOT_PROVISIONED(2),
}

/**
 * Session authentication (mirrors `auth_mac` in desktop/crates/enki-protocol). The PC hands the phone a random
 * session token through adb, which only the adb shell can do, and then proves it holds the token on every
 * connection by answering a fresh random challenge. Any other app on the phone can reach the loopback port,
 * but cannot answer. See docs/protocol.md.
 */
object Auth {
    const val TOKEN_LEN = 32
    const val NONCE_LEN = 16
    const val MAC_LEN = 32
    private const val ROOT_UID = 0
    private const val SHELL_UID = 2000
    private val CONTEXT = "ENKI-AUTH-1".toByteArray(Charsets.US_ASCII)

    /** `HMAC-SHA256(token, "ENKI-AUTH-1" || channel || nonce)`. The channel is included so an answer cannot be replayed on the other channel. */
    fun mac(token: ByteArray, channel: Channel, nonce: ByteArray): ByteArray {
        require(token.size == TOKEN_LEN) { "token must be $TOKEN_LEN bytes" }
        require(nonce.size == NONCE_LEN) { "nonce must be $NONCE_LEN bytes" }
        val hmac = Mac.getInstance("HmacSHA256")
        hmac.init(SecretKeySpec(token, "HmacSHA256"))
        hmac.update(CONTEXT)
        hmac.update(channel.code.toByte())
        hmac.update(nonce)
        return hmac.doFinal()
    }

    /** Constant-time comparison. */
    fun verify(token: ByteArray, channel: Channel, nonce: ByteArray, answer: ByteArray): Boolean =
        MessageDigest.isEqual(mac(token, channel, nonce), answer)

    /** Only the adb shell (or root) may give the phone a session token; ordinary apps have other uids. */
    fun isAdbUid(uid: Int): Boolean = uid == SHELL_UID || uid == ROOT_UID

    fun tokenFromHex(hex: String?): ByteArray? {
        if (hex == null || hex.length != TOKEN_LEN * 2) return null
        return ByteArray(TOKEN_LEN) { i ->
            val pair = hex.substring(i * 2, i * 2 + 2)
            if (!pair.all { it in '0'..'9' || it in 'a'..'f' || it in 'A'..'F' }) return null
            pair.toInt(16).toByte()
        }
    }

    fun toHex(bytes: ByteArray): String = bytes.joinToString("") { "%02x".format(it) }

    /**
     * Phone side of the exchange that follows the preambles: send a random challenge, read the PC's answer,
     * send the verdict, and return it. Nothing is served on a connection unless this returns [AuthStatus.OK].
     * [token] is null while the phone has not been provisioned; the answer is still read so the PC gets a clear verdict.
     */
    fun serverHandshake(
        input: InputStream,
        out: OutputStream,
        channel: Channel,
        token: ByteArray?,
        newNonce: () -> ByteArray = { ByteArray(NONCE_LEN).also { SecureRandom().nextBytes(it) } },
    ): AuthStatus {
        val nonce = newNonce()
        out.write(nonce)
        out.flush()
        val answer = ByteArray(MAC_LEN)
        var read = 0
        while (read < MAC_LEN) {
            val n = input.read(answer, read, MAC_LEN - read)
            if (n < 0) throw java.io.EOFException("connection closed during authentication")
            read += n
        }
        val status = when {
            token == null -> AuthStatus.NOT_PROVISIONED
            verify(token, channel, nonce, answer) -> AuthStatus.OK
            else -> AuthStatus.BAD_TOKEN
        }
        out.write(status.code)
        out.flush()
        return status
    }

    private val TLS_CONTEXT = "ENKI-AUTH-2".toByteArray(Charsets.US_ASCII)
    const val FINGERPRINT_LEN = 32

    /**
     * The answer on a TLS connection (mirrors `auth_mac_tls` in enki-protocol):
     * `HMAC-SHA256(key, "ENKI-AUTH-2" || channel || fingerprint || nonce)`. The fingerprint is the one of OUR
     * certificate, so a PC that was shown another certificate (someone in the middle) answers wrongly.
     */
    fun macTls(key: ByteArray, channel: Channel, fingerprint: ByteArray, nonce: ByteArray): ByteArray {
        require(key.size == 32) { "key must be 32 bytes" }
        require(fingerprint.size == FINGERPRINT_LEN) { "fingerprint must be $FINGERPRINT_LEN bytes" }
        require(nonce.size == NONCE_LEN) { "nonce must be $NONCE_LEN bytes" }
        val hmac = Mac.getInstance("HmacSHA256")
        hmac.init(SecretKeySpec(key, "HmacSHA256"))
        hmac.update(TLS_CONTEXT)
        hmac.update(channel.code.toByte())
        hmac.update(fingerprint)
        hmac.update(nonce)
        return hmac.doFinal()
    }

    /** What a Wi-Fi connection proved. Only [PAIRED] may do everything; [OFFER] may only pair. */
    enum class TlsVerdict { PAIRED, OFFER, BAD_TOKEN, NOT_PROVISIONED }

    /** Decides from the PC's answer which key it used (constant time per key). [offer] is the pairing key, if a code is open. */
    fun classifyTls(
        answer: ByteArray,
        channel: Channel,
        fingerprint: ByteArray,
        nonce: ByteArray,
        paired: List<ByteArray>,
        offer: ByteArray?,
    ): TlsVerdict {
        var matched = false
        for (key in paired) {
            if (MessageDigest.isEqual(macTls(key, channel, fingerprint, nonce), answer)) matched = true
        }
        if (matched) return TlsVerdict.PAIRED
        if (offer == null) return TlsVerdict.NOT_PROVISIONED
        return if (MessageDigest.isEqual(macTls(offer, channel, fingerprint, nonce), answer)) TlsVerdict.OFFER else TlsVerdict.BAD_TOKEN
    }

    /**
     * Phone side of the exchange on a TLS connection: like [serverHandshake], but the answer is checked against every
     * paired secret and the open pairing key, and the caller learns which one matched. [offer] is looked up after the
     * answer arrives so a code typed meanwhile counts. Writes the one-byte verdict (OK for PAIRED and OFFER).
     */
    fun serverHandshakeTls(
        input: InputStream,
        out: OutputStream,
        channel: Channel,
        fingerprint: ByteArray,
        paired: () -> List<ByteArray>,
        offer: () -> ByteArray?,
        newNonce: () -> ByteArray = { ByteArray(NONCE_LEN).also { SecureRandom().nextBytes(it) } },
    ): TlsVerdict {
        val nonce = newNonce()
        out.write(nonce)
        out.flush()
        val answer = ByteArray(MAC_LEN)
        var read = 0
        while (read < MAC_LEN) {
            val n = input.read(answer, read, MAC_LEN - read)
            if (n < 0) throw java.io.EOFException("connection closed during authentication")
            read += n
        }
        val verdict = classifyTls(answer, channel, fingerprint, nonce, paired(), offer())
        val status = when (verdict) {
            TlsVerdict.PAIRED, TlsVerdict.OFFER -> AuthStatus.OK
            TlsVerdict.BAD_TOKEN -> AuthStatus.BAD_TOKEN
            TlsVerdict.NOT_PROVISIONED -> AuthStatus.NOT_PROVISIONED
        }
        out.write(status.code)
        out.flush()
        return verdict
    }
}
