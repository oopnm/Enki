# Languages

Enki speaks 13 languages in both apps. English is the reference.

| Code | Language | Direction | Source | Review |
|---|---|---|---|---|
| en | English | LTR | hand written | reference |
| ar | العربية | RTL | hand written | reviewed by the author |
| zh | 中文 (Simplified) | LTR | hand written | machine-assisted, needs review |
| tr | Türkçe | LTR | `tools/translations/tr.ftl` | machine-assisted, needs review |
| fa | فارسی | RTL | `tools/translations/fa.ftl` | machine-assisted, needs review |
| ckb | کوردیی ناوەندی (Sorani) | RTL | `tools/translations/ckb.ftl` | machine-assisted, needs review |
| es | Español | LTR | `tools/translations/es.ftl` | machine-assisted, needs review |
| fr | Français | LTR | `tools/translations/fr.ftl` | machine-assisted, needs review |
| ru | Русский | LTR | `tools/translations/ru.ftl` | machine-assisted, needs review |
| de | Deutsch | LTR | `tools/translations/de.ftl` | machine-assisted, needs review |
| pt | Português | LTR | `tools/translations/pt.ftl` | machine-assisted, needs review |
| ja | 日本語 | LTR | `tools/translations/ja.ftl` | machine-assisted, needs review |
| hi | हिन्दी | LTR | `tools/translations/hi.ftl` | machine-assisted, needs review |

## How it works

- English, Arabic and Chinese live in `desktop/i18n/<lang>/enki.ftl` and `android/app/src/main/res/values*/strings.xml`.
- The other ten are written once in `tools/translations/<lang>.ftl`, one `message-id = text` per line, and
  `python3 tools/gen_translations.py` writes both the Fluent file and `strings.xml` from it, so the two apps use the
  same words. Do not edit the generated files; CI fails if they differ from their source.
- Texts that read the same everywhere (numbers, units) are taken from English.
- `python3 tools/check_i18n.py` checks that every language has every text with the same placeholders, and that every
  setting and choice has a label.

## Adding or fixing a language

1. Copy `tools/translations/es.ftl`, translate the right-hand sides, keep `{ $name }` placeholders.
2. Add the code to `NEW_LANGS` and `NATIVE` in `tools/gen_translations.py`, to `values` of `ui.language` in
   `protocol/settings.json` (then run `tools/gen_settings.py`), to `LANGS` in `tools/check_i18n.py`, to `LANGUAGES` in
   `desktop/crates/enki-i18n/src/lib.rs`, and add `setting-ui-language--<code>` to the English, Arabic and Chinese files.
3. Run `python3 tools/gen_translations.py && python3 tools/check_i18n.py`.

Right-to-left languages are listed in `enki-i18n` (`is_rtl`) and in `LocaleHelper.kt`.

## Fonts

Chinese, Japanese and Hindi need a system font that has those letters (for example Noto Sans CJK and Noto Sans
Devanagari). They are not bundled; the packages list them as optional.
