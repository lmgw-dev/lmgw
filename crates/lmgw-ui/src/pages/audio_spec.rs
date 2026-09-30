//! Audio-lab request vocabulary: the task table, the per-task field lists, and
//! the pure request builders.
//!
//! Ported 1:1 from the old Lit island
//! (`crates/lmgw-core/assets/audio-lab/audio-lab.js:16-119`), whose field names
//! were hand-mapped from audio.cpp's own `build_request_from_json`
//! (`app/cli/request.cpp`) rather than guessed from prose. That builder is
//! task-agnostic — it reads the whole set regardless of task — so [`TASKS`]
//! says which fields a task *uses*, while the raw overrides box can still reach
//! any of them.
//!
//! Everything here is pure and target-independent on purpose: the request
//! preview and the actual request are built by the same functions (they can
//! never drift), and both are covered by native unit tests.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// The largest integer a JSON number survives round-tripping through a
/// JavaScript/JSON reader unchanged (`Number.MAX_SAFE_INTEGER`).
const MAX_SAFE_INT: i64 = 9_007_199_254_740_991;

/// The documented rate for analysis timelines: audio.cpp resamples WAV input to
/// 16 kHz mono before VAD/diarization/alignment and reports spans in samples at
/// that rate. Editable in the UI rather than baked in, because a result that
/// carries its own `sample_rate` wins over it.
pub const ANALYSIS_RATE: f64 = 16000.0;

// ---------------------------------------------------------------------------
// Field vocabulary
// ---------------------------------------------------------------------------

/// Widget a request field is edited with. `Clip` is a server-side path: a
/// picker over the voice library *plus* free text, because audio.cpp resolves
/// the path on its own filesystem and a clip may live outside the library.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Widget {
    Text,
    Textarea,
    Int,
    Float,
    Bool,
    Clip,
}

pub struct FieldSpec {
    pub name: &'static str,
    pub label: &'static str,
    pub widget: Widget,
    /// Placeholder / explanatory line; empty means "model default".
    pub hint: &'static str,
}

macro_rules! field {
    ($name:expr, $label:expr, $widget:ident) => {
        FieldSpec {
            name: $name,
            label: $label,
            widget: Widget::$widget,
            hint: "",
        }
    };
    ($name:expr, $label:expr, $widget:ident, $hint:expr) => {
        FieldSpec {
            name: $name,
            label: $label,
            widget: Widget::$widget,
            hint: $hint,
        }
    };
}

/// `/v1/tasks/run` request fields (audio-lab.js:45-89).
pub const FIELDS: &[FieldSpec] = &[
    field!("text", "Text / prompt", Textarea),
    field!("lyrics", "Lyrics", Textarea),
    field!("target_text", "Target text", Textarea),
    field!("style_ref_text", "Style reference text", Text),
    field!("reference_text", "Reference text", Text),
    field!("instruct", "Instruction / style", Text),
    field!("audio", "Input audio", Clip),
    field!("source_audio", "Source audio", Clip),
    field!("target_voice", "Target voice", Clip),
    field!("voice_ref", "Reference voice", Clip),
    field!("style_ref", "Style reference", Clip),
    field!("prosody_ref", "Prosody reference", Clip),
    field!("voice_id", "Voice id", Text),
    field!("speaker", "Speaker", Text),
    field!("track_name", "Track name", Text),
    field!("language", "Language", Text),
    field!(
        "route",
        "Route",
        Text,
        "task_route; blank = the task default"
    ),
    field!("emotion", "Emotion", Text),
    field!(
        "seed",
        "Seed",
        Text,
        "uint64; sent as a string when past 2^53"
    ),
    field!("max_tokens", "Max tokens", Int),
    field!("max_steps", "Max steps", Int),
    field!("num_inference_steps", "Inference steps", Int),
    field!("num_beams", "Beams", Int),
    field!("top_k", "top_k", Int),
    field!("duration_seconds", "Duration (s)", Float, "-1 = auto"),
    field!("target_duration_seconds", "Target duration (s)", Float),
    field!(
        "reference_duration_seconds",
        "Reference duration (s)",
        Float
    ),
    field!("guidance_scale", "Guidance scale", Float),
    field!("temperature", "Temperature", Float),
    field!("top_p", "top_p", Float),
    field!("repetition_penalty", "Repetition penalty", Float),
    field!("speaking_rate", "Speaking rate", Float),
    field!("style_language", "Style language", Text),
    field!(
        "text_chunk_mode",
        "Text chunk mode",
        Text,
        "word_budget | tag_aware | japanese | endline"
    ),
    field!("audio_chunk_mode", "Audio chunk mode", Text),
    field!("use_prosody_code", "Use prosody code", Bool),
    field!("predict_target_prosody", "Predict target prosody", Bool),
    field!("source_shift_steps", "Source shift steps", Int),
    field!("prosody_shift_steps", "Prosody shift steps", Int),
    field!("style_shift_steps", "Style shift steps", Int),
    field!("pitch_shift", "Pitch shift", Float),
    field!("energy_scale", "Energy scale", Float),
    field!("repaint_start", "Repaint start", Float),
    field!("repaint_end", "Repaint end", Float),
    field!("repaint_strength", "Repaint strength", Float),
    field!("repaint_mode", "Repaint mode", Text),
    field!("audio_chunk_seconds", "Audio chunk (s)", Float),
    field!("text_chunk_size", "Text chunk size", Int),
    field!(
        "instruments",
        "Instruments",
        Text,
        "comma-separated; blank = all"
    ),
    field!(
        "output_format",
        "Output format",
        Text,
        "midi | abc (muscriptor); blank = the model default"
    ),
    field!("do_sample", "do_sample", Bool),
    field!("use_pitch_shift", "Pitch shift on", Bool),
    field!("return_timestamps", "Return timestamps", Bool),
];

