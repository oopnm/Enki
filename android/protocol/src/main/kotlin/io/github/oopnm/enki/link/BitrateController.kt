package io.github.oopnm.enki.link

/**
 * Keeps the encoder's bit rate in step with what the link can carry. Called once a second with the number of
 * frames the send queue dropped during that second: any drop means the PC is not keeping up, so the bit rate
 * is cut by 30 % (never below [minBps]); after [RECOVER_AFTER] seconds without drops it climbs back by 15 %
 * towards [targetBps], one step per calm period.
 */
class BitrateController(private val targetBps: Int, private val minBps: Int = 500_000) {
    init {
        require(minBps in 1..targetBps) { "need 0 < minBps <= targetBps" }
    }

    var current: Int = targetBps
        private set
    private var calmSeconds = 0

    /** Returns the new bit rate when it changed, otherwise null. */
    fun onSecond(droppedFrames: Long): Int? {
        if (droppedFrames > 0) {
            calmSeconds = 0
            return change(maxOf(minBps, (current.toLong() * CUT_PERCENT / 100).toInt()))
        }
        if (++calmSeconds < RECOVER_AFTER) return null
        calmSeconds = 0
        return change(minOf(targetBps, maxOf(current + MIN_STEP, (current.toLong() * RAISE_PERCENT / 100).toInt())))
    }

    private fun change(next: Int): Int? {
        if (next == current) return null
        current = next
        return next
    }

    companion object {
        const val RECOVER_AFTER = 5
        private const val CUT_PERCENT = 70
        private const val RAISE_PERCENT = 115
        private const val MIN_STEP = 50_000
    }
}
