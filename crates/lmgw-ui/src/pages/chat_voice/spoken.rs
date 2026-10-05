//! A message's `voice` as the thread lists it (chat-voice design §3), the
//! dictation mark of the composer (§5), and their words: the mic badge's
//! title on a spoken user turn, the speaker badge and timing line of a
//! spoken reply (§9.5). Pure, so it is tested natively.

use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};

/// `via` of a dictated user turn and of a turn spoken in voice mode.
pub(crate) const VIA_DICTATION: &str = "dictation";
pub(crate) const VIA_REALTIME: &str = "realtime";

/// `chat_messages.voice`: how a turn was spoken. `None` on the message is a
/// typed turn.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct MsgVoice {
    pub via: String,
    pub asr: Option<String>,
    pub asr_answered_by: Option<String>,
    pub asr_ms: Option<u64>,
    pub audio_ms: Option<u64>,
    pub tts: Option<String>,
    pub tts_answered_by: Option<String>,
    pub voice: Option<String>,
    /// A cut reply's text after the heard part (§8.4): shown greyed behind a
    /// "not heard" marker.
    pub unheard: Option<String>,
    #[serde(deserialize_with = "tolerant_timing")]
    pub timing: Option<VoiceTiming>,
    /// On a user turn said in voice mode: `audio` when the chat model heard
    /// it (voice-audio-input design §5), absent on the transcript path.
    pub input: Option<String>,
    /// On a user turn the model heard: why it could not be transcribed.
    pub transcript_error: Option<String>,
    /// The page's own note on the reply, never stored: voice mode's
    /// "not cut: another turn started" (`lmgw.chat.reply {skipped}`, §9.5).
    #[serde(skip)]
    pub note: Option<String>,
}

/// A spoken reply's timing (§8.7): each stage from the one before,
/// `to_first_audio_ms` their sum from the end of speech, `cold` the stages
/// that loaded during the turn.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct VoiceTiming {
    pub end_of_turn_ms: Option<u64>,
    pub asr_ms: Option<u64>,
    pub first_token_ms: Option<u64>,
    /// How long the chat model reasoned, part of `first_token_ms`.
    pub reasoning_ms: Option<u64>,
    pub first_clause_ms: Option<u64>,
    pub first_audio_ms: Option<u64>,
    pub total_ms: Option<u64>,
    pub to_first_audio_ms: Option<u64>,
    pub cold: Vec<String>,
    pub models: VoiceModels,
    /// How the turn reached the chat model, while audio input is on
    /// (voice-audio-input design §5): `audio` or `transcript`.
    pub input: Option<String>,
    /// Why the transcript.
    pub input_why: Option<String>,
    /// How long the first output waited for the turn's transcript.
    pub transcript_wait_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct VoiceModels {
    pub asr: Option<Served>,
    pub chat: Option<Served>,
    pub tts: Option<Served>,
}

/// A model a spoken turn went through: the alias asked for and, when
/// another answered, that one.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct Served {
    pub alias: String,
    pub answered_by: Option<String>,
    pub voice: Option<String>,
}

/// Voice turns the model hears, in words (voice-audio-input design §5).
mod input;
pub(crate) use input::NOT_TRANSCRIBED;

/// A timing this build cannot read goes alone, not the whole voice.
fn tolerant_timing<'de, D: Deserializer<'de>>(d: D) -> Result<Option<VoiceTiming>, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(serde_json::from_value(v).ok())
}

/// A message's `voice` field, read so that a shape this build cannot read
/// is a typed turn rather than a thread that fails to load.
pub(crate) fn tolerant<'de, D: Deserializer<'de>>(d: D) -> Result<Option<MsgVoice>, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(serde_json::from_value::<MsgVoice>(v)
        .ok()
        .filter(|m| !m.via.is_empty()))
}

/// A duration in the page's words: `412 ms`, `2.1 s`.
pub(crate) fn ms_text(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", ms as f64 / 1000.0)
    }
}

/// A model and, when another answered in its place, that one.
fn served_text(alias: &str, answered_by: Option<&str>) -> String {
    match answered_by {
        Some(b) if b != alias => format!("{b} (in place of {alias})"),
        _ => alias.to_string(),
    }
}

impl MsgVoice {
    /// A user turn that was spoken: dictated, or said in voice mode.
    pub(crate) fn is_spoken_user(&self) -> bool {
        self.via == VIA_DICTATION || self.via == VIA_REALTIME
    }

