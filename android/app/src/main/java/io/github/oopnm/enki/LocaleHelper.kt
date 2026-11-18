package io.github.oopnm.enki

import android.content.Context
import android.content.res.Configuration
import java.util.Locale

/** Applies the `ui.language` setting ("system" follows the phone) and its text direction to a context. */
object LocaleHelper {
    fun wrap(base: Context): Context {
        EnkiSettings.init(base)
        val tag = EnkiSettings.getString("ui.language")
        if (tag.isEmpty() || tag == "system") return base
        val locale = Locale(tag)
        Locale.setDefault(locale)
        val config = Configuration(base.resources.configuration)
        config.setLocale(locale)
        config.setLayoutDirection(locale) // right-to-left for Arabic
        return base.createConfigurationContext(config)
    }
}
