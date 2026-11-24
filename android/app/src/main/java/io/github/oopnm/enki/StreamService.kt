package io.github.oopnm.enki

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.graphics.drawable.Icon
import android.os.Build
import android.os.IBinder
import io.github.oopnm.enki.link.Link
import java.io.IOException

/** Foreground service that keeps the Enki Link server (and therefore the camera stream) alive in the background. */
class StreamService : Service() {
    private var server: LinkServer? = null
    private var finalStatus = "Stopped"

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            shutDown()
            return START_NOT_STICKY
        }
        if (server == null) startServer()
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        server?.stop()
        server = null
        EnkiState.update(false, finalStatus)
        super.onDestroy()
    }

    private fun startServer() {
        EnkiSettings.init(applicationContext)
        WifiState.init(applicationContext)
        createChannel()
        val initial = notification("Starting…")
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(NOTIFICATION_ID, initial, ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA)
        } else {
            startForeground(NOTIFICATION_ID, initial)
        }
        val version = runCatching { packageManager.getPackageInfo(packageName, 0).versionName }.getOrNull() ?: "?"
        val s = LinkServer(applicationContext, version) { text -> onStatus(text) }
        try {
            s.start()
            server = s
        } catch (e: IOException) {
            onStatus("Cannot listen on port ${Link.DEFAULT_PORT}: ${e.message}")
            shutDown(keepStatus = true)
        }
    }

    private fun onStatus(text: String) {
        EnkiState.update(true, text)
        getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification(text))
    }

    private fun shutDown(keepStatus: Boolean = false) {
        if (keepStatus) finalStatus = EnkiState.status
        server?.stop()
        server = null
        @Suppress("DEPRECATION") // the flags variant needs API 33; minSdk is 24
        stopForeground(true)
        stopSelf()
    }

    private fun createChannel() {
        if (Build.VERSION.SDK_INT < 26) return
        val channel = NotificationChannel(CHANNEL_ID, "Enki streaming", NotificationManager.IMPORTANCE_LOW)
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }

    private fun notification(text: String): Notification {
        @Suppress("DEPRECATION") // the channel-less builder is only used below API 26
        val builder = if (Build.VERSION.SDK_INT >= 26) Notification.Builder(this, CHANNEL_ID) else Notification.Builder(this)
        val open = PendingIntent.getActivity(this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)
        val stop = PendingIntent.getService(
            this, 1, Intent(this, StreamService::class.java).setAction(ACTION_STOP), PendingIntent.FLAG_IMMUTABLE,
        )
        return builder
            .addAction(
                Notification.Action.Builder(
                    Icon.createWithResource(this, android.R.drawable.ic_menu_close_clear_cancel), getString(R.string.stop), stop,
                ).build(),
            )
            .setContentTitle("Enki")
            .setContentText(text)
            .setSmallIcon(android.R.drawable.ic_menu_camera)
            .setContentIntent(open)
            .setOngoing(true)
            .build()
    }

    companion object {
        const val ACTION_STOP = "io.github.oopnm.enki.STOP"
        private const val CHANNEL_ID = "enki-streaming"
        private const val NOTIFICATION_ID = 1

        fun start(context: Context) {
            val intent = Intent(context, StreamService::class.java)
            if (Build.VERSION.SDK_INT >= 26) context.startForegroundService(intent) else context.startService(intent)
        }

        fun stop(context: Context) {
            context.startService(Intent(context, StreamService::class.java).setAction(ACTION_STOP))
        }
    }
}
