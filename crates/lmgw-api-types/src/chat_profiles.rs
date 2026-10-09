//! Personality profiles (personality-profiles design §1, §3.1): how a Chat
//! thread's model talks, in text and in voice. A profile holds a persona, a
//! length rule, a few static example exchanges, its own voice block, a
//! reasoning switch and the speech voice; any thread picks one by id
//! (`profile_id`), and "Default" is no profile at all.
//!
//! The gateway serializes these types for `/chat/api/profiles*`, the API
//! document is generated from them, and the profile editor (the UI kit) and
//! `lmgw-client` read them, so all sides share one shape. The examples' text
//! form ([`examples_to_text`], [`examples_from_text`]) is here for the same
//! reason: the self-admin tools and the editor's paste speak it alike.

use serde::{Deserialize, Deserializer, Serialize};

mod text;
pub use text::{examples_from_text, examples_to_text};

/// The name no profile may take, in any case: "Default" is the absence of a
/// profile (`profile_id` null), not a row (`400 profile_name_reserved`).
pub const RESERVED_NAME: &str = "default";

/// The built-in profiles' keys (`Profile::builtin`): the one that ships.
pub const BUILTIN_CONCISE: &str = "concise";

/// The content fields' names, as `follows_builtin` lists them.
pub mod field {
    pub const PERSONA: &str = "persona";
    pub const LENGTH_RULE: &str = "length_rule";
    pub const EXAMPLES: &str = "examples";
    pub const VOICE_BLOCK: &str = "voice_block";
    pub const REASONING: &str = "reasoning";
    /// Every field a built-in row can follow, in the wire's order.
    pub const FOLLOWABLE: [&str; 5] = [PERSONA, LENGTH_RULE, EXAMPLES, VOICE_BLOCK, REASONING];
}

/// One static example exchange: what the user says, and how the model
/// replies. Both sides are non-empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Example {
    pub user: String,
    pub reply: String,
}

/// A profile's reasoning switch; `null` where it is used inherits (the
/// thread's own fields win over it, the route's default comes after it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Reasoning {
    On,
    Off,
}

impl Reasoning {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
        }
    }

    /// `on` or `off`, trimmed, any case; anything else is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "on" => Some(Self::On),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

/// The speech part of a profile: the subset of a thread's voice settings a
/// profile carries, with the same checks and meanings. Each `null` inherits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProfileVoice {
    /// The text-to-speech alias (task `tts` or `vdes`).
    pub tts_alias: Option<String>,
    /// A voice of that text-to-speech model, or of the Chat's own when
    /// `tts_alias` is `null`.
    pub voice: Option<String>,
    /// The speaking style the text-to-speech model is given; an empty text
    /// is none.
    pub speech_style: Option<String>,
}

impl ProfileVoice {
    /// No field set.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A folder named by its id and name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FolderRef {
    pub id: i64,
    pub name: String,
}

/// What uses a profile, computed when it is read: what a delete would
/// clear.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UsedBy {
    /// Stored threads that picked it, archived ones included.
    pub threads: i64,
    /// Folders whose defaults name it.
    pub folders: Vec<FolderRef>,
}

/// A profile as `GET /chat/api/profiles` lists it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Profile {
    pub id: i64,
    /// Unique in any case; never `default`.
    pub name: String,
    /// The built-in profile this row is (`concise`); `null` for one of the
    /// owner's own. Read only.
    pub builtin: Option<String>,
    /// Who the model is and how it sounds; empty for none. When set, it
    /// takes the thread's system prompt's place.
    pub persona: String,
    /// How long replies are; empty for none.
    pub length_rule: String,
    /// Static example exchanges, in order.
    pub examples: Vec<Example>,
    /// The voice turn's block: `null` for the generic one, `""` for none, a
    /// text to use as it is.
    pub voice_block: Option<String>,
    /// Reasoning on or off; `null` inherits.
    pub reasoning: Option<Reasoning>,
    pub voice: ProfileVoice,
    /// The fields a built-in row takes from the built-in text, so they
    /// follow its improvements. Read only.
    pub follows_builtin: Vec<String>,
    pub used_by: UsedBy,
    pub created_at: String,
    pub updated_at: String,
}

/// `GET /chat/api/profiles`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProfileList {
    /// In name order.
    pub profiles: Vec<Profile>,
    /// The profile new threads start with (Settings → Chat), `null` for
    /// none.
    pub default_profile_id: Option<i64>,
}

/// A profile's content fields, with nothing saved: what Preview, Test and
/// Speak take, and what a create fills in.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProfileDraft {
    pub persona: String,
    pub length_rule: String,
    pub examples: Vec<Example>,
    pub voice_block: Option<String>,
    pub reasoning: Option<Reasoning>,
    pub voice: ProfileVoice,
}

/// `POST /chat/api/profiles`'s body: a `name` with any content fields (an
/// absent one is unset), or a `builtin` key alone to create that built-in
/// profile again after it was deleted (`409` while it exists).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProfileCreate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub builtin: Option<String>,
    pub persona: String,
    pub length_rule: String,
    pub examples: Vec<Example>,
    pub voice_block: Option<String>,
    pub reasoning: Option<Reasoning>,
    pub voice: ProfileVoice,
}

impl ProfileCreate {
    /// The content fields.
    pub fn draft(&self) -> ProfileDraft {
        ProfileDraft {
            persona: self.persona.clone(),
            length_rule: self.length_rule.clone(),
            examples: self.examples.clone(),
            voice_block: self.voice_block.clone(),
            reasoning: self.reasoning,
            voice: self.voice.clone(),
        }
    }
}

