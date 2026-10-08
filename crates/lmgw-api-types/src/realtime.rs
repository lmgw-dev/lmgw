//! `GET /v1/realtime`'s settings section and its VRAM budget (realtime
//! design §12): what `GET /api/settings-full` reports under `realtime`, and
//! the `realtime_budget` op's arguments and answer.
//!
//! A namespace of its own, like [`crate::bench_ops`]: `RealtimeSettings`
//! beside the glob-exported `AudioSettings` would read as one more class,
//! and it is not one.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The close code of a bound realtime session that another client took over
/// by binding its thread. The close's reason names who: "voice mode moved to
/// device 'phone'".
pub const CLOSE_TAKEN_OVER: u16 = 4000;

/// The close code of a session whose key was revoked: disabled, rotated,
/// deleted or expired. The reason starts with the [`RevokeKind`] token,
/// then a colon and the sentence: "device_disabled: device 'desktop' was
/// disabled". Reconnecting with the same key is refused.
pub const CLOSE_REVOKED: u16 = 4003;

/// The close code of a bound session whose thread left the key's reach:
/// the thread now carries what this key may not see, or what the key may
/// see narrowed. The key is still good; the thread is gone for it. The
/// reason is neutral and says no more: "chat thread 7 is out of reach for
/// this key". A client asks for its current thread again.
pub const CLOSE_OUT_OF_REACH: u16 = 4004;

/// What revoked a key, as a [`CLOSE_REVOKED`] close's reason starts with it
/// and the change feed's `revoked` event carries it (`kind`):
///
/// - `device_disabled`: a paired device was disabled; it may be enabled
///   again, so it keeps its pairing;
/// - `key_expired`: a paired device's key passed its expiry;
/// - `key_unknown`: a paired device's key was rotated or deleted: pair the
///   device again;
/// - `revoked`: any other kind of key, for any of these.
///
/// Forward compatible like every enum a client reads: a token a newer
/// gateway sends reads as [`RevokeKind::Unknown`], as sent. One an older
/// gateway does not send at all — a close reason without a token, a
/// `revoked` event without `kind` — is an empty `Unknown`, the default, so
/// a client never reads a device's rotation as another key's revocation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[non_exhaustive]
pub enum RevokeKind {
    DeviceDisabled,
    KeyExpired,
    KeyUnknown,
    Revoked,
    /// A token this build does not know (a newer gateway's), as sent; empty
    /// where the gateway sent none.
    #[serde(untagged)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unknown(String),
}

impl Default for RevokeKind {
    /// None was sent: an empty [`RevokeKind::Unknown`].
    fn default() -> Self {
        Self::Unknown(String::new())
    }
}

impl RevokeKind {
    /// The wire token: `device_disabled`, `key_expired`, `key_unknown`,
    /// `revoked`, or the unknown token as sent.
    pub fn as_str(&self) -> &str {
        match self {
            Self::DeviceDisabled => "device_disabled",
            Self::KeyExpired => "key_expired",
            Self::KeyUnknown => "key_unknown",
            Self::Revoked => "revoked",
            Self::Unknown(v) => v,
        }
    }

    /// The token a [`CLOSE_REVOKED`] close's reason starts with: the text
    /// before its first colon, or the whole reason, when that is a token
    /// (`[a-z0-9_]+`); an empty [`RevokeKind::Unknown`] for a reason without
    /// one.
    pub fn of_close_reason(reason: &str) -> Self {
        Self::split_close_reason(reason).0
    }

    /// A [`CLOSE_REVOKED`] close's reason as its kind and its sentence: the
    /// sentence after the token and its colon (empty for a bare token), or
    /// the whole reason when it carries no token — what a person reads.
    pub fn split_close_reason(reason: &str) -> (Self, &str) {
        let is_token = |t: &str| {
            !t.is_empty()
                && t.bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
        };
        let (token, text) = match reason.split_once(':') {
            Some((head, rest)) if is_token(head.trim()) => (Some(head.trim()), rest.trim_start()),
            None if is_token(reason.trim()) => (Some(reason.trim()), ""),
            _ => (None, reason),
        };
        let kind = match token {
            Some("device_disabled") => Self::DeviceDisabled,
            Some("key_expired") => Self::KeyExpired,
            Some("key_unknown") => Self::KeyUnknown,
            Some("revoked") => Self::Revoked,
            Some(other) => Self::Unknown(other.to_string()),
            None => Self::default(),
        };
        (kind, text)
    }
}