pub fn field_spec(name: &str) -> Option<&'static FieldSpec> {
    FIELDS.iter().find(|f| f.name == name)
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

pub struct TaskSpec {
    /// audio.cpp `--task` value.
    pub id: &'static str,
    pub label: &'static str,
    /// Picker grouping only — audio.cpp has no notion of it.
    pub group: &'static str,
    /// Fields this task reads (audio-lab.js:91-113).
    pub fields: &'static [&'static str],
}

/// All fourteen audio.cpp tasks, in the old island's `TASK_LABELS` order,
/// bucketed for the picker.
pub const TASKS: &[TaskSpec] = &[
    TaskSpec {
        id: "tts",
        label: "Text to speech",
        group: "Speech",
        fields: &[
            "text",
            "voice_ref",
            "voice_id",
            "reference_text",
            "language",
            "instruct",
            "seed",
            "max_tokens",
            "temperature",
            "top_p",
            "top_k",
            "speaking_rate",
            "pitch_shift",
            "energy_scale",
            "emotion",
            "style_language",
            "text_chunk_mode",
            "do_sample",
        ],
    },
    TaskSpec {
        id: "asr",
        label: "Transcription",
        group: "Speech",
        fields: &[
            "audio",
            "language",
            "return_timestamps",
            "audio_chunk_seconds",
            "audio_chunk_mode",
        ],
    },
    TaskSpec {
        id: "gen",
        label: "Generation",
        group: "Generation",
        fields: &[
            "text",
            "lyrics",
            "route",
            "duration_seconds",
            "source_audio",
            "seed",
            "guidance_scale",
            "num_inference_steps",
            "max_steps",
            "repaint_start",
            "repaint_end",
            "repaint_mode",
            "repaint_strength",
        ],
    },
    TaskSpec {
        id: "clon",
        label: "Voice cloning",
        group: "Generation",
        fields: &[
            "text",
            "voice_ref",
            "reference_text",
            "language",
            "seed",
            "max_tokens",
            "temperature",
            "top_p",
            "top_k",
            "style_language",
            "text_chunk_mode",
            "do_sample",
        ],
    },
    TaskSpec {
        id: "vc",
        label: "Voice conversion",
        group: "Conversion",
        fields: &[
            "source_audio",
            "target_voice",
            "route",
            "num_inference_steps",
            "guidance_scale",
            "use_pitch_shift",
            "use_prosody_code",
            "source_shift_steps",
            "audio_chunk_seconds",
        ],
    },
    TaskSpec {
        id: "svc",
        label: "Singing voice conversion",
        group: "Conversion",
        fields: &[
            "source_audio",
            "target_voice",
            "route",
            "num_inference_steps",
            "guidance_scale",
            "use_pitch_shift",
            "use_prosody_code",
            "source_shift_steps",
            "audio_chunk_seconds",
        ],
    },
    TaskSpec {
        id: "s2s",
        label: "Speech to speech",
        group: "Conversion",
        fields: &[
            "source_audio",
            "target_voice",
            "target_text",
            "style_ref",
            "style_ref_text",
            "prosody_ref",
            "route",
            "num_inference_steps",
            "guidance_scale",
            "use_prosody_code",
            "predict_target_prosody",
            "source_shift_steps",
            "prosody_shift_steps",
            "style_shift_steps",
        ],
    },
    TaskSpec {
        id: "sep",
        label: "Source separation",
        group: "Conversion",
        fields: &["audio", "track_name", "audio_chunk_seconds"],
    },
    TaskSpec {
        id: "vad",
        label: "Voice activity detection",
        group: "Analysis",
        fields: &["audio"],
    },
    TaskSpec {
        id: "diar",
        label: "Diarization",
        group: "Analysis",
        fields: &["audio", "return_timestamps"],
    },
    TaskSpec {
        id: "align",
        label: "Forced alignment",
        group: "Analysis",
        fields: &["audio", "text", "language", "return_timestamps"],
    },
    TaskSpec {
        id: "vdes",
        label: "Voice description",
        group: "Analysis",
        fields: &["audio", "language"],
    },
    TaskSpec {
        id: "spk",
        label: "Speaker embedding",
        group: "Analysis",
        fields: &["audio"],
    },
    // audio.cpp's fourteenth task: music -> notes. `muscriptor` transcribes to
    // MIDI or ABC (`output_format`), `sheetsage2` to a MIDI artifact; both
    // answer through `/v1/tasks/run` like the rest of the non-OpenAI tasks.
    TaskSpec {
        id: "midi",
        label: "Music transcription",
        group: "Analysis",
        fields: &[
            "audio",
            "instruments",
            "output_format",
            "seed",
            "max_tokens",
            "temperature",
            "guidance_scale",
            "num_beams",
            "do_sample",
        ],
    },
];

