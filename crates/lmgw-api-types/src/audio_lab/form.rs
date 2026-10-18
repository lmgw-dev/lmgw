//! The Audio lab's form (labs-store design §5): the speech panel's fields
//! and the one function that turns them into the `/v1/audio/speech` body,
//! the settings a preset keeps, and the whole form a kept group stores, so
//! "Open in form" puts every panel back as it was.
//!
//! The speech builder lives here rather than in the page because a saved
//! `audio_settings` preset is checked on the gateway by the same rules the
//! page builds the request with: a speed or a token count that is no number,
//! options that are no JSON.
//!
//! A kept run's request is the request as sent; the form is what the page
//! held, which the request alone does not say (the PCM format a stream is
//! assembled with, which panel and task were picked).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::lab_store::title_of;

/// The largest integer a JSON number survives round-tripping through a
/// JavaScript/JSON reader unchanged (`Number.MAX_SAFE_INTEGER`).
const MAX_SAFE_INT: i64 = 9_007_199_254_740_991;

/// `JSON.stringify` prints an integral float without a fraction (`3`, not
/// `3.0`) and audio.cpp's integer options reject the latter — so an integral
/// value goes out as a JSON integer, exactly as the old island sent it.
pub fn js_number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() <= MAX_SAFE_INT as f64 {
        Value::from(n as i64)
    } else {
        Value::from(n)
    }
}

/// audio.cpp takes uint64 seeds; a JSON number past 2^53 would lose precision
/// before it reaches option parsing, so those travel as a string.
pub fn seed_value(raw: &str) -> Value {
    match raw.parse::<i64>() {
        Ok(n) if n.abs() <= MAX_SAFE_INT => Value::from(n),
        _ => Value::from(raw.to_string()),
    }
}

/// A form field's number, named when it is none.
pub fn parse_number(name: &str, raw: &str) -> Result<Value, String> {
    let n: f64 = raw.parse().map_err(|_| format!("{name}: not a number"))?;
    if !n.is_finite() {
        return Err(format!("{name}: not a number"));
    }
    Ok(js_number(n))
}

/// The TTS panel's widgets, raw. Parsed by [`speech_body`] so the "Request"
/// disclosure and the actual call are literally the same bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SpeechForm {
    pub model: String,
    pub input: String,
    pub voice: String,
    pub voice_ref: String,
    pub reference_text: String,
    /// `speed`: a positive multiplier, on models that support speed control
    /// (audio.cpp rejects it on models that do not, rather than ignoring it).
    pub speed: String,
    /// Language code passed to models whose contract accepts one; the
    /// transcription panel sends the same field.
    pub language: String,
    /// OpenAI's `instructions`: delivery and style guidance, forwarded as the
    /// engine's `instruction` option.
    pub instructions: String,
    pub seed: String,
    pub max_tokens: String,
    pub response_format: String,
    /// Stream the answer as server-sent events of PCM.
    pub stream: bool,
    /// Engine options as JSON text, sent as the body's `options`.
    pub options: String,
}

/// The exact `POST /v1/audio/speech` document the panel sends.
pub fn speech_body(f: &SpeechForm) -> Result<Value, String> {
    let mut b = Map::new();
    b.insert("model".into(), Value::from(f.model.clone()));
    b.insert("input".into(), Value::from(f.input.clone()));
    for (key, raw) in [
        ("voice", &f.voice),
        ("voice_ref", &f.voice_ref),
        ("reference_text", &f.reference_text),
        ("language", &f.language),
        ("instructions", &f.instructions),
    ] {
        let s = raw.trim();
        if !s.is_empty() {
            b.insert(key.into(), Value::from(s.to_string()));
        }
    }
    let speed = f.speed.trim();
    if !speed.is_empty() {
        b.insert("speed".into(), parse_number("speed", speed)?);
    }
    let seed = f.seed.trim();
    if !seed.is_empty() {
        b.insert("seed".into(), seed_value(seed));
    }
    let max_tokens = f.max_tokens.trim();
    if !max_tokens.is_empty() {
        b.insert("max_tokens".into(), parse_number("max tokens", max_tokens)?);
    }
    if f.stream {
        // SSE only ever carries raw PCM — the header comes from the declared
        // rate/channels/depth on the client side.
        b.insert("stream_format".into(), Value::from("sse"));
        b.insert("response_format".into(), Value::from("pcm"));
    } else if !f.response_format.is_empty() {
        b.insert(
            "response_format".into(),
            Value::from(f.response_format.clone()),
        );
    }
    let options = f.options.trim();
    if !options.is_empty() {
        // Surfaced as an error rather than swallowed — a typo here silently
        // dropping every advanced parameter is the worst outcome.
        let v: Value =
            serde_json::from_str(options).map_err(|e| format!("options: invalid JSON — {e}"))?;
        b.insert("options".into(), v);
    }
    Ok(Value::Object(b))
}

