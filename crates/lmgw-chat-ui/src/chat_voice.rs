//! Chat voice in the page (chat-voice design): a thread's voice overrides as
//! the server sends them (`voice`) and what they resolve to
//! (`voice_resolved`), the draft the settings form edits, and its Voice
//! section ([`VoiceSection`]) — one set of fields for the thread's settings
//! drawer and a folder's defaults.

use leptos::prelude::*;
use lmgw_api_types::chat::{AudioInputMode, TurnDetectionMode};
use lmgw_api_types::chat_threads::SettingsAck;
use serde_json::{json, Value};

use super::chat::ChatThread;

/// The Voice section of the settings form.
mod section;
pub(super) use section::VoiceSection;
/// Whether a voice turn goes to the chat model as audio, in the page's
/// words (voice-audio-input §2.3).
mod audio_input;
/// The section's two languages: "I speak" and "Replies in".
mod languages;
pub use audio_input::AudioInputResolved;

/// Audio in the page: the player, the capture, the device lists, the shell
/// bridge (§11, §12).
pub(crate) mod audio;
/// The window's devices: the composer's button and popover (§2.4, §12).
mod devices;
pub(super) use devices::provide_voice_devices;

/// A message's `voice`, the composer's dictation mark, and their words
/// (§3, §5, §9.5).
mod spoken;
pub(super) use spoken::tolerant as tolerant_voice;
pub(super) use spoken::MsgVoice;
/// Delivery cues as chips in a spoken reply's bubble.
mod cues;
/// Right Ctrl, the hold-to-dictate key (§5).
mod keys;
/// The `state` frames' words, the hold and fallback lines (§4.3, §2.3).
mod state;
/// The composer's voice status line.
mod status;
pub(super) use status::VoiceStatusLine;
/// Dictation: microphone → `transcribe` → the composer (§5).
mod dictation;
/// The page's voice: dictation, read-aloud, status, keys (WP7).
mod page;
/// The page's one read-aloud (§6.5, §11.1).
mod read_aloud;
pub(super) use page::{provide_page_voice, PageVoice};
/// The composer's voice group (the microphone, the voice menu with
/// read-aloud and the devices) and the speaker button.
mod controls;
pub(super) use controls::{composer_title, SpeakerButton, VoiceControls};
/// The badges, the unheard rest and the timing of spoken turns.
mod badges;
pub(super) use badges::{spoken_html, MicBadge, SpokenReply};
/// Voice mode: the realtime panel bound to the thread (WP9, §9).
mod realtime;
pub(super) use realtime::{provide_realtime, Parts as RealtimeParts, RealtimePanel};
/// The realtime panel's visualisation: the three variants behind §10's
/// contract.
mod viz;

/// A thread's own voice settings (`chat_threads.voice`): every field
/// optional, absent = inherit Settings → Chat → Voice, then Settings →
/// Realtime. Serialized as the server takes it: only what is set. The type
/// is the gateway's.
pub use lmgw_api_types::chat::ThreadVoice;

/// `voice_resolved`: each field as the gateway resolves it, with where it
/// came from (`thread` | `profile` | `chat` | `realtime`; `row` for a speech
/// style the model describes itself, `speech_in` for a reply language that
/// follows the spoken one). The types are the ones the gateway writes it
/// with (`lmgw_api_types::voice_resolved`), a stage and the voice under the
/// names this page has always used.
pub use lmgw_api_types::voice_resolved::{
    Sourced, Stage as StageResolved, VoiceName as Named, VoiceProblem, VoiceResolved, VoiceSource,
};

/// Where a value came from, in the page's words; `source` is a
/// `VoiceSource::as_str`.
pub(super) fn source_label(source: Option<&str>) -> &'static str {
    match source {
        Some("thread") => "this thread",
        Some("chat") => "Settings → Chat",
        Some("realtime") => "Settings → Realtime",
        // A voice-design model's own description: the owner-wide style
        // stands back for it (a speech style only).
        Some("row") => "the model's own description",
        // A reply language that follows the spoken one.
        Some("speech_in") => "the language you speak",
        // The thread's personality profile supplies it.
        Some("profile") => "the personality profile",
        _ => "not set",
    }
}

/// A settings answer's `voice` and `voice_resolved`, laid onto the thread
/// as the page holds it (the server normalised what it took).
pub(super) fn apply_answer(t: &mut ChatThread, answer: &SettingsAck) {
    t.voice = answer.voice.clone();
    t.voice_resolved = Some(answer.voice_resolved.clone());
}