impl std::fmt::Display for RevokeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The close code of every session when the gateway stops or restarts
/// (RFC 6455's "going away"), with the reason [`SHUTTING_DOWN`]. A client
/// reconnects once the gateway is back.
pub const CLOSE_GOING_AWAY: u16 = 1001;

/// The reason of the [`CLOSE_GOING_AWAY`] close, and the message of a Chat
/// stream's last `error` frame (`gateway_stopping`) when the gateway stops
/// or restarts: one sentence for both, since a client cannot tell them
/// apart until it reconnects.
pub const SHUTTING_DOWN: &str = "lmgw is stopping or restarting";

/// The capability tasks whose models answer `POST /v1/audio/speech`, and so
/// can speak a realtime session's answers (§5.3): text to speech, and voice
/// design — the voice designed from a description, which a session sends as
/// its speech instructions. The one list the gateway's checks
/// (`realtime.tts_alias`, `session.lmgw.tts_model`, `capabilities.speech`)
/// and the dashboard's model picker go by.
pub const SPEECH_TASKS: &[&str] = &["tts", "vdes"];

/// The sounds the realtime tag hint names when a TTS renders them (WP10
/// D7): every sound a stage direction maps onto (the gateway's
/// `audio::tags::STAGE_DIRECTIONS`) but `pause`, which is no sound. The
/// gateway's tests keep the two lists in step.
pub const HINT_SOUNDS: &[&str] = &[
    "laughter",
    "sigh",
    "breath",
    "quick_breath",
    "cough",
    "lipsmack",
];

/// Whether `inner` — the text between `[` and `]` — is an inline tag in the
/// canonical client syntax (WP10, `[a-z][a-z _'-]{1,30}`): a sound such as
/// `[laughs]`, or a delivery cue such as `[laughing]` at a clause's start
/// (WP9b). The one rule the gateway's tag grammar (`audio::tags`) and the
/// dashboard's cue chips in a spoken reply's bubble go by.
///
/// A single letter is no tag (chat-voice WP7 review m12): `[x]` is a task
/// list's checkbox and `[a]` an enumeration, never a sound or a cue, so they
/// are spoken and shown as written. `[1]` (a citation) never was one.
pub fn is_tag_name(inner: &str) -> bool {
    let mut chars = inner.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && (2..=31).contains(&inner.len())
        && chars.all(|c| c.is_ascii_lowercase() || matches!(c, ' ' | '_' | '\'' | '-'))
}

/// The paragraph a spoken response's prompt gets about the sounds its TTS
/// can make (realtime design §7.2, WP10 D7), or `None` when it makes none.
/// `inline_tags` is the TTS's `capabilities.speech.inline_tags` (`none`,
/// `fixed`, `free`), `tags` the tags a `fixed` one renders. A fixed
/// vocabulary is named by its sounds ([`HINT_SOUNDS`]) — tokens such as
/// OmniVoice's `question-en` are nothing a model could place — or, when it
/// has none of them, whole. The gateway and the dashboard's preview make the
/// same text with it.
pub fn tag_hint_text(inline_tags: &str, tags: &[String]) -> Option<String> {
    match inline_tags {
        "fixed" => {
            let sounds: Vec<&String> = tags
                .iter()
                .filter(|t| HINT_SOUNDS.contains(&t.as_str()))
                .collect();
            let named = if sounds.is_empty() {
                tags.iter().collect()
            } else {
                sounds
            };
            if named.is_empty() {
                return None;
            }
            let list = named
                .iter()
                .map(|t| format!("[{t}]"))
                .collect::<Vec<_>>()
                .join(" ");
            Some(format!(
                "Your words are spoken by a voice that can also make these sounds: {list}. To \
                 make one, write it exactly so, before the words it goes with, rarely and only \
                 where it fits. Never put other words in square brackets."
            ))
        }
        "free" => Some(
            "Your words are spoken by a voice that performs short stage directions in square \
             brackets, such as [laughs] or [whispers]. To use one, write it before the words it \
             goes with, rarely and only where it fits. Never put anything else in square \
             brackets."
                .to_string(),
        ),
        _ => None,
    }
}

