//! The engine plus its files: what the apps actually hold. Every change is saved as it happens, the way a
//! settings screen expects, and the profile of the connected phone is swapped in when a phone says who it is.

use crate::clock::{Hlc, HlcClock};
use crate::engine::{Applied, Engine, Merge, RolledBack, SetOutcome};
use crate::schema::{Reject, Schema, Scope};
use crate::store::{valid_profile_id, PhoneProfile, Saved, Store, DEFAULT_PROFILE};
use enki_protocol::control::SettingEntry;
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Shared between the window and the receiver task.
pub type SettingsHandle = Arc<Mutex<Hub>>;

pub struct Hub {
    engine: Engine,
    store: Option<Store>,
    profile: String,
    phone_name: String,
    capabilities_json: Option<String>,
    /// The last file error, for the UI to show once.
    save_error: Option<String>,
}

/// A random 128-bit id as 32 hex digits, from the kernel.
pub fn random_device_id() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("cannot read random bytes from /dev/urandom");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub")
            .field("profile", &self.profile)
            .field("phone", &self.phone_name)
            .finish_non_exhaustive()
    }
}

impl Hub {
    /// Settings that live only in memory (tests, and a fallback if the config directory cannot be used).
    pub fn in_memory(device_id: impl Into<String>) -> Hub {
        Hub {
            engine: Engine::new(Schema::builtin(), device_id, HlcClock::system()),
            store: None,
            profile: DEFAULT_PROFILE.into(),
            phone_name: String::new(),
            capabilities_json: None,
            save_error: None,
        }
    }

    pub fn handle(self) -> SettingsHandle {
        Arc::new(Mutex::new(self))
    }

    /// Loads (or starts) the settings in `store`. Returns the hub and a warning when a file was damaged: the
    /// damaged file is left alone on disk until the next change replaces it.
    pub fn open(store: Store) -> (Hub, Option<String>) {
        let mut warning = None;
        let saved = match store.load() {
            Ok(s) => s,
            Err(e) => {
                warning = Some(format!("settings file ignored: {e}"));
                None
            }
        };
        let device_id = saved
            .as_ref()
            .map(|s| s.device_id.clone())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(random_device_id);
        let mut engine = Engine::new(Schema::builtin(), device_id, HlcClock::system());
        let mut profile = DEFAULT_PROFILE.to_string();
        if let Some(saved) = saved {
            engine.restore_clock(saved.clock);
            engine.load(saved.entries);
            if let Some(last) = saved
                .last_phone
                .filter(|id| id == DEFAULT_PROFILE || valid_profile_id(id))
            {
                profile = last;
            }
        }
        let mut hub = Hub {
            engine,
            store: Some(store),
            profile: profile.clone(),
            phone_name: String::new(),
            capabilities_json: None,
            save_error: None,
        };
        match hub.store.as_ref().map(|s| s.load_phone(&profile)) {
            Some(Ok(Some(p))) => {
                hub.adopt_profile(p);
            }
            Some(Err(e)) => warning = warning.or(Some(format!("phone profile ignored: {e}"))),
            _ => {}
        }
        (hub, warning)
    }

    fn adopt_profile(&mut self, p: PhoneProfile) -> Vec<(String, Value)> {
        self.engine.restore_clock(p.clock);
        self.phone_name = p.name;
        self.capabilities_json = p.capabilities_json;
        self.engine.replace_synced(p.entries)
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn schema(&self) -> &Schema {
        self.engine.schema()
    }

    pub fn device_id(&self) -> &str {
        self.engine.device_id()
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        self.engine.get(key)
    }

    pub fn get_string(&self, key: &str) -> String {
        self.engine.get_string(key)
    }

    pub fn get_bool(&self, key: &str) -> bool {
        self.engine.get_bool(key)
    }

    /// The profile in use: [`DEFAULT_PROFILE`] before any phone has connected, then the phone's id.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub fn phone_name(&self) -> &str {
        &self.phone_name
    }

    pub fn cached_capabilities(&self) -> Option<&str> {
        self.capabilities_json.as_deref()
    }