/// The PCM a streamed answer carries, as the page assembles it into a WAV:
/// the stream itself states none of it. The strings the inputs hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PcmFormat {
    pub rate: String,
    pub channels: String,
    pub bits: String,
}

impl Default for PcmFormat {
    /// The panel's own starting values: audio.cpp's PCM is 24 kHz mono
    /// 16-bit unless the model says otherwise.
    fn default() -> Self {
        Self {
            rate: "24000".into(),
            channels: "1".into(),
            bits: "16".into(),
        }
    }
}

/// The `audio_settings` preset (labs-store design §5): the speech panel's
/// fields and the PCM format a stream is assembled with.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioSettings {
    pub speech: SpeechForm,
    pub pcm: PcmFormat,
}

impl AudioSettings {
    /// What the settings say that does not read: a number field that is no
    /// number, options that are no JSON. A blank text and a blank model are
    /// settings too: the preset is a form part.
    pub fn check(&self) -> Result<(), String> {
        speech_body(&self.speech).map(|_| ())?;
        for (name, raw) in [
            ("PCM rate", &self.pcm.rate),
            ("PCM channels", &self.pcm.channels),
            ("PCM bits", &self.pcm.bits),
        ] {
            let raw = raw.trim();
            if !raw.is_empty() {
                raw.parse::<u32>()
                    .map_err(|_| format!("{name}: '{raw}' is not a whole number"))?;
            }
        }
        Ok(())
    }
}

/// The transcription panel's fields (the language is the speech form's).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AsrForm {
    /// `upload` or `path`.
    pub source: String,
    /// A clip on the model server, for the `path` source.
    pub path: String,
    /// The upload's file name, as the page held it; the bytes are the
    /// group's input artifact once sent.
    pub file_name: String,
    pub stream: bool,
    /// Send to `/v1/audio/transcriptions/details`.
    pub details: bool,
}

impl Default for AsrForm {
    fn default() -> Self {
        Self {
            source: "upload".into(),
            path: String::new(),
            file_name: String::new(),
            stream: false,
            details: false,
        }
    }
}

/// The generic task panel's fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskForm {
    /// Each field's text, by the request field it fills.
    pub values: BTreeMap<String, String>,
    /// Each checkbox, by the request field it sets.
    pub flags: BTreeMap<String, bool>,
    /// Raw JSON merged into the request last.
    pub json: String,
    /// Send to `/v1/tasks/stream`.
    pub stream: bool,
}

/// The whole form a kept group stores: which task's panel was shown, every
/// panel's fields and the speech settings, so a re-opened group puts the
/// page back as it was when it asked. The model is the plan's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioLabForm {
    /// The task picker's id (`tts`, `asr`, `vc`, ...).
    pub task: String,
    /// The OpenAI-shaped task was sent through `/v1/tasks/run` instead.
    pub generic: bool,
    pub settings: AudioSettings,
    pub asr: AsrForm,
    pub tasks: TaskForm,
    /// The timeline rate field, for spans in samples.
    pub timeline_rate: String,
}

/// The route a kept audio group's one cell asks: its plan's variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum AudioRoute {
    /// `/v1/audio/speech`, through `/audio-lab/api/speech`.
    Speech,
    /// `/v1/audio/transcriptions`, through `/audio-lab/api/transcriptions`.
    Transcription,
    /// `/v1/audio/transcriptions/details` (`?details=1`).
    TranscriptionDetails,
    /// `/v1/audio/alignments`, through `/audio-lab/api/alignments`.
    Alignment,
    /// `/v1/tasks/run`, through `/audio-lab/api/tasks/run`.
    TaskRun,
    /// `/v1/tasks/stream` (`?stream=1`).
    TaskStream,
}