    /// A reply spoken in voice mode (its cues render as chips, §9.5). Its
    /// role says it: a user turn said in voice mode has `via: realtime` too,
    /// and its brackets stay text (§9.7.5).
    pub(crate) fn is_spoken_reply(&self, role: &str) -> bool {
        role == "assistant" && self.via == VIA_REALTIME
    }

    /// A message's `voice` from an event's JSON (`lmgw.chat.user`, `lmgw.
    /// chat.reply`), read as tolerantly as a listed one.
    pub(crate) fn of_value(v: &Value) -> Option<MsgVoice> {
        serde_json::from_value::<MsgVoice>(v.clone())
            .ok()
            .filter(|m| !m.via.is_empty())
    }

    /// What a reply streaming in voice mode carries until its stored row's
    /// `voice` comes: its cues are chips from the first frame (§9.7.4).
    pub(crate) fn provisional_reply() -> MsgVoice {
        MsgVoice {
            via: VIA_REALTIME.into(),
            ..Default::default()
        }
    }

    /// The mic badge's title: how the turn was spoken, which model
    /// transcribed it (the fallback named), and how long that took.
    pub(crate) fn mic_title(&self) -> String {
        let mut t = if self.via == VIA_DICTATION {
            "Dictated".to_string()
        } else {
            "Spoken in voice mode".to_string()
        };
        if let Some(a) = &self.asr {
            t.push_str(&format!(
                " · transcribed by {}",
                served_text(a, self.asr_answered_by.as_deref())
            ));
        }
        match (self.asr_ms, self.audio_ms) {
            (Some(asr), Some(audio)) => t.push_str(&format!(
                " · {} for {} of speech",
                ms_text(asr),
                ms_text(audio)
            )),
            (Some(asr), None) => t.push_str(&format!(" · {}", ms_text(asr))),
            (None, Some(audio)) => t.push_str(&format!(" · {} of speech", ms_text(audio))),
            (None, None) => {}
        }
        t.push_str(&self.input_words());
        t
    }

    /// The speaker badge's title on a spoken reply.
    pub(crate) fn speaker_title(&self) -> String {
        let mut t = "Spoken reply".to_string();
        if let Some(a) = &self.tts {
            t.push_str(&format!(
                " · said by {}",
                served_text(a, self.tts_answered_by.as_deref())
            ));
        }
        if let Some(v) = &self.voice {
            t.push_str(&format!(" · voice {v}"));
        }
        if self
            .unheard
            .as_deref()
            .is_some_and(|u| !u.trim().is_empty())
        {
            t.push_str(" · interrupted: the greyed rest was not heard");
        }
        t
    }
}

impl VoiceTiming {
    /// The collapsed line (§9.5): "ASR 31 ms · first token 208 ms · first
    /// audio 782 ms · nemotron → gemma4 → pocket-tts", fallbacks named. The
    /// page renders it item by item ([`Self::line_items`]).
    #[cfg(test)]
    pub(crate) fn line(&self) -> String {
        self.line_items()
            .into_iter()
            .map(|(sep, item)| format!("{sep}{item}"))
            .collect()
    }

    /// [`Self::line`]'s items, each with the separator before it: the line
    /// breaks only between them, never inside a model's name (WP10 note:
    /// "audio/supertonic-" / "3-q8-0").
    pub(crate) fn line_items(&self) -> Vec<(&'static str, String)> {
        let mut out: Vec<(&'static str, String)> = Vec::new();
        let mut push = |sep: &'static str, item: String| {
            out.push((if out.is_empty() { "" } else { sep }, item));
        };
        if let Some(v) = self.asr_ms {
            push(" · ", format!("ASR {}", ms_text(v)));
        }
        if let Some(v) = self.first_token_ms {
            push(" · ", format!("first token {}", ms_text(v)));
        }
        if let Some(v) = self.reasoning_ms {
            push(" · ", format!("reasoning {}", ms_text(v)));
        }
        if let Some(v) = self.to_first_audio_ms.or(self.first_audio_ms) {
            push(" · ", format!("first audio {}", ms_text(v)));
        }
        if let Some(item) = self.input_item() {
            push(" · ", item);
        }
        let models = [&self.models.asr, &self.models.chat, &self.models.tts];
        for (i, s) in models.into_iter().flatten().enumerate() {
            let sep = if i == 0 { " · " } else { " → " };
            push(sep, served_text(&s.alias, s.answered_by.as_deref()));
        }
        out
    }

