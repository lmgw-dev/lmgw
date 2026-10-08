//! The GA session object (realtime design §2.2), and the `session.lmgw`
//! extension object (§5.4).
//!
//! **Tolerant on input, complete on output.** Every protocol field a client
//! may send is typed here, so a wrong *shape* is an `error` naming it; a field
//! this cascade does not know (Hugging Face's client sends
//! `session.extensions: [..]`, newer SDKs will add more) is ignored rather
//! than refused, because refusing it would break a stock client over a knob
//! that changes nothing here. The one strict object is `session.lmgw`: it is
//! lmgw's own, nobody sends it by accident, and a typo in it would otherwise
//! be a silently ignored setting.
//!
//! The session a server holds is always **complete** — `merge::normalize`
//! fills every default — so what `session.created` / `session.updated` echo
//! is the configuration actually in effect, not what the client happened to
//! send.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::tools::{Tool, ToolChoice};

/// `session.type`. Required on every `session.update` (§2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionType {
    Realtime,
    /// Transcription-only sessions are a separate surface (§19); parsed so
    /// the refusal can name it instead of reading as a typo.
    Transcription,
}

/// The GA session object.
///
/// `Option` fields that are `None` are left out of the echo, except the two
/// whose `null` *means* something — `audio.input.turn_detection` (manual
/// turns) and `audio.input.transcription` (no transcription events) — which
/// [`AudioInput`] always writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    #[serde(rename = "type")]
    pub kind: SessionType,
    /// `"realtime.session"` on the echo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    /// `sess_…`, minted per connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The name the client asked for (`?model=` or `session.model`), or the
    /// alias a model-less session started on. What actually answers is
    /// `lmgw.resolved.chat` (§5.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_modalities: Option<Vec<Modality>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioConfig>,
    /// Flat function tools (§7.4) and `mcp` tools (realtime-server-tools
    /// §1). An explicit `[]` clears them; an absent key leaves them as they
    /// were (§2.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<MaxOutputTokens>,
    /// For reasoning realtime models; read by the responder (§7.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Value>,
    /// Accepted and echoed, never acted on (§2.2): `truncation` is §7.5's
    /// (`lmgw.resolved.truncation` says what is in effect), the rest have no
    /// meaning for a cascade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracing: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Value>,
    /// lmgw's extension object (§5.4). Always present on the echo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lmgw: Option<LmgwExt>,
}

/// One entry of `output_modalities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modality {
    Audio,
    Text,
}

/// `max_output_tokens`: a count, or `"inf"`.
///
/// No upper bound is enforced: OpenAI documents 4096 for its own models, and
/// copying that number here would be an invented cap on a local model whose
/// real limit is its context (owner's rule; the per-send fit of §7.5 is what
/// refuses a request that does not fit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MaxOutputTokens {
    Count(u32),
    Inf(Inf),
}

/// The `"inf"` literal of [`MaxOutputTokens`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Inf {
    #[serde(rename = "inf")]
    Inf,
}

/// `audio.input` / `audio.output`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<AudioInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<AudioOutput>,
}

/// `audio.input`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<AudioFormat>,
    /// Whether input-transcription *events* are sent, and with which ASR
    /// model (§5.2). `null` = no events; the cascade transcribes anyway.
    #[serde(default)]
    pub transcription: Option<Transcription>,
    /// Accepted and echoed; not applied (§19).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise_reduction: Option<Value>,
    /// `null` = manual turns (§6.6).
    #[serde(default)]
    pub turn_detection: Option<TurnDetection>,
}

/// `audio.output`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<AudioFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<Voice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
}

/// `{type: "audio/pcm", rate}` and the two G.711 formats. Only PCM16 at
/// 24 kHz is served; G.711 parses so its refusal can name it (§19).
///
/// `type` is optional on input — the GA reference defaults it to
/// `audio/pcm`, so `{"rate": 24000}` is a PCM format, not a shape error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "type")]
pub enum AudioFormat {
    #[serde(rename = "audio/pcm")]
    Pcm { rate: u32 },
    #[serde(rename = "audio/pcmu")]
    Pcmu,
    #[serde(rename = "audio/pcma")]
    Pcma,
}

/// [`AudioFormat`] as the wire spells it once its `type` is filled in.
#[derive(Deserialize)]
#[serde(tag = "type")]
enum TaggedFormat {
    #[serde(rename = "audio/pcm")]
    Pcm {
        #[serde(default = "pcm_rate")]
        rate: u32,
    },
    #[serde(rename = "audio/pcmu")]
    Pcmu,
    #[serde(rename = "audio/pcma")]
    Pcma,
}

impl<'de> Deserialize<'de> for AudioFormat {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = super::with_default_type(Value::deserialize(d)?, "audio/pcm");
        Ok(
            match TaggedFormat::deserialize(v).map_err(serde::de::Error::custom)? {
                TaggedFormat::Pcm { rate } => Self::Pcm { rate },
                TaggedFormat::Pcmu => Self::Pcmu,
                TaggedFormat::Pcma => Self::Pcma,
            },
        )
    }
}

