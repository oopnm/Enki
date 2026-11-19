//! TOML files under `~/.config/enki`:
//!
//! * `settings.toml`: this computer's id, the local settings and which phone profile was used last.
//! * `phones/<id>.toml`: one profile per phone: its synced settings with their stamps, its name and the
//!   capabilities it reported (cached, so the right options show before the phone connects).
//!
//! Files are written to a temporary name and renamed, so a crash never leaves half a file.

use crate::clock::Hlc;
use crate::engine::{Entry, Stamp};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// Id of the profile used before any phone has connected. Its synced settings are copied to the first phone.
pub const DEFAULT_PROFILE: &str = "default";

#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub device_id: String,
    pub last_phone: Option<String>,
    pub clock: Hlc,
    pub entries: Vec<(String, Entry)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PhoneProfile {
    pub id: String,
    pub name: String,
    pub capabilities_json: Option<String>,
    pub clock: Hlc,
    pub entries: Vec<(String, Entry)>,
}

#[derive(Serialize, Deserialize)]
struct StoredEntry {
    value: Value,
    wall: u64,
    #[serde(default)]
    counter: u32,
    device: String,
}

#[derive(Serialize, Deserialize, Default)]
struct MainFile {
    #[serde(default)]
    schema: u32,
    #[serde(default)]
    device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_phone: Option<String>,
    #[serde(default)]
    clock_wall: u64,
    #[serde(default)]
    clock_counter: u32,
    #[serde(default)]
    values: BTreeMap<String, StoredEntry>,
}

#[derive(Serialize, Deserialize, Default)]
struct PhoneFile {
    #[serde(default)]
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capabilities_json: Option<String>,
    #[serde(default)]
    clock_wall: u64,
    #[serde(default)]
    clock_counter: u32,
    #[serde(default)]
    values: BTreeMap<String, StoredEntry>,
}

fn to_stored(entries: &[(String, Entry)]) -> BTreeMap<String, StoredEntry> {
    entries
        .iter()
        .map(|(k, e)| {
            (
                k.clone(),
                StoredEntry {
                    value: e.value.clone(),
                    wall: e.stamp.hlc.wall,
                    counter: e.stamp.hlc.counter,
                    device: e.stamp.device.clone(),
                },
            )
        })
        .collect()
}

fn from_stored(values: BTreeMap<String, StoredEntry>) -> Vec<(String, Entry)> {
    values
        .into_iter()
        .map(|(k, s)| {
            (
                k,
                Entry {
                    value: s.value,
                    stamp: Stamp {
                        hlc: Hlc {
                            wall: s.wall,
                            counter: s.counter,
                        },
                        device: s.device,
                    },
                },
            )
        })
        .collect()
}