/// The Voice fields as typed. Held by the page with the rest of the
/// settings draft.
#[derive(Clone, Copy)]
pub(super) struct VoiceDraft {
    pub asr: RwSignal<String>,
    pub tts: RwSignal<String>,
    pub voice: RwSignal<String>,
    /// `""` inherit, `"none"` no style for this thread, `"own"` the text in
    /// `style`.
    pub style_mode: RwSignal<String>,
    pub style: RwSignal<String>,
    pub language: RwSignal<String>,
    pub reply_language: RwSignal<String>,
    /// `""` inherit, `"on"`, `"off"`.
    pub read_aloud: RwSignal<String>,
    /// `""` inherit, or one of the three names.
    pub turn: RwSignal<String>,
    /// `""` inherit, `"off"` or `"on"`.
    pub audio_input: RwSignal<String>,
    /// A seed "New voice" drew that the thread does not hold yet; `None`
    /// keeps the thread's own — the patch then leaves the seed out, so the
    /// server keeps whatever it holds, a seed it drew meanwhile included
    /// (chat-voice §2.2).
    pub seed: RwSignal<Option<u32>>,
}

impl VoiceDraft {
    pub(super) fn new() -> Self {
        Self {
            asr: RwSignal::new(String::new()),
            tts: RwSignal::new(String::new()),
            voice: RwSignal::new(String::new()),
            style_mode: RwSignal::new(String::new()),
            style: RwSignal::new(String::new()),
            language: RwSignal::new(String::new()),
            reply_language: RwSignal::new(String::new()),
            read_aloud: RwSignal::new(String::new()),
            turn: RwSignal::new(String::new()),
            audio_input: RwSignal::new(String::new()),
            seed: RwSignal::new(None),
        }
    }

    /// Fill the boxes from `v`.
    pub(super) fn load(&self, v: &ThreadVoice) {
        let text = |o: &Option<String>| o.clone().unwrap_or_default();
        self.asr.set(text(&v.asr_alias));
        self.tts.set(text(&v.tts_alias));
        self.voice.set(text(&v.voice));
        let (mode, style) = match v.speech_style.as_deref() {
            None => ("", String::new()),
            Some(s) if s.trim().is_empty() => ("none", String::new()),
            Some(s) => ("own", s.to_string()),
        };
        self.style_mode.set(mode.into());
        self.style.set(style);
        self.language.set(text(&v.language));
        self.reply_language.set(text(&v.reply_language));
        self.read_aloud.set(
            match v.read_aloud {
                Some(true) => "on",
                Some(false) => "off",
                None => "",
            }
            .into(),
        );
        let spelled = |m: Option<&str>| m.map(str::to_string).unwrap_or_default();
        self.turn.set(spelled(
            v.turn_detection.as_ref().map(TurnDetectionMode::as_str),
        ));
        self.audio_input
            .set(spelled(v.audio_input.as_ref().map(AudioInputMode::as_str)));
        self.seed.set(None);
    }

    /// The thread holds `stored` now: a drawn seed equal to it is saved, so
    /// nothing is pending (and later saves leave the seed alone).
    pub(super) fn settle_seed(&self, stored: Option<u32>) {
        let drawn = self.seed.get_untracked();
        if drawn.is_some() && drawn == stored {
            self.seed.set(None);
        }
    }

    /// The boxes as a [`ThreadVoice`], normalised as the server normalises
    /// it (tracked).
    fn value(&self) -> ThreadVoice {
        let name = |s: String| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        ThreadVoice {
            asr_alias: name(self.asr.get()),
            tts_alias: name(self.tts.get()),
            voice: name(self.voice.get()),
            language: name(self.language.get()).map(|l| l.to_ascii_lowercase()),
            reply_language: name(self.reply_language.get()).map(|l| l.to_ascii_lowercase()),
            read_aloud: match self.read_aloud.get().as_str() {
                "on" => Some(true),
                "off" => Some(false),
                _ => None,
            },
            turn_detection: name(self.turn.get()).and_then(wire_mode),
            audio_input: name(self.audio_input.get()).and_then(wire_mode),
            speech_style: match self.style_mode.get().as_str() {
                "none" => Some(String::new()),
                // An own style left empty is none for this thread too.
                "own" => Some(self.style.get().trim().to_string()),
                _ => None,
            },
            seed: self.seed.get(),
        }
    }

    /// Does the form differ from `stored`? (tracked) The seed only when one
    /// was drawn here: the server's own is not the form's to differ from.
    pub(super) fn differs_from(&self, stored: &ThreadVoice) -> bool {
        let mut stored = stored.clone();
        stored.language = stored.language.map(|l| l.to_ascii_lowercase());
        stored.reply_language = stored.reply_language.map(|l| l.to_ascii_lowercase());
        if self.seed.get().is_none() {
            stored.seed = None;
        }
        self.value() != stored
    }

    /// Why the boxes cannot be saved, worded for a toast (tracked).
    pub(super) fn error(&self) -> Option<String> {
        use lmgw_api_types::chat_voice::thread_language;
        if thread_language(&self.language.get()).is_none() {
            return Some(
                "the language you speak is two letters (ISO 639-1, such as de), auto (speech \
                 recognition detects), or empty to inherit"
                    .into(),
            );
        }
        thread_language(&self.reply_language.get())
            .is_none()
            .then(|| {
                "the reply language is two letters (ISO 639-1, such as en), auto (the language \
                 you speak), or empty to inherit"
                    .into()
            })
    }