/// `Some(value)` for a field that was sent, `null` included: a bare
/// `Option<Option<T>>` would read `null` as absent.
fn present<'de, T, D>(de: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    T::deserialize(de).map(Some)
}

/// `POST /chat/api/profiles/{id}`'s body: the fields to change. An absent
/// field stays as it is; `null` unsets it. On a built-in row, a field
/// written with the built-in text follows the built-in again.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProfilePatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub persona: Option<Option<String>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub length_rule: Option<Option<String>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub examples: Option<Option<Vec<Example>>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub voice_block: Option<Option<String>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Option<Reasoning>>,
    /// The whole voice object; `null` unsets all three.
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub voice: Option<Option<ProfileVoice>>,
}

/// `POST /chat/api/profiles/{id}/delete`'s answer: what the delete cleared,
/// in the one transaction it ran in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProfileDeleted {
    /// The deleted profile's id.
    pub deleted: i64,
    /// Threads that used it and now use none.
    pub threads_cleared: i64,
    /// Folders whose defaults named it and no longer do.
    pub folders_cleared: Vec<FolderRef>,
    /// Whether it was the profile new threads start with (Settings → Chat),
    /// which is now none.
    pub default_cleared: bool,
}

/// `POST /chat/api/profiles/preview`'s body.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PreviewRequest {
    pub profile: ProfileDraft,
    /// Assemble as this thread would, with the draft in place of its own
    /// profile; absent: an empty thread with Settings' defaults.
    pub thread_id: Option<i64>,
    /// Count the tokens on this alias; absent: no count.
    pub model: Option<String>,
}

/// The token counts of a preview, through the universal counter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PreviewTokens {
    pub alias: String,
    /// The alias that counted, when it was not `alias` (a fallback).
    pub answered_by: Option<String>,
    #[serde(rename = "static")]
    pub static_part: u64,
    pub text_turn: u64,
    pub voice_turn: u64,
    /// How the counts are approximate, as `x-lmgw-count-approximate` says
    /// it; empty when they are exact.
    pub approx: Vec<String>,
}

/// `POST /chat/api/profiles/preview`'s answer: the system messages the
/// draft assembles to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PreviewAnswer {
    /// The part that does not depend on the turn: persona or thread prompt,
    /// length rule, examples.
    #[serde(rename = "static")]
    pub static_part: String,
    pub text_turn: String,
    pub voice_turn: String,
    pub tokens: Option<PreviewTokens>,
}

/// `POST /chat/api/profiles/test`'s body: one model call with the draft,
/// storing nothing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TestRequest {
    pub profile: ProfileDraft,
    pub thread_id: Option<i64>,
    pub model: String,
    /// The user's message.
    pub text: String,
    /// Assemble a voice turn rather than a text turn.
    pub voice: bool,
}

/// A test call's token usage, as the model reported it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TestUsage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
}

/// `POST /chat/api/profiles/test`'s answer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TestAnswer {
    /// The system message that was sent.
    pub system: String,
    pub reply: String,
    /// The reply's reasoning; empty when it did not reason.
    pub reasoning: String,
    /// Why reasoning was or was not asked as it was, in a sentence.
    pub reasoning_note: Option<String>,
    pub usage: TestUsage,
    pub first_token_ms: Option<u64>,
    pub total_ms: u64,
    pub reasoning_ms: Option<u64>,
    /// The alias that answered, when it was not the one asked (a fallback).
    pub answered_by: Option<String>,
}

/// `POST /chat/api/profiles/speak`'s body: the text spoken with the draft's
/// voice, answered as one WAV.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SpeakRequest {
    pub profile: ProfileDraft,
    pub thread_id: Option<i64>,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_patch_tells_absent_from_null() {
        let p: ProfilePatch =
            serde_json::from_value(json!({"persona": null, "reasoning": "off"})).unwrap();
        assert_eq!(p.persona, Some(None));
        assert_eq!(p.reasoning, Some(Some(Reasoning::Off)));
        assert_eq!(p.length_rule, None);
        assert_eq!(p.voice, None);
        // And writes back as it was read.
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            json!({"persona": null, "reasoning": "off"})
        );
    }

    #[test]
    fn input_refuses_an_unknown_field_by_name() {
        let e = serde_json::from_value::<ProfilePatch>(json!({"persnoa": "x"})).unwrap_err();
        assert!(e.to_string().contains("persnoa"), "{e}");
        let e = serde_json::from_value::<ProfileCreate>(json!({"name": "x", "voice": {"seed": 1}}))
            .unwrap_err();
        assert!(e.to_string().contains("seed"), "{e}");
    }

    #[test]
    fn the_wire_profile_has_every_field_with_nulls() {
        let v = serde_json::to_value(Profile {
            id: 3,
            name: "Concise".into(),
            builtin: Some(BUILTIN_CONCISE.into()),
            reasoning: Some(Reasoning::Off),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(v["voice_block"], json!(null));
        assert_eq!(v["reasoning"], json!("off"));
        assert_eq!(
            v["voice"],
            json!({"tts_alias": null, "voice": null, "speech_style": null})
        );
        assert_eq!(v["used_by"], json!({"threads": 0, "folders": []}));
    }

    #[test]
    fn static_is_the_preview_s_key() {
        let v = serde_json::to_value(PreviewAnswer {
            static_part: "s".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(v["static"], "s");
        assert!(v.get("static_part").is_none());
    }
}