/// Picker group order.
pub const GROUPS: &[&str] = &["Speech", "Generation", "Conversion", "Analysis"];

pub fn task_spec(id: &str) -> Option<&'static TaskSpec> {
    TASKS.iter().find(|t| t.id == id)
}

pub fn task_label(id: &str) -> String {
    task_spec(id)
        .map(|t| t.label.to_string())
        .unwrap_or_else(|| id.to_string())
}

pub fn task_fields(id: &str) -> &'static [&'static str] {
    task_spec(id).map(|t| t.fields).unwrap_or(&[])
}

/// `tts` and `asr` have OpenAI-shaped routes (`/v1/audio/speech`,
/// `/v1/audio/transcriptions`) and therefore dedicated panels; the other twelve
/// go through the generic `/v1/tasks/run`.
pub fn is_openai_shaped(task: &str) -> bool {
    task == "tts" || task == "asr"
}

// ---------------------------------------------------------------------------
// Number / value coercion
// ---------------------------------------------------------------------------

/// `JSON.stringify` prints an integral float without a fraction (`3`, not
/// `3.0`) and audio.cpp's integer options reject the latter — so an integral
/// value goes out as a JSON integer, exactly as the old island sent it.
fn js_number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() <= MAX_SAFE_INT as f64 {
        Value::from(n as i64)
    } else {
        Value::from(n)
    }
}

/// audio.cpp takes uint64 seeds; a JSON number past 2^53 would lose precision
/// before it reaches option parsing, so those travel as a string.
fn seed_value(raw: &str) -> Value {
    match raw.parse::<i64>() {
        Ok(n) if n.abs() <= MAX_SAFE_INT => Value::from(n),
        _ => Value::from(raw.to_string()),
    }
}

fn parse_number(name: &str, raw: &str) -> Result<Value, String> {
    let n: f64 = raw.parse().map_err(|_| format!("{name}: not a number"))?;
    if !n.is_finite() {
        return Err(format!("{name}: not a number"));
    }
    Ok(js_number(n))
}

// ---------------------------------------------------------------------------
// TTS (`POST /v1/audio/speech`)
// ---------------------------------------------------------------------------

