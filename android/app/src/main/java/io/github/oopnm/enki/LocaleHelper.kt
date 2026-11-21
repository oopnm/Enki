package io.github.oopnm.enki

import android.content.Context
import android.content.res.Configuration
import java.util.Locale

/** Applies the `ui.language` setting ("system" follows the phone) and its text direction to a context. */
object LocaleHelper {
    /** Languages written right to left. */
    private val RTL = setOf("ar", "fa", "ckb", "he", "ur")

    fun wrap(base: Context): Context {
        EnkiSettings.init(base)
        val tag = EnkiSettings.getString("ui.language")
        if (tag.isEmpty() || tag == "system") return base
        val locale = Locale.forLanguageTag(tag)
        Locale.setDefault(locale)
        val config = Configuration(base.resources.configuration)
        config.setLocale(locale)
        config.setLayoutDirection(locale)
        // Do not rely on the system's guess for the direction: Sorani Kurdish is right to left too.
        val direction = if (tag in RTL) Configuration.SCREENLAYOUT_LAYOUTDIR_RTL else Configuration.SCREENLAYOUT_LAYOUTDIR_LTR
        config.screenLayout = (config.screenLayout and Configuration.SCREENLAYOUT_LAYOUTDIR_MASK.inv()) or direction
        return base.createConfigurationContext(config)
    }

}