    /// The expanded readout: every stage, and the cold starts.
    pub(crate) fn details(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut stage = |name: &str, v: Option<u64>| {
            if let Some(v) = v {
                out.push(format!("{name}: {}", ms_text(v)));
            }
        };
        stage("end of turn", self.end_of_turn_ms);
        stage("transcription", self.asr_ms);
        stage("first token", self.first_token_ms);
        stage("reasoning, within the first token", self.reasoning_ms);
        stage("first clause", self.first_clause_ms);
        stage("first audio", self.first_audio_ms);
        stage(
            "to first audio, from the end of speech",
            self.to_first_audio_ms,
        );
        stage("whole response", self.total_ms);
        if let Some(d) = self.input_detail() {
            out.push(d);
        }
        if !self.cold.is_empty() {
            out.push(format!("loaded during the turn: {}", self.cold.join(", ")));
        }
        for (name, s) in [
            ("speech-to-text", &self.models.asr),
            ("chat", &self.models.chat),
            ("text-to-speech", &self.models.tts),
        ] {
            if let Some(s) = s {
                let mut l = format!(
                    "{name}: {}",
                    served_text(&s.alias, s.answered_by.as_deref())
                );
                if let Some(v) = &s.voice {
                    l.push_str(&format!(", voice {v}"));
                }
                out.push(l);
            }
        }
        out
    }
}

/// The answer of `POST …/transcribe` (§5).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct Transcript {
    pub text: String,
    pub alias: Option<String>,
    pub asr_answered_by: Option<String>,
    pub asr_ms: Option<u64>,
    pub audio_ms: Option<u64>,
    pub language: Option<String>,
}

/// The composer holds dictated text (§5): what the send says about it.
/// Several dictations into one message add up.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DictationMark {
    pub asr: Option<String>,
    pub asr_answered_by: Option<String>,
    pub asr_ms: Option<u64>,
    /// `None` once any part's length is unknown.
    pub audio_ms: Option<u64>,
    pub parts: u32,
}

impl DictationMark {
    /// Add one transcription to the mark.
    pub(crate) fn add(mark: Option<DictationMark>, t: &Transcript) -> DictationMark {
        let first = mark.is_none();
        let m = mark.unwrap_or_default();
        let sum = |a: Option<u64>, b: Option<u64>| match (a, b) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
        DictationMark {
            asr: t.alias.clone().or(m.asr),
            // A fallback that had any part of the audio stays named.
            asr_answered_by: t.asr_answered_by.clone().or(m.asr_answered_by),
            asr_ms: if first {
                t.asr_ms
            } else {
                sum(m.asr_ms, t.asr_ms)
            },
            audio_ms: if first {
                t.audio_ms
            } else {
                sum(m.audio_ms, t.audio_ms)
            },
            parts: m.parts + 1,
        }
    }

    /// The send's `voice` (§5): `{via: "dictation", asr, asr_answered_by,
    /// asr_ms, audio_ms}`.
    pub(crate) fn send_json(&self) -> Value {
        json!({
            "via": VIA_DICTATION,
            "asr": self.asr,
            "asr_answered_by": self.asr_answered_by,
            "asr_ms": self.asr_ms,
            "audio_ms": self.audio_ms,
        })
    }

    /// The voice the sent bubble shows until the thread is read back.
    pub(crate) fn as_msg_voice(&self) -> MsgVoice {
        MsgVoice {
            via: VIA_DICTATION.into(),
            asr: self.asr.clone(),
            asr_answered_by: self.asr_answered_by.clone(),
            asr_ms: self.asr_ms,
            audio_ms: self.audio_ms,
            ..Default::default()
        }
    }

    /// The composer's tooltip while it holds dictated text.
    pub(crate) fn title(&self) -> String {
        let mut t = self.as_msg_voice().mic_title();
        if self.parts > 1 {
            t.push_str(&format!(" ({} dictations)", self.parts));
        }
        t.push_str(" — Enter sends it as a spoken turn; emptying the box drops the mark");
        t
    }
}

