//! The settings schema (`protocol/settings.json`): one description of every setting that both apps load, so
//! the same keys, ranges and defaults exist on the PC and on the phone.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;

/// The schema both apps are built from.
pub const SCHEMA_JSON: &str = include_str!("../../../../protocol/settings.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Shared between the PC and the phone: a change on one side shows on the other.
    Synced,
    /// Belongs to one device and is never sent.
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Apply {
    /// Takes effect at once while streaming.
    Live,
    /// Restarts the encoder or decoder; rolled back if that fails.
    Restart,
    /// Only stored (language, source choice).
    Passive,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Bool,
    Enum(Vec<String>),
    Int { min: i64, max: i64, step: i64 },
    Float { min: f64, max: f64 },
    Str { max_len: usize },
}

/// Why a value or change is refused. The codes are the `code` of a `setting_reject` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectCode {
    UnknownKey,
    BadValue,
    LocalOnly,
    Unsupported,
    ApplyFailed,
    ClockSkew,
}

impl RejectCode {
    pub fn as_str(self) -> &'static str {
        match self {
            RejectCode::UnknownKey => "unknown_key",
            RejectCode::BadValue => "bad_value",
            RejectCode::LocalOnly => "local_only",
            RejectCode::Unsupported => "unsupported",
            RejectCode::ApplyFailed => "apply_failed",
            RejectCode::ClockSkew => "clock_skew",
        }
    }

    pub fn parse(code: &str) -> RejectCode {
        match code {
            "unknown_key" => RejectCode::UnknownKey,
            "bad_value" => RejectCode::BadValue,
            "local_only" => RejectCode::LocalOnly,
            "unsupported" => RejectCode::Unsupported,
            "clock_skew" => RejectCode::ClockSkew,
            _ => RejectCode::ApplyFailed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reject {
    pub code: RejectCode,
    pub message: String,
}

impl Reject {
    pub fn new(code: RejectCode, message: impl Into<String>) -> Self {
        Reject {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Def {
    pub key: String,
    pub group: String,
    pub kind: Kind,
    pub default: Value,
    pub scope: Scope,
    pub apply: Apply,
    /// Which apps show a control for it (`"desktop"`, `"phone"`). Every synced setting is on both.
    pub apps: Vec<String>,
}

impl Def {
    pub fn on_phone(&self) -> bool {
        self.apps.iter().any(|a| a == "phone")
    }

    pub fn on_desktop(&self) -> bool {
        self.apps.iter().any(|a| a == "desktop")
    }

    /// Checks `value` against the type and range and returns it in its canonical JSON form (so equal values
    /// compare equal whichever side wrote them).
    pub fn check(&self, value: &Value) -> Result<Value, Reject> {
        let bad = |why: String| Reject::new(RejectCode::BadValue, format!("{}: {why}", self.key));
        match &self.kind {
            Kind::Bool => value
                .as_bool()
                .map(Value::from)
                .ok_or_else(|| bad("must be true or false".into())),
            Kind::Enum(values) => match value.as_str() {
                Some(s) if values.iter().any(|v| v == s) => Ok(value.clone()),
                _ => Err(bad(format!("must be one of {}", values.join(", ")))),
            },
            Kind::Int { min, max, step } => {
                let v = value
                    .as_i64()
                    .ok_or_else(|| bad("must be a whole number".into()))?;
                if v < *min || v > *max {
                    return Err(bad(format!("must be between {min} and {max}")));
                }
                if *step > 1 && (v - min) % step != 0 {
                    return Err(bad(format!("must be {min} plus a multiple of {step}")));
                }
                Ok(Value::from(v))
            }
            Kind::Float { min, max } => {
                let v = value
                    .as_f64()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| bad("must be a number".into()))?;
                if v < *min || v > *max {
                    return Err(bad(format!("must be between {min} and {max}")));
                }
                Ok(Value::from(v))
            }
            Kind::Str { max_len } => match value.as_str() {
                Some(s) if s.chars().count() <= *max_len && !s.chars().any(char::is_control) => {
                    Ok(value.clone())
                }
                Some(_) => Err(bad(format!(
                    "must be at most {max_len} characters, without control characters"
                ))),
                None => Err(bad("must be text".into())),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Schema {
    pub version: u32,
    pub groups: Vec<String>,
    pub settings: Vec<Def>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SchemaError(pub String);

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid settings schema: {}", self.0)
    }
}

impl std::error::Error for SchemaError {}

#[derive(Deserialize)]
struct RawSchema {
    schema_version: u32,
    groups: Vec<String>,
    settings: Vec<RawDef>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDef {
    key: String,
    group: String,
    #[serde(rename = "type")]
    ty: String,
    values: Option<Vec<String>>,
    min: Option<f64>,
    max: Option<f64>,
    step: Option<i64>,
    max_len: Option<usize>,
    default: Value,
    scope: Scope,
    apply: Apply,
    apps: Option<Vec<String>>,
}

impl Schema {
    /// The schema compiled into this build. Panics only if `protocol/settings.json` is broken, which the tests catch.
    pub fn builtin() -> Schema {
        Schema::parse(SCHEMA_JSON).expect("protocol/settings.json is valid")
    }

    pub fn parse(json: &str) -> Result<Schema, SchemaError> {
        let raw: RawSchema = serde_json::from_str(json).map_err(|e| SchemaError(e.to_string()))?;
        let mut seen = BTreeSet::new();
        let mut settings = Vec::new();
        for r in raw.settings {
            let err = |why: &str| SchemaError(format!("{}: {why}", r.key));
            let valid_key = !r.key.is_empty()
                && r.key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_');
            if !valid_key {
                return Err(err("keys use lowercase letters, digits, '.' and '_'"));
            }
            if !seen.insert(r.key.clone()) {
                return Err(err("duplicate key"));
            }
            if !raw.groups.contains(&r.group) {
                return Err(err("group is not listed in \"groups\""));
            }
            let kind = match r.ty.as_str() {
                "bool" => Kind::Bool,
                "enum" => {
                    let values = r
                        .values
                        .clone()
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| err("enum needs \"values\""))?;
                    let unique: BTreeSet<_> = values.iter().collect();
                    if unique.len() != values.len() {
                        return Err(err("enum values must be unique"));
                    }
                    Kind::Enum(values)
                }
                "int" => {
                    let (min, max) = (
                        r.min.ok_or_else(|| err("int needs min"))?,
                        r.max.ok_or_else(|| err("int needs max"))?,
                    );
                    if min.fract() != 0.0 || max.fract() != 0.0 || min > max {
                        return Err(err("int range must be whole numbers with min <= max"));
                    }
                    Kind::Int {
                        min: min as i64,
                        max: max as i64,
                        step: r.step.unwrap_or(1).max(1),
                    }
                }
                "float" => {
                    let (min, max) = (
                        r.min.ok_or_else(|| err("float needs min"))?,
                        r.max.ok_or_else(|| err("float needs max"))?,
                    );
                    if !(min.is_finite() && max.is_finite() && min <= max) {
                        return Err(err("float range needs finite min <= max"));
                    }
                    Kind::Float { min, max }
                }
                "string" => Kind::Str {
                    max_len: r.max_len.ok_or_else(|| err("string needs max_len"))?,
                },
                _ => return Err(err("unknown type")),
            };
            let apps = r
                .apps
                .clone()
                .unwrap_or_else(|| vec!["desktop".to_string(), "phone".to_string()]);
            if apps.is_empty() || apps.iter().any(|a| a != "desktop" && a != "phone") {
                return Err(err("apps lists \"desktop\" and/or \"phone\""));
            }
            if r.scope == Scope::Synced && apps.len() != 2 {
                return Err(err("a synced setting must be on both apps"));
            }
            let mut def = Def {
                key: r.key.clone(),
                group: r.group.clone(),
                kind,
                default: Value::Null,
                scope: r.scope,
                apply: r.apply,
                apps,
            };
            def.default = def
                .check(&r.default)
                .map_err(|e| SchemaError(format!("default of {}", e.message)))?;
            settings.push(def);
        }
        Ok(Schema {
            version: raw.schema_version,
            groups: raw.groups,
            settings,
        })
    }

    pub fn get(&self, key: &str) -> Option<&Def> {
        self.settings.iter().find(|d| d.key == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_shipped_schema_is_valid_and_complete() {
        let schema = Schema::builtin();
        assert_eq!(schema.version, 1);
        for key in [
            "camera.facing",
            "video.auto_rotate",
            "video.rotate",
            "video.mirror",
            "ui.language",
            "source.mode",
            "output.device",
        ] {
            assert!(schema.get(key).is_some(), "{key} is in the schema");
        }
        assert_eq!(schema.get("camera.facing").unwrap().scope, Scope::Synced);
        assert_eq!(schema.get("ui.language").unwrap().scope, Scope::Local);
        assert_eq!(schema.get("camera.facing").unwrap().default, json!("back"));
        let device = schema.get("output.device").unwrap();
        assert!(device.on_desktop() && !device.on_phone());
        assert!(schema
            .settings
            .iter()
            .filter(|d| d.scope == Scope::Synced)
            .all(|d| d.on_desktop() && d.on_phone()));
    }

    fn one(def: &str) -> Result<Schema, SchemaError> {
        Schema::parse(&format!(
            r#"{{"schema_version":1,"groups":["g"],"settings":[{def}]}}"#
        ))
    }

    #[test]
    fn values_are_checked_against_type_and_range() {
        let schema = one(r#"{"key":"zoom.level","group":"g","type":"float","min":1,"max":8,"default":1,"scope":"synced","apply":"live"},
            {"key":"video.fps","group":"g","type":"int","min":15,"max":60,"step":15,"default":30,"scope":"synced","apply":"restart"}"#)
        .unwrap();
        let zoom = schema.get("zoom.level").unwrap();
        assert_eq!(
            zoom.check(&json!(2)).unwrap(),
            json!(2.0),
            "whole numbers are accepted for floats"
        );
        assert!(
            zoom.check(&json!(9.0)).is_err()
                && zoom.check(&json!("2")).is_err()
                && zoom.check(&json!(null)).is_err()
        );
        let fps = schema.get("video.fps").unwrap();
        assert!(fps.check(&json!(45)).is_ok());
        assert!(fps.check(&json!(40)).is_err(), "not on the step");
        assert!(fps.check(&json!(30.5)).is_err() && fps.check(&json!(75)).is_err());
        let facing = Schema::builtin();
        let facing = facing.get("camera.facing").unwrap();
        assert!(facing.check(&json!("front")).is_ok());
        assert_eq!(
            facing.check(&json!("side")).unwrap_err().code,
            RejectCode::BadValue
        );
        assert!(facing.check(&json!(true)).is_err());
        let device = Schema::builtin();
        let device = device.get("output.device").unwrap();
        assert!(device.check(&json!("/dev/video3")).is_ok());
        assert!(device.check(&json!("a".repeat(65))).is_err());
        assert!(device.check(&json!("bad\nline")).is_err());
    }

    #[test]
    fn a_broken_schema_is_refused_with_the_reason() {
        let cases = [
            (
                r#"{"key":"A","group":"g","type":"bool","default":false,"scope":"local","apply":"passive"}"#,
                "lowercase",
            ),
            (
                r#"{"key":"a","group":"nope","type":"bool","default":false,"scope":"local","apply":"passive"}"#,
                "group",
            ),
            (
                r#"{"key":"a","group":"g","type":"enum","values":[],"default":"x","scope":"local","apply":"passive"}"#,
                "values",
            ),
            (
                r#"{"key":"a","group":"g","type":"enum","values":["x","x"],"default":"x","scope":"local","apply":"passive"}"#,
                "unique",
            ),
            (
                r#"{"key":"a","group":"g","type":"bool","default":"yes","scope":"local","apply":"passive"}"#,
                "default",
            ),
            (
                r#"{"key":"a","group":"g","type":"int","min":5,"max":1,"default":3,"scope":"local","apply":"passive"}"#,
                "range",
            ),
            (
                r#"{"key":"a","group":"g","type":"colour","default":1,"scope":"local","apply":"passive"}"#,
                "type",
            ),
        ];
        for (def, needle) in cases {
            let err = one(def).unwrap_err();
            assert!(err.0.contains(needle), "{def} -> {err}");
        }
        let lonely = r#"{"key":"a","group":"g","type":"bool","default":false,"scope":"synced","apply":"live","apps":["desktop"]}"#;
        assert!(one(lonely).unwrap_err().0.contains("both"));
        let twice = r#"{"key":"a","group":"g","type":"bool","default":false,"scope":"local","apply":"passive"}"#;
        assert!(one(&format!("{twice},{twice}"))
            .unwrap_err()
            .0
            .contains("duplicate"));
        assert!(Schema::parse("not json").is_err());
    }

    #[test]
    fn reject_codes_round_trip() {
        for code in [
            RejectCode::UnknownKey,
            RejectCode::BadValue,
            RejectCode::LocalOnly,
            RejectCode::Unsupported,
            RejectCode::ApplyFailed,
            RejectCode::ClockSkew,
        ] {
            assert_eq!(RejectCode::parse(code.as_str()), code);
        }
        assert_eq!(RejectCode::parse("something new"), RejectCode::ApplyFailed);
    }
}
