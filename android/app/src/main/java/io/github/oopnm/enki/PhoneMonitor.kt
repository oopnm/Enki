package io.github.oopnm.enki

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.BatteryManager
import android.os.Build
import android.os.PowerManager

/** A snapshot of the phone's battery: level in percent, temperature in tenths of a degree Celsius. */
data class BatteryInfo(val percent: Int, val tempDc: Int, val charging: Boolean)

/**
 * Watches battery level, battery temperature and (from Android 10) the system's thermal status.
 * [onBattery] is called when the battery numbers change; [onHot] when the phone becomes hot or cools down again.
 */
class PhoneMonitor(
    private val context: Context,
    private val onBattery: (BatteryInfo) -> Unit,
    private val onHot: (Boolean) -> Unit,
) {
    @Volatile var latest: BatteryInfo? = null
        private set

    @Volatile private var systemHot = false
    @Volatile private var batteryHot = false
    @Volatile private var reportedHot = false
    private var enabled = false
    private var thermalListener: Any? = null

    private val receiver = object : BroadcastReceiver() {
        override fun onReceive(c: Context, intent: Intent) = handle(intent)
    }

    @Synchronized
    fun enable() {
        if (enabled) return
        enabled = true
        val sticky = context.registerReceiver(receiver, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
        if (sticky != null) handle(sticky)
        if (Build.VERSION.SDK_INT >= 29) {
            val power = context.getSystemService(Context.POWER_SERVICE) as PowerManager
            val listener = PowerManager.OnThermalStatusChangedListener { status ->
                systemHot = status >= PowerManager.THERMAL_STATUS_SEVERE
                report()
            }
            thermalListener = listener
            power.addThermalStatusListener(listener)
        }
    }

    @Synchronized
    fun disable() {
        if (!enabled) return
        enabled = false
        runCatching { context.unregisterReceiver(receiver) }
        if (Build.VERSION.SDK_INT >= 29) {
            (thermalListener as? PowerManager.OnThermalStatusChangedListener)?.let {
                runCatching { (context.getSystemService(Context.POWER_SERVICE) as PowerManager).removeThermalStatusListener(it) }
            }
        }
        thermalListener = null
    }

    private fun handle(intent: Intent) {
        val level = intent.getIntExtra(BatteryManager.EXTRA_LEVEL, -1)
        val scale = intent.getIntExtra(BatteryManager.EXTRA_SCALE, 100)
        if (level < 0 || scale <= 0) return
        val temp = intent.getIntExtra(BatteryManager.EXTRA_TEMPERATURE, Int.MIN_VALUE)
        val status = intent.getIntExtra(BatteryManager.EXTRA_STATUS, -1)
        val charging = status == BatteryManager.BATTERY_STATUS_CHARGING || status == BatteryManager.BATTERY_STATUS_FULL
        val info = BatteryInfo((level * 100 / scale).coerceIn(0, 100), if (temp == Int.MIN_VALUE) 0 else temp, charging)
        // Hot from 43 °C, cool again below 40 °C, so it does not flap around the limit.
        if (temp != Int.MIN_VALUE) batteryHot = if (batteryHot) temp >= 400 else temp >= 430
        val changed = latest != info
        latest = info
        if (changed) onBattery(info)
        report()
    }

    private fun report() {
        val hot = systemHot || batteryHot
        if (hot != reportedHot) {
            reportedHot = hot
            onHot(hot)
        }
    }
}