    pub fn take_save_error(&mut self) -> Option<String> {
        self.save_error.take()
    }

    pub fn set_local(&mut self, key: &str, value: &Value) -> Result<SetOutcome, Reject> {
        let out = self.engine.set_local(key, value)?;
        if out.changed {
            self.save();
        }
        Ok(out)
    }

    pub fn apply_remote(&mut self, entry: &SettingEntry) -> Result<Applied, Reject> {
        let applied = self.engine.apply_remote(entry)?;
        if !matches!(applied, Applied::Stale(_)) {
            self.save();
        }
        Ok(applied)
    }

    pub fn merge_snapshot(&mut self, remote: &[SettingEntry]) -> Merge {
        let merge = self.engine.merge_snapshot(remote);
        self.save();
        merge
    }

    pub fn snapshot(&self) -> Vec<SettingEntry> {
        self.engine.snapshot()
    }

    pub fn acked(&mut self, key: &str, hlc: (u64, u32), device: &str) -> bool {
        self.engine.acked(key, hlc, device)
    }

    pub fn rejected(
        &mut self,
        key: &str,
        hlc: (u64, u32),
        device: &str,
        reject: &Reject,
    ) -> Option<RolledBack> {
        let rolled = self.engine.rejected(key, hlc, device, reject);
        if rolled.is_some() {
            self.save();
        }
        rolled
    }

    pub fn cache_capabilities(&mut self, json: String) {
        if self.capabilities_json.as_deref() != Some(json.as_str()) {
            self.capabilities_json = Some(json);
            self.save();
        }
    }

    /// A phone said who it is. Its profile replaces the synced settings (a phone seen before gets what it had; a new
    /// phone starts from the settings in use). Returns the keys whose value changed, for the screen to refresh.
    /// An id that is not a plain id keeps the current profile.
    pub fn switch_phone(&mut self, id: &str, name: &str) -> Vec<(String, Value)> {
        if !valid_profile_id(id) {
            return Vec::new();
        }
        if id == self.profile {
            if !name.is_empty() && name != self.phone_name {
                self.phone_name = name.to_string();
                self.save();
            }
            return Vec::new();
        }
        self.save(); // the profile we are leaving
        let loaded = match self.store.as_ref().map(|s| s.load_phone(id)) {
            Some(Ok(p)) => p,
            Some(Err(e)) => {
                self.save_error = Some(format!("phone profile ignored: {e}"));
                None
            }
            None => None,
        };
        self.profile = id.to_string();
        let changed = match loaded {
            Some(p) => self.adopt_profile(p),
            None => {
                self.capabilities_json = None; // another phone's cameras do not apply
                Vec::new()
            }
        };
        if !name.is_empty() {
            self.phone_name = name.to_string();
        }
        self.save();
        changed
    }

