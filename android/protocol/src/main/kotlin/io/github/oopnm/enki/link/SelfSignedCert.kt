package io.github.oopnm.enki.link

import java.io.ByteArrayInputStream
import java.math.BigInteger
import java.security.KeyFactory
import java.security.KeyPair
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.MessageDigest
import java.security.PrivateKey
import java.security.SecureRandom
import java.security.Signature
import java.security.cert.CertificateFactory
import java.security.cert.X509Certificate
import java.security.spec.ECGenParameterSpec
import java.security.spec.PKCS8EncodedKeySpec

/**
 * The phone's own TLS identity: an EC P-256 key and a self-signed certificate made here, with a small DER writer.
 * (The JDK has no public API for building a certificate and Android's key store makes them in a vendor-specific
 * way; a plain software key is the same on every phone and testable on a PC.) The PC pins the certificate's
 * SHA-256 when it pairs, so the certificate needs no authority and no valid host name.
 */
class PhoneIdentity(val privateKeyPkcs8: ByteArray, val certificateDer: ByteArray) {
    val fingerprint: ByteArray get() = MessageDigest.getInstance("SHA-256").digest(certificateDer)

    /** A key store holding the identity, for `KeyManagerFactory`. */
    fun keyStore(): KeyStore {
        val key: PrivateKey = KeyFactory.getInstance("EC").generatePrivate(PKCS8EncodedKeySpec(privateKeyPkcs8))
        val cert = CertificateFactory.getInstance("X.509").generateCertificate(ByteArrayInputStream(certificateDer)) as X509Certificate
        return KeyStore.getInstance("PKCS12").apply {
            load(null, null)
            setKeyEntry("enki", key, PASSWORD, arrayOf(cert))
        }
    }

    companion object {
        val PASSWORD = "enki".toCharArray()

        fun generate(commonName: String = "Enki phone"): PhoneIdentity {
            val pair: KeyPair = KeyPairGenerator.getInstance("EC").run {
                initialize(ECGenParameterSpec("secp256r1"), SecureRandom())
                generateKeyPair()
            }
            val der = SelfSignedCert.build(pair, commonName)
            return PhoneIdentity(pair.private.encoded, der)
        }
    }
}

internal object SelfSignedCert {
    // OIDs, DER-encoded without the tag and length.
    private val OID_ECDSA_SHA256 = byteArrayOf(0x2A, 0x86.toByte(), 0x48, 0xCE.toByte(), 0x3D, 0x04, 0x03, 0x02)
    private val OID_COMMON_NAME = byteArrayOf(0x55, 0x04, 0x03)
    private val OID_BASIC_CONSTRAINTS = byteArrayOf(0x55, 0x1D, 0x13)
    private val OID_KEY_USAGE = byteArrayOf(0x55, 0x1D, 0x0F)

    fun build(pair: KeyPair, commonName: String): ByteArray {
        val signatureAlgorithm = seq(oid(OID_ECDSA_SHA256))
        val name = seq(set(seq(oid(OID_COMMON_NAME), tlv(0x0C, commonName.toByteArray(Charsets.UTF_8)))))
        val serial = ByteArray(16).also { SecureRandom().nextBytes(it) }
        serial[0] = (serial[0].toInt() and 0x7F or 0x40).toByte() // positive, no redundant leading zero
        val validity = seq(tlv(0x17, "200101000000Z".toByteArray()), tlv(0x17, "491231235959Z".toByteArray()))
        val extensions = tlv(
            0xA3,
            seq(
                // basicConstraints: not a CA.
                seq(oid(OID_BASIC_CONSTRAINTS), tlv(0x01, byteArrayOf(0xFF.toByte())), tlv(0x04, seq())),
                // keyUsage: digitalSignature (what an ECDHE-ECDSA handshake needs).
                seq(oid(OID_KEY_USAGE), tlv(0x01, byteArrayOf(0xFF.toByte())), tlv(0x04, tlv(0x03, byteArrayOf(0x07, 0x80.toByte())))),
            ),
        )
        val tbs = seq(
            tlv(0xA0, tlv(0x02, byteArrayOf(2))), // version 3
            tlv(0x02, serial),
            signatureAlgorithm,
            name,
            validity,
            name,
            pair.public.encoded, // SubjectPublicKeyInfo as the JDK writes it
            extensions,
        )
        val signature = Signature.getInstance("SHA256withECDSA").run {
            initSign(pair.private)
            update(tbs)
            sign() // already DER (SEQUENCE of r and s)
        }
        return seq(tbs, signatureAlgorithm, tlv(0x03, byteArrayOf(0) + signature))
    }

    private fun length(n: Int): ByteArray = when {
        n < 0x80 -> byteArrayOf(n.toByte())
        n < 0x100 -> byteArrayOf(0x81.toByte(), n.toByte())
        else -> byteArrayOf(0x82.toByte(), (n shr 8).toByte(), n.toByte())
    }

    private fun tlv(tag: Int, content: ByteArray): ByteArray = byteArrayOf(tag.toByte()) + length(content.size) + content
    private fun seq(vararg parts: ByteArray): ByteArray = tlv(0x30, parts.fold(ByteArray(0)) { a, b -> a + b })
    private fun set(vararg parts: ByteArray): ByteArray = tlv(0x31, parts.fold(ByteArray(0)) { a, b -> a + b })
    private fun oid(bytes: ByteArray): ByteArray = tlv(0x06, bytes)
}
