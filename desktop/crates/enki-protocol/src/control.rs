//! Control channel messages: JSON objects with a `"type"` field, each sent as a big-endian `u32` length followed
//! by the UTF-8 JSON (max [`MAX_CONTROL`] bytes). Unknown message types decode to [`Message::Unknown`] and unknown
//! fields are ignored, so newer peers can talk to older ones. See `docs/protocol.md`.

use serde::{Deserialize, Serialize};
pub use serde_json::Value;

pub const MAX_CONTROL: usize = 1024 * 1024;

/// What one phone camera can do. Everything the PC shows as a choice comes from here.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraCaps {
    /// The phone's camera id (opaque).
    pub id: String,
    /// `"back"`, `"front"` or `"external"`.
    pub facing: String,
    /// Fixed sensor rotation in degrees (0, 90, 180, 270).
    pub sensor_orientation: u16,
    /// Video sizes the camera can feed the encoder, as `[width, height]`, largest first.
    pub sizes: Vec<[u32; 2]>,
    /// Frame-rate ranges as `[min, max]`.
    pub fps_ranges: Vec<[u32; 2]>,
    /// Largest zoom ratio (1.0 = no zoom).
    pub zoom_max: f32,
    pub flash: bool,
    /// Subset of `"continuous"`, `"auto"`, `"manual"`.
    pub focus_modes: Vec<String>,
    /// Exposure compensation range in steps, and the size of one step in EV.
    pub ev_range: [i32; 2],
    pub ev_step: f32,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub cameras: Vec<CameraCaps>,
    /// Video codecs the phone can encode: `"h264"`, `"hevc"`.
    pub codecs: Vec<String>,
}

impl Capabilities {
    pub fn camera(&self, facing: &str) -> Option<&CameraCaps> {
        self.cameras.iter().find(|c| c.facing == facing)
    }
}

/// One setting value with the stamp of the write that produced it (docs/protocol.md, "Settings sync").
/// `hlc` is the hybrid logical clock `[wall_ms, counter]`; `device` is the id of the app that wrote it.
/// The newest `(hlc, device)` pair wins on both sides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingEntry {
    pub key: String,
    pub value: Value,
    pub hlc: (u64, u32),
    pub device: String,
}

/// Feature flag in `hello.features`: the peer speaks the settings messages.
pub const FEATURE_SETTINGS: &str = "settings";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// Phone → PC, first message after authentication.
    Hello {
        version: u32,
        #[serde(default)]
        app: String,
        #[serde(default)]
        app_version: String,
        #[serde(default)]
        device: String,
        #[serde(default)]
        android: u32,
        /// The phone app's own random id (stable across launches). Empty for an app that predates settings sync.
        #[serde(default)]
        device_id: String,
        /// Optional features, e.g. [`FEATURE_SETTINGS`].
        #[serde(default)]
        features: Vec<String>,
    },
    /// Phone → PC, right after `hello`.
    Capabilities(Capabilities),
    /// PC → phone: change one setting.
    Set {
        key: String,
        value: Value,
    },
    /// Phone → PC: a setting's current value.
    State {
        key: String,
        value: Value,
    },
    Error {
        code: String,
        #[serde(default)]
        message: String,
    },
    /// PC → phone, once a second. The phone answers with a [`Message::Pong`] carrying the same `seq`.
    Ping {
        seq: u64,
    },
    Pong {
        seq: u64,
    },
    /// Both directions: every synced setting the sender has explicitly set. The phone sends one right after its
    /// capabilities; the PC merges it and answers with a snapshot of only the entries where its own write is
    /// newer or the phone has none (`reply: true`, which the phone applies and never answers).
    SettingsSnapshot {
        #[serde(default)]
        schema: u32,
        #[serde(default)]
        reply: bool,
        #[serde(default)]
        settings: Vec<SettingEntry>,
    },
    /// Both directions: one setting was changed by the sender.
    Setting(SettingEntry),
    /// The receiver applied the change with this stamp (a stale change is answered with the newer `setting` instead).
    SettingAck {
        key: String,
        hlc: (u64, u32),
        device: String,
    },
    /// The receiver refused the change with this stamp; the sender rolls the setting back.
    /// `code`: `unknown_key`, `bad_value`, `local_only`, `unsupported`, `apply_failed`, `clock_skew`.
    SettingReject {
        key: String,
        hlc: (u64, u32),
        device: String,
        code: String,
        #[serde(default)]
        message: String,
    },
    /// PC → phone, on a Wi-Fi connection that proved a pairing key, or on the adb connection: "make me a paired
    /// computer". `device` is the PC's settings device id and `name` its host name, shown on the phone.
    Pair {
        #[serde(default)]
        name: String,
        #[serde(default)]
        device: String,
    },
    /// Phone → PC, the answer to `pair`: the secret that from now on proves this PC to the phone on Wi-Fi, the
    /// SHA-256 of the phone's certificate (both hex), and where to find the phone.
    Paired {
        secret: String,
        fingerprint: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        device_id: String,
        #[serde(default)]
        port: u16,
        #[serde(default)]
        hosts: Vec<String>,
    },
    /// Any type this version does not know.
    #[serde(other)]
    Unknown,
}