/// The one rate the GA protocol allows for `audio/pcm`.
pub const PCM_RATE: u32 = 24_000;

fn pcm_rate() -> u32 {
    PCM_RATE
}

/// `audio.input.transcription`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Transcription {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// `audio.input.turn_detection` (§6). After `merge::normalize` every
/// `create_response` / `interrupt_response` is `Some`: `@openai/agents`
/// decides whether to send `response.cancel` from the echoed
/// `interrupt_response`, so the echo must carry it even when the client sent
/// `{type: "semantic_vad"}` alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnDetection {
    ServerVad {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        threshold: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix_padding_ms: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        silence_duration_ms: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        create_response: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interrupt_response: Option<bool>,
        /// Accepted and echoed; not applied (§19).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idle_timeout_ms: Option<u64>,
    },
    SemanticVad {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        eagerness: Option<Eagerness>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        create_response: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interrupt_response: Option<bool>,
    },
}

impl TurnDetection {
    /// The wire name of the variant, which is what a type change in a
    /// `session.update` is detected by (`merge`).
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::ServerVad { .. } => "server_vad",
            Self::SemanticVad { .. } => "semantic_vad",
        }
    }
}

/// `semantic_vad.eagerness`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Eagerness {
    Low,
    Medium,
    High,
    Auto,
}

/// `audio.output.voice`: a name, or `{id}` naming a voice-library clip (§5.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Voice {
    Name(String),
    Id { id: String },
}

/// The `session.lmgw` extension object (§5.4) — **strict**: an unknown key is
/// an `error`, because this object is lmgw's own and a misspelt knob would
/// otherwise be silently ignored.
///
/// The knobs are carried and echoed; each echoes its setting's default when
/// the client sent none, so the echo is the configuration in effect.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LmgwExt {
    /// The TTS alias (§5.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tts_model: Option<String>,
    /// Barge-in evidence needed before `speech_started` fires while the
    /// client is playing (§6.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barge_in_min_ms: Option<u32>,
    /// Playback time during which no barge-in evidence counts (§6.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barge_in_guard_ms: Option<u32>,
    /// The silence window of the turn after a barge-in (§6.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_interrupt_silence_ms: Option<u32>,
    /// Input during the client's playback is not listened to — no barge-in,
    /// no turn — for a client without echo cancellation (§6.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub half_duplex: Option<bool>,
    /// How long past the modelled playback end, on top of the ping's round
    /// trip, input still counts as heard during the answer (§6.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub echo_tail_ms: Option<u32>,
    /// What decides a barge-in once the evidence gate passed: `"words"` or
    /// `"duration"` (§6.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barge_in_check: Option<crate::config::BargeInCheck>,
    /// How long the word check may take before the duration rule decides;
    /// 0 = no bound (§6.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barge_in_check_timeout_ms: Option<u32>,
    /// The scripts whose words count for the word check, by Unicode name;
    /// empty = every script (§6.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barge_in_check_scripts: Option<Vec<String>>,
    /// The ASR alias the word check transcribes with; empty = the
    /// session's own ASR alias (§6.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barge_in_check_alias: Option<String>,
    /// How far output audio runs ahead of real time (§8.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_lead_ms: Option<u32>,
    /// How far synthesis may run ahead of the paced send, in seconds; 0 = no
    /// bound (§8.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesis_ahead_s: Option<u32>,
    /// The longest silence in synthesized speech, in milliseconds; 0 = keep
    /// the engine's silences (§8.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longest_pause_ms: Option<u32>,
    /// The speech instructions every clause is sent with (WP10, §5.4): a
    /// speaking style, or the description a voice-design TTS designs its
    /// voice from. `""` = none for this session; absent or `null` = the
    /// owner's `realtime.speech_instructions`. Echoed only as the client set
    /// it — what is in effect is `resolved.speech`. Never derived from
    /// `instructions`, which is the chat model's prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_instructions: Option<String>,
    /// The seed every clause is sent with where the TTS reads one (WP10
    /// D6) — the same voice from a voice-design row across responses and
    /// sessions. Echoed only as the client set it; without it a
    /// voice-design row gets one per session (`resolved.speech.seed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_seed: Option<u32>,
    /// Whether an audio response's prompt says what square brackets do: the
    /// sounds its TTS can make (WP10 D7), or the delivery cues it takes
    /// (WP9b). Echoes `realtime.tag_hint` when the client sent none; the
    /// text is `resolved.speech.tag_hint`. Off, cues still apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag_hint: Option<bool>,
    /// Server-written: what the session's names resolved to (§5.1). Accepted
    /// on input — a client that sends back the session it was given carries
    /// it — and always replaced by the server's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<Resolved>,
}