/// A phone id becomes a file name, and the phone chooses it: only plain ids are accepted.
pub fn valid_profile_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id != DEFAULT_PROFILE
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Store {
        Store { dir: dir.into() }
    }

    /// `$XDG_CONFIG_HOME/enki`, or `~/.config/enki`.
    pub fn default_dir() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("enki")
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn main_path(&self) -> PathBuf {
        self.dir.join("settings.toml")
    }

    fn phone_path(&self, id: &str) -> PathBuf {
        self.dir.join("phones").join(format!("{id}.toml"))
    }

    fn write(path: &Path, text: &str) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }

    /// `Ok(None)` when the file does not exist yet. A file that cannot be parsed is an error, so the caller can
    /// tell the user instead of silently starting over.
    fn read<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<Option<T>> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map(Some).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {e}", path.display()),
                )
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn load(&self) -> io::Result<Option<Saved>> {
        Ok(Self::read::<MainFile>(&self.main_path())?.map(|f| Saved {
            device_id: f.device_id,
            last_phone: f.last_phone,
            clock: Hlc {
                wall: f.clock_wall,
                counter: f.clock_counter,
            },
            entries: from_stored(f.values),
        }))
    }

    pub fn save(&self, saved: &Saved) -> io::Result<()> {
        let file = MainFile {
            schema: 1,
            device_id: saved.device_id.clone(),
            last_phone: saved.last_phone.clone(),
            clock_wall: saved.clock.wall,
            clock_counter: saved.clock.counter,
            values: to_stored(&saved.entries),
        };
        let text = toml::to_string_pretty(&file)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        Self::write(&self.main_path(), &text)
    }

    pub fn load_phone(&self, id: &str) -> io::Result<Option<PhoneProfile>> {
        if id != DEFAULT_PROFILE && !valid_profile_id(id) {
            return Ok(None);
        }
        Ok(
            Self::read::<PhoneFile>(&self.phone_path(id))?.map(|f| PhoneProfile {
                id: id.to_string(),
                name: f.name,
                capabilities_json: f.capabilities_json,
                clock: Hlc {
                    wall: f.clock_wall,
                    counter: f.clock_counter,
                },
                entries: from_stored(f.values),
            }),
        )
    }

    pub fn save_phone(&self, profile: &PhoneProfile) -> io::Result<()> {
        if profile.id != DEFAULT_PROFILE && !valid_profile_id(&profile.id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a valid profile id: {:?}", profile.id),
            ));
        }
        let file = PhoneFile {
            name: profile.name.clone(),
            capabilities_json: profile.capabilities_json.clone(),
            clock_wall: profile.clock.wall,
            clock_counter: profile.clock.counter,
            values: to_stored(&profile.entries),
        };
        let text = toml::to_string_pretty(&file)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        Self::write(&self.phone_path(&profile.id), &text)
    }

    /// `(id, name)` of every saved phone profile, without the default one.
    pub fn list_phones(&self) -> Vec<(String, String)> {
        let mut found = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.dir.join("phones")) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(id) = name.strip_suffix(".toml") else {
                    continue;
                };
                if id == DEFAULT_PROFILE || !valid_profile_id(id) {
                    continue;
                }
                if let Ok(Some(profile)) = self.load_phone(id) {
                    found.push((id.to_string(), profile.name));
                }
            }
        }
        found.sort();
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(value: Value, wall: u64, counter: u32, device: &str) -> Entry {
        Entry {
            value,
            stamp: Stamp {
                hlc: Hlc { wall, counter },
                device: device.into(),
            },
        }
    }

    #[test]
    fn the_main_file_round_trips_every_value_type() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("enki"));
        assert!(store.load().unwrap().is_none(), "nothing saved yet");
        let saved = Saved {
            device_id: "pc-1".into(),
            last_phone: Some("abc123".into()),
            clock: Hlc {
                wall: 1_700_000_000_123,
                counter: 4,
            },
            entries: vec![
                ("ui.language".into(), entry(json!("ar"), 1, 0, "pc-1")),
                ("video.auto_rotate".into(), entry(json!(true), 2, 3, "pc-1")),
                ("zoom.level".into(), entry(json!(2.5), 3, 0, "pc-1")),
                ("video.fps".into(), entry(json!(30), 4, 0, "phone")),
                (
                    "output.device".into(),
                    entry(json!("/dev/video10"), 5, 0, "pc-1"),
                ),
            ],
        };
        store.save(&saved).unwrap();
        let mut loaded = store.load().unwrap().unwrap();
        let mut expected = saved.clone();
        loaded.entries.sort_by(|a, b| a.0.cmp(&b.0));
        expected.entries.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(loaded, expected);
        let text = std::fs::read_to_string(store.dir().join("settings.toml")).unwrap();
        assert!(
            text.contains("device_id = \"pc-1\"") && text.contains("[values.\"ui.language\"]"),
            "{text}"
        );
    }

    #[test]
    fn phone_profiles_are_separate_files_with_cached_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let profile = PhoneProfile {
            id: "0123abcd".into(),
            name: "Pixel 6".into(),
            capabilities_json: Some(r#"{"type":"capabilities","cameras":[]}"#.into()),
            clock: Hlc {
                wall: 9,
                counter: 1,
            },
            entries: vec![("camera.facing".into(), entry(json!("front"), 9, 1, "phone"))],
        };
        store.save_phone(&profile).unwrap();
        assert_eq!(store.load_phone("0123abcd").unwrap().unwrap(), profile);
        assert!(store.load_phone("other").unwrap().is_none());
        store
            .save_phone(&PhoneProfile {
                id: DEFAULT_PROFILE.into(),
                name: String::new(),
                capabilities_json: None,
                clock: Hlc::default(),
                entries: vec![],
            })
            .unwrap();
        assert_eq!(
            store.list_phones(),
            vec![("0123abcd".to_string(), "Pixel 6".to_string())]
        );
    }

    #[test]
    fn a_phone_cannot_pick_a_path_through_its_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        for bad in ["../evil", "a/b", "", "x.y", "default", &"a".repeat(65)] {
            assert!(!valid_profile_id(bad), "{bad:?}");
            let profile = PhoneProfile {
                id: bad.into(),
                name: "x".into(),
                capabilities_json: None,
                clock: Hlc::default(),
                entries: vec![],
            };
            assert!(store.save_phone(&profile).is_err() || bad == DEFAULT_PROFILE);
            assert!(store.load_phone(bad).unwrap().is_none() || bad == DEFAULT_PROFILE);
        }
        assert!(valid_profile_id("0123456789abcdef0123456789abcdef"));
        assert!(!dir.path().join("evil.toml").exists());
    }

    #[test]
    fn a_damaged_file_is_an_error_not_silently_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.toml"), "this is = not [ valid").unwrap();
        let err = Store::new(dir.path()).load().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("settings.toml"));
    }

    #[test]
    fn saving_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store
            .save(&Saved {
                device_id: "a".into(),
                last_phone: None,
                clock: Hlc::default(),
                entries: vec![],
            })
            .unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["settings.toml"]);
    }
}
