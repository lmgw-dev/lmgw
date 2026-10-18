//! What a Chat thread's voice resolves to (chat-voice design §2.3): a
//! thread's `voice_resolved`, in `GET /chat/api/threads/{id}`, the thread
//! answers of create, pin and archive, and the settings answer. Each field
//! says the level it came from: the thread, its personality profile,
//! Settings → Chat, Settings → Realtime.
//!
//! The gateway resolves this with logic types of its own and writes it as
//! these; the dashboard reads it with them. A test in the gateway keeps the
//! two writing the same bytes.

use serde::{Deserialize, Serialize};

use crate::chat::{AudioInputMode, TurnDetectionMode};
use crate::chat_threads::InputPath;
use crate::chat_voice::LanguageNote;

/// A thread's voice, resolved.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceResolved {
    /// The speech-to-text stage: dictation, voice mode, audio attachments.
    pub asr: Stage,
    /// The text-to-speech stage: read-aloud and voice mode.
    pub tts: Stage,
    /// The voice the text-to-speech model is asked for.
    pub voice: VoiceName,
    /// The speech instructions the read-aloud uses.
    pub speech_style: SpeechStyle,
    /// The language the user speaks, ISO 639-1; `value: null` is none, the
    /// speech-to-text model detects it.
    pub language: Sourced<Option<String>>,
    /// The language replies are in, ISO 639-1: the thread's, Settings', else
    /// the spoken one (`source: speech_in`); `value: null` is none, the reply
    /// follows the user.
    pub reply_language: Sourced<Option<String>>,
    /// Where a stage's model does not take its language as set.
    pub language_notes: Vec<LanguageNote>,
    /// Whether replies are read aloud as they stream.
    pub read_aloud: Sourced<bool>,
    /// How voice mode detects the end of a turn.
    pub turn_detection: Sourced<TurnDetectionMode>,
    /// Whether a voice turn goes to the chat model as audio, and why not.
    pub audio_input: Option<AudioInputShown>,
    /// The thread's text-to-speech seed; `null` until its first use draws
    /// one.
    pub seed: Option<u32>,
    /// What blocks a voice feature now: a stage with no alias, or one whose
    /// alias no longer resolves.
    pub problems: Vec<VoiceProblem>,
    /// Whether voice mode may bind the thread.
    pub realtime: RealtimeAvailability,
}

/// The level a value came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub enum VoiceSource {
    /// The thread's own override.
    Thread,
    /// The thread's personality profile.
    Profile,
    /// Settings → Chat → Voice.
    Chat,
    /// Settings → Realtime.
    Realtime,
    /// The text-to-speech model's own description of its voice (a speech
    /// style only).
    Row,
    /// The reply language only: none is set for it, so it is the language the
    /// user speaks.
    SpeechIn,
    /// A value this build does not know (a newer gateway's), as sent: it
    /// writes back unchanged.
    #[serde(untagged)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unknown(String),
}

impl VoiceSource {
    /// The value as the wire spells it.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Thread => "thread",
            Self::Profile => "profile",
            Self::Chat => "chat",
            Self::Realtime => "realtime",
            Self::Row => "row",
            Self::SpeechIn => "speech_in",
            Self::Unknown(s) => s,
        }
    }
}

/// What a reader that finds no value holds: an empty unknown value.
impl Default for VoiceSource {
    fn default() -> Self {
        Self::Unknown(String::new())
    }
}

/// A value and the level it came from; `source: null` when no level sets it.
// One component per value type in the document, named after it
// (`Sourced_boolean`, `Sourced_Nullable_string`, `Sourced_TurnDetectionMode`):
// by default schemars numbers the instances (`Sourced`, `Sourced2`), and the
// number follows the order they are met in, which a client generator would
// take for a name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "Sourced_{T}"))]
pub struct Sourced<T: Default> {
    pub value: T,
    pub source: Option<VoiceSource>,
}

