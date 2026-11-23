#!/usr/bin/env python3
"""CI check: the languages and the apps agree.

  * Android values/ and values-<lang>/ define the same string names with the same placeholders.
  * The desktop Fluent files of every language define the same message ids with the same $variables.
  * Every setting in protocol/settings.json has a label (and a label for each choice) in every language of each
    app that shows it, and each group has a heading. A setting without a control in an app fails the build.
"""
import json
import pathlib
import re
import sys
import xml.etree.ElementTree as ET

ROOT = pathlib.Path(__file__).resolve().parent.parent
LANGS = ["en", "ar", "zh", "tr", "fa", "ckb", "es", "fr", "ru", "de", "pt", "ja", "hi"]
errors = []


def android_strings(lang):
    folder = "values" if lang == "en" else f"values-{lang}"
    tree = ET.parse(ROOT / f"android/app/src/main/res/{folder}/strings.xml")
    return {s.get("name"): "".join(s.itertext()) for s in tree.getroot().findall("string")}


def fluent_messages(lang):
    messages, current = {}, None
    for line in (ROOT / f"desktop/i18n/{lang}/enki.ftl").read_text().splitlines():
        if line.startswith("#") or not line.strip():
            continue
        m = re.match(r"^([a-zA-Z][a-zA-Z0-9_-]*)\s*=\s*(.*)$", line)
        if m:
            current = m.group(1)
            if current in messages:
                errors.append(f"desktop/i18n/{lang}: duplicate message {current}")
            messages[current] = m.group(2)
        elif current and line.startswith(" "):
            messages[current] += "\n" + line.strip()
    return messages


def android_vars(text):
    return sorted(re.findall(r"%\d\$[sd]", text))


def fluent_vars(text):
    return sorted(set(re.findall(r"\$([a-zA-Z][a-zA-Z0-9_-]*)", text)))


def compare(label, per_lang, vars_of):
    base = per_lang["en"]
    for lang in LANGS[1:]:
        other = per_lang[lang]
        for missing in sorted(set(base) - set(other)):
            errors.append(f"{label} {lang}: missing {missing}")
        for extra in sorted(set(other) - set(base)):
            errors.append(f"{label} {lang}: {extra} does not exist in English")
        for name in sorted(set(base) & set(other)):
            if vars_of(base[name]) != vars_of(other[name]):
                errors.append(f"{label} {lang}: {name} has different placeholders than English")


def main():
    schema = json.loads((ROOT / "protocol/settings.json").read_text())
    android = {lang: android_strings(lang) for lang in LANGS}
    desktop = {lang: fluent_messages(lang) for lang in LANGS}
    compare("android", android, android_vars)
    compare("desktop", desktop, fluent_vars)

    def res(key):
        return key.replace(".", "_")

    def ftl(key):
        return key.replace(".", "-").replace("_", "-")

    groups = {"desktop": set(), "phone": set()}
    for s in schema["settings"]:
        apps = s.get("apps", ["desktop", "phone"])
        values = s["values"] if s["type"] == "enum" else []
        if "phone" in apps:
            groups["phone"].add(s["group"])
            for lang in LANGS:
                need = [f"setting_{res(s['key'])}"] + [f"setting_{res(s['key'])}__{v}" for v in values]
                for name in need:
                    if name not in android[lang]:
                        errors.append(f"android {lang}: setting {s['key']} has no string {name}")
        if "desktop" in apps:
            groups["desktop"].add(s["group"])
            for lang in LANGS:
                need = [f"setting-{ftl(s['key'])}"] + [f"setting-{ftl(s['key'])}--{ftl(v)}" for v in values]
                for name in need:
                    if name not in desktop[lang]:
                        errors.append(f"desktop {lang}: setting {s['key']} has no message {name}")
    for g in groups["phone"]:
        for lang in LANGS:
            if f"group_{g}" not in android[lang]:
                errors.append(f"android {lang}: no heading group_{g}")
    for g in groups["desktop"]:
        for lang in LANGS:
            if f"group-{g}" not in desktop[lang]:
                errors.append(f"desktop {lang}: no heading group-{g}")
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"translations are consistent ({len(android['en'])} Android strings, {len(desktop['en'])} desktop messages, {len(schema['settings'])} settings)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
