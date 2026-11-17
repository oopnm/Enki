package io.github.oopnm.enki

/**
 * The session token the PC gave this app through adb (see [ProvisionProvider]). Kept in memory only: it is
 * gone when the app process ends or the stream is stopped, and the PC then provisions a new one.
 */
object SessionToken {
    @Volatile
    private var token: ByteArray? = null

    fun set(value: ByteArray) {
        token = value
    }

    fun get(): ByteArray? = token

    fun clear() {
        token = null
    }
}