    /// Writes the files. Errors are kept for [`Hub::take_save_error`].
    pub fn save(&mut self) {
        let Some(store) = &self.store else { return };
        let all = self.engine.export();
        let (synced, local): (Vec<_>, Vec<_>) = all.into_iter().partition(|(k, _)| {
            self.engine
                .schema()
                .get(k)
                .map(|d| d.scope == Scope::Synced)
                .unwrap_or(false)
        });
        let clock: Hlc = self.engine.clock_last();
        let main = Saved {
            device_id: self.engine.device_id().to_string(),
            last_phone: Some(self.profile.clone()),
            clock,
            entries: local,
        };
        let profile = PhoneProfile {
            id: self.profile.clone(),
            name: self.phone_name.clone(),
            capabilities_json: self.capabilities_json.clone(),
            clock,
            entries: synced,
        };
        let result = store.save(&main).and_then(|_| store.save_phone(&profile));
        if let Err(e) = result {
            self.save_error = Some(format!("could not save settings: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn open(dir: &std::path::Path) -> Hub {
        let (hub, warning) = Hub::open(Store::new(dir));
        assert_eq!(warning, None);
        hub
    }

    #[test]
    fn settings_survive_a_restart_and_the_device_id_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let id;
        {
            let mut hub = open(dir.path());
            id = hub.device_id().to_string();
            assert_eq!(id.len(), 32);
            hub.set_local("ui.language", &json!("ar")).unwrap();
            hub.set_local("video.rotate", &json!("90")).unwrap();
            assert_eq!(hub.take_save_error(), None);
        }
        let hub = open(dir.path());
        assert_eq!(hub.device_id(), id);
        assert_eq!(hub.get("ui.language"), Some(json!("ar")));
        assert_eq!(hub.get("video.rotate"), Some(json!("90")));
        assert_eq!(hub.profile(), DEFAULT_PROFILE);
    }

    #[test]
    fn each_phone_has_its_own_synced_settings_and_the_first_one_inherits_the_current_ones() {
        let dir = tempfile::tempdir().unwrap();
        let mut hub = open(dir.path());
        hub.set_local("video.rotate", &json!("180")).unwrap();
        hub.set_local("ui.language", &json!("zh")).unwrap();
        // The first phone ever starts from what the user had chosen.
        assert!(hub.switch_phone("aaaa1111", "Pixel 6").is_empty());
        assert_eq!(hub.get("video.rotate"), Some(json!("180")));
        hub.set_local("video.rotate", &json!("270")).unwrap();
        // A second phone also starts from the settings in use, then diverges.
        let changed = hub.switch_phone("bbbb2222", "Galaxy");
        assert!(changed.is_empty());
        hub.set_local("video.rotate", &json!("0")).unwrap();
        // Going back to the first phone restores its own values; local settings never change with the phone.
        let changed = hub.switch_phone("aaaa1111", "Pixel 6");
        assert_eq!(changed, vec![("video.rotate".to_string(), json!("270"))]);
        assert_eq!(hub.get("ui.language"), Some(json!("zh")));
        assert_eq!(hub.phone_name(), "Pixel 6");
        // The last phone is the one used on the next start.
        drop(hub);
        let hub = open(dir.path());
        assert_eq!(hub.profile(), "aaaa1111");
        assert_eq!(hub.get("video.rotate"), Some(json!("270")));
    }

    #[test]
    fn capabilities_are_cached_per_phone() {
        let dir = tempfile::tempdir().unwrap();
        let mut hub = open(dir.path());
        hub.switch_phone("aaaa1111", "Pixel 6");
        hub.cache_capabilities(r#"{"cameras":[1]}"#.into());
        hub.switch_phone("bbbb2222", "Galaxy");
        assert_eq!(
            hub.cached_capabilities(),
            None,
            "another phone's cameras do not apply"
        );
        hub.switch_phone("aaaa1111", "Pixel 6");
        assert_eq!(hub.cached_capabilities(), Some(r#"{"cameras":[1]}"#));
        drop(hub);
        assert_eq!(
            open(dir.path()).cached_capabilities(),
            Some(r#"{"cameras":[1]}"#)
        );
    }

    #[test]
    fn a_bad_phone_id_keeps_the_current_profile() {
        let mut hub = Hub::in_memory("pc");
        assert!(hub.switch_phone("../../etc/passwd", "x").is_empty());
        assert_eq!(hub.profile(), DEFAULT_PROFILE);
    }

    #[test]
    fn remote_changes_are_saved_but_stale_ones_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let mut hub = open(dir.path());
        let entry = SettingEntry {
            key: "camera.facing".into(),
            value: json!("front"),
            hlc: (1, 0),
            device: "phone".into(),
        };
        assert!(matches!(
            hub.apply_remote(&entry).unwrap(),
            Applied::Changed { .. }
        ));
        assert_eq!(open(dir.path()).get("camera.facing"), Some(json!("front")));
    }

    #[test]
    fn a_damaged_main_file_is_reported_and_the_app_still_starts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.toml"), "= broken").unwrap();
        let (hub, warning) = Hub::open(Store::new(dir.path()));
        assert!(warning.unwrap().contains("settings file ignored"));
        assert_eq!(hub.get("camera.facing"), Some(json!("back")));
    }
}
