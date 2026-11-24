package io.github.oopnm.enki.link

import java.security.SecureRandom
import javax.crypto.Mac
import javax.crypto.spec.SecretKeySpec

/**
 * Pairing codes (mirrors `pairing` in desktop/crates/enki-protocol). The PC shows a 10-symbol code, the user types it
 * on the phone, and both derive the same one-time key from it. See docs/protocol.md, "Wi-Fi".
 */
object Pairing {
    /** Digits and capitals without I, L, O, U: 32 symbols, 5 bits each. */
    const val ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
    const val CODE_LEN = 10
    private const val ROUNDS = 20_000
    private val SALT = "ENKI-PAIR-1".toByteArray(Charsets.US_ASCII)

    /** Upper case, spaces and dashes dropped, O read as 0 and I or L as 1; null if the result is not exactly 10 symbols. */
    fun normalize(code: String): String? {
        val out = StringBuilder()
        for (raw in code) {
            val c = when (val u = raw.uppercaseChar()) {
                ' ', '-' -> continue
                'O' -> '0'
                'I', 'L' -> '1'
                else -> u
            }
            if (c !in ALPHABET) return null
            out.append(c)
        }
        return if (out.length == CODE_LEN) out.toString() else null
    }

    /** PBKDF2-HMAC-SHA256 of the normalized code (20 000 rounds); null for a malformed code. */
    fun key(code: String): ByteArray? {
        val normalized = normalize(code) ?: return null
        return pbkdf2(normalized.toByteArray(Charsets.US_ASCII), SALT, ROUNDS)
    }

    /** One 32-byte block of PBKDF2 (RFC 8018) with HMAC-SHA256. */
    internal fun pbkdf2(password: ByteArray, salt: ByteArray, rounds: Int): ByteArray {
        val mac = Mac.getInstance("HmacSHA256")
        mac.init(SecretKeySpec(password, "HmacSHA256"))
        mac.update(salt)
        mac.update(byteArrayOf(0, 0, 0, 1))
        var u = mac.doFinal()
        val out = u.copyOf()
        for (i in 1 until rounds) {
            u = mac.doFinal(u)
            for (j in out.indices) out[j] = (out[j].toInt() xor u[j].toInt()).toByte()
        }
        return out
    }

    /** A new secret for a paired computer. */
    fun newSecret(): ByteArray = ByteArray(32).also { SecureRandom().nextBytes(it) }
}
