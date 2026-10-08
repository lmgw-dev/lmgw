//! A level of lmgw's admin tools, as the wire says it.

use serde::{Deserialize, Serialize};

/// A level of lmgw's admin tools, written `off`, `read_only` or `full`:
/// what a device's admin tools may do now (the change feed's
/// `hello.self_admin` and `state.self_admin`), the level set on a device
/// key (`KeyRow.self_admin`, and `self_admin` in `key_create` and
/// `key_set`), or the gateway's own (`GatewayStatus.self_admin`,
/// `SettingsFull.self_admin`).
///
/// Forward compatible like every enum a client reads: a value a newer
/// gateway sends reads as [`AdminLevel::Unknown`], as sent, and writes back
/// unchanged. The document lists the known values only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub enum AdminLevel {
    /// No admin tools.
    #[default]
    Off,
    /// Reads lmgw's configuration and state.
    ReadOnly,
    /// Also changes them.
    Full,
    /// A value this build does not know (a newer gateway's), as sent: it
    /// writes back unchanged.
    #[serde(untagged)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unknown(String),
}

impl AdminLevel {
    /// The wire spelling: `off`, `read_only`, `full`, or the unknown value
    /// as sent.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Off => "off",
            Self::ReadOnly => "read_only",
            Self::Full => "full",
            Self::Unknown(v) => v,
        }
    }
}

impl std::fmt::Display for AdminLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_feed::{Hello, LiveState};
    use crate::{KeyRow, SettingsFull};
    use serde_json::json;

    const LEVELS: [(AdminLevel, &str); 3] = [
        (AdminLevel::Off, "off"),
        (AdminLevel::ReadOnly, "read_only"),
        (AdminLevel::Full, "full"),
    ];

    /// The bytes are the strings `self_admin` always carried, in `hello`,
    /// `state` and a key row alike, and read back as the known variants.
    #[test]
    fn the_wire_bytes_are_the_strings_they_always_were() {
        for (level, word) in LEVELS {
            assert_eq!(
                serde_json::to_string(&level).unwrap(),
                format!("\"{word}\"")
            );
            assert_eq!(level.as_str(), word);
            let back: AdminLevel = serde_json::from_str(&format!("\"{word}\"")).unwrap();
            assert_eq!(back, level);
            let pinned = format!("\"self_admin\":\"{word}\"");
            let hello = Hello {
                self_admin: level.clone(),
                ..Default::default()
            };
            assert!(serde_json::to_string(&hello).unwrap().contains(&pinned));
            let state = LiveState {
                self_admin: level.clone(),
                ..Default::default()
            };
            assert!(serde_json::to_string(&state).unwrap().contains(&pinned));
            let row = KeyRow {
                self_admin: level.clone(),
                ..Default::default()
            };
            assert!(serde_json::to_string(&row).unwrap().contains(&pinned));
            let settings = SettingsFull {
                self_admin: level.clone(),
                ..Default::default()
            };
            assert!(serde_json::to_string(&settings).unwrap().contains(&pinned));
        }
        // A body that leaves it out reads as off, as the string read empty
        // before and a feed that is not a device's says.
        let state: LiveState = serde_json::from_value(json!({})).unwrap();
        assert_eq!(state.self_admin, AdminLevel::Off);
    }

    #[test]
    fn a_newer_gateway_s_level_reads_and_writes_back_as_sent() {
        let state: LiveState = serde_json::from_value(json!({"self_admin": "admin"})).unwrap();
        assert_eq!(state.self_admin, AdminLevel::Unknown("admin".into()));
        assert_eq!(
            serde_json::to_value(&state).unwrap()["self_admin"],
            json!("admin")
        );
        assert_eq!(state.self_admin.to_string(), "admin");
    }

    #[cfg(feature = "schema")]
    #[test]
    fn the_document_lists_the_known_levels_only() {
        let s = serde_json::to_value(schemars::schema_for!(AdminLevel)).unwrap();
        let list = s.get("oneOf").or(s.get("anyOf")).expect("a list of values");
        let values: Vec<_> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["const"].clone())
            .collect();
        assert_eq!(values, [json!("off"), json!("read_only"), json!("full")]);
    }
}
