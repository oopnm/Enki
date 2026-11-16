package io.github.oopnm.enki

import android.content.ContentProvider
import android.content.ContentValues
import android.database.Cursor
import android.net.Uri
import android.os.Binder
import android.os.Bundle
import io.github.oopnm.enki.link.Auth

/**
 * Lets the PC give the app its session token with
 * `adb shell content call --uri content://io.github.oopnm.enki.provision --method provision --arg <64 hex digits>`.
 *
 * The provider is exported, but `call` refuses every caller except the adb shell (uid 2000) and root, and the
 * kernel sets the calling uid, so another app cannot impersonate adb. Without the token nobody can answer the
 * Enki Link challenge, so apps on the phone cannot read the camera stream. All other provider operations
 * do nothing.
 */
class ProvisionProvider : ContentProvider() {
    override fun onCreate(): Boolean = true

    override fun call(method: String, arg: String?, extras: Bundle?): Bundle? {
        if (!Auth.isAdbUid(Binder.getCallingUid())) {
            throw SecurityException("only adb may provision the session token")
        }
        if (method != METHOD_PROVISION) throw IllegalArgumentException("unknown method")
        val token = Auth.tokenFromHex(arg) ?: throw IllegalArgumentException("the token must be 64 hex digits")
        SessionToken.set(token)
        return Bundle().apply { putBoolean("ok", true) }
    }

    override fun query(uri: Uri, projection: Array<out String>?, selection: String?, selectionArgs: Array<out String>?, sortOrder: String?): Cursor? = null

    override fun getType(uri: Uri): String? = null

    override fun insert(uri: Uri, values: ContentValues?): Uri? = null

    override fun delete(uri: Uri, selection: String?, selectionArgs: Array<out String>?): Int = 0

    override fun update(uri: Uri, values: ContentValues?, selection: String?, selectionArgs: Array<out String>?): Int = 0

    companion object {
        const val AUTHORITY = "io.github.oopnm.enki.provision"
        const val METHOD_PROVISION = "provision"
    }
}
