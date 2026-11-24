package io.github.oopnm.enki

import android.content.Context
import android.content.SharedPreferences
import android.util.Base64
import io.github.oopnm.enki.link.Auth
import io.github.oopnm.enki.link.Pairing
import io.github.oopnm.enki.link.PhoneIdentity
import java.net.Inet4Address
import java.net.NetworkInterface
import java.util.Collections
import java.util.concurrent.CopyOnWriteArrayList
import org.json.JSONArray
import org.json.JSONObject

/**
 * What the phone keeps for Wi-Fi: its TLS identity (a key and a self-signed certificate, made once), the computers it
 * is paired with (each with the secret that proves it), and the pairing code that is open right now. Stored in the
 * app's private storage; the PC pins the certificate's fingerprint when it pairs.
 */
object WifiState {
    private const val PREFS = "enki_wifi"
    private const val KEY_IDENTITY_KEY = "identity_key"
    private const val KEY_IDENTITY_CERT = "identity_cert"
    private const val KEY_COMPUTERS = "computers"

    /** How long a typed pairing code works, and how many wrong answers close it early. */
    const val OFFER_SECONDS = 180L
    private const val MAX_FAILURES = 5

    class Computer(val id: String, val name: String, val secret: ByteArray)

    private lateinit var prefs: SharedPreferences
    @Volatile private var ready = false
    @Volatile private var identity: PhoneIdentity? = null

    /** Called after the list of computers or the open code changes, so the screen can redraw. */
    val changeListeners = CopyOnWriteArrayList<() -> Unit>()

    @Volatile private var offerKey: ByteArray? = null
    @Volatile private var offerUntil = 0L
    @Volatile private var failures = 0

    @Synchronized
    fun init(appContext: Context) {
        if (ready) return
        prefs = appContext.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        ready = true
    }

    /** The phone's TLS identity, made on first use. */
    @Synchronized
    fun identity(): PhoneIdentity {
        identity?.let { return it }
        val key = prefs.getString(KEY_IDENTITY_KEY, null)
        val cert = prefs.getString(KEY_IDENTITY_CERT, null)
        val loaded = if (key != null && cert != null) {
            runCatching { PhoneIdentity(Base64.decode(key, Base64.NO_WRAP), Base64.decode(cert, Base64.NO_WRAP)).also { it.keyStore() } }.getOrNull()
        } else {
            null
        }
        val result = loaded ?: PhoneIdentity.generate().also {
            prefs.edit()
                .putString(KEY_IDENTITY_KEY, Base64.encodeToString(it.privateKeyPkcs8, Base64.NO_WRAP))
                .putString(KEY_IDENTITY_CERT, Base64.encodeToString(it.certificateDer, Base64.NO_WRAP))
                .apply()
        }
        identity = result
        return result
    }

    @Synchronized
    fun computers(): List<Computer> {
        val text = prefs.getString(KEY_COMPUTERS, null) ?: return emptyList()
        return runCatching {
            val array = JSONArray(text)
            (0 until array.length()).mapNotNull { i ->
                val o = array.getJSONObject(i)
                val secret = Auth.tokenFromHex(o.getString("secret")) ?: return@mapNotNull null
                Computer(o.getString("id"), o.optString("name"), secret)
            }
        }.getOrDefault(emptyList())
    }

    fun secrets(): List<ByteArray> = computers().map { it.secret }

    private fun save(list: List<Computer>) {
        val array = JSONArray()
        for (c in list) array.put(JSONObject().put("id", c.id).put("name", c.name).put("secret", Auth.toHex(c.secret)))
        prefs.edit().putString(KEY_COMPUTERS, array.toString()).apply()
        changeListeners.forEach { it() }
    }

    /** Pairs a computer: a new secret for it, replacing an older pairing with the same id. */
    @Synchronized
    fun pair(id: String, name: String): Computer {
        val computer = Computer(id, name, Pairing.newSecret())
        save(computers().filter { it.id != id } + computer)
        return computer
    }

    @Synchronized
    fun forget(id: String) = save(computers().filter { it.id != id })

    // ---- the pairing code typed on this phone

    /** Opens pairing for [OFFER_SECONDS] with the code the computer shows. False if it is not a valid code. */
    fun openOffer(code: String): Boolean {
        val key = Pairing.key(code) ?: return false
        failures = 0
        offerUntil = System.currentTimeMillis() + OFFER_SECONDS * 1000
        offerKey = key
        changeListeners.forEach { it() }
        return true
    }

    /** The key of the open code, or null if none is open, it ran out, or too many wrong answers closed it. */
    fun offer(): ByteArray? {
        val key = offerKey ?: return null
        if (System.currentTimeMillis() > offerUntil) {
            offerKey = null
            return null
        }
        return key
    }

    /** A wrong answer while a code was open: after five the code is closed. */
    fun offerFailed() {
        if (++failures >= MAX_FAILURES) closeOffer()
    }

    fun closeOffer() {
        offerKey = null
        changeListeners.forEach { it() }
    }

    /** The phone's IPv4 addresses on the local network, for the PC to try without mDNS. */
    fun hosts(): List<String> = runCatching {
        Collections.list(NetworkInterface.getNetworkInterfaces())
            .filter { it.isUp && !it.isLoopback }
            .flatMap { Collections.list(it.inetAddresses) }
            .filterIsInstance<Inet4Address>()
            .filter { it.isSiteLocalAddress }
            .mapNotNull { it.hostAddress }
    }.getOrDefault(emptyList())
}