/// `session.lmgw.resolved.semantic_vad`: what decides when a pause ends a
/// `semantic_vad` turn (§6.3) — read-only, from the settings.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SemanticResolved {
    /// A Smart Turn score at or above this commits 200 ms into the pause.
    pub threshold: f64,
    /// At or above this (below the threshold): commits at `floor_window_ms`
    /// of silence; below it the pause waits for `max_wait_ms`.
    pub floor: f64,
    pub floor_window_ms: u32,
    /// The longest any pause waits.
    pub max_wait_ms: u32,
    /// Where a pause that could not be scored commits.
    pub silence_duration_ms: u32,
}

/// `session.lmgw.resolved`: every substitution, visible (§5.1). `null` is
/// "nothing resolved", never "something silently chosen".
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Resolved {
    /// The chat alias that answers.
    #[serde(default)]
    pub chat: Option<String>,
    /// The ASR alias (§5.2); `null` when none is configured.
    #[serde(default)]
    pub asr: Option<String>,
    /// The TTS alias (§5.3); `null` when none is configured.
    #[serde(default)]
    pub tts: Option<String>,
    /// The voice the TTS alias speaks with (§5.3) — always a name; `null`
    /// when nothing resolves to one. `designed` for a voice-design TTS that
    /// is sent no voice: it designs one from `speech.text` (R2).
    #[serde(default)]
    pub voice: Option<String>,
    /// The detector actually running: `server_vad`, or `semantic_vad` —
    /// Smart Turn deciding when a pause ends the turn (§6.3). A
    /// `semantic_vad` session says `server_vad` when the owner set
    /// `realtime.semantic_vad_engine` to it. `null` = manual turns.
    #[serde(default)]
    pub turn_detection: Option<String>,
    /// The Smart Turn rule in effect: the session's eagerness's row of
    /// `realtime.semantic_vad` (§6.3). `null` when the session does not run
    /// it.
    #[serde(default)]
    pub semantic_vad: Option<SemanticResolved>,
    /// `"disabled"`: `truncation: "auto"` is accepted but not honoured in v1
    /// (§7.5).
    #[serde(default)]
    pub truncation: Option<String>,
    /// What the TTS alias does with speech instructions and inline tags, and
    /// what a response sends it (WP10); `null` when there is no TTS alias.
    #[serde(default)]
    pub speech: Option<SpeechResolved>,
    /// The Chat thread the session is bound to (chat-voice design §8.1):
    /// it owns the chat model, the speech models and the conversation.
    /// Absent for a session that is not bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_thread: Option<ChatThreadRef>,
}

/// `session.lmgw.resolved.chat_thread` — and `lmgw.chat.thread`, when what
/// it says changes (chat-voice design §8.7).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChatThreadRef {
    /// Negative for a temporary thread.
    pub id: i64,
    pub title: String,
    pub temporary: bool,
    /// The thread's turns may dispatch lmgw's own admin tools: the
    /// self-admin toolset is attached and what the binder's admin tools may
    /// do is `full` — the gateway's `self_admin`, capped by a device's own
    /// level (§8.1, the panel's flag). Re-read before each response, so a
    /// level that moved shows at the next one, in `lmgw.chat.thread`.
    #[serde(default)]
    pub admin_tools: bool,
}

/// `session.lmgw.resolved.speech` (WP10 D3): the expressive half of the
/// session's speech, as it is in effect — read-only.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SpeechResolved {
    /// What the TTS does with speech instructions —
    /// `capabilities.speech.instructions`: `none`, `style`, `voice_design` or
    /// `passthrough` (a cloud alias the owner described no further).
    #[serde(default)]
    pub instructions: String,
    /// The speech instructions in effect; `null` = none.
    #[serde(default)]
    pub text: Option<String>,
    /// Where `text` comes from: `session` (`session.lmgw.speech_instructions`),
    /// `setting` (`realtime.speech_instructions`) or `row` (the TTS row's
    /// own default description, which its engine applies itself); `null`
    /// with no text.
    #[serde(default)]
    pub source: Option<String>,
    /// The TTS reads no instructions: `text` does not reach it.
    #[serde(default)]
    pub dropped: bool,
    /// What the TTS does with inline tags: `none`, `fixed` or `free`.
    #[serde(default)]
    pub tags: String,
    /// The TTS takes delivery cues (WP9b): a `style` or `passthrough` TTS
    /// that renders no tags gets a clause's leading `[laughing]` as how to
    /// say it, after the style, until the sentence ends. A cloud alias
    /// nobody described is `passthrough` by assumption and takes none.
    #[serde(default)]
    pub cues: bool,
    /// The paragraph an audio response's prompt gets about what square
    /// brackets do: the sounds the TTS can make, or the delivery cues it
    /// takes; `null` = none.
    #[serde(default)]
    pub tag_hint: Option<String>,
    /// The seed every clause is sent with; `null` = none.
    #[serde(default)]
    pub seed: Option<u32>,
}
