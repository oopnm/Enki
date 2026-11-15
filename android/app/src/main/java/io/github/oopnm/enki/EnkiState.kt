package io.github.oopnm.enki

/** Tiny shared state so MainActivity can show what StreamService is doing. Listener runs on any thread. */
object EnkiState {
    @Volatile var running = false
        private set
    @Volatile var status = "Stopped"
        private set
    @Volatile var listener: (() -> Unit)? = null

    fun update(running: Boolean, status: String) {
        this.running = running
        this.status = status
        listener?.invoke()
    }
}