/// Whether a TTS takes delivery cues (realtime design §5.5, WP9b C1): a
/// leading `[laughing]` of a clause goes into that clause's instructions
/// instead of its text. `instructions` is the TTS's
/// `capabilities.speech.instructions`, `None` when it publishes none — a
/// cloud alias the owner described no further — and `inline_tags` and
/// `tags` are as for [`tag_hint_text`]. True for a `style` or
/// `passthrough` TTS that renders no tags — the hint's own condition, so a
/// TTS takes tags or cues, never both: a sound it renders beats a style,
/// and square brackets keep one meaning in the prompt. A `voice_design`
/// TTS takes none (its description is its voice, and a cue in it would
/// make a new voice every clause), nor does one that reads no
/// instructions, nor one whose instructions nobody declared: a cloud TTS
/// still gets its style, but gpt-4o-mini-tts ignored every cue phrasing
/// tried, so a hint would only have the chat model write brackets that are
/// stripped. The alias override `capabilities.speech.instructions:
/// "style"` turns them on for one. The gateway and the dashboard decide it
/// the same way with it.
pub fn takes_cues(instructions: Option<&str>, inline_tags: &str, tags: &[String]) -> bool {
    matches!(instructions, Some("style" | "passthrough"))
        && tag_hint_text(inline_tags, tags).is_none()
}

/// The paragraph a spoken response's prompt gets about delivery cues (realtime
/// design §7.2, WP9b C6), for a TTS that [`takes_cues`]. It asks for one or
/// two words because a tag is at most 31 bytes: three words such as
/// "whispering very conspiratorially" are more, and a longer bracket is no
/// cue but read out.
pub fn cue_hint_text() -> String {
    "Your words are spoken by a voice that can change how it speaks. To ask for a delivery, \
     begin a sentence with a short cue of one or two lowercase English words in square \
     brackets, such as [laughing], [whispering] or [excited]; it lasts until that sentence \
     ends. Use cues rarely and only where they fit. Never put other words in square brackets."
        .to_string()
}

/// What a spoken response's prompt is told about square brackets (realtime
/// design §7.2, WP9b C6): the sounds the TTS renders ([`tag_hint_text`]),
/// else the delivery cues it takes ([`cue_hint_text`]), else `None`. The
/// gateway's hint and the dashboard's preview are this one text.
/// `instructions` is `None` when the TTS declares none ([`takes_cues`]).
pub fn speech_hint_text(
    instructions: Option<&str>,
    inline_tags: &str,
    tags: &[String],
) -> Option<String> {
    tag_hint_text(inline_tags, tags)
        .or_else(|| takes_cues(instructions, inline_tags, tags).then(cue_hint_text))
}

/// One eagerness's row of `realtime.semantic_vad`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SemanticVadRow {
    /// A Smart Turn score at or above this commits the turn, `0.0..=1.0`.
    pub threshold: f64,
    /// A score at or above this and below the threshold commits at
    /// `semantic_floor_window_ms`; equal to the threshold turns the middle
    /// band off.
    pub floor: f64,
    /// The longest a pause waits, whatever the score.
    pub max_wait_ms: u32,
    /// Where a pause that could not be scored commits, and the window the
    /// `server_vad` engine runs this eagerness on.
    pub silence_duration_ms: u32,
}

/// `realtime.semantic_vad`: the rows by eagerness (`auto` is `medium`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SemanticVadTable {
    pub high: SemanticVadRow,
    pub medium: SemanticVadRow,
    pub low: SemanticVadRow,
}

