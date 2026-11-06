//! The media tools' default models (chat-media-tools design §3.8): which
//! model `image__generate`, `image__edit`, `audio__speak` and
//! `audio__transcribe` use when a call leaves out `model`, and the voice
//! `audio__speak` uses when it leaves out `voice`.
//!
//! Three levels, the narrowest that says something wins: a Chat thread's
//! own, its folder's, Settings → Chat's. Each level is one
//! [`MediaDefaults`]. In Settings an absent or empty field is "no default —
//! the model chooses"; in a folder or a thread an absent field inherits the
//! level above, and an empty one is "no default" whatever the levels above
//! say.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One level's default models, by tool (module doc).
///
/// Input is strict (`deny_unknown_fields`): a write that names a field this
/// build does not know is refused with the fields it does, rather than
/// stored as nothing. A stored level is read through
/// [`MediaDefaults::from_stored`], which drops such a key on its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MediaDefaults {
    /// `image__generate`'s model, as its `model` enum names it
    /// (`image/<id>` or an alias).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generate: Option<String>,
    /// `image__edit`'s model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit: Option<String>,
    /// `audio__speak`'s model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speak: Option<String>,
    /// The voice `audio__speak` uses with the default speech model when a
    /// call names none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// `audio__transcribe`'s model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcribe: Option<String>,
}

/// One field of [`MediaDefaults`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum MediaField {
    Generate,
    Edit,
    Speak,
    Voice,
    Transcribe,
}

impl MediaField {
    pub const ALL: [Self; 5] = [
        Self::Generate,
        Self::Edit,
        Self::Speak,
        Self::Voice,
        Self::Transcribe,
    ];
    /// The four that name a model.
    pub const MODELS: [Self; 4] = [Self::Generate, Self::Edit, Self::Speak, Self::Transcribe];

    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Generate => "generate",
            Self::Edit => "edit",
            Self::Speak => "speak",
            Self::Voice => "voice",
            Self::Transcribe => "transcribe",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == s)
    }

    /// The tool whose `model` it is; the voice is `audio__speak`'s.
    pub fn tool(self) -> &'static str {
        match self {
            Self::Generate => "image__generate",
            Self::Edit => "image__edit",
            Self::Speak | Self::Voice => "audio__speak",
            Self::Transcribe => "audio__transcribe",
        }
    }

    /// What a person reads beside its picker.
    pub fn label(self) -> &'static str {
        match self {
            Self::Generate => "Drawing pictures",
            Self::Edit => "Editing pictures",
            Self::Speak => "Speaking text",
            Self::Voice => "Voice",
            Self::Transcribe => "Transcribing recordings",
        }
    }
}

impl MediaDefaults {
    /// A stored level, read tolerantly: a key this build does not know is
    /// dropped on its own, and anything but an object (`null` included) is
    /// no defaults.
    pub fn from_stored(v: serde_json::Value) -> Self {
        let serde_json::Value::Object(mut map) = v else {
            return Self::default();
        };
        map.retain(|k, _| MediaField::parse(k).is_some());
        // A field whose value is not a string is dropped on its own too.
        map.retain(|_, v| v.is_string() || v.is_null());
        serde_json::from_value(serde_json::Value::Object(map)).unwrap_or_default()
    }

    /// [`Self::from_stored`] as a serde field reader, for a level stored
    /// inside a larger blob (Settings'): a key a newer build added must not
    /// fail the whole blob.
    pub fn deserialize_stored<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        Ok(Self::from_stored(v))
    }

    pub fn get(&self, f: MediaField) -> Option<&str> {
        match f {
            MediaField::Generate => self.generate.as_deref(),
            MediaField::Edit => self.edit.as_deref(),
            MediaField::Speak => self.speak.as_deref(),
            MediaField::Voice => self.voice.as_deref(),
            MediaField::Transcribe => self.transcribe.as_deref(),
        }
    }

    pub fn slot(&mut self, f: MediaField) -> &mut Option<String> {
        match f {
            MediaField::Generate => &mut self.generate,
            MediaField::Edit => &mut self.edit,
            MediaField::Speak => &mut self.speak,
            MediaField::Voice => &mut self.voice,
            MediaField::Transcribe => &mut self.transcribe,
        }
    }

    /// Nothing set at all: a folder or thread that inherits everything.
    pub fn is_empty(&self) -> bool {
        MediaField::ALL.iter().all(|f| self.get(*f).is_none())
    }

    /// Every value trimmed; a field Settings leaves empty is absent there
    /// (`settings`), while a folder's or a thread's empty one stays: it
    /// says "no default" (module doc).
    pub fn normalised(mut self, settings: bool) -> Self {
        for f in MediaField::ALL {
            let slot = self.slot(f);
            *slot = slot
                .take()
                .map(|v| v.trim().to_string())
                .filter(|v| !(settings && v.is_empty()));
        }
        self
    }

    /// `over` laid on `self`: each field `over` says replaces this one's.
    pub fn laid_over(mut self, over: &MediaDefaults) -> Self {
        for f in MediaField::ALL {
            if let Some(v) = over.get(f) {
                *self.slot(f) = Some(v.to_string());
            }
        }
        self
    }
}

