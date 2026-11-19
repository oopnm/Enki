//! Translations with Fluent. The messages live in `desktop/i18n/<lang>/enki.ftl` and are compiled into the program.
//! English is the reference: a message missing in another language falls back to the English one, and a message
//! missing everywhere shows its id (so a typo is visible, never a crash). `tools/check_i18n.py` fails CI when the
//! languages do not have the same message ids.

use fluent_bundle::concurrent::FluentBundle;
use fluent_bundle::{FluentArgs, FluentResource, FluentValue};
use unic_langid::LanguageIdentifier;

/// `(code, native name, Fluent source)` of every language, English first.
pub const LANGUAGES: &[(&str, &str, &str)] = &[
    ("en", "English", include_str!("../../../i18n/en/enki.ftl")),
    ("ar", "العربية", include_str!("../../../i18n/ar/enki.ftl")),
    ("zh", "中文", include_str!("../../../i18n/zh/enki.ftl")),
];

/// Languages written right to left.
pub fn is_rtl(lang: &str) -> bool {
    matches!(lang, "ar" | "he" | "fa" | "ur")
}

/// `en`, `ar` or `zh` from a locale string such as `ar_IQ.UTF-8`; `en` for anything else.
pub fn match_language(locale: &str) -> &'static str {
    let primary = locale
        .split(['_', '-', '.', '@'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    LANGUAGES
        .iter()
        .map(|(code, _, _)| *code)
        .find(|code| *code == primary)
        .unwrap_or("en")
}

/// The language the system asks for (`LC_ALL`, `LC_MESSAGES`, `LANG`), or `en`.
pub fn system_language() -> &'static str {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(var) {
            if !value.is_empty() && value != "C" && value != "POSIX" {
                return match_language(&value);
            }
        }
    }
    "en"
}

fn bundle(code: &str, source: &str) -> FluentBundle<FluentResource> {
    let id: LanguageIdentifier = code.parse().expect("language codes in LANGUAGES are valid");
    let mut bundle = FluentBundle::new_concurrent(vec![id]);
    // Isolation marks around placeables confuse plain GTK labels; the messages keep numbers and names apart themselves.
    bundle.set_use_isolating(false);
    let resource =
        FluentResource::try_new(source.to_string()).unwrap_or_else(|(partial, _)| partial);
    let _ = bundle.add_resource(resource); // duplicate ids are reported by the CI check, the first one wins here
    bundle
}

pub struct I18n {
    code: &'static str,
    current: FluentBundle<FluentResource>,
    english: FluentBundle<FluentResource>,
}

impl I18n {
    /// `lang` is `en`, `ar`, `zh` or `system`; anything else is English.
    pub fn new(lang: &str) -> I18n {
        let code = if lang == "system" {
            system_language()
        } else {
            match_language(lang)
        };
        let source_of = |c: &str| {
            LANGUAGES
                .iter()
                .find(|(l, _, _)| *l == c)
                .map(|(_, _, s)| *s)
                .unwrap_or("")
        };
        I18n {
            code,
            current: bundle(code, source_of(code)),
            english: bundle("en", source_of("en")),
        }
    }

    pub fn language(&self) -> &'static str {
        self.code
    }

    pub fn rtl(&self) -> bool {
        is_rtl(self.code)
    }

    pub fn tr(&self, id: &str) -> String {
        self.format(id, None)
    }

    /// With arguments, for example `tr_args("status-streaming", &[("fps", 30.into())])`.
    pub fn tr_args(&self, id: &str, args: &[(&str, FluentValue<'_>)]) -> String {
        let mut fluent_args = FluentArgs::new();
        for (name, value) in args {
            fluent_args.set(*name, value.clone());
        }
        self.format(id, Some(&fluent_args))
    }

    fn format(&self, id: &str, args: Option<&FluentArgs<'_>>) -> String {
        for bundle in [&self.current, &self.english] {
            if let Some(pattern) = bundle.get_message(id).and_then(|m| m.value()) {
                let mut errors = Vec::new();
                return bundle
                    .format_pattern(pattern, args, &mut errors)
                    .into_owned();
            }
        }
        id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Message ids in a Fluent file: lines of the form `id = ...` at column 0 (attributes and terms are not used).
    fn ids(source: &str) -> BTreeSet<String> {
        source
            .lines()
            .filter(|l| !l.starts_with('#') && !l.starts_with(' ') && l.contains('='))
            .map(|l| l.split('=').next().unwrap().trim().to_string())
            .collect()
    }

    #[test]
    fn every_language_parses_without_errors() {
        for (code, _, source) in LANGUAGES {
            let result = FluentResource::try_new(source.to_string());
            assert!(
                result.is_ok(),
                "{code} has Fluent syntax errors: {:?}",
                result.err().map(|(_, e)| e)
            );
        }
    }

    #[test]
    fn every_language_has_the_same_messages_as_english() {
        let english = ids(LANGUAGES[0].2);
        assert!(english.len() > 20, "english messages were read");
        for (code, _, source) in &LANGUAGES[1..] {
            let own = ids(source);
            assert_eq!(
                english.difference(&own).collect::<Vec<_>>(),
                Vec::<&String>::new(),
                "{code} lacks messages"
            );
            assert_eq!(
                own.difference(&english).collect::<Vec<_>>(),
                Vec::<&String>::new(),
                "{code} has extra messages"
            );
        }
    }

    #[test]
    fn translations_differ_and_arguments_are_filled_in() {
        let en = I18n::new("en");
        let ar = I18n::new("ar");
        let zh = I18n::new("zh");
        assert_eq!(en.tr("button-stop"), "Stop streaming");
        assert_ne!(ar.tr("button-stop"), en.tr("button-stop"));
        assert_ne!(zh.tr("button-stop"), en.tr("button-stop"));
        let text = en.tr_args(
            "status-streaming",
            &[("fps", 30.into()), ("mbps", "2.5".into())],
        );
        assert!(text.contains("30") && text.contains("2.5"), "{text}");
    }

    #[test]
    fn a_missing_message_shows_its_id_and_an_unknown_language_is_english() {
        assert_eq!(I18n::new("en").tr("no-such-message"), "no-such-message");
        assert_eq!(I18n::new("klingon").language(), "en");
    }

    #[test]
    fn right_to_left_and_locale_matching() {
        assert!(I18n::new("ar").rtl() && !I18n::new("en").rtl() && !I18n::new("zh").rtl());
        assert_eq!(match_language("ar_IQ.UTF-8"), "ar");
        assert_eq!(match_language("zh-CN"), "zh");
        assert_eq!(match_language("de_DE.UTF-8"), "en");
        assert_eq!(match_language(""), "en");
    }
}
