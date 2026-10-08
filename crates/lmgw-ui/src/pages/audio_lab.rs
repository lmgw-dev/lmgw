//! Audio lab — Chat's counterpart for audio.cpp models: pick a served model,
//! fill the form its task needs, fire the request, hear the result.
//!
//! Port of the old Lit island (`crates/lmgw-core/assets/audio-lab/audio-lab.js`)
//! onto the same `/audio-lab/api/*` endpoints, which are unchanged: they are
//! thin in-process wrappers over the *real* `/v1/audio/*` and `/v1/tasks/*`
//! handlers, so what this page exercises is the gateway passthrough — including
//! its error shape, which is why upstream failures are rendered from
//! `error.message` rather than guessed at.
//!
//! The request vocabulary and the pure builders live in [`super::audio_spec`],
//! SSE + WAV assembly in [`super::audio_stream`]; both are unit-tested natively.
//!
//! The layout is the labs' shared one ([`super::lab_frame`]): the form beside
//! the session's run history, the voice library folded to the right.
//!
//! Deliberately absent, as in the old app: microphone capture (live
//! transcription needs a duplex chunked upload the gateway does not proxy),
//! any persistence of the history or of presets, and any container control —
//! starting or applying the audio container stays on its own surface.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use gloo_net::http::Request;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use super::audio_spec::{
    field_spec, fmt_ms, is_openai_shaped, multipart_preview, samples_to_secs, speech_body,
    task_body, task_fields, task_label, timeline_rate, transcription_body, SpeechForm, Widget,
    GROUPS, TASKS,
};
use super::audio_stream::{pcm_to_wav, read_sse, AudioEvent, StreamAcc};
use super::lab_frame::{Check, Fname, LabFrame, LabView, TaskGroup, TaskMenu, TaskOpt};
use crate::catalog::CatalogEntry;
use crate::fmt::human_bytes;
use crate::scope::Scope;
use crate::widgets::model_picker::ListStatus;
use crate::widgets::voice_picker::voice_name;
use crate::widgets::{use_toasts, ConfirmButton, Explain, ModelPicker, Select};

// ---------------------------------------------------------------------------
// Server shapes
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
struct AudioModel {
    alias: String,
    model_id: String,
    family: String,
    task: String,
    mode: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
struct RuntimeRow {
    model_id: String,
    state: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct ModelsResp {
    models: Vec<AudioModel>,
    /// Per-model containers of the audio class (per-model-containers §3.2) —
    /// there is no single audio.cpp container to report a state for any more.
    runtime: Vec<RuntimeRow>,
    voices_dir: Option<String>,
    /// The class's `voice_dir` points at this library, so a clip's name is a
    /// voice every TTS model here answers to — not just a path to paste.
    voice_library_active: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
struct Clip {
    name: String,
    size: u64,
    server_path: String,
    /// The name a request's `voice` uses once the class's `voice_dir` points
    /// at this library — the file name without its extension.
    voice: String,
    /// The clip's line in `prompt_text`: what it says, injected as
    /// `reference_text` when a request clones it by name.
    transcript: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct RefsResp {
    clips: Vec<Clip>,
    error: Option<String>,
    /// An upload's clips the settings' transcription model wrote a
    /// transcript for — `{clip, transcript_source, answered_by, by}` or
    /// `{clip, transcribe_error}` each (audio-class gap 5).
    transcribed: Vec<Value>,
    /// One clip transcribed: which model wrote it, in a sentence — a
    /// fallback that answered named as one.
    message: Option<String>,
}

// ---------------------------------------------------------------------------
// Fetch helpers
//
// The `/audio-lab/api/*` plane answers failures as `{"error": "…"}` (or the
// upstream's own `{"error": {"message": …}}`), not the admin plane's
// `{code, message}` — so `crate::api` would swallow the message.
// ---------------------------------------------------------------------------

/// Message out of an error body, falling back to the raw text and finally the
/// status line — the old island's `consumeError`.
fn error_message(text: &str, status: u16) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if let Some(m) = v["error"]["message"].as_str().filter(|s| !s.is_empty()) {
            return m.to_string();
        }
        if let Some(m) = v["error"].as_str().filter(|s| !s.is_empty()) {
            return m.to_string();
        }
    }
    if text.trim().is_empty() {
        format!("HTTP {status}")
    } else {
        text.to_string()
    }
}

async fn finish<T: DeserializeOwned>(resp: gloo_net::http::Response) -> Result<T, String> {
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if (200..300).contains(&status) {
        serde_json::from_str(&text).map_err(|e| format!("bad response body: {e}"))
    } else {
        Err(error_message(&text, status))
    }
}

async fn lab_get<T: DeserializeOwned>(url: String) -> Result<T, String> {
    let resp = Request::get(&url).send().await.map_err(|e| e.to_string())?;
    finish(resp).await
}

async fn lab_post_empty<T: DeserializeOwned>(url: String) -> Result<T, String> {
    let resp = Request::post(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    finish(resp).await
}

/// POST with a JSON body — the transcript save, and the only lab call that
/// carries one to a `/refs/` route.
async fn lab_post_json<T: DeserializeOwned>(url: String, body: Value) -> Result<T, String> {
    let resp = Request::post(&url)
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    finish(resp).await
}

fn enc(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct Track {
    /// Stem id for `named_audio_outputs` (separation); empty for a single track.
    id: String,
    url: String,
    name: String,
    size: usize,
    sample_rate: Option<f64>,
}

/// One shape for every response kind, so the renderer never has to ask which
/// endpoint produced it: a task result may carry any mix of audio tracks, text
/// and timeline rows.
#[derive(Clone, Debug, Default)]
struct RunResult {
    tracks: Vec<Track>,
    text: String,
    language: String,
    segments: Vec<Value>,
    speaker_turns: Vec<Value>,
    words: Vec<Value>,
    json: Option<Value>,
    /// `/v1/tasks/stream` only — a buffered event list inside the JSON.
    events: Option<Vec<Value>>,
}

impl RunResult {
    fn has_timeline(&self) -> bool {
        !self.segments.is_empty() || !self.speaker_turns.is_empty() || !self.words.is_empty()
    }
    fn result_rate(&self) -> Option<f64> {
        self.tracks.iter().find_map(|t| t.sample_rate)
    }
}

/// One finished run in the history: what was asked of which model, and what
/// came back (or why nothing did).
#[derive(Clone, Debug)]
struct AudioRun {
    head: RunHead,
    result: RunResult,
    stats: Option<RunStats>,
    error: String,
    /// The object URLs its players and downloads point at, revoked with it.
    urls: Vec<String>,
}

/// What a run was, fixed when it starts.
#[derive(Clone, Debug, PartialEq)]
struct RunHead {
    id: u32,
    model: String,
    task: String,
    endpoint: &'static str,
    at: String,
}

#[derive(Clone, Debug, Default)]
struct RunStats {
    status: u16,
    ttfb_ms: Option<f64>,
    total_ms: Option<f64>,
    bytes: Option<usize>,
    mime: String,
    /// audio.cpp's own numbers, when the JSON body carries them — server
    /// measurement beats the client's round trip (same rule as Chat).
    wall_ms: Option<f64>,
    audio_ms: Option<f64>,
    rtf: Option<f64>,
    /// `x-lmgw-speech`: what lmgw changed in the request on its way to the
    /// model (a voice moved, tags stripped, characters the engine cannot
    /// say replaced or dropped).
    shaped: Option<String>,
}

/// `alias` → a safe file stem for downloads (runs of unusable characters
/// collapse to one dash, as in the old app).
fn track_stem(alias: &str) -> String {
    let mut out = String::with_capacity(alias.len());
    let mut dash = false;
    for c in alias.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    if out.is_empty() {
        "audio".to_string()
    } else {
        out
    }
}

fn blob_url(bytes: &[u8], mime: &str) -> Option<String> {
    let arr = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::new();
    parts.push(&arr);
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type(mime);
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).ok()?;
    web_sys::Url::create_object_url_with_blob(&blob).ok()
}

// ---------------------------------------------------------------------------
// Form + page state (Copy bundles of signals, so handlers and child components
// can capture them freely — signals created inside a handler would die with
// that handler's view scope)
// ---------------------------------------------------------------------------

/// The lab route behind the ASR panel. `?details=1` is the whole difference
/// between the two transcription endpoints as far as the client is concerned
/// — same body, same form fields, richer answer.
fn transcription_url(details: bool) -> String {
    match details {
        true => "/audio-lab/api/transcriptions?details=1".into(),
        false => "/audio-lab/api/transcriptions".into(),
    }
}

#[derive(Clone, Copy)]
struct Form {
    // TTS
    input: RwSignal<String>,
    voice: RwSignal<String>,
    voice_ref: RwSignal<String>,
    reference_text: RwSignal<String>,
    speed: RwSignal<String>,
    instructions: RwSignal<String>,
    seed: RwSignal<String>,
    max_tokens: RwSignal<String>,
    response_format: RwSignal<String>,
    stream: RwSignal<bool>,
    pcm_rate: RwSignal<String>,
    pcm_channels: RwSignal<String>,
    pcm_bits: RwSignal<String>,
    options: RwSignal<String>,
    // ASR
    asr_source: RwSignal<String>, // "upload" | "path"
    asr_path: RwSignal<String>,
    asr_file: StoredValue<Option<web_sys::File>>,
    asr_file_name: RwSignal<String>,
    language: RwSignal<String>,
    asr_stream: RwSignal<bool>,
    /// Send the same request to `…/transcriptions/details`, which answers
    /// with the word timings, segments and speaker turns the plain route
    /// drops — the lab is where you find out whether *this* model makes any.
    asr_details: RwSignal<bool>,
    // Generic /v1/tasks/run panel
    task_values: RwSignal<BTreeMap<String, String>>,
    task_flags: RwSignal<BTreeMap<String, bool>>,
    task_json: RwSignal<String>,
    task_stream: RwSignal<bool>,
    // Result view
    timeline_rate: RwSignal<String>,
}

impl Form {
    fn new() -> Self {
        Self {
            input: RwSignal::new(String::new()),
            voice: RwSignal::new(String::new()),
            voice_ref: RwSignal::new(String::new()),
            reference_text: RwSignal::new(String::new()),
            speed: RwSignal::new(String::new()),
            instructions: RwSignal::new(String::new()),
            seed: RwSignal::new(String::new()),
            max_tokens: RwSignal::new(String::new()),
            response_format: RwSignal::new(String::new()),
            stream: RwSignal::new(false),
            pcm_rate: RwSignal::new("24000".to_string()),
            pcm_channels: RwSignal::new("1".to_string()),
            pcm_bits: RwSignal::new("16".to_string()),
            options: RwSignal::new(String::new()),
            asr_source: RwSignal::new("upload".to_string()),
            asr_path: RwSignal::new(String::new()),
            asr_file: StoredValue::new(None),
            asr_file_name: RwSignal::new(String::new()),
            language: RwSignal::new(String::new()),
            asr_stream: RwSignal::new(false),
            asr_details: RwSignal::new(false),
            task_values: RwSignal::new(BTreeMap::new()),
            task_flags: RwSignal::new(BTreeMap::new()),
            task_json: RwSignal::new(String::new()),
            task_stream: RwSignal::new(false),
            timeline_rate: RwSignal::new("16000".to_string()),
        }
    }
}

#[derive(Clone, Copy)]
struct Lab {
    models: RwSignal<Vec<AudioModel>>,
    runtime: RwSignal<Vec<RuntimeRow>>,
    alias: RwSignal<String>,
    /// Which task's form is shown. Follows the selected model, but is
    /// overridable: audio.cpp routes by the *model*, never by this picker, so
    /// an override only changes which fields the form offers (a bench needs to
    /// be able to look at every request shape).
    task_sel: RwSignal<String>,
    /// Force the generic panel for a tts/asr model — the same model is
    /// reachable both ways, and comparing them is half the point of a bench.
    generic: RwSignal<bool>,
    voices: RwSignal<Vec<String>>,
    /// Where `voices` stands for the selected model. It is only ever read
    /// from a container that is already up, so this says why it is empty.
    voice_list: RwSignal<VoiceList>,
    clips: RwSignal<Vec<Clip>>,
    clips_err: RwSignal<String>,
    voices_dir: RwSignal<String>,
    voice_library_active: RwSignal<bool>,
    form: Form,
    /// Where the model list stands (the picker says so while it loads).
    list: RwSignal<ListStatus>,
    running: RwSignal<bool>,
    /// The run in flight: what it is, and what has arrived so far (a stream
    /// fills it as it plays). It joins `runs` when it ends.
    current: RwSignal<Option<RunHead>>,
    error: RwSignal<String>,
    result: RwSignal<Option<RunResult>>,
    stats: RwSignal<Option<RunStats>>,
    /// Finished runs, newest first.
    runs: RwSignal<Vec<AudioRun>>,
    next_run: StoredValue<u32>,
    /// Stop was pressed: an aborted run says so instead of nothing.
    stopped: StoredValue<bool>,
    view: RwSignal<LabView>,
    uploading: RwSignal<bool>,
    preview_clip: RwSignal<Option<String>>,
    aborter: StoredValue<Option<web_sys::AbortController>>,
    /// Object URLs of the run in flight; they move into its history entry.
    urls: StoredValue<Vec<String>>,
    /// The page's lifetime: a read or a run that answers after the lab was
    /// left ends there (review code:C4).
    scope: Scope,
}

/// The voice list of the selected model. Opening the lab must not load a
/// model onto the GPU, so the list is read from a running container only, and
/// loading one for it is the owner's explicit call.
#[derive(Clone, Debug, PartialEq)]
enum VoiceList {
    /// No model selected yet.
    Idle,
    /// The model is not running; nothing was started to ask it.
    NotRunning,
    Loading,
    Loaded,
    Failed(String),
}

enum Dispatch {
    Json {
        url: String,
        body: Value,
    },
    Form {
        url: String,
        form: web_sys::FormData,
    },
}

impl Lab {
    fn model(&self) -> Option<AudioModel> {
        let alias = self.alias.get();
        self.models.get().into_iter().find(|m| m.alias == alias)
    }

    /// The selected model's own container answers — each audio model has its
    /// own container now, so "audio is up" says nothing about this one.
    fn model_ready(&self) -> bool {
        let Some(m) = self.model() else {
            return false;
        };
        self.runtime.with(|rows| {
            rows.iter()
                .any(|r| r.model_id == m.model_id && r.state == "ready")
        })
    }

    /// Read the selected model's voices. `start` also loads the model when it
    /// is down, which is only ever sent from the button that says so.
    fn load_voices(self, start: bool) {
        let alias = self.alias.get_untracked();
        if alias.is_empty() {
            return;
        }
        self.voice_list.set(VoiceList::Loading);
        let url = format!(
            "/audio-lab/api/voices?model={}{}",
            enc(&alias),
            if start { "&start=1" } else { "" }
        );
        self.scope.spawn(async move {
            let res = lab_get::<Value>(url).await;
            // The picker moved on while this was in flight.
            if self.alias.get_untracked() != alias {
                return;
            }
            match res {
                Ok(v) if v["running"] == Value::Bool(false) => {
                    self.voices.set(Vec::new());
                    self.voice_list.set(VoiceList::NotRunning);
                }
                Ok(v) => {
                    self.voices.set(
                        v["voices"]
                            .as_array()
                            .map(|a| a.iter().filter_map(voice_name).collect())
                            .unwrap_or_default(),
                    );
                    self.voice_list.set(VoiceList::Loaded);
                }
                Err(e) => self.voice_list.set(VoiceList::Failed(e)),
            }
        });
    }

    fn task(&self) -> String {
        self.task_sel.get()
    }

    fn streaming_model(&self) -> bool {
        self.model().map(|m| m.mode == "streaming").unwrap_or(false)
    }

    fn use_generic(&self) -> bool {
        !is_openai_shaped(&self.task()) || self.generic.get()
    }

    fn speech_form(&self) -> SpeechForm {
        let f = self.form;
        SpeechForm {
            model: self.alias.get(),
            input: f.input.get(),
            voice: f.voice.get(),
            voice_ref: f.voice_ref.get(),
            reference_text: f.reference_text.get(),
            speed: f.speed.get(),
            // Shared with the ASR panel: one language box per lab, because it
            // means the same thing on both routes.
            language: f.language.get(),
            instructions: f.instructions.get(),
            seed: f.seed.get(),
            max_tokens: f.max_tokens.get(),
            response_format: f.response_format.get(),
            stream: f.stream.get(),
            options: f.options.get(),
        }
    }

    fn task_body(&self) -> Result<Value, String> {
        task_body(
            &self.alias.get(),
            &self.task(),
            &self.form.task_values.get(),
            &self.form.task_flags.get(),
            &self.form.task_json.get(),
        )
    }

    /// Which gateway route this run will hit — shown next to the request
    /// preview so there is never a doubt about what is being tested.
    fn endpoint(&self) -> &'static str {
        if self.use_generic() {
            if self.form.task_stream.get() {
                "POST /v1/tasks/stream"
            } else {
                "POST /v1/tasks/run"
            }
        } else if self.task() == "tts" {
            "POST /v1/audio/speech"
        } else if self.form.asr_details.get() {
            "POST /v1/audio/transcriptions/details"
        } else {
            "POST /v1/audio/transcriptions"
        }
    }

    fn run_label(&self) -> &'static str {
        if self.use_generic() {
            "Run task"
        } else if self.task() == "tts" {
            "Synthesize"
        } else {
            "Transcribe"
        }
    }