/// The realtime settings, with `default_instructions` spelled
/// out the way the Chat's default system prompt is: the text in force, the
/// built-in text beside it, and whether the setting is unset.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RealtimeSettings {
    /// The chat alias a session starts on; empty = none.
    pub default_model: String,
    /// Client model names mapped to lmgw aliases.
    pub model_map: BTreeMap<String, String>,
    /// The speech-to-text alias turns are transcribed with; empty =
    /// the Chat's transcription model.
    pub asr_alias: String,
    /// The text-to-speech alias a session speaks with; empty = none.
    /// Its task is `tts` or `vdes`.
    pub tts_alias: String,
    /// The voice a session speaks with when it names none or an OpenAI
    /// built-in voice; empty = the TTS row's default preset.
    pub default_voice: String,
    /// Client voice names mapped to voices of the TTS model.
    pub voice_map: BTreeMap<String, String>,
    /// The speech instructions a session's TTS gets while the client sends
    /// none: a speaking style, or a voice-design row's description;
    /// empty = none.
    pub speech_instructions: String,
    /// An audio response's prompt says what square brackets do for the TTS:
    /// the sounds it can make, or the delivery cues it takes.
    pub tag_hint: bool,
    /// The instructions a session with audio output uses while the client
    /// gives none: the configured text, `""` for none, or the built-in text
    /// while the setting is unset.
    pub default_instructions: String,
    /// The built-in voice-assistant prompt — what saving
    /// `default_instructions` as this text returns to.
    pub default_instructions_builtin: String,
    /// The setting is unset, so it follows the built-in text across
    /// releases.
    pub default_instructions_is_builtin: bool,
    /// `server_vad.threshold`, `0.0..=1.0`.
    pub threshold: f64,
    pub prefix_padding_ms: u32,
    pub silence_duration_ms: u32,
    /// `smart_turn` | `server_vad` (the escape hatch).
    pub semantic_vad_engine: String,
    pub semantic_vad: SemanticVadTable,
    pub semantic_floor_window_ms: u32,
    pub post_interrupt_silence_ms: u32,
    pub barge_in_min_ms: u32,
    pub barge_in_guard_ms: u32,
    pub half_duplex: bool,
    pub echo_tail_ms: u32,
    /// `words` | `duration`.
    pub barge_in_check: String,
    pub backchannel_words: Vec<String>,
    /// Unicode script names; empty = every script.
    pub barge_in_check_scripts: Vec<String>,
    /// `0` = no bound.
    pub barge_in_check_timeout_ms: u32,
    /// Empty = the session's own ASR alias.
    pub barge_in_check_alias: String,
    pub output_lead_ms: u32,
    /// `0` = no bound.
    pub synthesis_ahead_s: u32,
    /// `0` = keep the engine's silences.
    pub longest_pause_ms: u32,
    pub warm_on_connect: bool,
    /// In MiB; `0` = no bound (not both).
    pub max_message_mb: u32,
    /// In MiB; `0` = bounded by the message limit (not both).
    pub max_frame_mb: u32,
    /// `0` = no pings and no liveness bound at all.
    pub ping_interval_s: u32,
}

/// `realtime_budget`'s arguments: the cascade to size. An argument left out
/// is the saved setting, so the dashboard can size the choices in its draft
/// before they are saved; `""` is "none".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RealtimeBudgetArgs {
    pub default_model: Option<String>,
    /// `""` falls back to the Chat's transcription model, as a session does.
    pub asr_alias: Option<String>,
    pub tts_alias: Option<String>,
    /// `words` | `duration`; with `duration` there is no word check to size.
    pub barge_in_check: Option<String>,
    /// `""` = the ASR alias.
    pub barge_in_check_alias: Option<String>,
}

/// One stage of the cascade and what it is expected to hold on the GPU.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RealtimeBudgetStage {
    /// `turn` | `chat` | `asr` | `check` | `tts`.
    pub stage: String,
    /// The stage in words.
    pub label: String,
    /// The alias the stage runs on; `None` for turn detection and for a
    /// stage nothing is configured for.
    pub alias: Option<String>,
    /// `local` (a model lmgw runs on this GPU) | `cloud` | `external` (a
    /// server lmgw does not manage) | `cpu` | `shared` (the same local model
    /// as an earlier stage, counted once) | `unset` | `unresolved`.
    pub placement: String,
    /// The local model, as `<class>/<model id>`.
    pub model: Option<String>,
    /// Its container is up now.
    pub running: bool,
    /// The expected footprint; `None` = unknown, and the total is then a
    /// lower bound.
    pub bytes: Option<u64>,
    /// Where the figure comes from, or why there is none.
    pub note: String,
}