/// Insert dictated `add` into `text` at a textarea's selection — `start` and
/// `end` in UTF-16 code units, as `selectionStart`/`selectionEnd` count —
/// replacing what is selected, with a space where it would touch a word.
/// Answers the new text and the caret after the insertion, in UTF-16 units.
pub(crate) fn insert_dictated(text: &str, start: u32, end: u32, add: &str) -> (String, u32) {
    let add = add.trim();
    let byte_at = |units: u32| {
        let mut n = 0u32;
        for (i, c) in text.char_indices() {
            if n >= units {
                return i;
            }
            n += c.len_utf16() as u32;
        }
        text.len()
    };
    let (s, e) = (byte_at(start.min(end)), byte_at(start.max(end)));
    let (head, tail) = (&text[..s], &text[e..]);
    // No space inside an opening bracket or quote, nor before a closing one
    // or punctuation (review n5). `“` and `«`/`»` open in one language and
    // close in another, so neither side gets a space at them.
    let before = head
        .chars()
        .next_back()
        .is_some_and(|c| !c.is_whitespace() && !matches!(c, '(' | '[' | '„' | '“' | '«' | '»'));
    let after = tail.chars().next().is_some_and(|c| {
        !c.is_whitespace()
            && !matches!(
                c,
                ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '”' | '“' | '»' | '«'
            )
    });
    let mut out = String::with_capacity(text.len() + add.len() + 2);
    out.push_str(head);
    if before && !add.is_empty() {
        out.push(' ');
    }
    out.push_str(add);
    let caret = out.encode_utf16().count() as u32;
    if after && !add.is_empty() {
        out.push(' ');
    }
    out.push_str(tail);
    (out, caret)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_voice_this_build_cannot_read_is_a_typed_turn() {
        #[derive(Deserialize)]
        struct Row {
            #[serde(default, deserialize_with = "tolerant")]
            voice: Option<MsgVoice>,
        }
        let r: Row = serde_json::from_value(json!({"voice": {"via": 3}})).unwrap();
        assert_eq!(r.voice, None);
        let r: Row = serde_json::from_value(json!({"voice": null})).unwrap();
        assert_eq!(r.voice, None);
        let r: Row = serde_json::from_value(json!({})).unwrap();
        assert_eq!(r.voice, None);
        // A timing of an unknown shape goes alone.
        let r: Row = serde_json::from_value(json!({"voice": {
            "via": "realtime", "unheard": " rest", "timing": {"cold": "tts"}}}))
        .unwrap();
        let v = r.voice.unwrap();
        assert!(v.is_spoken_reply("assistant") && v.timing.is_none());
        assert!(
            !v.is_spoken_reply("user"),
            "a user turn said in voice mode is no reply"
        );
        assert_eq!(v.unheard.as_deref(), Some(" rest"));
    }

    #[test]
    fn the_mic_title_names_the_model_its_fallback_and_the_times() {
        let v = MsgVoice {
            via: "dictation".into(),
            asr: Some("audio/parakeet".into()),
            asr_ms: Some(702),
            audio_ms: Some(15823),
            ..Default::default()
        };
        assert_eq!(
            v.mic_title(),
            "Dictated · transcribed by audio/parakeet · 702 ms for 15.8 s of speech"
        );
        let v = MsgVoice {
            asr_answered_by: Some("openai/whisper-1".into()),
            asr_ms: None,
            ..v
        };
        assert_eq!(
            v.mic_title(),
            "Dictated · transcribed by openai/whisper-1 (in place of audio/parakeet) · 15.8 s of \
             speech"
        );
        assert!(v.is_spoken_user() && !v.is_spoken_reply("assistant"));
    }

    #[test]
    fn the_timing_line_reads_like_the_design() {
        let t: VoiceTiming = serde_json::from_value(json!({
            "asr_ms": 31, "first_token_ms": 208, "first_audio_ms": 36,
            "to_first_audio_ms": 782, "total_ms": 4120, "cold": ["tts"],
            "models": {"asr": {"alias": "nemotron", "answered_by": null},
                       "chat": {"alias": "gemma4-e4b-voice", "answered_by": "openai/gpt"},
                       "tts": {"alias": "pocket-tts", "answered_by": null, "voice": "alba"}}
        }))
        .unwrap();
        assert_eq!(
            t.line(),
            "ASR 31 ms · first token 208 ms · first audio 782 ms · nemotron → openai/gpt (in \
             place of gemma4-e4b-voice) → pocket-tts"
        );
        let items = t.line_items();
        assert_eq!(items[0], ("", "ASR 31 ms".to_string()));
        assert_eq!(items[3], (" · ", "nemotron".to_string()));
        assert_eq!(items[5], (" → ", "pocket-tts".to_string()));
        let d = t.details();
        assert!(d.contains(&"loaded during the turn: tts".to_string()));
        assert!(d.contains(&"text-to-speech: pocket-tts, voice alba".to_string()));
        assert!(d.contains(&"whole response: 4.1 s".to_string()));
        assert!(!t.line().contains("reasoning"));

        // A model that reasoned: how long, beside the first token.
        let t: VoiceTiming = serde_json::from_value(json!({
            "first_token_ms": 1900, "reasoning_ms": 1600, "first_audio_ms": 36
        }))
        .unwrap();
        assert_eq!(
            t.line(),
            "first token 1.9 s · reasoning 1.6 s · first audio 36 ms"
        );
        assert!(t
            .details()
            .contains(&"reasoning, within the first token: 1.6 s".to_string()));
    }

    #[test]
    fn dictations_into_one_message_add_up() {
        let t = |ms, audio, by: Option<&str>| Transcript {
            text: "x".into(),
            alias: Some("asr".into()),
            asr_answered_by: by.map(str::to_string),
            asr_ms: Some(ms),
            audio_ms: audio,
            language: None,
        };
        let m = DictationMark::add(None, &t(100, Some(2000), None));
        let m = DictationMark::add(Some(m), &t(50, Some(1000), Some("cloud/whisper")));
        assert_eq!(m.asr_ms, Some(150));
        assert_eq!(m.audio_ms, Some(3000));
        assert_eq!(m.parts, 2);
        assert_eq!(m.asr_answered_by.as_deref(), Some("cloud/whisper"));
        let m = DictationMark::add(Some(m), &t(10, None, None));
        assert_eq!(
            m.audio_ms, None,
            "a part of unknown length makes the sum unknown"
        );
        assert_eq!(m.asr_answered_by.as_deref(), Some("cloud/whisper"));
        assert_eq!(
            m.send_json(),
            json!({"via": "dictation", "asr": "asr", "asr_answered_by": "cloud/whisper",
                   "asr_ms": 160, "audio_ms": null})
        );
    }

    #[test]
    fn dictated_text_lands_at_the_caret_with_spaces_where_words_meet() {
        assert_eq!(
            insert_dictated("", 0, 0, " Hallo Welt. "),
            ("Hallo Welt.".into(), 11)
        );
        assert_eq!(
            insert_dictated("Bitte", 5, 5, "danke"),
            ("Bitte danke".into(), 11)
        );
        assert_eq!(
            insert_dictated("Bitte ", 6, 6, "danke"),
            ("Bitte danke".into(), 11)
        );
        // In the middle of two words: a space on both sides.
        assert_eq!(
            insert_dictated("einszwei", 4, 4, "und"),
            ("eins und zwei".into(), 8)
        );
        // Before punctuation: none after.
        assert_eq!(insert_dictated("Ja.", 2, 2, "doch"), ("Ja doch.".into(), 7));
        // A selection is replaced.
        assert_eq!(
            insert_dictated("ein altes Wort", 4, 9, "neues"),
            ("ein neues Wort".into(), 9)
        );
        // Offsets are UTF-16 units: "ü" is one, "𝄞" two.
        assert_eq!(insert_dictated("ü𝄞x", 3, 3, "y"), ("ü𝄞 y x".into(), 5));
        // Past the end: appended.
        assert_eq!(insert_dictated("a", 99, 99, "b"), ("a b".into(), 3));
        // Nothing to insert changes nothing.
        assert_eq!(insert_dictated("ab", 1, 1, "  "), ("ab".into(), 1));
        // Inside brackets and quotes: no space on their inner side.
        assert_eq!(insert_dictated("()", 1, 1, "Text"), ("(Text)".into(), 5));
        assert_eq!(insert_dictated("„“", 1, 1, "Text"), ("„Text“".into(), 5));
        assert_eq!(insert_dictated("“”", 1, 1, "Text"), ("“Text”".into(), 5));
        assert_eq!(insert_dictated("»«", 1, 1, "Text"), ("»Text«".into(), 5));
        assert_eq!(insert_dictated("[]", 1, 1, "Text"), ("[Text]".into(), 5));
    }
}