    /// The exact outgoing document. Built by the same functions the dispatch
    /// uses, so the preview can never drift from what is sent.
    fn preview(&self) -> String {
        let pretty = |r: Result<Value, String>| match r {
            Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_default(),
            Err(e) => format!("⚠ {e}"),
        };
        if self.use_generic() {
            return pretty(self.task_body());
        }
        if self.task() == "tts" {
            return pretty(speech_body(&self.speech_form()));
        }
        let f = self.form;
        if f.asr_source.get() == "path" {
            return pretty(Ok(transcription_body(
                &self.alias.get(),
                &f.asr_path.get(),
                &f.language.get(),
            )));
        }
        let name = f.asr_file_name.get();
        multipart_preview(
            &self.alias.get(),
            (!name.is_empty()).then_some(name.as_str()),
            &f.language.get(),
            f.asr_stream.get(),
        )
    }

    fn build(&self) -> Result<Dispatch, String> {
        let alias = self.alias.get_untracked();
        if alias.is_empty() {
            return Err("pick a model first".into());
        }
        let f = self.form;
        if self.use_generic() {
            let body = self.task_body()?;
            let q = if f.task_stream.get_untracked() {
                "?stream=1"
            } else {
                ""
            };
            return Ok(Dispatch::Json {
                url: format!("/audio-lab/api/tasks/run{q}"),
                body,
            });
        }
        if self.task() == "tts" {
            let body = speech_body(&self.speech_form())?;
            if body["input"].as_str().unwrap_or("").trim().is_empty() {
                return Err("text is required".into());
            }
            return Ok(Dispatch::Json {
                url: "/audio-lab/api/speech".into(),
                body,
            });
        }
        // ASR
        if f.asr_source.get_untracked() == "path" {
            let body = transcription_body(
                &alias,
                &f.asr_path.get_untracked(),
                &f.language.get_untracked(),
            );
            if body["audio"].as_str().unwrap_or("").is_empty() {
                return Err("a server-side audio path is required".into());
            }
            return Ok(Dispatch::Json {
                url: transcription_url(f.asr_details.get_untracked()),
                body,
            });
        }
        let file = f
            .asr_file
            .get_value()
            .ok_or("pick an audio file to upload")?;
        let form = web_sys::FormData::new().map_err(|_| "FormData unavailable")?;
        let _ = form.append_with_str("model", &alias);
        let _ = form.append_with_blob_and_filename("file", &file, &file.name());
        let lang = f.language.get_untracked();
        if !lang.trim().is_empty() {
            let _ = form.append_with_str("language", lang.trim());
        }
        if f.asr_stream.get_untracked() {
            let _ = form.append_with_str("stream", "true");
        }
        Ok(Dispatch::Form {
            url: transcription_url(f.asr_details.get_untracked()),
            form,
        })
    }