impl Message {
    pub fn set(key: &str, value: impl Into<Value>) -> Message {
        Message::Set {
            key: key.to_string(),
            value: value.into(),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("control messages always serialize")
    }

    pub fn from_json(text: &str) -> Result<Message, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// The length-prefixed wire form.
    pub fn encode(&self) -> Vec<u8> {
        frame(&self.to_json())
    }
}

/// Wire form of any JSON text: `u32` big-endian length, then the bytes.
pub fn frame(json: &str) -> Vec<u8> {
    let body = json.as_bytes();
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vectors() -> Vec<(String, String)> {
        include_str!("../../../../protocol/vectors/control.txt")
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(|l| {
                let (name, json) = l.split_once('|').unwrap();
                (name.to_string(), json.to_string())
            })
            .collect()
    }

    fn vector(name: &str) -> String {
        vectors()
            .into_iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no vector {name}"))
            .1
    }

    const PHONE_ID: &str = "0123456789abcdef0123456789abcdef";
    const PC_ID: &str = "fedcba9876543210fedcba9876543210";

    fn entry(key: &str, value: Value, hlc: (u64, u32), device: &str) -> SettingEntry {
        SettingEntry {
            key: key.into(),
            value,
            hlc,
            device: device.into(),
        }
    }

    /// The camera the Android tests encode into the `capabilities` vector.
    fn sample_caps() -> Capabilities {
        Capabilities {
            cameras: vec![CameraCaps {
                id: "0".into(),
                facing: "back".into(),
                sensor_orientation: 90,
                sizes: vec![[1920, 1080], [1280, 720]],
                fps_ranges: vec![[15, 30], [30, 30]],
                zoom_max: 8.0,
                flash: true,
                focus_modes: vec!["continuous".into(), "auto".into()],
                ev_range: [-4, 4],
                ev_step: 0.5,
            }],
            codecs: vec!["h264".into(), "hevc".into()],
        }
    }

    #[test]
    fn shared_vectors_decode_to_the_expected_messages() {
        assert_eq!(
            Message::from_json(&vector("hello")).unwrap(),
            Message::Hello {
                version: 1,
                app: "enki-android".into(),
                app_version: "0.2.0".into(),
                device: "Pixel 6".into(),
                android: 28,
                device_id: PHONE_ID.into(),
                features: vec![FEATURE_SETTINGS.into()],
            }
        );
        // An app from before settings sync has no id and no features.
        assert_eq!(
            Message::from_json(&vector("hello_legacy")).unwrap(),
            Message::Hello {
                version: 1,
                app: "enki-android".into(),
                app_version: "0.2.0".into(),
                device: "Pixel 6".into(),
                android: 28,
                device_id: String::new(),
                features: vec![],
            }
        );
        assert_eq!(
            Message::from_json(&vector("capabilities")).unwrap(),
            Message::Capabilities(sample_caps())
        );
        assert_eq!(
            Message::from_json(&vector("ping")).unwrap(),
            Message::Ping { seq: 7 }
        );
        assert_eq!(
            Message::from_json(&vector("pong")).unwrap(),
            Message::Pong { seq: 7 }
        );
        assert_eq!(
            Message::from_json(&vector("state_rotation")).unwrap(),
            Message::State {
                key: "image.rotation".into(),
                value: 180.into()
            }
        );
        assert_eq!(
            Message::from_json(&vector("state_bitrate")).unwrap(),
            Message::State {
                key: "stream.bitrate".into(),
                value: 3000.into()
            }
        );
        assert_eq!(
            Message::from_json(&vector("set_facing")).unwrap(),
            Message::set("camera.facing", "front")
        );
        assert_eq!(
            Message::from_json(&vector("error")).unwrap(),
            Message::Error {
                code: "bad_value".into(),
                message: "camera.facing must be \"front\" or \"back\"".into()
            }
        );
    }

    #[test]
    fn settings_messages_decode_and_encode_like_the_shared_vectors() {
        let snapshot = Message::SettingsSnapshot {
            schema: 1,
            reply: false,
            settings: vec![
                entry(
                    "camera.facing",
                    "front".into(),
                    (1_700_000_000_000, 2),
                    PHONE_ID,
                ),
                entry(
                    "video.auto_rotate",
                    true.into(),
                    (1_700_000_000_500, 0),
                    PHONE_ID,
                ),
            ],
        };
        let reply = Message::SettingsSnapshot {
            schema: 1,
            reply: true,
            settings: vec![entry(
                "video.rotate",
                "90".into(),
                (1_700_000_001_000, 0),
                PC_ID,
            )],
        };
        let setting = Message::Setting(entry(
            "video.rotate",
            "90".into(),
            (1_700_000_001_000, 0),
            PC_ID,
        ));
        let ack = Message::SettingAck {
            key: "video.rotate".into(),
            hlc: (1_700_000_001_000, 0),
            device: PC_ID.into(),
        };
        let reject = Message::SettingReject {
            key: "camera.facing".into(),
            hlc: (1_700_000_002_000, 1),
            device: PC_ID.into(),
            code: "unsupported".into(),
            message: "this phone has no front camera".into(),
        };
        let as_value = |m: &Message| serde_json::from_str::<Value>(&m.to_json()).unwrap();
        let from_vector = |n: &str| serde_json::from_str::<Value>(&vector(n)).unwrap();
        for (name, message) in [
            ("settings_snapshot", snapshot),
            ("settings_snapshot_reply", reply),
            ("setting", setting),
            ("setting_ack", ack),
            ("setting_reject", reject),
        ] {
            assert_eq!(
                Message::from_json(&vector(name)).unwrap(),
                message,
                "{name} decodes"
            );
            assert_eq!(as_value(&message), from_vector(name), "{name} encodes");
        }
    }

    #[test]
    fn a_snapshot_without_optional_parts_still_decodes() {
        let m = Message::from_json(r#"{"type":"settings_snapshot"}"#).unwrap();
        assert_eq!(
            m,
            Message::SettingsSnapshot {
                schema: 0,
                reply: false,
                settings: vec![]
            }
        );
        assert!(
            Message::from_json(r#"{"type":"setting","key":"a"}"#).is_err(),
            "a setting needs value, hlc and device"
        );
    }

    #[test]
    fn what_we_send_matches_the_vectors_the_phone_parses() {
        // Both sides compare JSON values, so key order and spacing do not matter.
        let as_value = |m: &Message| serde_json::from_str::<Value>(&m.to_json()).unwrap();
        let from_vector = |n: &str| serde_json::from_str::<Value>(&vector(n)).unwrap();
        assert_eq!(as_value(&Message::Ping { seq: 7 }), from_vector("ping"));
        assert_eq!(
            as_value(&Message::set("camera.facing", "front")),
            from_vector("set_facing")
        );
        assert_eq!(
            as_value(&Message::Capabilities(sample_caps())),
            from_vector("capabilities")
        );
    }

    #[test]
    fn pairing_messages_match_the_shared_vectors() {
        let as_value = |m: &Message| serde_json::from_str::<Value>(&m.to_json()).unwrap();
        let from_vector = |n: &str| serde_json::from_str::<Value>(&vector(n)).unwrap();
        let pair = Message::Pair {
            name: "my-laptop".into(),
            device: PC_ID.into(),
        };
        assert_eq!(Message::from_json(&vector("pair")).unwrap(), pair);
        assert_eq!(as_value(&pair), from_vector("pair"));
        let paired = Message::Paired {
            secret: (0..32u8).map(|b| format!("{b:02x}")).collect(),
            fingerprint: (100..132u8).map(|b| format!("{b:02x}")).collect(),
            name: "Pixel 6".into(),
            device_id: PHONE_ID.into(),
            port: 28201,
            hosts: vec!["192.168.1.20".into(), "10.0.0.5".into()],
        };
        assert_eq!(Message::from_json(&vector("paired")).unwrap(), paired);
        assert_eq!(as_value(&paired), from_vector("paired"));
    }

    #[test]
    fn unknown_types_and_fields_are_ignored() {
        assert_eq!(
            Message::from_json(&vector("future_type")).unwrap(),
            Message::Unknown
        );
        let m = Message::from_json(r#"{"type":"pong","seq":3,"extra":{"a":1}}"#).unwrap();
        assert_eq!(m, Message::Pong { seq: 3 });
        // A capabilities message from a newer phone with fields we do not know, and one missing optional parts.
        let m = Message::from_json(
            r#"{"type":"capabilities","cameras":[{"id":"1","facing":"front","hdr":true}]}"#,
        )
        .unwrap();
        match m {
            Message::Capabilities(c) => {
                assert_eq!(c.cameras[0].facing, "front");
                assert!(c.cameras[0].sizes.is_empty() && c.codecs.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(Message::from_json("not json").is_err());
        assert!(Message::from_json(r#"{"no_type":1}"#).is_err());
        assert!(
            Message::from_json(r#"{"type":"ping"}"#).is_err(),
            "ping needs a seq"
        );
    }

    #[test]
    fn framing_is_length_prefixed() {
        let bytes = Message::Ping { seq: 1 }.encode();
        let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        assert_eq!(len, bytes.len() - 4);
        assert_eq!(&bytes[4..], br#"{"type":"ping","seq":1}"#);
    }

    #[test]
    fn camera_lookup_by_facing() {
        let caps = sample_caps();
        assert_eq!(caps.camera("back").unwrap().id, "0");
        assert!(caps.camera("front").is_none());
    }
}