/// One speech stage.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Stage {
    /// `null`: nothing configured at any level.
    pub alias: Option<String>,
    /// The level `alias` came from; `null` with no alias.
    pub source: Option<VoiceSource>,
    /// What the levels below the thread give: the alias the thread uses once
    /// its own override is cleared; equal to `alias` when the thread sets
    /// none.
    pub inherited: Option<String>,
    /// `false` for an alias served off this machine; `null` when the alias
    /// does not resolve.
    pub local: Option<bool>,
    /// A model lmgw starts itself: the GPU hold and a benchmark run's lease
    /// apply to it.
    pub managed: bool,
    /// An audio model that runs on the CPU: the GPU hold does not stop it, a
    /// benchmark run's lease does.
    pub cpu: bool,
    /// What answers instead while the model is blocked; `null` off this
    /// machine, or when the model has no usable fallback.
    pub fallback: Option<Fallback>,
    /// A fallback the model names that cannot stand in, and why.
    pub fallback_unusable: Option<FallbackUnusable>,
}

/// A hold fallback, and whether it is local (a usable one never is).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Fallback {
    pub alias: String,
    pub local: bool,
}

/// A named hold fallback that cannot answer: `why` is the clause the hold's
/// refusal names it with ("does not resolve", "is itself a local model").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FallbackUnusable {
    pub alias: String,
    pub why: String,
}

/// The voice the text-to-speech model is asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceName {
    /// The voice to ask for; `null`: none is named, and voice mode's own
    /// chain decides.
    pub name: Option<String>,
    /// Where `name` came from. With no name, `realtime` when Settings →
    /// Realtime's default voice is where that chain starts, else `null`.
    pub source: Option<VoiceSource>,
    /// Settings → Realtime's default voice, when no name is named: applied by
    /// voice mode, never sent as the voice.
    pub inherits: Option<String>,
    /// Why a voice set at a less specific level is not used here.
    pub note: Option<String>,
}

/// The speech instructions; `""` is none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SpeechStyle {
    pub text: String,
    /// The level the text came from; `row` when the text-to-speech model's
    /// own description stands in for it.
    pub source: VoiceSource,
}

/// Something that blocks a voice feature.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceProblem {
    /// `asr` or `tts`.
    pub stage: String,
    /// `not_configured`, `unresolved`, or `voice_needs_transcript` (a voice
    /// clip the text-to-speech model cannot clone without its transcript).
    pub code: String,
    pub message: String,
}

/// Whether voice mode may bind the thread.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RealtimeAvailability {
    pub ok: bool,
    /// The refusal's code, the one the bind answers with
    /// (`chat_thread_admin`); `null` when `ok`.
    pub code: Option<String>,
    /// The refusal in words; `null` when `ok`.
    pub reason: Option<String>,
    /// The thread carries lmgw's admin tools while the self-admin level is
    /// `full`: its tools can change lmgw. Shown, never a refusal.
    pub admin_tools: bool,
}

/// `audio_input`: the setting, the level it came from, and the verdict.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioInputShown {
    /// The setting in effect.
    pub value: AudioInputMode,
    /// The level `value` came from.
    pub source: VoiceSource,
    #[serde(flatten)]
    pub verdict: AudioInputVerdict,
}