/// The TTS panel's widgets, raw. Parsed here so the "Request" disclosure and
/// the actual call are literally the same bytes.
#[derive(Clone, Default, Debug)]
pub struct SpeechForm {
    pub model: String,
    pub input: String,
    pub voice: String,
    pub voice_ref: String,
    pub reference_text: String,
    /// `speed` — a positive multiplier, on models that support speed control
    /// (audio.cpp rejects it on models that do not, rather than ignoring it).
    pub speed: String,
    /// Language code passed to models whose contract accepts one.
    pub language: String,
    /// OpenAI's `instructions` — delivery/style guidance, forwarded as the
    /// engine's `instruction` option.
    pub instructions: String,
    pub seed: String,
    pub max_tokens: String,
    pub response_format: String,
    pub stream: bool,
    pub options: String,
}

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

// ---------------------------------------------------------------------------
// ASR (`POST /v1/audio/transcriptions`)
// ---------------------------------------------------------------------------

/// JSON body for the "server-side path" ASR source.
pub fn transcription_body(model: &str, path: &str, language: &str) -> Value {
    let mut b = Map::new();
    b.insert("model".into(), Value::from(model));
    b.insert("audio".into(), Value::from(path.trim()));
    let lang = language.trim();
    if !lang.is_empty() {
        b.insert("language".into(), Value::from(lang));
    }
    Value::Object(b)
}