/// `realtime_budget`'s answer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RealtimeBudget {
    /// Turn detection, chat, ASR, the word check (when it uses another
    /// model), TTS — in that order.
    pub stages: Vec<RealtimeBudgetStage>,
    /// The known figures summed, each local model once.
    pub total_bytes: u64,
    /// The stages whose figure is unknown; while any is listed the total is
    /// a lower bound.
    pub unknown: Vec<String>,
    /// `vram.headroom_mb`: what admission keeps free above a model's
    /// estimate when it starts one.
    pub headroom_bytes: u64,
    /// `total_bytes + headroom_bytes`: what the last of the cascade's models
    /// to start needs free.
    pub needed_bytes: u64,
    /// What lmgw may plan against: `vram.budget_mb`, or the devices' real
    /// total. `None` when admission has nothing to measure against.
    pub capacity_bytes: Option<u64>,
    /// Where the capacity comes from, or why there is none.
    pub capacity_source: String,
    /// GPU memory held now by programs lmgw cannot free (games, the
    /// desktop), measured per process. `None` when it cannot be measured.
    pub outside_bytes: Option<u64>,
    /// Why `outside_bytes` is missing, when it is.
    pub outside_note: Option<String>,
    /// `fits` | `tight` (fits what lmgw may use, not beside what other
    /// programs hold now) | `too_large` | `unknown`.
    pub verdict: String,
    /// The verdict in a sentence.
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review G-8, G-10: a token is `[a-z0-9_]+`, bare or before a colon;
    /// a reason without one, and a `revoked` event without `kind`, read as
    /// an empty `Unknown` — never as another key's `revoked`.
    #[test]
    fn a_revocation_kind_reads_from_what_was_sent() {
        let split = RevokeKind::split_close_reason;
        assert_eq!(
            split("key_unknown: device 'desk' was rotated — pair it again"),
            (
                RevokeKind::KeyUnknown,
                "device 'desk' was rotated — pair it again"
            )
        );
        assert_eq!(split("key_unknown"), (RevokeKind::KeyUnknown, ""));
        assert_eq!(
            split("key_v2: moved"),
            (RevokeKind::Unknown("key_v2".into()), "moved")
        );
        assert_eq!(
            split("device 'desk' was disabled"),
            (
                RevokeKind::Unknown(String::new()),
                "device 'desk' was disabled"
            )
        );
        assert_eq!(RevokeKind::default(), RevokeKind::Unknown(String::new()));
        let older: crate::chat_feed::Revoked = serde_json::from_str(
            r#"{"reason": "rotated", "message": "device 'desk' was rotated"}"#,
        )
        .unwrap();
        assert_eq!(older.kind, RevokeKind::Unknown(String::new()));
        let newer: crate::chat_feed::Revoked =
            serde_json::from_str(r#"{"reason": "rotated", "message": "…", "kind": "key_unknown"}"#)
                .unwrap();
        assert_eq!(newer.kind, RevokeKind::KeyUnknown);
        assert_eq!(
            serde_json::to_value(&newer).unwrap()["kind"],
            serde_json::json!("key_unknown")
        );
    }

    #[test]
    fn a_tag_is_a_lowercase_word_of_two_to_thirty_one_bytes() {
        for t in [
            "laughs",
            "clears throat",
            "don't-know",
            "quick_breath",
            "ok",
        ] {
            assert!(is_tag_name(t), "{t}");
        }
        let long = "a".repeat(32);
        // A checkbox, an enumeration, a citation, a speaker label.
        for t in [
            "x",
            "a",
            "1",
            "S1",
            "Laughing",
            "x2",
            "",
            " leading",
            long.as_str(),
        ] {
            assert!(!is_tag_name(t), "{t}");
        }
        assert!(is_tag_name(&"a".repeat(31)));
    }
}