/// Where a voice turn's words go: as audio, or as the transcript and why.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioInputVerdict {
    /// `audio` when the model hears the turn, `transcript` otherwise.
    pub path: InputPath,
    /// The model the turn goes to: the thread's, or the fallback a block hands
    /// it to.
    pub model: String,
    /// Why the transcript, in the page's words; `null` on the audio path.
    pub why: Option<String>,
    /// What handed the turn to `model` when the resolve swapped ("under the
    /// GPU hold"). Absent when nothing did.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub lead: Option<String>,
    /// The verdict a GPU block would bring: its fallback's, or the refusal
    /// when there is none. Absent unless no block is live now and one would
    /// hand the turn elsewhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub blocked: Option<Box<AudioInputVerdict>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_source_a_newer_gateway_adds_reads_and_writes_back_as_sent() {
        let s: Sourced<bool> =
            serde_json::from_value(json!({"value": true, "source": "something_new"})).unwrap();
        assert_eq!(s.source, Some(VoiceSource::Unknown("something_new".into())));
        assert_eq!(
            s.source.as_ref().map(VoiceSource::as_str),
            Some("something_new")
        );
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            json!({"value": true, "source": "something_new"})
        );
        // Wherever a source sits: a stage, a voice, a style, `audio_input`.
        let r: VoiceResolved = serde_json::from_value(json!({
            "asr": {"alias": "a", "source": "something_new"},
            "voice": {"source": "something_new"},
            "speech_style": {"text": "calm", "source": "something_new"},
            "audio_input": {"value": "auto", "source": "something_new", "path": "hybrid"},
            "turn_detection": {"value": "lookahead", "source": "something_new"},
        }))
        .unwrap();
        let new = || VoiceSource::Unknown("something_new".into());
        assert_eq!(r.asr.source, Some(new()));
        assert_eq!(r.voice.source, Some(new()));
        assert_eq!(r.speech_style.source, new());
        let a = r.audio_input.unwrap();
        assert_eq!(a.source, new());
        assert_eq!(a.value, AudioInputMode::Unknown("auto".into()));
        assert_eq!(a.verdict.path, InputPath::Unknown("hybrid".into()));
        assert_eq!(
            r.turn_detection.value,
            TurnDetectionMode::Unknown("lookahead".into())
        );
    }

    #[test]
    fn the_known_sources_read_as_their_variants_and_spell_as_sent() {
        for (name, source) in [
            ("thread", VoiceSource::Thread),
            ("profile", VoiceSource::Profile),
            ("chat", VoiceSource::Chat),
            ("realtime", VoiceSource::Realtime),
            ("row", VoiceSource::Row),
            ("speech_in", VoiceSource::SpeechIn),
        ] {
            let read: VoiceSource = serde_json::from_value(json!(name)).unwrap();
            assert_eq!(read, source);
            assert_eq!(read.as_str(), name);
            assert_eq!(serde_json::to_value(&read).unwrap(), json!(name));
        }
        // A reader that finds no source holds an empty one.
        assert_eq!(VoiceSource::default().as_str(), "");
        assert_eq!(VoiceResolved::default().speech_style.source.as_str(), "");
    }

    #[test]
    fn an_older_gateway_s_answer_with_keys_missing_still_reads() {
        let r: VoiceResolved = serde_json::from_value(json!({})).unwrap();
        assert_eq!(r, VoiceResolved::default());
        assert_eq!(r.audio_input, None);
        let r: VoiceResolved = serde_json::from_value(json!({
            "tts": {"alias": "t", "fallback": {"alias": "openai/tts", "local": false}}
        }))
        .unwrap();
        assert_eq!(r.tts.fallback.unwrap().alias, "openai/tts");
    }

    #[test]
    fn a_verdict_leaves_out_the_lead_and_the_forecast_it_does_not_have() {
        let shown = AudioInputShown {
            value: AudioInputMode::On,
            source: VoiceSource::Chat,
            verdict: AudioInputVerdict {
                path: InputPath::Audio,
                model: "m".into(),
                ..Default::default()
            },
        };
        assert_eq!(
            serde_json::to_value(&shown).unwrap(),
            json!({"value": "on", "source": "chat", "path": "audio", "model": "m", "why": null})
        );
        let with = AudioInputShown {
            verdict: AudioInputVerdict {
                lead: Some("under the GPU hold".into()),
                blocked: Some(Box::new(AudioInputVerdict {
                    path: InputPath::Transcript,
                    model: "f".into(),
                    why: Some("f does not take audio input".into()),
                    ..Default::default()
                })),
                ..shown.verdict.clone()
            },
            ..shown
        };
        let v = serde_json::to_value(&with).unwrap();
        assert_eq!(v["lead"], "under the GPU hold");
        assert_eq!(v["blocked"]["path"], "transcript");
        assert!(v["blocked"].get("lead").is_none());
        let back: AudioInputShown = serde_json::from_value(v).unwrap();
        assert_eq!(back, with);
    }

    #[cfg(feature = "schema")]
    #[test]
    fn the_document_lists_the_known_sources_only() {
        let s = serde_json::to_value(schemars::schema_for!(VoiceSource)).unwrap();
        let list = s.get("oneOf").or(s.get("anyOf")).expect("a list of values");
        let values: Vec<_> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["const"].clone())
            .collect();
        assert_eq!(
            values,
            [
                json!("thread"),
                json!("profile"),
                json!("chat"),
                json!("realtime"),
                json!("row"),
                json!("speech_in")
            ]
        );
    }
}