    /// The action bar's line: what a run would call, or what is missing.
    /// The same refusals [`Self::build`] makes, read live so they show
    /// before the button is pressed — a request override that does not parse
    /// included, which used to surface only inside the folded preview.
    fn check(&self) -> Check {
        if self.models.with(Vec::is_empty) {
            return Check::Needs(match self.list.get() {
                ListStatus::Loading => "loading the served models…".into(),
                _ => "no audio model is served here".into(),
            });
        }
        if self.alias.with(String::is_empty) {
            return Check::Needs("pick a model".into());
        }
        let f = self.form;
        let parsed = if self.use_generic() {
            self.task_body().map(|_| ())
        } else if self.task() == "tts" {
            match speech_body(&self.speech_form()) {
                Ok(b) if b["input"].as_str().unwrap_or("").trim().is_empty() => {
                    return Check::Needs("type the text to synthesize".into())
                }
                other => other.map(|_| ()),
            }
        } else if f.asr_source.get() == "path" {
            if f.asr_path.with(|p| p.trim().is_empty()) {
                return Check::Needs("a server-side audio path is required".into());
            }
            Ok(())
        } else {
            if f.asr_file_name.with(String::is_empty) {
                return Check::Needs("pick an audio file to upload".into());
            }
            Ok(())
        };
        match parsed {
            Err(e) => Check::Invalid(e),
            Ok(()) if self.model_ready() => Check::Ready(self.endpoint().to_string()),
            // Not an error: a stopped model starts on the first request
            // (per-model containers §3.2) — it just pays a cold load.
            Ok(()) => Check::Ready(format!(
                "{} · the first run starts the model",
                self.endpoint()
            )),
        }
    }

    fn revoke(urls: &[String]) {
        for url in urls {
            let _ = web_sys::Url::revoke_object_url(url);
        }
    }

    /// Everything this page handed to a player, in flight and in the history.
    fn revoke_tracks(&self) {
        Self::revoke(&self.urls.get_value());
        self.urls.set_value(Vec::new());
        if let Some(runs) = self.runs.try_get_untracked() {
            for r in &runs {
                Self::revoke(&r.urls);
            }
        }
    }

    /// The run in flight is over: into the history it goes, newest first.
    fn settle(&self) {
        let Some(head) = self.current.get_untracked() else {
            return;
        };
        let mut error = self.error.get_untracked();
        let result = self.result.get_untracked().unwrap_or_default();
        if error.is_empty() && self.stopped.get_value() {
            error = "stopped before it finished".into();
        }
        let run = AudioRun {
            head,
            result,
            stats: self.stats.get_untracked(),
            error,
            urls: self.urls.get_value(),
        };
        self.urls.set_value(Vec::new());
        self.runs.update(|v| v.insert(0, run));
        self.current.set(None);
        self.result.set(None);
        self.stats.set(None);
        self.error.set(String::new());
    }

    /// The model the run in flight was sent to — what its files are named
    /// after — not whatever the picker says by the time a cold load answers
    /// (code:A5).
    fn sent_model(&self) -> String {
        self.current
            .with_untracked(|h| h.as_ref().map(|h| h.model.clone()))
            .unwrap_or_else(|| self.alias.get_untracked())
    }

