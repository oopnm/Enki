package io.github.oopnm.enki.link

import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.PipedInputStream
import java.io.PipedOutputStream
import java.net.InetAddress
import java.security.MessageDigest
import java.security.cert.CertificateFactory
import java.security.cert.X509Certificate
import javax.net.ssl.KeyManagerFactory
import javax.net.ssl.SSLContext
import javax.net.ssl.SSLSocket
import javax.net.ssl.TrustManager
import javax.net.ssl.X509TrustManager
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class WifiTest {
    private fun unhex(s: String) = ByteArray(s.length / 2) { s.substring(it * 2, it * 2 + 2).toInt(16).toByte() }
    private fun lines(path: String) = File(path).readLines().filter { it.isNotBlank() && !it.startsWith("#") }.map { it.split("|") }

    @Test
    fun sharedTlsAuthVectors() {
        val all = lines("../../protocol/vectors/auth_tls.txt")
        assertTrue(all.size >= 2)
        for (f in all) {
            val mac = Auth.macTls(unhex(f[1]), Channel.from(f[2].toInt()), unhex(f[3]), unhex(f[4]))
            assertArrayEquals(f[0], unhex(f[5]), mac)
        }
    }

    @Test
    fun sharedPairingVectors() {
        val all = lines("../../protocol/vectors/pairing.txt")
        assertTrue(all.size >= 4)
        for (f in all) {
            assertEquals(f[0], f[1], Pairing.normalize(f[0]))
            assertArrayEquals(f[0], unhex(f[2]), Pairing.key(f[0]))
        }
    }

    @Test
    fun codesAreChecked() {
        assertNull(Pairing.normalize("ABCDE-FGHJ"))
        assertNull(Pairing.normalize("ABCDE-FGHJKM"))
        assertNull(Pairing.normalize("ABCDE-FGHJU"))
        assertEquals("ABCDEFGHJK", Pairing.normalize("abcde fghjk"))
        assertNull(Pairing.key("short"))
    }

    @Test
    fun pairedMessageMatchesTheSharedVector() {
        val vector = File("../../protocol/vectors/control.txt").readLines().first { it.startsWith("paired|") }.substringAfter("|")
        val secret = (0 until 32).joinToString("") { "%02x".format(it) }
        val fp = (100 until 132).joinToString("") { "%02x".format(it) }
        assertEquals(
            vector,
            ControlMessages.paired(secret, fp, "Pixel 6", "0123456789abcdef0123456789abcdef", 28201, listOf("192.168.1.20", "10.0.0.5")),
        )
    }

    // --- the handshake on a TLS connection

    private val fp = ByteArray(32) { (it + 1).toByte() }
    private val secret = ByteArray(32) { 5 }
    private val offer = ByteArray(32) { 6 }

    private fun run(answerKey: ByteArray?, paired: List<ByteArray>, offerKey: ByteArray?, seenFp: ByteArray = fp): Pair<Auth.TlsVerdict, Int> {
        val nonce = ByteArray(16) { 9 }
        val answer = if (answerKey == null) ByteArray(32) else Auth.macTls(answerKey, Channel.CONTROL, seenFp, nonce)
        val out = ByteArrayOutputStream()
        val verdict = Auth.serverHandshakeTls(ByteArrayInputStream(answer), out, Channel.CONTROL, fp, { paired }, { offerKey }) { nonce }
        val bytes = out.toByteArray()
        return verdict to bytes.last().toInt()
    }

    @Test
    fun tlsHandshakeClassifiesTheKey() {
        assertEquals(Auth.TlsVerdict.PAIRED to 0, run(secret, listOf(secret), null))
        assertEquals(Auth.TlsVerdict.OFFER to 0, run(offer, listOf(secret), offer))
        assertEquals(Auth.TlsVerdict.BAD_TOKEN to 1, run(ByteArray(32) { 7 }, listOf(secret), offer))
        // No code open and a stranger's answer: nothing to compare with.
        assertEquals(Auth.TlsVerdict.NOT_PROVISIONED to 2, run(offer, listOf(secret), null))
        // The PC was shown another certificate (a machine in the middle): even the right key fails.
        assertEquals(Auth.TlsVerdict.BAD_TOKEN to 1, run(offer, listOf(secret), offer, seenFp = ByteArray(32) { 1 }))
        assertEquals(Auth.TlsVerdict.NOT_PROVISIONED to 2, run(secret, emptyList(), null))
    }

    // --- the certificate

    @Test
    fun theCertificateIsAValidSelfSignedOne() {
        val id = PhoneIdentity.generate()
        val cert = CertificateFactory.getInstance("X.509").generateCertificate(ByteArrayInputStream(id.certificateDer)) as X509Certificate
        cert.verify(cert.publicKey) // signed by its own key
        assertEquals(3, cert.version)
        assertEquals(-1, cert.basicConstraints)
        assertEquals("EC", cert.publicKey.algorithm)
        assertArrayEquals(MessageDigest.getInstance("SHA-256").digest(id.certificateDer), id.fingerprint)
        assertTrue(id.keyStore().isKeyEntry("enki"))
    }

    /** A real TLS handshake in the JVM: a client that pins the fingerprint talks to a server made from the identity. */
    @Test
    fun tlsWorksWithThePinnedCertificate() {
        val id = PhoneIdentity.generate()
        val kmf = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm()).apply { init(id.keyStore(), PhoneIdentity.PASSWORD) }
        val serverContext = SSLContext.getInstance("TLS").apply { init(kmf.keyManagers, null, null) }
        val server = serverContext.serverSocketFactory.createServerSocket(0, 1, InetAddress.getLoopbackAddress())
        val result = java.util.concurrent.atomic.AtomicReference<String>()
        val thread = Thread {
            runCatching {
                val s = server.accept() as SSLSocket
                s.startHandshake()
                s.outputStream.write("hello".toByteArray()); s.outputStream.flush(); s.close()
            }.onFailure { result.set("server: $it") }
        }
        thread.start()
        fun client(pin: ByteArray): String {
            val trust = object : X509TrustManager {
                override fun checkClientTrusted(chain: Array<X509Certificate>, authType: String) {}
                override fun checkServerTrusted(chain: Array<X509Certificate>, authType: String) {
                    val actual = MessageDigest.getInstance("SHA-256").digest(chain[0].encoded)
                    if (!MessageDigest.isEqual(actual, pin)) throw java.security.cert.CertificateException("pin mismatch")
                }
                override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()
            }
            val ctx = SSLContext.getInstance("TLS").apply { init(null, arrayOf<TrustManager>(trust), null) }
            return ctx.socketFactory.createSocket(InetAddress.getLoopbackAddress(), server.localPort).use { sock ->
                val s = sock as SSLSocket
                s.startHandshake()
                String(s.inputStream.readNBytes(5))
            }
        }
        assertEquals("hello", client(id.fingerprint))
        thread.join(5000)
        assertNull(result.get())
        server.close()
    }

    /** Writes the certificate the Rust side parses (protocol/vectors/phone_cert.txt): `ENKI_REGEN_CERT=1 gradle :protocol:test`. */
    @Test
    fun regenerateCertificateVector() {
        if (System.getenv("ENKI_REGEN_CERT") == null) return
        val id = PhoneIdentity.generate()
        File("../../protocol/vectors/phone_cert.txt").writeText(
            "# A certificate made by the phone app's PhoneIdentity (Kotlin). The Rust tests check that rustls/webpki accept it.\n" +
                "# certificate DER (hex) | SHA-256 fingerprint (hex)\n" +
                Auth.toHex(id.certificateDer) + "|" + Auth.toHex(id.fingerprint) + "\n",
        )
    }

    @Test
    fun theCommittedCertificateVectorIsConsistent() {
        val f = lines("../../protocol/vectors/phone_cert.txt").firstOrNull() ?: return
        val der = unhex(f[0])
        val cert = CertificateFactory.getInstance("X.509").generateCertificate(ByteArrayInputStream(der)) as X509Certificate
        cert.verify(cert.publicKey)
        assertEquals(f[1], Auth.toHex(MessageDigest.getInstance("SHA-256").digest(der)))
    }
}
