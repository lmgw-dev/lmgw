//! Presets (labs-store design §1.1, §3.2, §5): named form parts a lab keeps,
//! the answers and bodies of `/labs/api/{lab}/presets`. A preset's body is
//! its kind's type, written as `kind` and `body` beside its name: the
//! Decision lab's question sets, the Image lab's prompts and the Audio lab's
//! speech settings.

use serde::{Deserialize, Serialize};

use super::LabKind;
use crate::audio_lab::AudioSettings;
use crate::decision_lab::QuestionSet;
use crate::image_lab::ImageGenForm;

/// What a preset holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum LabPresetKind {
    /// The Decision lab's questions, model and thresholds.
    QuestionSet,
    /// The Image lab's form: the model, the prompt and every setting.
    ImagePrompt,
    /// The Audio lab's speech panel and the PCM format of a stream.
    AudioSettings,
}

impl LabPresetKind {
    pub const ALL: [Self; 3] = [Self::QuestionSet, Self::ImagePrompt, Self::AudioSettings];

    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QuestionSet => "question_set",
            Self::ImagePrompt => "image_prompt",
            Self::AudioSettings => "audio_settings",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// The lab whose presets are of this kind.
    pub fn lab(self) -> LabKind {
        match self {
            Self::QuestionSet => LabKind::Decision,
            Self::ImagePrompt => LabKind::Image,
            Self::AudioSettings => LabKind::Audio,
        }
    }
}

/// A preset's kind and its body, as `"kind": …, "body": …`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "body", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum LabPresetBody {
    QuestionSet(QuestionSet),
    // Boxed: a form of strings is several times a question set's size, and
    // every view of a preset would carry the largest.
    ImagePrompt(Box<ImageGenForm>),
    AudioSettings(Box<AudioSettings>),
}

impl LabPresetBody {
    pub fn kind(&self) -> LabPresetKind {
        match self {
            Self::QuestionSet(_) => LabPresetKind::QuestionSet,
            Self::ImagePrompt(_) => LabPresetKind::ImagePrompt,
            Self::AudioSettings(_) => LabPresetKind::AudioSettings,
        }
    }

    /// What the kind's type checks; the message names the field.
    pub fn check(&self) -> Result<(), String> {
        match self {
            Self::QuestionSet(s) => s.check(),
            Self::ImagePrompt(f) => f.check_fields(),
            Self::AudioSettings(a) => a.check(),
        }
    }
}

/// `POST /labs/api/{lab}/presets`: a preset to save, whole.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabPresetWrite {
    /// Unique among the lab's presets of its kind.
    pub name: String,
    #[serde(flatten)]
    pub preset: LabPresetBody,
}

/// `POST /labs/api/{lab}/presets/{id}`: the preset rewritten whole, if it
/// is still the one read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabPresetUpdate {
    /// Unique among the lab's presets of its kind.
    pub name: String,
    #[serde(flatten)]
    pub preset: LabPresetBody,
    /// The preset's `updated_at` as read: when it has moved since (another
    /// edit, a threshold written), the update is a 409 `set_changed`.
    pub updated_at: String,
}

/// A kept preset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabPresetView {
    pub id: i64,
    pub lab: LabKind,
    pub name: String,
    #[serde(flatten)]
    pub preset: LabPresetBody,
    pub created_at: String,
    pub updated_at: String,
}

/// A lab's presets, by name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabPresetList {
    pub presets: Vec<LabPresetView>,
}

/// The query of `GET /labs/api/{lab}/presets`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabPresetsQuery {
    /// Only presets of this kind.
    pub kind: Option<LabPresetKind>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preset_is_its_name_beside_its_kind_and_body() {
        let w = LabPresetWrite {
            name: "triage".into(),
            preset: LabPresetBody::QuestionSet(QuestionSet {
                default_model: "decide/laya".into(),
                ..Default::default()
            }),
        };
        let v = serde_json::to_value(&w).unwrap();
        assert_eq!(v["name"], "triage");
        assert_eq!(v["kind"], "question_set");
        assert_eq!(v["body"]["default_model"], "decide/laya");
        let back: LabPresetWrite = serde_json::from_value(v).unwrap();
        assert_eq!(back, w);
        assert_eq!(back.preset.kind().lab(), LabKind::Decision);
        assert_eq!(
            LabPresetKind::parse("question_set"),
            Some(LabPresetKind::QuestionSet)
        );
    }

    #[test]
    fn each_kind_is_one_labs_and_reads_back_by_its_spelling() {
        for k in LabPresetKind::ALL {
            assert_eq!(LabPresetKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(LabPresetKind::ImagePrompt.lab(), LabKind::Image);
        assert_eq!(LabPresetKind::AudioSettings.lab(), LabKind::Audio);
        let w = LabPresetWrite {
            name: "night".into(),
            preset: LabPresetBody::ImagePrompt(Box::new(ImageGenForm {
                prompt: "a city at night".into(),
                ..Default::default()
            })),
        };
        let v = serde_json::to_value(&w).unwrap();
        assert_eq!(v["kind"], "image_prompt");
        assert_eq!(v["body"]["prompt"], "a city at night");
        assert_eq!(serde_json::from_value::<LabPresetWrite>(v).unwrap(), w);
    }
}