/// One model a media tool can use, as a picker lists it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MediaChoice {
    /// The name the tool's `model` takes and a default stores.
    pub name: String,
    /// What a person reads: the name, and what it is (its family, on this
    /// machine, a cloud model via its upstream).
    pub label: String,
    /// One of lmgw's own rows rather than an alias.
    pub local: bool,
}

/// What each media tool can use, for the default pickers: the same rows
/// and aliases its `model` enum lists for the caller.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MediaChoices {
    pub generate: Vec<MediaChoice>,
    pub edit: Vec<MediaChoice>,
    pub speak: Vec<MediaChoice>,
    pub transcribe: Vec<MediaChoice>,
    /// The voices of each speech row, by its name; a cloud alias has none
    /// listed (its voices are the provider's own).
    pub voices: BTreeMap<String, Vec<String>>,
}

impl MediaChoices {
    /// The choices of a model field; none for the voice.
    pub fn of(&self, f: MediaField) -> &[MediaChoice] {
        match f {
            MediaField::Generate => &self.generate,
            MediaField::Edit => &self.edit,
            MediaField::Speak => &self.speak,
            MediaField::Transcribe => &self.transcribe,
            MediaField::Voice => &[],
        }
    }
}

/// A stored default that does not resolve now, and why: the model was
/// switched off or deleted, an alias lost the route, the caller's key may
/// not use it, a voice its model does not have.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MediaUnavailable {
    /// `generate`, `edit`, `speak`, `voice` or `transcribe`.
    pub field: String,
    /// The stored name.
    pub name: String,
    pub reason: String,
}

/// `GET /chat/api/media/defaults`: the media tools' defaults at each level
/// asked for, what they resolve to, and what the pickers offer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MediaDefaultsView {
    /// What each tool can use, for the caller.
    pub choices: MediaChoices,
    /// Settings → Chat's defaults, as stored.
    pub settings: MediaDefaults,
    /// The folder's own, when a folder (or a thread in one) was asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<MediaDefaults>,
    /// The thread's own, when a thread was asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<MediaDefaults>,
    /// What the narrowest level asked for takes from the levels above it,
    /// as stored there (a name that no longer resolves included; see
    /// `unavailable`). For Settings itself, nothing.
    pub inherited: MediaDefaults,
    /// What a call that leaves the argument out uses now at the narrowest
    /// level asked for: a field absent here has no default, and the tool
    /// requires the argument.
    pub resolved: MediaDefaults,
    /// Every stored name in the levels above that does not resolve, with
    /// the reason. Never replaced by another model: it is no default until
    /// it is picked again or the model comes back.
    pub unavailable: Vec<MediaUnavailable>,
}

impl MediaDefaultsView {
    /// Why `name` in `field` does not resolve, if it does not.
    pub fn why_unavailable(&self, field: MediaField, name: &str) -> Option<&str> {
        self.unavailable
            .iter()
            .find(|u| u.field == field.as_str() && u.name == name)
            .map(|u| u.reason.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_level_lays_its_own_fields_over_and_settings_drop_empties() {
        let settings = MediaDefaults {
            generate: Some(" image/a ".into()),
            speak: Some(String::new()),
            ..Default::default()
        }
        .normalised(true);
        assert_eq!(settings.generate.as_deref(), Some("image/a"));
        assert_eq!(settings.speak, None);
        let thread = MediaDefaults {
            generate: Some(String::new()),
            edit: Some("image/b".into()),
            ..Default::default()
        }
        .normalised(false);
        let out = settings.laid_over(&thread);
        assert_eq!(
            out.generate.as_deref(),
            Some(""),
            "an empty override is none"
        );
        assert_eq!(out.edit.as_deref(), Some("image/b"));
        assert!(MediaDefaults::default().is_empty());
    }

    #[test]
    fn input_names_the_known_fields_and_a_stored_level_drops_the_rest() {
        let e = serde_json::from_str::<MediaDefaults>(r#"{"image__generate":"image/x"}"#)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("unknown field `image__generate`")
                && e.contains("`generate`, `edit`, `speak`, `voice`, `transcribe`"),
            "{e}"
        );
        let stored = MediaDefaults::from_stored(serde_json::json!({
            "generate": "image/a", "later": "x", "voice": 3
        }));
        assert_eq!(stored.generate.as_deref(), Some("image/a"));
        assert_eq!(stored.voice, None);
        assert!(MediaDefaults::from_stored(serde_json::Value::Null).is_empty());
    }
}