    fn push_track(
        &self,
        out: &mut RunResult,
        id: &str,
        b64: Option<&str>,
        rate: Option<f64>,
        mime: &str,
    ) -> usize {
        // Short strings are never audio; the old island used the same guard so
        // a stray `data` field cannot produce a broken player.
        let Some(b64) = b64.filter(|s| s.len() >= 64) else {
            return 0;
        };
        let Some(bytes) = super::audio_stream::b64_decode(b64) else {
            return 0;
        };
        let n = bytes.len();
        if let Some(url) = blob_url(&bytes, mime) {
            self.urls.update_value(|v| v.push(url.clone()));
            let stem = track_stem(&self.sent_model());
            out.tracks.push(Track {
                id: id.to_string(),
                url,
                name: if id.is_empty() {
                    format!("{stem}.wav")
                } else {
                    format!("{stem}-{id}.wav")
                },
                size: n,
                sample_rate: rate,
            });
        }
        n
    }
}

fn now() -> f64 {
    js_sys::Date::now()
}

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

#[component]
pub fn AudioLab() -> impl IntoView {
    let toasts = use_toasts();
    let lab = Lab {
        models: RwSignal::new(Vec::new()),
        runtime: RwSignal::new(Vec::new()),
        alias: RwSignal::new(String::new()),
        task_sel: RwSignal::new("tts".to_string()),
        generic: RwSignal::new(false),
        voices: RwSignal::new(Vec::new()),
        voice_list: RwSignal::new(VoiceList::Idle),
        clips: RwSignal::new(Vec::new()),
        clips_err: RwSignal::new(String::new()),
        voices_dir: RwSignal::new(String::new()),
        voice_library_active: RwSignal::new(false),
        form: Form::new(),
        list: RwSignal::new(ListStatus::Loading),
        running: RwSignal::new(false),
        current: RwSignal::new(None),
        error: RwSignal::new(String::new()),
        result: RwSignal::new(None),
        stats: RwSignal::new(None),
        runs: RwSignal::new(Vec::new()),
        next_run: StoredValue::new(1),
        stopped: StoredValue::new(false),
        view: RwSignal::new(LabView::Form),
        uploading: RwSignal::new(false),
        preview_clip: RwSignal::new(None),
        aborter: StoredValue::new(None),
        urls: StoredValue::new(Vec::new()),
        scope: Scope::new(),
    };

    let load_models = move || {
        lab.scope.spawn(async move {
            match lab_get::<ModelsResp>("/audio-lab/api/models".into()).await {
                Ok(m) => {
                    lab.runtime.set(m.runtime);
                    lab.voices_dir.set(m.voices_dir.unwrap_or_default());
                    lab.voice_library_active.set(m.voice_library_active);
                    let first = m.models.first().map(|f| f.alias.clone());
                    lab.models.set(m.models);
                    lab.list.set(ListStatus::Ready);
                    if lab.alias.get_untracked().is_empty() {
                        if let Some(alias) = first {
                            lab.alias.set(alias);
                        }
                    }
                }
                Err(e) => {
                    lab.models.set(Vec::new());
                    lab.list.set(ListStatus::Failed(e));
                }
            }
        });
    };
    let load_clips = move || {
        lab.scope.spawn(async move {
            match lab_get::<RefsResp>("/audio-lab/api/refs".into()).await {
                Ok(r) => {
                    lab.clips.set(r.clips);
                    lab.clips_err.set(r.error.unwrap_or_default());
                }
                Err(e) => {
                    lab.clips.set(Vec::new());
                    lab.clips_err.set(e);
                }
            }
        });
    };
    load_models();
    load_clips();

    // The runtime rows follow the live bus once it has spoken, so a model
    // that comes up (or goes away) while the page is open is seen here.
    let live = crate::live::use_live();
    Effect::new(move |_| {
        if let Some(rows) = live.runtime.get() {
            lab.runtime.set(
                rows.into_iter()
                    .filter(|r| r.class == "audio")
                    .map(|r| RuntimeRow {
                        model_id: r.model_id,
                        state: r.state,
                    })
                    .collect(),
            );
        }
    });

    // Model change → adopt its task. The voice list is read only from a
    // running container: when the model is not up it waits (the panel offers
    // to load it), and it is read the moment the model turns ready. One-way
    // (the picker never writes the alias back), so no notify-on-equal
    // ping-pong. The history stays: comparing models is what it is for.
    let model_ready = Memo::new(move |_| lab.model_ready());
    Effect::new(move |prev: Option<(String, bool)>| {
        let alias = lab.alias.get();
        let ready = model_ready.get();
        let switched = prev.as_ref().is_none_or(|(a, _)| *a != alias);
        if switched {
            lab.voices.set(Vec::new());
            if let Some(m) = lab
                .models
                .get_untracked()
                .into_iter()
                .find(|m| m.alias == alias)
            {
                lab.task_sel.set(m.task);
            }
        }
        if alias.is_empty() {
            lab.voice_list.set(VoiceList::Idle);
        } else if ready {
            let have = lab.voice_list.get_untracked();
            if switched || !matches!(have, VoiceList::Loaded | VoiceList::Loading) {
                lab.load_voices(false);
            }
        } else if switched {
            lab.voice_list.set(VoiceList::NotRunning);
        }
        (alias, ready)
    });

    let stop = move || {
        if let Some(a) = lab.aborter.get_value() {
            lab.stopped.set_value(true);
            a.abort();
        }
    };

    let run = move || {
        if lab.running.get_untracked() {
            return;
        }
        lab.result.set(None);
        lab.stats.set(None);
        lab.error.set(String::new());
        lab.stopped.set_value(false);
        let id = lab.next_run.get_value();
        lab.next_run.set_value(id + 1);
        let (task, endpoint) = untrack(|| {
            let mut task = task_label(&lab.task());
            if is_openai_shaped(&lab.task()) && lab.use_generic() {
                task.push_str(" · via tasks");
            }
            (task, lab.endpoint())
        });
        lab.current.set(Some(RunHead {
            id,
            model: lab.alias.get_untracked(),
            task,
            endpoint,
            at: now_hms(),
        }));
        lab.view.set(LabView::Results);
        let dispatch = match lab.build() {
            Ok(d) => d,
            Err(e) => {
                lab.error.set(e);
                lab.settle();
                return;
            }
        };
        lab.running.set(true);
        let ctrl = web_sys::AbortController::new().ok();
        let signal = ctrl.as_ref().map(|c| c.signal());
        lab.aborter.set_value(ctrl);
        let t0 = now();
        // A run the lab was left during ends with it: its result has no
        // history to land in (and would leave object URLs behind).
        lab.scope.spawn(async move {
            let sent = match dispatch {
                Dispatch::Json { url, body } => {
                    match Request::post(&url)
                        .abort_signal(signal.as_ref())
                        .json(&body)
                    {
                        Ok(req) => req.send().await.map_err(|e| e.to_string()),
                        Err(e) => Err(e.to_string()),
                    }
                }
                Dispatch::Form { url, form } => {
                    match Request::post(&url).abort_signal(signal.as_ref()).body(form) {
                        Ok(req) => req.send().await.map_err(|e| e.to_string()),
                        Err(e) => Err(e.to_string()),
                    }
                }
            };
            match sent {
                Err(e) => {
                    if !e.to_lowercase().contains("abort") {
                        lab.error.set(e);
                    }
                }
                Ok(resp) => {
                    let status = resp.status();
                    let ct = resp
                        .headers()
                        .get("content-type")
                        .unwrap_or_default()
                        .to_string();
                    let shaped = resp.headers().get("x-lmgw-speech");
                    lab.stats.set(Some(RunStats {
                        status,
                        ttfb_ms: Some(now() - t0),
                        mime: ct.clone(),
                        shaped,
                        ..Default::default()
                    }));
                    if ct.starts_with("text/event-stream") {
                        consume_stream(lab, &resp, t0).await;
                    } else if !(200..300).contains(&status) {
                        let text = resp.text().await.unwrap_or_default();
                        lab.error.set(error_message(&text, status));
                    } else if ct.starts_with("application/json") {
                        match resp.text().await {
                            Ok(text) => match serde_json::from_str::<Value>(&text) {
                                Ok(j) => consume_json(lab, j),
                                Err(e) => lab.error.set(format!("bad response body: {e}")),
                            },
                            Err(e) => lab.error.set(e.to_string()),
                        }
                    } else {
                        match resp.binary().await {
                            Ok(bytes) => consume_binary(lab, &bytes, &ct),
                            Err(e) => lab.error.set(e.to_string()),
                        }
                    }
                }
            }
            lab.stats.update(|s| {
                if let Some(s) = s {
                    if s.total_ms.is_none() {
                        s.total_ms = Some(now() - t0);
                    }
                }
            });
            lab.settle();
            lab.running.set(false);
            lab.aborter.set_value(None);
        });
    };

    on_cleanup(move || {
        lab.revoke_tracks();
        lab.aborter
            .try_with_value(|a| a.as_ref().map(web_sys::AbortController::abort));
    });

    let entries = Signal::derive(move || {
        lab.models
            .get()
            .iter()
            .map(|m| CatalogEntry {
                id: m.alias.clone(),
                source: "audiocpp".into(),
                group_label: "audio.cpp".into(),
                vendor: Some(m.family.clone()),
                task: Some(m.task.clone()),
                ctx: None,
                price_in: None,
                price_out: None,
                price_varies: false,
                vision: Some(false),
                tools: false,
                reasoning: false,
                reasoning_facts: None,
                local: true,
            })
            .collect::<Vec<_>>()
    });
    let task_groups = Signal::derive(move || {
        let native = lab.model().map(|m| m.task).unwrap_or_default();
        GROUPS
            .iter()
            .map(|g| TaskGroup {
                label: g,
                opts: TASKS
                    .iter()
                    .filter(|t| t.group == *g)
                    .map(|t| TaskOpt {
                        id: t.id.to_string(),
                        label: t.label.to_string(),
                        tag: t.id.to_string(),
                        native: t.id == native,
                        off: None,
                    })
                    .collect(),
            })
            .collect::<Vec<_>>()
    });
    let preview = Memo::new(move |_| lab.preview());
    let check = Memo::new(move |_| lab.check());

    view! {
        <LabFrame
            persist="audio-lab.library"
            side_label="Voice library"
            side_badge=Signal::derive(move || {
                let n = lab.clips.with(Vec::len);
                if n == 0 { String::new() } else { n.to_string() }
            })
            side=move || view! { <VoiceLibrary lab=lab on_reload=load_clips toasts=toasts/> }
            head=move || {
                view! {
                    <span class="lab-label">"Model"</span>
                    <ModelPicker value=lab.alias entries=entries status=lab.list/>
                    // Which family (the `options` keys that apply) and whether
                    // it streams: what the old head said and the picker does
                    // not (par:PAR-8).
                    {move || {
                        lab.model()
                            .map(|m| {
                                view! {
                                    <span
                                        class="type-badge"
                                        title="the model family — it decides which options keys apply"
                                    >
                                        {m.family.clone()}
                                    </span>
                                    {(m.mode == "streaming")
                                        .then(|| {
                                            view! {
                                                <span
                                                    class="chip ok"
                                                    title="a streaming model: its reply arrives while it is produced"
                                                >
                                                    <i class="dot"></i>
                                                    "streaming"
                                                </span>
                                            }
                                        })}
                                }
                            })
                    }}
                    <span class="lab-label">"Task"</span>
                    <TaskMenu
                        value=lab.task_sel
                        groups=task_groups
                        on_pick=Callback::new(move |t: String| lab.task_sel.set(t))
                    />
                    // The same model is reachable both ways; comparing them
                    // is half the point of a bench.
                    <Show when=move || is_openai_shaped(&lab.task()) && lab.model().is_some()>
                        <label
                            class="check"
                            title="Run this model through the generic task route instead of its OpenAI-shaped one"
                        >
                            <input
                                type="checkbox"
                                prop:checked=move || lab.generic.get()
                                on:change=move |ev| lab.generic.set(event_target_checked(&ev))
                            />
                            <span class="mono-sm">"via /v1/tasks/run"</span>
                        </label>
                    </Show>
                    <span class="spacer"></span>
                    {move || {
                        let (cls, label, title) = if model_ready.get() {
                            ("chip ok", "running", "This model's container is up")
                        } else {
                            // Not an error: a stopped model starts on the
                            // first request (§3.2) — it just pays a cold load.
                            (
                                "chip off",
                                "not loaded",
                                "The first run starts this model's own container, so it pays a cold load before the first byte",
                            )
                        };
                        view! {
                            <span class=cls title=title>
                                <i class="dot"></i>
                                {label}
                            </span>
                        }
                    }}
                    <button
                        class="btn ghost"
                        title="Reload the served model list and the voice library"
                        on:click=move |_| {
                            load_models();
                            load_clips();
                        }
                    >
                        "Reload"
                    </button>
                }
            }
            form=move || {
                view! {
                    <Notices lab=lab on_retry=load_models/>
                    {move || {
                        if lab.use_generic() {
                            view! { <GenericPanel lab=lab/> }.into_any()
                        } else if lab.task() == "tts" {
                            view! { <TtsPanel lab=lab/> }.into_any()
                        } else {
                            view! { <AsrPanel lab=lab/> }.into_any()
                        }
                    }}
                    <details class="card req-card">
                        <summary>
                            "Request — "
                            <span class="mono-sm">{move || lab.endpoint()}</span>
                        </summary>
                        <pre class="preset">{move || preview.get()}</pre>
                    </details>
                }
            }
            results=move || {
                view! {
                    {move || {
                        lab.current
                            .get()
                            .map(|head| {
                                view! {
                                    <RunCard
                                        head=head
                                        lab=lab
                                        live=true
                                        on_stop=Callback::new(move |()| stop())
                                    />
                                }
                            })
                    }}
                    <For each=move || lab.runs.get() key=|r| r.head.id let:r>
                        <RunCard head=r.head.clone() lab=lab done=r/>
                    </For>
                }
            }
            results_n=Signal::derive(move || lab.runs.with(Vec::len))
            check=check
            run_label=Signal::derive(move || lab.run_label().to_string())
            running=lab.running
            on_run=Callback::new(move |()| run())
            on_stop=Callback::new(move |()| stop())
            on_clear=Callback::new(move |()| {
                for r in lab.runs.get_untracked() {
                    Lab::revoke(&r.urls);
                }
                lab.runs.set(Vec::new());
            })
            view=lab.view
        />
    }
}

fn now_hms() -> String {
    let d = js_sys::Date::new_0();
    format!(
        "{:02}:{:02}:{:02}",
        d.get_hours(),
        d.get_minutes(),
        d.get_seconds()
    )
}

// ---------------------------------------------------------------------------
// Response consumption
// ---------------------------------------------------------------------------

/// A complete audio file (or a JSON envelope carrying one) — nothing to
/// assemble, the server already produced a playable container.
fn consume_json(lab: Lab, j: Value) {
    // `/v1/tasks/stream` wraps the same document as `{events, result}`.
    let r = if j["result"].is_object() {
        j["result"].clone()
    } else {
        j.clone()
    };
    let timing = if r["timing"].is_object() {
        r["timing"].clone()
    } else {
        j["timing"].clone()
    };
    let mut out = RunResult {
        json: Some(j.clone()),
        events: j["events"].as_array().cloned(),
        text: r["text"].as_str().unwrap_or("").to_string(),
        language: r["language"].as_str().unwrap_or("").to_string(),
        segments: r["segments"].as_array().cloned().unwrap_or_default(),
        speaker_turns: r["speaker_turns"].as_array().cloned().unwrap_or_default(),
        words: r["words"].as_array().cloned().unwrap_or_default(),
        ..Default::default()
    };
    // A single `audio` (+ `sample_rate`) for one-output tasks,
    // `named_audio_outputs` for separation's stem list; `data` / `b64_json`
    // cover `/v1/audio/speech`'s json envelope.
    let single = r["audio"]
        .as_str()
        .or_else(|| j["data"].as_str())
        .or_else(|| j["b64_json"].as_str());
    let mut bytes = lab.push_track(&mut out, "", single, r["sample_rate"].as_f64(), "audio/wav");
    for n in r["named_audio_outputs"].as_array().into_iter().flatten() {
        bytes += lab.push_track(
            &mut out,
            n["id"].as_str().unwrap_or(""),
            n["audio"].as_str(),
            n["sample_rate"].as_f64(),
            "audio/wav",
        );
    }
    lab.stats.update(|s| {
        if let Some(s) = s {
            if bytes > 0 {
                s.bytes = Some(bytes);
            }
            s.wall_ms = timing["wall_ms"].as_f64();
            s.audio_ms = timing["audio_duration_ms"].as_f64();
            s.rtf = timing["rtf"].as_f64().filter(|v| v.is_finite());
        }
    });
    lab.result.set(Some(out));
}

fn consume_binary(lab: Lab, bytes: &[u8], mime: &str) {
    let ext = if mime.contains("wav") {
        "wav"
    } else if mime.contains("mpeg") {
        "mp3"
    } else {
        "bin"
    };
    let mut out = RunResult::default();
    if let Some(url) = blob_url(bytes, mime) {
        lab.urls.update_value(|v| v.push(url.clone()));
        out.tracks.push(Track {
            id: String::new(),
            url,
            name: format!("{}.{ext}", track_stem(&lab.sent_model())),
            size: bytes.len(),
            sample_rate: None,
        });
    }
    lab.stats.update(|s| {
        if let Some(s) = s {
            s.bytes = Some(bytes.len());
        }
    });
    lab.result.set(Some(out));
}

/// SSE covers both streaming shapes: `speech.audio.delta` (base64 PCM, which is
/// reassembled and wrapped in a WAV header from the declared format) and
/// `transcript.text.delta` (text, appended live so a long transcription shows
/// progress). A stream can carry both.
async fn consume_stream(lab: Lab, resp: &gloo_net::http::Response, t0: f64) {
    let acc = Rc::new(RefCell::new(StreamAcc::default()));
    let first = Rc::new(Cell::new(false));
    lab.result.set(Some(RunResult::default()));
    {
        let acc = acc.clone();
        let _ = read_sse(resp, move |ev| {
            if !first.get() {
                first.set(true);
                lab.stats.update(|s| {
                    if let Some(s) = s {
                        s.ttfb_ms = Some(now() - t0);
                    }
                });
            }
            let live_text = matches!(ev, AudioEvent::TextDelta(_) | AudioEvent::TextDone(_));
            let is_err = matches!(ev, AudioEvent::Error(_));
            let mut a = acc.borrow_mut();
            a.apply(ev);
            if is_err {
                lab.error.set(a.error.clone().unwrap_or_default());
            }
            if live_text {
                lab.result.set(Some(RunResult {
                    text: a.text.clone(),
                    ..Default::default()
                }));
            }
        })
        .await;
    }
    let acc = acc.borrow();
    let mut out = RunResult {
        text: acc.text.clone(),
        ..Default::default()
    };
    if !acc.pcm.is_empty() {
        let f = lab.form;
        let rate = f.pcm_rate.get_untracked().trim().parse().unwrap_or(24000);
        let channels = f.pcm_channels.get_untracked().trim().parse().unwrap_or(1);
        let bits = f.pcm_bits.get_untracked().trim().parse().unwrap_or(16);
        let wav = pcm_to_wav(&acc.pcm, rate, channels, bits);
        if let Some(url) = blob_url(&wav, "audio/wav") {
            lab.urls.update_value(|v| v.push(url.clone()));
            out.tracks.push(Track {
                id: String::new(),
                url,
                name: format!("{}.wav", track_stem(&lab.sent_model())),
                size: wav.len(),
                sample_rate: Some(rate as f64),
            });
        }
        lab.stats.update(|s| {
            if let Some(s) = s {
                s.bytes = Some(acc.pcm.len());
            }
        });
    }
    lab.result.set(Some(out));
}

// ---------------------------------------------------------------------------
// Notices
// ---------------------------------------------------------------------------

#[component]
fn Notices(lab: Lab, on_retry: impl Fn() + Copy + Send + Sync + 'static) -> impl IntoView {
    view! {
        {move || match lab.list.get() {
            ListStatus::Failed(e) => {
                view! {
                    <div class="notice err">
                        {format!("⚠ The audio model list did not load: {e}")}
                        " "
                        <button class="btn ghost sm" on:click=move |_| on_retry()>
                            "Retry"
                        </button>
                    </div>
                }
                    .into_any()
            }
            ListStatus::Ready if lab.models.with(Vec::is_empty) => {
                view! {
                    <div class="notice">
                        "No enabled audio models. Add one under "
                        <A href="/models">"Models · Local · audio"</A>
                        " — the picker fills itself from "
                        <span class="mono-sm">"/audio-lab/api/models"</span> "."
                    </div>
                }
                    .into_any()
            }
            _ => ().into_any(),
        }}
        {move || {
            let sel = lab.task_sel.get();
            lab.model()
                .filter(|m| m.task != sel)
                .map(|m| {
                    view! {
                        <div class="notice">
                            {format!(
                                "Form only: {} is a “{}” model. audio.cpp routes by the model, not by this picker — the request below is sent as-is.",
                                m.alias,
                                task_label(&m.task),
                            )}
                        </div>
                    }
                })
        }}
    }
}

// ---------------------------------------------------------------------------
// Panels
// ---------------------------------------------------------------------------

/// Why the voice picker is empty, and — for a model that is not running —
/// the one way to fill it, which says that it loads the model.
#[component]
fn VoiceListNote(lab: Lab) -> impl IntoView {
    move || match lab.voice_list.get() {
        VoiceList::NotRunning => view! {
            <div class="row mini-note dim">
                <span>"Voices load once the model is running."</span>
                <button
                    class="btn ghost sm"
                    title="Start this model's container and ask it for its voices"
                    on:click=move |_| lab.load_voices(true)
                >
                    "Load voices (starts the model)"
                </button>
            </div>
        }
        .into_any(),
        VoiceList::Loading => view! { <p class="dim mini-note">"Loading voices…"</p> }.into_any(),
        VoiceList::Failed(e) => view! {
            <div class="notice err" style="margin-top:8px">
                {format!("⚠ voice list: {e}")}
                <button class="btn ghost sm" on:click=move |_| lab.load_voices(false)>
                    "Retry"
                </button>
            </div>
        }
        .into_any(),
        VoiceList::Idle | VoiceList::Loaded => ().into_any(),
    }
}

#[component]
fn TtsPanel(lab: Lab) -> impl IntoView {
    let f = lab.form;
    let voice_opts = Signal::derive(move || {
        std::iter::once((String::new(), "(none)".to_string()))
            .chain(lab.voices.get().into_iter().map(|v| (v.clone(), v)))
            .collect::<Vec<_>>()
    });
    let clip_opts = Signal::derive(move || {
        std::iter::once((String::new(), "(none)".to_string()))
            .chain(lab.clips.get().into_iter().map(|c| (c.server_path, c.name)))
            .collect::<Vec<_>>()
    });
    let format_opts = Signal::derive(|| {
        vec![
            (String::new(), "wav (default)".to_string()),
            ("json".to_string(), "json (base64 wav)".to_string()),
            ("pcm".to_string(), "pcm".to_string()),
        ]
    });
    let bits_opts = Signal::derive(|| {
        vec![
            ("16".to_string(), "16 (s16le)".to_string()),
            ("32".to_string(), "32 (f32le)".to_string()),
        ]
    });
    view! {
        <div class="card">
            <h3 class="lab-h3">"Text"</h3>
            <textarea
                class="input ta"
                placeholder="Text to synthesize…"
                prop:value=move || f.input.get()
                on:input=move |ev| f.input.set(event_target_value(&ev))
            ></textarea>
        </div>

        <div class="card">
            <h3 class="lab-h3">"Voice"</h3>
            <div class="field-grid">
                <div class="field">
                    <label>"Built-in voice / preset"</label>
                    <Select value=f.voice options=voice_opts placeholder="(none)"/>
                </div>
                <div class="field">
                    <label>"Reference clip"</label>
                    <Select value=f.voice_ref options=clip_opts placeholder="(none)"/>
                </div>
            </div>
            <div class="field" style="margin-top:10px">
                <label>"Instructions — delivery/style guidance, where the model takes it"</label>
                <input
                    class="input"
                    placeholder="e.g. read this slowly and warmly"
                    prop:value=move || f.instructions.get()
                    on:input=move |ev| f.instructions.set(event_target_value(&ev))
                />
            </div>
            <div class="field" style="margin-top:10px">
                <label>"Reference text — the transcript of the reference clip"</label>
                <input
                    class="input"
                    placeholder="What the reference clip says"
                    prop:value=move || f.reference_text.get()
                    on:input=move |ev| f.reference_text.set(event_target_value(&ev))
                />
            </div>
            <VoiceListNote lab=lab/>
            <Show when=move || {
                lab.voice_list.get() == VoiceList::Loaded && lab.voices.with(Vec::is_empty)
            }>
                <p class="dim mini-note">
                    "This model reports no built-in voices, so cloning families need a reference clip. "
                    "Upload one on the right — it is written to the audio models dir, which the container sees read-only at "
                    <span class="mono-sm">"/models/voices"</span> "."
                </p>
            </Show>
        </div>

        <div class="card">
            <h3 class="lab-h3">"Synthesis"</h3>
            <div class="field-grid">
                <TextField label="Seed" sig=f.seed placeholder="model default"/>
                <TextField label="Max tokens" sig=f.max_tokens placeholder="model default"/>
                <TextField label="Speed" sig=f.speed placeholder="1.0 · model default"/>
                <TextField label="Language" sig=f.language placeholder="model default"/>
                <div class="field">
                    <label>"Response format"</label>
                    <Select value=f.response_format options=format_opts placeholder="wav (default)"/>
                </div>
                <Show when=move || lab.streaming_model()>
                    <CheckField label="Stream (SSE, pcm)" sig=f.stream/>
                </Show>
            </div>
            <Show when=move || f.stream.get()>
                <div class="field-grid" style="margin-top:10px">
                    <TextField label="PCM sample rate" sig=f.pcm_rate placeholder="24000"/>
                    <TextField label="PCM channels" sig=f.pcm_channels placeholder="1"/>
                    <div class="field">
                        <label>"PCM bit depth"</label>
                        <Select value=f.pcm_bits options=bits_opts placeholder="16 (s16le)"/>
                    </div>
                </div>
                <p class="dim mini-note">
                    "A PCM stream carries no format header, so these values are a declaration, not a measurement — "
                    "they only decide how the received bytes are wrapped for playback. Getting them wrong yields audio at the wrong pitch, not an error."
                </p>
            </Show>
        </div>

        <div class="card">
            <h3 class="lab-h3">"Advanced parameters"</h3>
            <div class="field">
                <label>"options (JSON) — passed through to the model family"</label>
                <textarea
                    class="input ta mono"
                    placeholder=r#"{"temperature": 0.8, "top_p": 0.9}"#
                    prop:value=move || f.options.get()
                    on:input=move |ev| f.options.set(event_target_value(&ev))
                ></textarea>
            </div>
        </div>
    }
}

#[component]
fn AsrPanel(lab: Lab) -> impl IntoView {
    let f = lab.form;
    let upload = move || f.asr_source.get() == "upload";
    view! {
        <div class="card">
            <h3 class="lab-h3">"Audio"</h3>
            <div class="row">
                <div class="seg">
                    <button
                        class="seg-btn"
                        class:active=move || upload()
                        on:click=move |_| f.asr_source.set("upload".into())
                    >
                        "Upload a file (multipart)"
                    </button>
                    <button
                        class="seg-btn"
                        class:active=move || !upload()
                        on:click=move |_| f.asr_source.set("path".into())
                    >
                        "Server-side path (JSON)"
                    </button>
                </div>
                <div class="field" style="min-width:160px">
                    <label>"Language"</label>
                    <input
                        class="input"
                        placeholder="auto"
                        prop:value=move || f.language.get()
                        on:input=move |ev| f.language.set(event_target_value(&ev))
                    />
                </div>
            </div>
            <Show
                when=upload
                fallback=move || {
                    view! {
                        <div class="field" style="margin-top:10px">
                            <label>"Path as the container sees it"</label>
                            <input
                                class="input mono"
                                placeholder="/models/voices/clip.wav"
                                prop:value=move || f.asr_path.get()
                                on:input=move |ev| f.asr_path.set(event_target_value(&ev))
                            />
                        </div>
                        <div class="row" style="margin-top:8px">
                            <For each=move || lab.clips.get() key=|c| c.name.clone() let:c>
                                {
                                    let path = c.server_path.clone();
                                    view! {
                                        <button
                                            class="btn ghost mono-sm"
                                            on:click=move |_| f.asr_path.set(path.clone())
                                        >
                                            {c.name.clone()}
                                        </button>
                                    }
                                }
                            </For>
                        </div>
                    }
                }
            >
                <div class="field" style="margin-top:10px">
                    <label>"File"</label>
                    <input
                        class="input file"
                        type="file"
                        accept="audio/*"
                        on:change=move |ev| {
                            let el: web_sys::HtmlInputElement = event_target(&ev);
                            let file = el.files().and_then(|l| l.get(0));
                            f.asr_file_name
                                .set(file.as_ref().map(|x| x.name()).unwrap_or_default());
                            f.asr_file.set_value(file);
                        }
                    />
                </div>
                <p class="dim mini-note">
                    "Uploaded bytes go through the gateway's multipart re-encode — the same path an OpenAI Whisper client takes."
                </p>
            </Show>
            // Streaming rides the multipart route only, so the toggle is not
            // offered for a server-side path (where it would silently do nothing).
            <Show when=move || lab.streaming_model() && upload() && !f.asr_details.get()>
                <div style="margin-top:10px">
                    <CheckField label="Stream the transcript (SSE, upload only)" sig=f.asr_stream/>
                </div>
            </Show>
            <Show when=move || !f.asr_stream.get()>
                <div style="margin-top:10px">
                    <CheckField
                        label="Detailed result (words, segments, speaker turns)"
                        sig=f.asr_details
                    />
                </div>
            </Show>
            <Explain summary="The details come from the model, not from the route." persist="audio-lab.explain.details">
                "A model that does not align words or separate speakers answers with text and "
                "timing either way. Streaming and details are exclusive — a transcript delta has "
                "nowhere to carry them."
            </Explain>
        </div>
        <Explain summary="Live microphone transcription is not offered here." persist="audio-lab.explain.live">
            "It is a different route ("
            <span class="mono-sm">"/v1/audio/transcriptions/live"</span>
            "): it needs a duplex chunked upload the gateway does not proxy and a browser cannot drive."
        </Explain>
    }
}

#[component]
fn GenericPanel(lab: Lab) -> impl IntoView {
    let f = lab.form;
    let wide = move || {
        task_fields(&lab.task())
            .iter()
            .filter(|n| {
                matches!(
                    field_spec(n).map(|s| s.widget),
                    Some(Widget::Textarea) | Some(Widget::Clip)
                )
            })
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
    };
    let narrow = move || {
        task_fields(&lab.task())
            .iter()
            .filter(|n| {
                matches!(
                    field_spec(n).map(|s| s.widget),
                    Some(Widget::Text) | Some(Widget::Int) | Some(Widget::Float)
                )
            })
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
    };
    let flags = move || {
        task_fields(&lab.task())
            .iter()
            .filter(|n| field_spec(n).map(|s| s.widget) == Some(Widget::Bool))
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
    };
    view! {
        <div class="card">
            <h3 class="lab-h3">{move || format!("{} — request", task_label(&lab.task()))}</h3>
            <div class="field-stack">
                <For each=wide key=|n| n.clone() let:name>
                    <FieldView name=name lab=lab/>
                </For>
            </div>
            <div class="field-grid" style="margin-top:10px">
                <For each=narrow key=|n| n.clone() let:name>
                    <FieldView name=name lab=lab/>
                </For>
            </div>
            <div class="row" style="margin-top:10px">
                <For each=flags key=|n| n.clone() let:name>
                    <FieldView name=name lab=lab/>
                </For>
            </div>
            <Show when=move || task_fields(&lab.task()).is_empty()>
                <p class="dim mini-note">
                    "No field list for this task — use the overrides box below; the request object is passed through untouched."
                </p>
            </Show>
        </div>

        <div class="card">
            <h3 class="lab-h3">"Request overrides"</h3>
            <div class="field">
                <label>
                    "JSON merged into " <span class="mono-sm">"request"</span>
                    " last — reaches fields with no widget"
                </label>
                <textarea
                    class="input ta mono"
                    placeholder=r#"{"options": {"num_inference_steps": 10}}"#
                    prop:value=move || f.task_json.get()
                    on:input=move |ev| f.task_json.set(event_target_value(&ev))
                ></textarea>
            </div>
            <Show when=move || lab.streaming_model()>
                <div style="margin-top:10px">
                    <CheckField
                        label="Use /v1/tasks/stream (buffered event list)"
                        sig=f.task_stream
                    />
                </div>
            </Show>
        </div>
    }
}

/// One widget for a `/v1/tasks/run` request field. `clip` fields get the
/// voice-library picker *and* a free-text path, because audio.cpp resolves them
/// on its own filesystem and a clip may live outside the library.
#[component]
fn FieldView(name: String, lab: Lab) -> impl IntoView {
    let Some(spec) = field_spec(&name) else {
        return ().into_any();
    };
    let f = lab.form;
    let key = name.clone();
    let label = view! {
        <label>
            {spec.label} " " <Fname name=spec.name/>
        </label>
    };
    if spec.widget == Widget::Bool {
        let k = key.clone();
        let checked = move || f.task_flags.get().get(&k).copied().unwrap_or(false);
        let k = key.clone();
        return view! {
            <label class="check">
                <input
                    type="checkbox"
                    prop:checked=checked
                    on:change=move |ev| {
                        let on = event_target_checked(&ev);
                        f.task_flags.update(|m| {
                            m.insert(k.clone(), on);
                        });
                    }
                />
                <span>
                    {spec.label} " " <Fname name=spec.name/>
                </span>
            </label>
        }
        .into_any();
    }

    // A local mirror per field keeps typing cheap and lets the clip picker and
    // its free-text twin drive one value; the write-through is one-way and
    // guarded, so it cannot ping-pong with the map.
    let local = RwSignal::new(
        f.task_values
            .get_untracked()
            .get(&key)
            .cloned()
            .unwrap_or_default(),
    );
    let k = key.clone();
    Effect::new(move |_| {
        let v = local.get();
        let same = f
            .task_values
            .with_untracked(|m| m.get(&k).map(String::as_str) == Some(v.as_str()));
        if !same {
            f.task_values.update(|m| {
                m.insert(k.clone(), v);
            });
        }
    });

    let placeholder = if spec.hint.is_empty() {
        "model default"
    } else {
        spec.hint
    };
    match spec.widget {
        Widget::Textarea => view! {
            <div class="field">
                {label}
                <textarea
                    class="input ta"
                    prop:value=move || local.get()
                    on:input=move |ev| local.set(event_target_value(&ev))
                ></textarea>
            </div>
        }
        .into_any(),
        Widget::Clip => {
            let clip_opts = Signal::derive(move || {
                std::iter::once((String::new(), "(none)".to_string()))
                    .chain(lab.clips.get().into_iter().map(|c| (c.server_path, c.name)))
                    .collect::<Vec<_>>()
            });
            view! {
                <div class="field">
                    {label}
                    <div class="clip-row">
                        <Select value=local options=clip_opts placeholder="(none)"/>
                        <input
                            class="input mono"
                            placeholder="/models/voices/clip.wav"
                            prop:value=move || local.get()
                            on:input=move |ev| local.set(event_target_value(&ev))
                        />
                    </div>
                </div>
            }
            .into_any()
        }
        _ => view! {
            <div class="field">
                {label}
                <input
                    class="input"
                    placeholder=placeholder
                    prop:value=move || local.get()
                    on:input=move |ev| local.set(event_target_value(&ev))
                />
            </div>
        }
        .into_any(),
    }
}

#[component]
fn TextField(
    label: &'static str,
    sig: RwSignal<String>,
    #[prop(default = "")] placeholder: &'static str,
) -> impl IntoView {
    view! {
        <div class="field">
            <label>{label}</label>
            <input
                class="input"
                placeholder=placeholder
                prop:value=move || sig.get()
                on:input=move |ev| sig.set(event_target_value(&ev))
            />
        </div>
    }
}

#[component]
fn CheckField(label: &'static str, sig: RwSignal<bool>) -> impl IntoView {
    view! {
        <label class="check">
            <input
                type="checkbox"
                prop:checked=move || sig.get()
                on:change=move |ev| sig.set(event_target_checked(&ev))
            />
            <span>{label}</span>
        </label>
    }
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

/// One run in the results column: which model, which task, which route and
/// when; then what came back, or why nothing did.
#[component]
fn RunCard(
    head: RunHead,
    lab: Lab,
    /// The run in flight: it reads the live signals and offers Stop.
    #[prop(optional)]
    live: bool,
    /// A finished run, from the history.
    #[prop(optional)]
    done: Option<AudioRun>,
    #[prop(optional)] on_stop: Option<Callback<()>>,
) -> impl IntoView {
    let rate_field = lab.form.timeline_rate;
    let failed = done.as_ref().is_some_and(|d| !d.error.is_empty());
    let body = move || match &done {
        Some(d) => result_body(
            d.result.clone(),
            d.stats.clone(),
            d.error.clone(),
            rate_field,
        ),
        None => result_body(
            lab.result.get().unwrap_or_default(),
            lab.stats.get(),
            lab.error.get(),
            rate_field,
        ),
    };
    view! {
        <div class="card result-card" class:failed=failed>
            <div class="run-head">
                <span class="run-model mono-sm" title=head.model.clone()>
                    {head.model.clone()}
                </span>
                <span class="type-badge">{head.task.clone()}</span>
                <span class="mono-sm dim">{head.endpoint}</span>
                <span class="spacer"></span>
                {live
                    .then(|| {
                        view! {
                            <span class="chip live">
                                <i class="dot"></i>
                                "running…"
                            </span>
                            {on_stop
                                .map(|stop| {
                                    view! {
                                        <button class="btn danger sm" on:click=move |_| stop.run(())>
                                            "Stop"
                                        </button>
                                    }
                                })}
                        }
                    })}
                <span class="mono-sm dim">{head.at.clone()}</span>
            </div>
            {body}
        </div>
    }
}

/// What one run brought back: players, text, timelines, raw events or JSON,
/// the error, and the numbers.
fn result_body(
    r: RunResult,
    s: Option<RunStats>,
    error: String,
    rate_field: RwSignal<String>,
) -> impl IntoView {
    let result_rate = r.result_rate();
    // Read where the cells render, so typing a rate re-reads the spans
    // without rebuilding the card (and the field being typed in).
    let rate = Signal::derive(move || timeline_rate(result_rate, &rate_field.get()));
    let has_timeline = r.has_timeline();
    let locked = result_rate.is_some();
    let raw_json = r
        .json
        .clone()
        .filter(|_| r.tracks.is_empty() && r.text.is_empty() && !has_timeline);
    view! {
        {(!error.is_empty()).then(|| view! { <div class="notice err">{format!("⚠ {error}")}</div> })}
        {r
            .tracks
            .iter()
            .map(|t| {
                view! {
                    {(!t.id.is_empty())
                        .then(|| {
                            view! { <div class="track-name">{t.id.clone()}</div> }
                        })}
                    <audio controls src=t.url.clone()></audio>
                    <div class="row">
                        <a class="btn ghost sm" href=t.url.clone() download=t.name.clone()>
                            {format!("Download {}", t.name)}
                        </a>
                        <span class="dim mono-sm">
                            {format!(
                                "{}{}",
                                human_bytes(t.size as u64),
                                t.sample_rate.map(|r| format!(" · {r:.0} Hz")).unwrap_or_default(),
                            )}
                        </span>
                    </div>
                }
            })
            .collect::<Vec<_>>()}
        {(!r.text.is_empty())
            .then(|| {
                view! {
                    <div class="transcript">{r.text.clone()}</div>
                    {(!r.language.is_empty())
                        .then(|| {
                            view! {
                                <div class="dim mini-note">{format!("language: {}", r.language)}</div>
                            }
                        })}
                }
            })}
        {has_timeline
            .then(|| {
                view! {
                    <div class="field rate-field">
                        <label>"timeline rate (Hz)"</label>
                        <input
                            class="input mono w-num"
                            disabled=locked
                            title=if locked {
                                "the result states its own sample rate"
                            } else {
                                "analysis spans are sample indices; audio.cpp resamples to 16 kHz"
                            }
                            prop:value=move || rate_field.get()
                            on:input=move |ev| rate_field.set(event_target_value(&ev))
                        />
                    </div>
                }
            })}
        {(!r.segments.is_empty())
            .then(|| {
                view! {
                    <Timeline
                        title="Speech segments"
                        rows=r.segments.clone()
                        cols=vec!["text"]
                        rate=rate
                    />
                }
            })}
        {(!r.speaker_turns.is_empty())
            .then(|| {
                view! {
                    <Timeline
                        title="Speaker turns"
                        rows=r.speaker_turns.clone()
                        cols=vec!["speaker_id", "text"]
                        rate=rate
                    />
                }
            })}
        {(!r.words.is_empty())
            .then(|| {
                view! {
                    <Timeline title="Word timings" rows=r.words.clone() cols=vec!["word"] rate=rate/>
                }
            })}
        {r
            .events
            .clone()
            .map(|events| {
                view! {
                    <details class="lab-details">
                        <summary>{format!("Stream events — {}", events.len())}</summary>
                        <pre class="preset">
                            {serde_json::to_string_pretty(&events).unwrap_or_default()}
                        </pre>
                    </details>
                }
            })}
        {raw_json
            .map(|j| {
                view! {
                    <details class="lab-details" open>
                        <summary>"Response JSON"</summary>
                        <pre class="preset">{serde_json::to_string_pretty(&j).unwrap_or_default()}</pre>
                    </details>
                }
            })}
        {s
            .map(|s| {
                let mut parts = vec![
                    format!("status {}", s.status),
                    format!("ttfb {}", fmt_ms(s.ttfb_ms)),
                    format!("total {}", fmt_ms(s.total_ms)),
                ];
                if let Some(b) = s.bytes {
                    parts.push(format!("size {}", human_bytes(b as u64)));
                }
                if s.wall_ms.is_some() {
                    parts.push(format!("infer {}", fmt_ms(s.wall_ms)));
                }
                if s.audio_ms.is_some() {
                    parts.push(format!("audio {}", fmt_ms(s.audio_ms)));
                }
                if let Some(rtf) = s.rtf {
                    parts.push(format!("rtf {rtf:.3}"));
                }
                view! {
                    <div class="lab-stats mono-sm dim">
                        <span>{parts.join(" · ")}</span>
                        <span>{s.mime}</span>
                    </div>
                    {s
                        .shaped
                        .map(|h| {
                            view! {
                                <div
                                    class="dim mini-note mono-sm"
                                    title="x-lmgw-speech: what lmgw changed in the request for this model"
                                >
                                    {format!("shaped: {h}")}
                                </div>
                            }
                        })}
                }
            })}
    }
}

#[component]
fn Timeline(
    title: &'static str,
    rows: Vec<Value>,
    cols: Vec<&'static str>,
    rate: Signal<f64>,
) -> impl IntoView {
    let n = rows.len();
    view! {
        <details class="lab-details" open>
            <summary>{format!("{title} — {n}")}</summary>
            <div class="table-scroll">
                <table class="data">
                    <thead>
                        <tr>
                            <th>"start"</th>
                            <th>"end"</th>
                            {cols.iter().map(|c| view! { <th>{*c}</th> }).collect::<Vec<_>>()}
                            <th>"conf"</th>
                        </tr>
                    </thead>
                    <tbody>
                        {rows
                            .iter()
                            .map(|row| {
                                let (start, end) = (
                                    row["start_sample"].as_f64(),
                                    row["end_sample"].as_f64(),
                                );
                                view! {
                                    <tr>
                                        <td class="num">
                                            {move || samples_to_secs(start, rate.get())}
                                        </td>
                                        <td class="num">{move || samples_to_secs(end, rate.get())}</td>
                                        {cols
                                            .iter()
                                            .map(|c| {
                                                let v = &row[*c];
                                                let text = v
                                                    .as_str()
                                                    .map(str::to_string)
                                                    .unwrap_or_else(|| {
                                                        if v.is_null() {
                                                            String::new()
                                                        } else {
                                                            v.to_string()
                                                        }
                                                    });
                                                view! { <td class="wrap">{text}</td> }
                                            })
                                            .collect::<Vec<_>>()}
                                        <td class="num">
                                            {row["confidence"]
                                                .as_f64()
                                                .map(|c| format!("{c:.3}"))
                                                .unwrap_or_else(|| "–".to_string())}
                                        </td>
                                    </tr>
                                }
                            })
                            .collect::<Vec<_>>()}
                    </tbody>
                </table>
            </div>
        </details>
    }
}

// ---------------------------------------------------------------------------
// Voice library
// ---------------------------------------------------------------------------

#[component]
fn VoiceLibrary(
    lab: Lab,
    on_reload: impl Fn() + Copy + Send + Sync + 'static,
    toasts: crate::widgets::Toasts,
) -> impl IntoView {
    let upload = move |ev: web_sys::Event| {
        let el: web_sys::HtmlInputElement = event_target(&ev);
        let Some(files) = el.files() else { return };
        if files.length() == 0 {
            return;
        }
        let Ok(form) = web_sys::FormData::new() else {
            return;
        };
        for i in 0..files.length() {
            if let Some(file) = files.get(i) {
                let _ = form.append_with_blob_and_filename("file", &file, &file.name());
            }
        }
        el.set_value(""); // so re-picking the same file fires again
        lab.uploading.set(true);
        spawn_local(async move {
            let sent = match Request::post("/audio-lab/api/refs").body(form) {
                Ok(req) => req.send().await.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            match sent {
                Ok(resp) => match finish::<RefsResp>(resp).await {
                    Ok(r) => {
                        lab.clips.set(r.clips);
                        lab.clips_err.set(String::new());
                        let failed: Vec<String> = r
                            .transcribed
                            .iter()
                            .filter_map(|t| t["transcribe_error"].as_str().map(str::to_string))
                            .collect();
                        let by = r
                            .transcribed
                            .first()
                            .and_then(|t| t["by"].as_str())
                            .map(|by| format!(" by {by}"))
                            .unwrap_or_default();
                        match (r.transcribed.len(), failed.first()) {
                            (0, _) => toasts.ok("clip uploaded"),
                            (_, None) => toasts.ok(format!("clip uploaded and transcribed{by}")),
                            (_, Some(why)) => {
                                toasts.warn(format!("clip uploaded, not transcribed: {why}"))
                            }
                        }
                    }
                    Err(e) => lab.clips_err.set(e),
                },
                Err(e) => lab.clips_err.set(e),
            }
            lab.uploading.set(false);
        });
    };

    // The transcript index audio.cpp reads (`<voice>|<text>` per line). Saved
    // on blur rather than behind a button: one input per clip, and a row of
    // Save buttons would be the only thing in this panel that needs one.
    let save_text = move |voice: String, text: String| {
        spawn_local(async move {
            let url = format!("/audio-lab/api/refs/{}/text", enc(&voice));
            let body = json!({ "transcript": text });
            match lab_post_json::<RefsResp>(url, body).await {
                Ok(r) => {
                    lab.clips.set(r.clips);
                    lab.clips_err.set(String::new());
                }
                Err(e) => lab.clips_err.set(e),
            }
        });
    };

    // A speech-to-text model writes the transcript (audio-class gap
    // 5): the setting's, `Settings → Runtimes → Audio → Clip transcripts`.
    // One at a time — the clip being transcribed, or `*` for all missing.
    let transcribing = RwSignal::new(None::<String>);
    let transcribe = move |name: String| {
        transcribing.set(Some(name.clone()));
        spawn_local(async move {
            let url = format!("/audio-lab/api/refs/{}/transcribe", enc(&name));
            match lab_post_json::<RefsResp>(url, json!({})).await {
                Ok(r) => {
                    lab.clips.set(r.clips);
                    lab.clips_err.set(String::new());
                    // Who wrote it, a fallback named as one (review V1).
                    toasts.ok(r.message.unwrap_or_else(|| format!("{name} transcribed")));
                }
                Err(e) => lab.clips_err.set(e),
            }
            transcribing.set(None);
        });
    };
    let transcribe_missing = move |_| {
        transcribing.set(Some("*".into()));
        spawn_local(async move {
            match crate::api::post::<Value, _>("/api/op/voice_transcribe", &json!({})).await {
                Ok(v) => {
                    let msg = v["message"].as_str().unwrap_or("transcribed").to_string();
                    match v["failed"].as_array().is_some_and(|f| !f.is_empty()) {
                        true => toasts.warn(msg),
                        false => toasts.ok(msg),
                    }
                }
                Err(e) => lab.clips_err.set(e.to_string()),
            }
            transcribing.set(None);
            on_reload();
        });
    };

    let delete = move |name: String| {
        spawn_local(async move {
            let url = format!("/audio-lab/api/refs/{}/delete", enc(&name));
            match lab_post_empty::<RefsResp>(url).await {
                Ok(r) => {
                    toasts.ok(format!("deleted {name}"));
                    if !lab.scope.alive() {
                        return;
                    }
                    lab.clips.set(r.clips);
                    lab.clips_err.set(String::new());
                    if lab.preview_clip.get_untracked().as_deref() == Some(name.as_str()) {
                        lab.preview_clip.set(None);
                    }
                }
                Err(e) => lab.clips_err.set(e),
            }
        });
    };

    view! {
        <div class="lab-help">
            <p class="dim mini-note">
                "Reference clips live in the audio models dir and reach the container read-only as "
                <span class="mono-sm">"/models/voices/<name>"</span>
                " — which is what a " <span class="mono-sm">"voice_ref"</span>
                " needs, since audio.cpp takes a server path rather than an upload."
            </p>
            {move || {
                match lab.voice_library_active.get() {
                    true => {
                        view! {
                            <p class="dim mini-note">
                                "This directory is the class's voice library, so a clip's name is "
                                "also a voice: " <span class="mono-sm">
                                    r#""voice": "<name>""#
                                </span>
                                " clones it, and the line below it is sent as the reference text."
                            </p>
                        }
                            .into_any()
                    }
                    false => {
                        view! {
                            <p class="dim mini-note">
                                "The voice library under "
                                <a href=super::settings::href("audio.voice_dir")>
                                    "Settings → Runtimes → Audio"
                                </a>
                                " does not point here, so these clips are reachable as a " <span class="mono-sm">"voice_ref"</span>
                                " path only. Set it to " <span class="mono-sm">"/models/voices"</span>
                                " to clone them by name."
                            </p>
                        }
                            .into_any()
                    }
                }
            }}
            <div class="field">
                <label>"Upload clips"</label>
                <input
                    class="input file"
                    type="file"
                    accept="audio/*"
                    multiple
                    disabled=move || lab.uploading.get()
                    on:change=upload
                />
            </div>
            <Show when=move || lab.uploading.get()>
                <div class="chip live">
                    <i class="dot"></i>
                    "uploading…"
                </div>
            </Show>
            {move || {
                let e = lab.clips_err.get();
                (!e.is_empty()).then(|| view! { <div class="notice err">{format!("⚠ {e}")}</div> })
            }}
            <div class="clip-list">
                <Show
                    when=move || !lab.clips.get().is_empty()
                    fallback=|| view! { <div class="empty">"No clips yet."</div> }
                >
                    // Keyed on the transcript too, so a transcript written by
                    // the server shows in the row's box.
                    <For
                        each=move || lab.clips.get()
                        key=|c| (c.name.clone(), c.transcript.clone())
                        let:c
                    >
                        {
                            let name = c.name.clone();
                            let (n2, n3, n4) = (name.clone(), name.clone(), name.clone());
                            let text_sig = RwSignal::new(c.transcript.clone());
                            let voice = c.voice.clone();
                            view! {
                                <div class="clip-item" title=c.server_path.clone()>
                                    <span class="n">{c.name.clone()}</span>
                                    <span class="dim mono-sm">{human_bytes(c.size)}</span>
                                    <button
                                        class="btn ghost"
                                        title="Preview"
                                        on:click=move |_| lab.preview_clip.set(Some(n2.clone()))
                                    >
                                        "▸"
                                    </button>
                                    <button
                                        class="btn ghost sm"
                                        title="Write the transcript with the speech-to-text \
                                               model of Settings → Runtimes → Audio → Clip \
                                               transcripts (replaces this one)"
                                        disabled=move || transcribing.get().is_some()
                                        on:click=move |_| transcribe(n4.clone())
                                    >
                                        "ASR"
                                    </button>
                                    <ConfirmButton
                                        label="✕"
                                        confirm="Delete?"
                                        class="btn ghost sm del"
                                        title="Delete this clip"
                                        on_confirm=Callback::new(move |()| delete(n3.clone()))
                                    />
                                </div>
                                <div class="clip-text">
                                    <input
                                        class="input mono-sm"
                                        placeholder="what this clip says (prompt_text)"
                                        prop:value=move || text_sig.get()
                                        on:input=move |ev| text_sig.set(event_target_value(&ev))
                                        on:change=move |_| save_text(
                                            voice.clone(),
                                            text_sig.get_untracked(),
                                        )
                                    />
                                </div>
                            }
                        }
                    </For>
                </Show>
            </div>
            {move || {
                lab.preview_clip
                    .get()
                    .map(|name| {
                        view! {
                            <audio
                                controls
                                autoplay
                                src=format!("/audio-lab/api/refs/{}", enc(&name))
                            ></audio>
                        }
                    })
            }}
            <Show when=move || transcribing.get().is_some()>
                <div class="chip live">
                    <i class="dot"></i>
                    "transcribing…"
                </div>
            </Show>
            <div class="row" style="margin-top:auto">
                <button class="btn ghost" on:click=move |_| on_reload()>
                    "Refresh"
                </button>
                <button
                    class="btn ghost"
                    title="Transcribe every clip without a transcript, with the speech-to-text \
                           model of Settings → Runtimes → Audio → Clip transcripts"
                    disabled=move || transcribing.get().is_some()
                    on:click=transcribe_missing
                >
                    "Transcribe missing"
                </button>
            </div>
            {move || {
                let dir = lab.voices_dir.get();
                if dir.is_empty() {
                    view! {
                        <p class="dim mini-note">
                            "No audio models dir configured — set one under "
                            <a href=super::settings::href("audio.models_dir")>
                                "Settings → Runtimes → Audio"
                            </a>
                            " before uploading."
                        </p>
                    }
                        .into_any()
                } else {
                    view! { <p class="dim mono-sm path-note">{dir}</p> }.into_any()
                }
            }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_bodies_render_their_message() {
        assert_eq!(
            error_message(r#"{"error": {"message": "model not served"}}"#, 502),
            "model not served"
        );
        assert_eq!(
            error_message(r#"{"error": "audio models dir is not configured"}"#, 400),
            "audio models dir is not configured"
        );
        assert_eq!(error_message("plain text boom", 500), "plain text boom");
        assert_eq!(error_message("", 503), "HTTP 503");
    }

    #[test]
    fn download_names_collapse_unusable_characters() {
        assert_eq!(track_stem("audio/kokoro"), "audio-kokoro");
        assert_eq!(track_stem("my model v1.2"), "my-model-v1.2");
        assert_eq!(track_stem(""), "audio");
    }

    #[test]
    fn voice_entries_accept_ids_or_objects() {
        assert_eq!(
            voice_name(&Value::from("af_heart")).as_deref(),
            Some("af_heart")
        );
        assert_eq!(
            voice_name(&serde_json::json!({"id": "am_adam"})).as_deref(),
            Some("am_adam")
        );
        assert_eq!(voice_name(&serde_json::json!({"x": 1})), None);
    }
}