impl AudioRoute {
    pub const ALL: [Self; 6] = [
        Self::Speech,
        Self::Transcription,
        Self::TranscriptionDetails,
        Self::Alignment,
        Self::TaskRun,
        Self::TaskStream,
    ];

    /// The plan's variant.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Speech => "speech",
            Self::Transcription => "transcription",
            Self::TranscriptionDetails => "transcription_details",
            Self::Alignment => "alignment",
            Self::TaskRun => "task_run",
            Self::TaskStream => "task_stream",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == s)
    }

    /// The `/v1` route the cell's run records.
    pub fn endpoint(self) -> &'static str {
        match self {
            Self::Speech => "/v1/audio/speech",
            Self::Transcription => "/v1/audio/transcriptions",
            Self::TranscriptionDetails => "/v1/audio/transcriptions/details",
            Self::Alignment => "/v1/audio/alignments",
            Self::TaskRun => "/v1/tasks/run",
            Self::TaskStream => "/v1/tasks/stream",
        }
    }
}

/// `POST /audio-lab/api/groups`: a kept group of one cell, made before its
/// request is sent. The request itself goes to the route's lab hand-off with
/// `?group=<id>`, which records it as the group's run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioAsk {
    /// The alias the request names.
    pub model: String,
    pub route: AudioRoute,
    pub form: AudioLabForm,
}

impl AudioAsk {
    /// The group's title: the text to speak, the file or path to
    /// transcribe, else the task.
    pub fn title(&self) -> String {
        let f = &self.form;
        let asr = if f.asr.source == "path" {
            &f.asr.path
        } else {
            &f.asr.file_name
        };
        let text = match self.route {
            AudioRoute::Speech => &f.settings.speech.input,
            AudioRoute::Transcription
            | AudioRoute::TranscriptionDetails
            | AudioRoute::Alignment => asr,
            AudioRoute::TaskRun | AudioRoute::TaskStream => &f.task,
        };
        title_of(text).unwrap_or_else(|| self.route.as_str().replace('_', " "))
    }
}

/// The `?group=` of a lab hand-off that is recorded: the kept group (made
/// by `POST /audio-lab/api/groups`) whose run this request is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GroupQuery {
    /// The group; absent, the request is relayed and nothing is kept.
    #[serde(default)]
    pub group: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_check_their_numbers_and_options_but_not_their_text() {
        assert!(AudioSettings::default().check().is_ok());
        let mut s = AudioSettings::default();
        s.speech.speed = "fast".into();
        assert!(s.check().unwrap_err().starts_with("speed:"));
        let mut s = AudioSettings::default();
        s.speech.options = "{nope".into();
        assert!(s.check().unwrap_err().starts_with("options:"));
        let mut s = AudioSettings::default();
        s.pcm.rate = "24k".into();
        assert!(s.check().unwrap_err().contains("PCM rate"));
    }

    #[test]
    fn a_route_reads_back_and_names_its_endpoint() {
        for r in AudioRoute::ALL {
            assert_eq!(AudioRoute::parse(r.as_str()), Some(r));
            assert_eq!(
                serde_json::to_value(r).unwrap(),
                Value::from(r.as_str()),
                "the wire spelling is the variant's"
            );
        }
        assert_eq!(
            AudioRoute::TranscriptionDetails.endpoint(),
            "/v1/audio/transcriptions/details"
        );
    }

    #[test]
    fn a_title_is_what_the_route_was_asked_about() {
        let mut ask = AudioAsk {
            model: "audio/tts".into(),
            route: AudioRoute::Speech,
            form: AudioLabForm::default(),
        };
        ask.form.settings.speech.input = "Hello there.\nSecond line".into();
        assert_eq!(ask.title(), "Hello there.");
        ask.route = AudioRoute::Transcription;
        ask.form.asr.file_name = "memo.wav".into();
        assert_eq!(ask.title(), "memo.wav");
        ask.form.asr.source = "path".into();
        ask.form.asr.path = "/models/voices/a.wav".into();
        assert_eq!(ask.title(), "/models/voices/a.wav");
        ask.route = AudioRoute::TaskStream;
        assert_eq!(ask.title(), "task stream");
    }
}