/// Human description of the multipart upload the ASR panel would send — there
/// is no JSON to show, but the preview must still be exact.
pub fn multipart_preview(model: &str, file: Option<&str>, language: &str, stream: bool) -> String {
    let mut lines = vec![
        "multipart/form-data".to_string(),
        format!("  model    = {model}"),
        format!("  file     = {}", file.unwrap_or("(no file selected)")),
    ];
    let lang = language.trim();
    if !lang.is_empty() {
        lines.push(format!("  language = {lang}"));
    }
    if stream {
        lines.push("  stream   = true".to_string());
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Generic task (`POST /v1/tasks/run`)
// ---------------------------------------------------------------------------

/// `{model, request}` for `/v1/tasks/run`. Only fields the user actually filled
/// are sent — an empty widget means "the model's own default", never an
/// explicit zero. `overrides` (raw JSON) merges last, so it can override a
/// widget or reach a field with no widget at all; audio.cpp's own WebUI uses
/// the same precedence order.
///
/// Key order in the emitted object is alphabetical (serde_json's map), where
/// the old island emitted TASK_FIELDS order — a cosmetic difference in the
/// preview only, JSON objects being unordered.
pub fn task_body(
    model: &str,
    task: &str,
    values: &BTreeMap<String, String>,
    flags: &BTreeMap<String, bool>,
    overrides: &str,
) -> Result<Value, String> {
    let mut req = Map::new();
    for name in task_fields(task) {
        let Some(spec) = field_spec(name) else {
            continue;
        };
        if spec.widget == Widget::Bool {
            // A false checkbox is "unset", not `false`: the model's default wins.
            if flags.get(*name).copied().unwrap_or(false) {
                req.insert((*name).into(), Value::Bool(true));
            }
            continue;
        }
        let raw = values.get(*name).map(String::as_str).unwrap_or("").trim();
        if raw.is_empty() {
            continue;
        }
        let v = match spec.widget {
            Widget::Int | Widget::Float => parse_number(name, raw)?,
            _ if *name == "seed" => seed_value(raw),
            _ => Value::from(raw.to_string()),
        };
        req.insert((*name).into(), v);
    }
    let extra = overrides.trim();
    if !extra.is_empty() {
        let v: Value = serde_json::from_str(extra)
            .map_err(|e| format!("request overrides: invalid JSON — {e}"))?;
        let Value::Object(map) = v else {
            return Err("request overrides must be a JSON object".into());
        };
        for (k, v) in map {
            req.insert(k, v);
        }
    }
    let mut out = Map::new();
    out.insert("model".into(), Value::from(model));
    out.insert("request".into(), Value::Object(req));
    Ok(Value::Object(out))
}

// ---------------------------------------------------------------------------
// Timelines and formatting
// ---------------------------------------------------------------------------

/// Timeline rows arrive in samples. Any task with audio output states its own
/// `sample_rate`, which wins; the analysis tasks have no audio to state one on,
/// so the editable field (else [`ANALYSIS_RATE`]) supplies it.
pub fn timeline_rate(from_result: Option<f64>, user_field: &str) -> f64 {
    from_result
        .filter(|r| *r > 0.0)
        .or_else(|| user_field.trim().parse::<f64>().ok().filter(|r| *r > 0.0))
        .unwrap_or(ANALYSIS_RATE)
}

/// A sample index rendered as seconds at `rate`; missing values stay visible
/// as an en dash rather than a misleading 0.00s.
pub fn samples_to_secs(sample: Option<f64>, rate: f64) -> String {
    match sample {
        Some(n) if rate > 0.0 => format!("{:.2}s", n / rate),
        _ => "–".to_string(),
    }
}

pub fn fmt_ms(ms: Option<f64>) -> String {
    match ms {
        None => "–".to_string(),
        Some(ms) if ms >= 1000.0 => format!("{:.2}s", ms / 1000.0),
        Some(ms) => format!("{:.0}ms", ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vals(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn every_task_field_has_a_widget() {
        for t in TASKS {
            for name in t.fields {
                assert!(
                    field_spec(name).is_some(),
                    "task {} references unknown field {name}",
                    t.id
                );
            }
        }
    }

    #[test]
    fn task_table_matches_audio_cpps_task_vocabulary() {
        assert_eq!(TASKS.len(), 14);
        let ids: Vec<&str> = TASKS.iter().map(|t| t.id).collect();
        assert_eq!(
            ids,
            vec![
                "tts", "asr", "gen", "clon", "vc", "svc", "s2s", "sep", "vad", "diar", "align",
                "vdes", "spk", "midi"
            ]
        );
        assert_eq!(FIELDS.len(), 53);
        assert!(is_openai_shaped("tts") && is_openai_shaped("asr"));
        assert!(!is_openai_shaped("vc"));
        assert_eq!(task_fields("vad"), &["audio"]);
        assert_eq!(task_fields("diar"), &["audio", "return_timestamps"]);
        assert_eq!(
            task_fields("sep"),
            &["audio", "track_name", "audio_chunk_seconds"]
        );
        // Every task is in a group the picker renders.
        for t in TASKS {
            assert!(GROUPS.contains(&t.group), "{} has a stray group", t.id);
        }
    }

    #[test]
    fn speech_body_sends_only_what_was_filled_in() {
        let f = SpeechForm {
            model: "audio/kokoro".into(),
            input: "hello".into(),
            ..Default::default()
        };
        assert_eq!(
            speech_body(&f).unwrap(),
            json!({"model": "audio/kokoro", "input": "hello"})
        );
    }

    /// `speed` is audio.cpp's newest speech field and it is a *number*: a
    /// string there is rejected, and an integral one must not print as `1.0`
    /// where the engine wants an int — the same coercion the rest of the lab
    /// uses.
    #[test]
    fn speech_body_carries_speed_language_and_instructions() {
        let f = SpeechForm {
            model: "m".into(),
            input: "hi".into(),
            speed: "1.25".into(),
            language: "de".into(),
            instructions: "read it slowly".into(),
            ..Default::default()
        };
        let b = speech_body(&f).unwrap();
        assert_eq!(b["speed"], json!(1.25));
        assert_eq!(b["language"], json!("de"));
        assert_eq!(b["instructions"], json!("read it slowly"));

        // Blank stays out: an unset widget means the model's own default,
        // never an explicit 1.0 that a model without speed control refuses.
        let b = speech_body(&SpeechForm {
            model: "m".into(),
            input: "hi".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(b.get("speed").is_none());
        assert!(b.get("language").is_none());
        assert!(b.get("instructions").is_none());

        let err = speech_body(&SpeechForm {
            model: "m".into(),
            input: "hi".into(),
            speed: "fast".into(),
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.starts_with("speed:"), "{err}");
    }

    #[test]
    fn speech_body_streaming_forces_pcm_over_sse() {
        let f = SpeechForm {
            model: "m".into(),
            input: "hi".into(),
            response_format: "json".into(),
            stream: true,
            ..Default::default()
        };
        let b = speech_body(&f).unwrap();
        assert_eq!(b["stream_format"], json!("sse"));
        assert_eq!(b["response_format"], json!("pcm"));
    }

    #[test]
    fn speech_body_seeds_past_2_53_stay_strings() {
        let mk = |seed: &str| {
            speech_body(&SpeechForm {
                model: "m".into(),
                input: "x".into(),
                seed: seed.into(),
                ..Default::default()
            })
            .unwrap()["seed"]
                .clone()
        };
        assert_eq!(mk("42"), json!(42));
        assert_eq!(mk("18446744073709551615"), json!("18446744073709551615"));
    }

    #[test]
    fn speech_body_surfaces_bad_options_json() {
        let f = SpeechForm {
            model: "m".into(),
            input: "x".into(),
            options: "{temperature: 0.8}".into(),
            ..Default::default()
        };
        assert!(speech_body(&f).unwrap_err().starts_with("options:"));
    }

    #[test]
    fn task_body_omits_empty_widgets_and_false_flags() {
        let body = task_body(
            "audio/whisper",
            "asr",
            &vals(&[("audio", " /models/voices/a.wav "), ("language", "  ")]),
            &BTreeMap::from([("return_timestamps".to_string(), false)]),
            "",
        )
        .unwrap();
        assert_eq!(
            body,
            json!({
                "model": "audio/whisper",
                "request": {"audio": "/models/voices/a.wav"}
            })
        );
    }

    #[test]
    fn task_body_types_numbers_like_json_stringify() {
        let body = task_body(
            "m",
            "vc",
            &vals(&[
                ("source_audio", "/models/voices/s.wav"),
                ("num_inference_steps", "10"),
                ("guidance_scale", "1.5"),
                ("audio_chunk_seconds", "30"),
            ]),
            &BTreeMap::from([("use_pitch_shift".to_string(), true)]),
            "",
        )
        .unwrap();
        let req = &body["request"];
        assert_eq!(req["num_inference_steps"], json!(10));
        assert_eq!(req["guidance_scale"], json!(1.5));
        // an integral float prints as an integer, as JSON.stringify would
        assert_eq!(
            serde_json::to_string(&req["audio_chunk_seconds"]).unwrap(),
            "30"
        );
        assert_eq!(req["use_pitch_shift"], json!(true));
    }

    #[test]
    fn task_body_overrides_merge_last() {
        let body = task_body(
            "m",
            "gen",
            &vals(&[("text", "a song"), ("duration_seconds", "-1")]),
            &BTreeMap::new(),
            r#"{"duration_seconds": 12, "options": {"num_inference_steps": 10}}"#,
        )
        .unwrap();
        assert_eq!(body["request"]["duration_seconds"], json!(12));
        assert_eq!(body["request"]["options"]["num_inference_steps"], json!(10));
        assert_eq!(body["request"]["text"], json!("a song"));
    }

    #[test]
    fn task_body_rejects_non_object_overrides_and_bad_numbers() {
        let err = task_body("m", "gen", &BTreeMap::new(), &BTreeMap::new(), "[1,2]").unwrap_err();
        assert_eq!(err, "request overrides must be a JSON object");
        let err = task_body(
            "m",
            "vc",
            &vals(&[("num_inference_steps", "ten")]),
            &BTreeMap::new(),
            "",
        )
        .unwrap_err();
        assert_eq!(err, "num_inference_steps: not a number");
    }

    #[test]
    fn transcription_and_multipart_previews() {
        assert_eq!(
            transcription_body("m", " /models/voices/a.wav ", " de "),
            json!({"model": "m", "audio": "/models/voices/a.wav", "language": "de"})
        );
        assert_eq!(
            multipart_preview("m", Some("clip.wav"), "", false),
            "multipart/form-data\n  model    = m\n  file     = clip.wav"
        );
        assert_eq!(
            multipart_preview("m", None, "en", true),
            "multipart/form-data\n  model    = m\n  file     = (no file selected)\n  language = en\n  stream   = true"
        );
    }

    #[test]
    fn timeline_rate_defers_to_the_result_then_the_field() {
        assert_eq!(timeline_rate(Some(24000.0), "16000"), 24000.0);
        assert_eq!(timeline_rate(None, "22050"), 22050.0);
        assert_eq!(timeline_rate(None, ""), ANALYSIS_RATE);
        assert_eq!(timeline_rate(None, "junk"), ANALYSIS_RATE);
        assert_eq!(timeline_rate(Some(0.0), "0"), ANALYSIS_RATE);
    }

    #[test]
    fn samples_render_as_seconds() {
        assert_eq!(samples_to_secs(Some(16000.0), 16000.0), "1.00s");
        assert_eq!(samples_to_secs(Some(24000.0), 16000.0), "1.50s");
        assert_eq!(samples_to_secs(None, 16000.0), "–");
        assert_eq!(fmt_ms(Some(999.4)), "999ms");
        assert_eq!(fmt_ms(Some(1500.0)), "1.50s");
        assert_eq!(fmt_ms(None), "–");
    }
}