    /// The settings body's `voice` — the whole object. The seed only when
    /// "New voice" drew one, so the server keeps one it drew meanwhile.
    pub(super) fn patch(&self) -> Result<Value, String> {
        untrack(|| {
            if let Some(e) = self.error() {
                return Err(e);
            }
            Ok(json!(self.value()))
        })
    }
}

/// A mode as the form spells it, read as the gateway's type: a name it knows
/// is its variant, any other stays as typed (`Unknown`), as it would arrive.
fn wire_mode<T: serde::de::DeserializeOwned>(text: String) -> Option<T> {
    serde_json::from_value(Value::String(text)).ok()
}

/// A fresh seed for "New voice".
pub(super) fn new_seed() -> u32 {
    (js_sys::Math::random() * f64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The form holds the modes as text; the thread's voice holds them as
    /// the gateway's types, and what the form saves must be the text it
    /// held, a name this build does not know included.
    #[test]
    fn the_forms_mode_names_read_as_the_gateways_types_and_write_back_as_typed() {
        assert_eq!(
            wire_mode::<TurnDetectionMode>("push_to_talk".into()),
            Some(TurnDetectionMode::PushToTalk)
        );
        assert_eq!(
            wire_mode::<AudioInputMode>("on".into()),
            Some(AudioInputMode::On)
        );
        let v = ThreadVoice {
            turn_detection: wire_mode("lookahead".into()),
            audio_input: wire_mode("off".into()),
            ..Default::default()
        };
        assert_eq!(
            v.turn_detection,
            Some(TurnDetectionMode::Unknown("lookahead".into()))
        );
        assert_eq!(
            json!(v),
            json!({ "turn_detection": "lookahead", "audio_input": "off" })
        );
    }

    #[test]
    fn a_thread_voice_serializes_only_what_is_set() {
        assert_eq!(json!(ThreadVoice::default()), json!({}));
        let v = ThreadVoice {
            speech_style: Some(String::new()),
            seed: Some(7),
            ..Default::default()
        };
        assert_eq!(json!(v), json!({ "speech_style": "", "seed": 7 }));
    }

    #[test]
    fn the_resolution_reads_the_servers_shape() {
        let r: VoiceResolved = serde_json::from_value(json!({
            "asr": {"alias": "a", "source": "chat", "inherited": "a", "local": true, "cpu": true,
                    "fallback": null, "fallback_unusable": null},
            "tts": {"alias": "t", "source": "thread", "inherited": "rt", "local": true,
                    "cpu": false, "fallback": {"alias": "openai/tts", "local": false},
                    "fallback_unusable": null},
            "voice": {"name": null, "source": "realtime", "inherits": "M5", "note": "chosen"},
            "speech_style": {"text": "", "source": "realtime"},
            "language": {"value": null, "source": null},
            "read_aloud": {"value": false, "source": "chat"},
            "turn_detection": {"value": "semantic_vad", "source": "thread"},
            "seed": 1234567, "problems": [],
            "realtime": {"ok": false, "code": "chat_thread_admin", "reason": "no",
                         "admin_tools": false}
        }))
        .unwrap();
        assert!(r.asr.cpu);
        assert_eq!(r.tts.fallback.unwrap().alias, "openai/tts");
        assert_eq!(r.tts.inherited.as_deref(), Some("rt"));
        assert_eq!(r.voice.inherits.as_deref(), Some("M5"));
        assert_eq!(
            source_label(r.turn_detection.source.as_ref().map(VoiceSource::as_str)),
            "this thread"
        );
        assert_eq!(r.turn_detection.value, TurnDetectionMode::SemanticVad);
        assert_eq!(r.speech_style.source, VoiceSource::Realtime);
        assert_eq!(r.language.value, None);
        assert_eq!(
            r.reply_language,
            Sourced::default(),
            "absent: an older server"
        );
        assert_eq!(r.realtime.code.as_deref(), Some("chat_thread_admin"));
        assert_eq!(r.seed, Some(1234567));
        assert_eq!(r.audio_input, None, "absent: an older server");
    }

    /// A source a newer gateway adds still reads, and the page words it as
    /// not set rather than guessing.
    #[test]
    fn a_source_this_build_does_not_know_reads_and_is_worded_as_not_set() {
        let s: Sourced<bool> =
            serde_json::from_value(json!({"value": true, "source": "something_new"})).unwrap();
        assert_eq!(s.source, Some(VoiceSource::Unknown("something_new".into())));
        assert_eq!(
            source_label(s.source.as_ref().map(VoiceSource::as_str)),
            "not set"
        );
    }
}
