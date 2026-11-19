//! The two-way sync engine. Both apps run the same logic (this one in Rust, a mirror in Kotlin checked against
//! shared vectors), so the rules below hold on both sides:
//!
//! * Every synced key carries a stamp: the hybrid logical clock value of the write and the id of the device that
//!   wrote it. The newest stamp wins; equal clock values are broken by the device id, so both sides pick the same
//!   winner without talking.
//! * Only differences travel: on connect each side sends what it has explicitly set, and the other answers with
//!   just the entries where it is newer or the peer has none ([`Engine::merge_snapshot`]).
//! * A change is never echoed back to the device it came from ([`Engine::apply_remote`] returns what to do locally,
//!   never something to send back except a reply to a stale change).
//! * A change that the other side refuses is rolled back, so the screen always shows the truth
//!   ([`Engine::rejected`]).

use crate::clock::{Hlc, HlcClock};
use crate::schema::{Reject, RejectCode, Schema, Scope};
use enki_protocol::control::SettingEntry;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Which write produced a value. Ordered by clock, then by device id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Stamp {
    pub hlc: Hlc,
    pub device: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub value: Value,
    pub stamp: Stamp,
}

/// A local change waiting for the other side's answer. `baseline` is what the key held before the first
/// unanswered change, which is what a refusal rolls back to.
#[derive(Debug, Clone)]
struct Pending {
    latest: Stamp,
    baseline: Option<Entry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SetOutcome {
    /// The value differs from what was there.
    pub changed: bool,
    /// The change to send to the other device: only for a changed synced key.
    pub send: Option<SettingEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Applied {
    /// The change was newer and the value differs: apply it (move the camera, refresh the control).
    Changed { key: String, value: Value },
    /// Nothing to do: a duplicate, or newer but with the same value.
    Unchanged,
    /// Older than what we hold. Answer with this entry so the sender catches up.
    Stale(SettingEntry),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Merge {
    /// Keys whose value changed here because the peer's write was newer.
    pub changed: Vec<(String, Value)>,
    /// Entries to send to the peer: ours are newer, or it has none.
    pub send_back: Vec<SettingEntry>,
    /// Entries refused (unknown key from a newer schema, a bad value, a local-only key).
    pub ignored: Vec<(String, Reject)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RolledBack {
    pub key: String,
    /// The value shown again after the other side refused the change.
    pub value: Value,
    pub reject: Reject,
}

#[derive(Debug)]
pub struct Engine {
    schema: Schema,
    device_id: String,
    clock: HlcClock,
    entries: BTreeMap<String, Entry>,
    pending: BTreeMap<String, Pending>,
}

fn wire(key: &str, entry: &Entry) -> SettingEntry {
    SettingEntry {
        key: key.to_string(),
        value: entry.value.clone(),
        hlc: entry.stamp.hlc.into(),
        device: entry.stamp.device.clone(),
    }
}

impl Engine {
    pub fn new(schema: Schema, device_id: impl Into<String>, clock: HlcClock) -> Engine {
        Engine {
            schema,
            device_id: device_id.into(),
            clock,
            entries: BTreeMap::new(),
            pending: BTreeMap::new(),
        }
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn clock_last(&self) -> Hlc {
        self.clock.last()
    }

    /// The current value of a key (its default until someone sets it). `None` for a key not in the schema.
    pub fn get(&self, key: &str) -> Option<Value> {
        let def = self.schema.get(key)?;
        Some(
            self.entries
                .get(key)
                .map(|e| e.value.clone())
                .unwrap_or_else(|| def.default.clone()),
        )
    }

    pub fn get_string(&self, key: &str) -> String {
        self.get(key)
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    }

    pub fn get_bool(&self, key: &str) -> bool {
        self.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
    }

    pub fn get_int(&self, key: &str) -> i64 {
        self.get(key).and_then(|v| v.as_i64()).unwrap_or(0)
    }

    pub fn entry(&self, key: &str) -> Option<&Entry> {
        self.entries.get(key)
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// A change made on this device (a click, a tap).
    pub fn set_local(&mut self, key: &str, value: &Value) -> Result<SetOutcome, Reject> {
        let def = self.schema.get(key).ok_or_else(|| {
            Reject::new(RejectCode::UnknownKey, format!("{key} is not a setting"))
        })?;
        let value = def.check(value)?;
        let scope = def.scope;
        if self.get(key).as_ref() == Some(&value) {
            return Ok(SetOutcome {
                changed: false,
                send: None,
            });
        }
        let stamp = Stamp {
            hlc: self.clock.tick(),
            device: self.device_id.clone(),
        };
        let previous = self.entries.insert(
            key.to_string(),
            Entry {
                value,
                stamp: stamp.clone(),
            },
        );
        if scope == Scope::Local {
            return Ok(SetOutcome {
                changed: true,
                send: None,
            });
        }
        match self.pending.get_mut(key) {
            Some(p) => p.latest = stamp,
            None => {
                self.pending.insert(
                    key.to_string(),
                    Pending {
                        latest: stamp,
                        baseline: previous,
                    },
                );
            }
        }
        let entry = &self.entries[key];
        Ok(SetOutcome {
            changed: true,
            send: Some(wire(key, entry)),
        })
    }

    /// A change that arrived from the other device.
    pub fn apply_remote(&mut self, incoming: &SettingEntry) -> Result<Applied, Reject> {
        let key = incoming.key.as_str();
        let def = self.schema.get(key).ok_or_else(|| {
            Reject::new(
                RejectCode::UnknownKey,
                format!("{key} is not a setting here"),
            )
        })?;
        if def.scope == Scope::Local {
            return Err(Reject::new(
                RejectCode::LocalOnly,
                format!("{key} belongs to one device and is not synced"),
            ));
        }
        let value = def.check(&incoming.value)?;
        let hlc = Hlc::from(incoming.hlc);
        self.clock.observe(hlc).map_err(|skew| {
            Reject::new(
                RejectCode::ClockSkew,
                format!(
                    "the sender's clock is {} s ahead of this device's",
                    skew.ahead_ms / 1000
                ),
            )
        })?;
        let stamp = Stamp {
            hlc,
            device: incoming.device.clone(),
        };
        let old_value = self.get(key);
        if let Some(current) = self.entries.get(key) {
            if current.stamp == stamp {
                return Ok(Applied::Unchanged);
            }
            if current.stamp > stamp {
                return Ok(Applied::Stale(wire(key, current)));
            }
        }
        if matches!(self.pending.get(key), Some(p) if p.latest < stamp) {
            self.pending.remove(key); // their write is newer than our unanswered one: it won
        }
        self.entries.insert(
            key.to_string(),
            Entry {
                value: value.clone(),
                stamp,
            },
        );
        Ok(if old_value.as_ref() == Some(&value) {
            Applied::Unchanged
        } else {
            Applied::Changed {
                key: key.to_string(),
                value,
            }
        })
    }

    /// Every synced key that was explicitly set, for the connect-time exchange.
    pub fn snapshot(&self) -> Vec<SettingEntry> {
        self.entries
            .iter()
            .filter(|(k, _)| {
                self.schema
                    .get(k)
                    .map(|d| d.scope == Scope::Synced)
                    .unwrap_or(false)
            })
            .map(|(k, e)| wire(k, e))
            .collect()
    }

    /// Merges the peer's snapshot. The result says what changed here and what the peer still needs.
    pub fn merge_snapshot(&mut self, remote: &[SettingEntry]) -> Merge {
        self.pending.clear(); // the exchange decides every key anew
        let mut merge = Merge::default();
        let mut seen = BTreeSet::new();
        for incoming in remote {
            seen.insert(incoming.key.clone());
            match self.apply_remote(incoming) {
                Ok(Applied::Changed { key, value }) => merge.changed.push((key, value)),
                Ok(Applied::Unchanged) => {}
                Ok(Applied::Stale(current)) => merge.send_back.push(current),
                Err(reject) => merge.ignored.push((incoming.key.clone(), reject)),
            }
        }
        for ours in self.snapshot() {
            if !seen.contains(&ours.key) {
                merge.send_back.push(ours);
            }
        }
        merge
    }

    /// The other side applied our change with this stamp.
    pub fn acked(&mut self, key: &str, hlc: (u64, u32), device: &str) -> bool {
        let stamp = Stamp {
            hlc: hlc.into(),
            device: device.to_string(),
        };
        if matches!(self.pending.get(key), Some(p) if p.latest == stamp) {
            self.pending.remove(key);
            true
        } else {
            false
        }
    }

    /// The other side refused our change with this stamp. Returns what to show again, or `None` when we have
    /// moved on since (a newer change of ours, or theirs, already replaced it).
    pub fn rejected(
        &mut self,
        key: &str,
        hlc: (u64, u32),
        device: &str,
        reject: &Reject,
    ) -> Option<RolledBack> {
        let stamp = Stamp {
            hlc: hlc.into(),
            device: device.to_string(),
        };
        if !matches!(self.pending.get(key), Some(p) if p.latest == stamp) {
            return None;
        }
        let pending = self.pending.remove(key)?;
        match pending.baseline {
            Some(entry) => {
                self.entries.insert(key.to_string(), entry);
            }
            None => {
                self.entries.remove(key);
            }
        }
        Some(RolledBack {
            key: key.to_string(),
            value: self.get(key)?,
            reject: reject.clone(),
        })
    }

    /// All explicitly set entries (local and synced), for saving.
    pub fn export(&self) -> Vec<(String, Entry)> {
        self.entries
            .iter()
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect()
    }

    /// Loads saved entries. Anything that is no longer valid (key removed from the schema, value out of range)
    /// is skipped; returns how many were skipped. The clock is moved past every stamp this device wrote.
    pub fn load(&mut self, saved: Vec<(String, Entry)>) -> usize {
        let mut skipped = 0;
        for (key, entry) in saved {
            let Some(def) = self.schema.get(&key) else {
                skipped += 1;
                continue;
            };
            match def.check(&entry.value) {
                Ok(value) => {
                    if entry.stamp.device == self.device_id {
                        self.clock.advance_to(entry.stamp.hlc);
                    }
                    self.entries.insert(
                        key,
                        Entry {
                            value,
                            stamp: entry.stamp,
                        },
                    );
                }
                Err(_) => skipped += 1,
            }
        }
        skipped
    }

    /// Restores the clock saved with the entries (never moves it back).
    pub fn restore_clock(&mut self, saved: Hlc) {
        self.clock.advance_to(saved);
    }

    /// Switches to another phone's synced entries (a different profile) and keeps the local ones. Returns the
    /// keys whose value changed, so the screen can refresh.
    pub fn replace_synced(&mut self, synced: Vec<(String, Entry)>) -> Vec<(String, Value)> {
        let keys: Vec<String> = self
            .schema
            .settings
            .iter()
            .filter(|d| d.scope == Scope::Synced)
            .map(|d| d.key.clone())
            .collect();
        let before: Vec<Option<Value>> = keys.iter().map(|k| self.get(k)).collect();
        for k in &keys {
            self.entries.remove(k);
        }
        self.pending.clear();
        self.load(synced);
        keys.into_iter()
            .zip(before)
            .filter_map(|(k, old)| {
                let now = self.get(&k);
                (now != old).then(|| now.map(|v| (k, v))).flatten()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    const PC: &str = "pc-0001";
    const PHONE: &str = "phone-0001";

    fn engine(id: &str, start: u64) -> (Engine, Arc<AtomicU64>) {
        let time = Arc::new(AtomicU64::new(start));
        let t = time.clone();
        let clock = HlcClock::with_source(move || t.load(Ordering::SeqCst));
        (Engine::new(Schema::builtin(), id, clock), time)
    }

    fn pair() -> (Engine, Arc<AtomicU64>, Engine, Arc<AtomicU64>) {
        let (pc, tp) = engine(PC, 1_000_000);
        let (phone, th) = engine(PHONE, 1_000_000);
        (pc, tp, phone, th)
    }

    #[test]
    fn a_local_change_travels_once_and_is_not_echoed() {
        let (mut pc, _, mut phone, _) = pair();
        let out = pc.set_local("camera.facing", &json!("front")).unwrap();
        assert!(out.changed);
        let sent = out.send.expect("a synced key is sent");
        assert_eq!(pc.get("camera.facing"), Some(json!("front")));
        assert!(pc.has_pending());
        let applied = phone.apply_remote(&sent).unwrap();
        assert_eq!(
            applied,
            Applied::Changed {
                key: "camera.facing".into(),
                value: json!("front")
            }
        );
        assert_eq!(phone.get("camera.facing"), Some(json!("front")));
        assert!(
            !phone.has_pending(),
            "applying a remote change creates nothing to send back"
        );
        // The same message arriving twice does nothing.
        assert_eq!(phone.apply_remote(&sent).unwrap(), Applied::Unchanged);
        assert!(pc.acked("camera.facing", sent.hlc, &sent.device));
        assert!(!pc.has_pending());
        // Setting the same value again is not a change.
        let again = pc.set_local("camera.facing", &json!("front")).unwrap();
        assert_eq!(
            again,
            SetOutcome {
                changed: false,
                send: None
            }
        );
    }

    #[test]
    fn the_newest_write_wins_and_both_sides_agree_on_ties() {
        let (mut pc, _, mut phone, _) = pair();
        // Both change the same key in the same millisecond: same clock value, so the device id decides.
        let from_pc = pc
            .set_local("video.rotate", &json!("90"))
            .unwrap()
            .send
            .unwrap();
        let from_phone = phone
            .set_local("video.rotate", &json!("270"))
            .unwrap()
            .send
            .unwrap();
        assert_eq!(from_pc.hlc, from_phone.hlc);
        let on_phone = phone.apply_remote(&from_pc).unwrap();
        let on_pc = pc.apply_remote(&from_phone).unwrap();
        assert_eq!(
            pc.get("video.rotate"),
            phone.get("video.rotate"),
            "same winner on both sides"
        );
        // "phone-0001" > "pc-0001" so the phone's write wins.
        assert_eq!(pc.get("video.rotate"), Some(json!("270")));
        assert!(
            matches!(on_phone, Applied::Stale(_)),
            "the phone keeps its own and tells the PC"
        );
        assert!(matches!(on_pc, Applied::Changed { .. }));
        // A stale change is answered with the newer entry, which the sender then takes.
        if let Applied::Stale(newer) = on_phone {
            assert_eq!(newer.value, json!("270"));
            assert!(matches!(
                pc.apply_remote(&newer).unwrap(),
                Applied::Unchanged
            ));
        }
    }

    #[test]
    fn a_later_write_beats_an_earlier_one_whoever_made_it() {
        let (mut pc, tp, mut phone, th) = pair();
        let first = phone
            .set_local("video.mirror", &json!("on"))
            .unwrap()
            .send
            .unwrap();
        tp.store(1_005_000, Ordering::SeqCst);
        pc.apply_remote(&first).unwrap();
        th.store(1_000_000, Ordering::SeqCst);
        let second = pc
            .set_local("video.mirror", &json!("off"))
            .unwrap()
            .send
            .unwrap();
        assert!(Hlc::from(second.hlc) > Hlc::from(first.hlc));
        phone.apply_remote(&second).unwrap();
        assert_eq!(phone.get("video.mirror"), Some(json!("off")));
        // And a phone whose own clock is behind still stamps its next write after what it has seen.
        let third = phone
            .set_local("video.mirror", &json!("on"))
            .unwrap()
            .send
            .unwrap();
        assert!(Hlc::from(third.hlc) > Hlc::from(second.hlc));
    }

    #[test]
    fn a_refused_change_rolls_back_to_the_last_agreed_value() {
        let (mut pc, _, _, _) = pair();
        let sent = pc
            .set_local("camera.facing", &json!("front"))
            .unwrap()
            .send
            .unwrap();
        let reject = Reject::new(RejectCode::Unsupported, "no front camera");
        let rolled = pc
            .rejected("camera.facing", sent.hlc, &sent.device, &reject)
            .expect("rolled back");
        assert_eq!(rolled.value, json!("back"));
        assert_eq!(pc.get("camera.facing"), Some(json!("back")));
        assert!(!pc.has_pending());
        assert!(
            pc.snapshot().is_empty(),
            "the refused value is gone, even from the snapshot"
        );
    }

    #[test]
    fn rolling_back_goes_to_the_last_acknowledged_value_not_an_unacknowledged_one() {
        let (mut pc, _, _, _) = pair();
        let a = pc
            .set_local("video.rotate", &json!("90"))
            .unwrap()
            .send
            .unwrap();
        pc.acked("video.rotate", a.hlc, &a.device);
        let b = pc
            .set_local("video.rotate", &json!("180"))
            .unwrap()
            .send
            .unwrap();
        let c = pc
            .set_local("video.rotate", &json!("270"))
            .unwrap()
            .send
            .unwrap();
        let reject = Reject::new(RejectCode::ApplyFailed, "encoder restart failed");
        // The answer to the older change B arrives late: C is already on screen, so nothing changes.
        assert!(pc
            .rejected("video.rotate", b.hlc, &b.device, &reject)
            .is_none());
        assert_eq!(pc.get("video.rotate"), Some(json!("270")));
        // C is refused too: back to A, the last value both sides agreed on, not to B.
        let rolled = pc
            .rejected("video.rotate", c.hlc, &c.device, &reject)
            .unwrap();
        assert_eq!(rolled.value, json!("90"));
    }

    #[test]
    fn a_remote_write_that_wins_replaces_our_unanswered_change() {
        let (mut pc, tp, mut phone, _) = pair();
        let mine = pc
            .set_local("video.rotate", &json!("90"))
            .unwrap()
            .send
            .unwrap();
        tp.store(1_000_500, Ordering::SeqCst);
        let theirs = phone
            .set_local("video.rotate", &json!("270"))
            .unwrap()
            .send
            .unwrap();
        assert!(Hlc::from(theirs.hlc) >= Hlc::from(mine.hlc));
        // Our change was never delivered; their newer one arrives first.
        pc.apply_remote(&theirs).unwrap();
        assert!(!pc.has_pending());
        let stale_answer = Reject::new(RejectCode::BadValue, "late");
        assert!(pc
            .rejected("video.rotate", mine.hlc, &mine.device, &stale_answer)
            .is_none());
        assert_eq!(pc.get("video.rotate"), Some(json!("270")));
    }

    #[test]
    fn local_settings_are_never_sent_or_accepted() {
        let (mut pc, _, mut phone, _) = pair();
        let out = pc.set_local("ui.language", &json!("ar")).unwrap();
        assert!(out.changed && out.send.is_none());
        assert!(pc.snapshot().is_empty());
        assert!(!pc.has_pending());
        let forged = SettingEntry {
            key: "ui.language".into(),
            value: json!("zh"),
            hlc: (9_999_999, 0),
            device: "x".into(),
        };
        assert_eq!(
            phone.apply_remote(&forged).unwrap_err().code,
            RejectCode::LocalOnly
        );
        assert_eq!(pc.get("ui.language"), Some(json!("ar")));
    }

    #[test]
    fn bad_input_is_refused_with_a_code() {
        let (mut pc, _, mut phone, _) = pair();
        assert_eq!(
            pc.set_local("nope", &json!(1)).unwrap_err().code,
            RejectCode::UnknownKey
        );
        assert_eq!(
            pc.set_local("camera.facing", &json!("up"))
                .unwrap_err()
                .code,
            RejectCode::BadValue
        );
        assert!(
            pc.entry("camera.facing").is_none(),
            "a refused value changes nothing"
        );
        let bad = SettingEntry {
            key: "video.mirror".into(),
            value: json!(7),
            hlc: (1_000_001, 0),
            device: "x".into(),
        };
        assert_eq!(
            phone.apply_remote(&bad).unwrap_err().code,
            RejectCode::BadValue
        );
        let unknown = SettingEntry {
            key: "future.thing".into(),
            value: json!(1),
            hlc: (1_000_001, 0),
            device: "x".into(),
        };
        assert_eq!(
            phone.apply_remote(&unknown).unwrap_err().code,
            RejectCode::UnknownKey
        );
        let skewed = SettingEntry {
            key: "video.mirror".into(),
            value: json!("on"),
            hlc: (9_000_000_000_000, 0),
            device: "x".into(),
        };
        assert_eq!(
            phone.apply_remote(&skewed).unwrap_err().code,
            RejectCode::ClockSkew
        );
        assert!(phone.entry("video.mirror").is_none());
    }

    #[test]
    fn connecting_sends_only_the_differences_and_converges() {
        let (mut pc, tp, mut phone, th) = pair();
        // Offline edits on both sides: the PC changed rotate and mirror, the phone changed facing and, later, mirror.
        pc.set_local("video.rotate", &json!("90")).unwrap();
        tp.store(1_000_100, Ordering::SeqCst);
        pc.set_local("video.mirror", &json!("on")).unwrap();
        th.store(1_000_050, Ordering::SeqCst);
        phone.set_local("camera.facing", &json!("front")).unwrap();
        th.store(1_000_200, Ordering::SeqCst);
        phone.set_local("video.mirror", &json!("off")).unwrap();
        // Same value agreed earlier, so it must not travel again.
        let shared = pc
            .set_local("video.auto_rotate", &json!(true))
            .unwrap()
            .send
            .unwrap();
        phone.apply_remote(&shared).unwrap();

        // Phone -> PC: the phone's snapshot, then the PC's reply with only what the phone lacks or has older.
        let merge = pc.merge_snapshot(&phone.snapshot());
        let changed: Vec<_> = merge.changed.iter().map(|(k, _)| k.as_str()).collect();
        assert!(
            changed.contains(&"camera.facing") && changed.contains(&"video.mirror"),
            "{changed:?}"
        );
        let back: Vec<_> = merge.send_back.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(
            back,
            vec!["video.rotate"],
            "only the key the phone lacks goes back"
        );
        let on_phone = phone.merge_snapshot(&merge.send_back);
        assert_eq!(
            on_phone.changed,
            vec![("video.rotate".to_string(), json!("90"))]
        );
        assert!(on_phone.send_back.iter().all(|e| [
            "camera.facing",
            "video.mirror",
            "video.auto_rotate"
        ]
        .contains(&e.key.as_str())));
        for key in [
            "camera.facing",
            "video.auto_rotate",
            "video.rotate",
            "video.mirror",
        ] {
            assert_eq!(pc.get(key), phone.get(key), "{key} converged");
        }
        assert_eq!(
            pc.get("video.mirror"),
            Some(json!("off")),
            "the later edit won"
        );
        assert!(!pc.has_pending() && !phone.has_pending());
    }

    #[test]
    fn a_snapshot_with_unknown_or_bad_entries_is_merged_around_them() {
        let (mut pc, _, _, _) = pair();
        let good = SettingEntry {
            key: "camera.facing".into(),
            value: json!("front"),
            hlc: (1_000_001, 0),
            device: PHONE.into(),
        };
        let future = SettingEntry {
            key: "audio.gain".into(),
            value: json!(3),
            hlc: (1_000_001, 0),
            device: PHONE.into(),
        };
        let bad = SettingEntry {
            key: "video.rotate".into(),
            value: json!("45"),
            hlc: (1_000_001, 0),
            device: PHONE.into(),
        };
        let merge = pc.merge_snapshot(&[future, good, bad]);
        assert_eq!(
            merge.changed,
            vec![("camera.facing".to_string(), json!("front"))]
        );
        let codes: Vec<_> = merge
            .ignored
            .iter()
            .map(|(k, r)| (k.as_str(), r.code))
            .collect();
        assert_eq!(
            codes,
            vec![
                ("audio.gain", RejectCode::UnknownKey),
                ("video.rotate", RejectCode::BadValue)
            ]
        );
        assert!(merge.send_back.is_empty());
    }

    #[test]
    fn save_and_load_keep_values_stamps_and_the_clock() {
        let (mut pc, _, _, _) = pair();
        pc.set_local("video.rotate", &json!("180")).unwrap();
        pc.set_local("ui.language", &json!("zh")).unwrap();
        let saved = pc.export();
        let last = pc.clock_last();
        let (mut fresh, _) = engine(PC, 5); // a system clock that was set far back
        let skipped = fresh.load(saved);
        assert_eq!(skipped, 0);
        assert_eq!(fresh.get("video.rotate"), Some(json!("180")));
        assert_eq!(fresh.get("ui.language"), Some(json!("zh")));
        let next = fresh
            .set_local("video.rotate", &json!("0"))
            .unwrap()
            .send
            .unwrap();
        assert!(
            Hlc::from(next.hlc) > last,
            "a clock set back does not stamp writes older than saved ones"
        );
    }

    #[test]
    fn loading_skips_what_the_schema_no_longer_allows() {
        let (mut pc, _, _, _) = pair();
        let stamp = Stamp {
            hlc: Hlc {
                wall: 1,
                counter: 0,
            },
            device: PC.into(),
        };
        let skipped = pc.load(vec![
            (
                "camera.facing".into(),
                Entry {
                    value: json!("front"),
                    stamp: stamp.clone(),
                },
            ),
            (
                "old.removed.key".into(),
                Entry {
                    value: json!(1),
                    stamp: stamp.clone(),
                },
            ),
            (
                "video.rotate".into(),
                Entry {
                    value: json!("45"),
                    stamp,
                },
            ),
        ]);
        assert_eq!(skipped, 2);
        assert_eq!(pc.get("camera.facing"), Some(json!("front")));
        assert_eq!(pc.get("video.rotate"), Some(json!("auto")));
    }

    #[test]
    fn switching_phone_profile_replaces_only_synced_values_and_reports_the_changes() {
        let (mut pc, _, _, _) = pair();
        pc.set_local("ui.language", &json!("ar")).unwrap();
        pc.set_local("video.rotate", &json!("90")).unwrap();
        pc.set_local("camera.facing", &json!("front")).unwrap();
        let other = Stamp {
            hlc: Hlc {
                wall: 7,
                counter: 0,
            },
            device: PC.into(),
        };
        let changed = pc.replace_synced(vec![(
            "camera.facing".into(),
            Entry {
                value: json!("front"),
                stamp: other,
            },
        )]);
        assert_eq!(
            changed,
            vec![("video.rotate".to_string(), json!("auto"))],
            "facing is the same in both profiles"
        );
        assert_eq!(
            pc.get("ui.language"),
            Some(json!("ar")),
            "local settings stay"
        );
        assert!(!pc.has_pending());
    }

    /// `key=value@wall.counter#device` as used by protocol/vectors/settings.txt.
    fn parse_entry(text: &str) -> SettingEntry {
        let (head, device) = text.rsplit_once('#').unwrap();
        let (head, stamp) = head.rsplit_once('@').unwrap();
        let (key, value) = head.split_once('=').unwrap();
        let (wall, counter) = stamp.split_once('.').unwrap();
        SettingEntry {
            key: key.into(),
            value: serde_json::from_str(value).unwrap(),
            hlc: (wall.parse().unwrap(), counter.parse().unwrap()),
            device: device.into(),
        }
    }

    #[test]
    fn the_shared_decision_vectors_hold() {
        let text = include_str!("../../../../protocol/vectors/settings.txt");
        let mut rows = 0;
        for line in text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        {
            let parts: Vec<&str> = line.split('|').collect();
            let (name, current, incoming, expected) = (parts[0], parts[1], parts[2], parts[3]);
            let (mut engine, _) = engine("x", 1_000_000);
            if current != "-" {
                let e = parse_entry(current);
                let stamp = Stamp {
                    hlc: e.hlc.into(),
                    device: e.device.clone(),
                };
                assert_eq!(
                    engine.load(vec![(
                        e.key.clone(),
                        Entry {
                            value: e.value,
                            stamp
                        }
                    )]),
                    0,
                    "{name}: current loads"
                );
            }
            let got = match engine.apply_remote(&parse_entry(incoming)) {
                Ok(Applied::Changed { .. }) => "changed".to_string(),
                Ok(Applied::Unchanged) => "unchanged".to_string(),
                Ok(Applied::Stale(_)) => "stale".to_string(),
                Err(reject) => format!("reject:{}", reject.code.as_str()),
            };
            assert_eq!(got, expected, "{name}");
            rows += 1;
        }
        assert!(rows >= 10, "the vector file was read");
    }

    /// Small deterministic generator so the convergence test needs no extra crate.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }

    #[test]
    fn random_offline_edits_always_converge_after_one_exchange() {
        let choices: [(&str, [Value; 3]); 3] = [
            ("video.rotate", [json!("0"), json!("90"), json!("auto")]),
            ("video.mirror", [json!("on"), json!("off"), json!("auto")]),
            (
                "camera.facing",
                [json!("front"), json!("back"), json!("front")],
            ),
        ];
        for seed in 0..200u64 {
            let mut rng = Lcg(seed + 1);
            let (mut pc, tp, mut phone, th) = pair();
            for _ in 0..rng.next(12) {
                let (key, values) = &choices[rng.next(3) as usize];
                let value = &values[rng.next(3) as usize];
                // The two clocks drift apart by a few milliseconds, and often read the same value.
                tp.store(1_000_000 + rng.next(20), Ordering::SeqCst);
                th.store(1_000_000 + rng.next(20), Ordering::SeqCst);
                if rng.next(2) == 0 {
                    let _ = pc.set_local(key, value);
                } else {
                    let _ = phone.set_local(key, value);
                }
            }
            let merge = pc.merge_snapshot(&phone.snapshot());
            phone.merge_snapshot(&merge.send_back);
            for (key, _) in &choices {
                assert_eq!(pc.get(key), phone.get(key), "seed {seed}: {key} diverged");
            }
        }
    }
}
