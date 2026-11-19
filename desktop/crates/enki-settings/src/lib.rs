//! Enki settings: one schema (`protocol/settings.json`), a validating two-way sync [`engine`], and TOML
//! storage with one profile per phone. See `docs/protocol.md` ("Settings sync") for the wire side.

pub mod clock;
pub mod engine;
pub mod hub;
pub mod schema;
pub mod store;

pub use clock::{ClockSkew, Hlc, HlcClock};
pub use engine::{Applied, Engine, Entry, Merge, RolledBack, SetOutcome, Stamp};
pub use hub::{random_device_id, Hub, SettingsHandle};
pub use schema::{Apply, Def, Kind, Reject, RejectCode, Schema, SchemaError, Scope};
pub use store::{valid_profile_id, PhoneProfile, Saved, Store, DEFAULT_PROFILE};
