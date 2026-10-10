//! The Audio lab's shapes (`/audio-lab/api/*`): the model list the lab builds
//! its forms from, the voice library of reference clips, and the transcripts a
//! speech-to-text model writes for them.
//!
//! Synthesis, transcription, alignment and generic tasks are not here: the lab
//! hands those requests to the `/v1/audio/*` and `/v1/tasks/*` handlers and
//! relays what they answer.
//!
//! The gateway builds and reads these types itself, the dashboard's Audio lab
//! page reads them, and the API document is generated from them.

use serde::{Deserialize, Deserializer, Serialize};

use crate::status::RuntimeStatus;

/// One enabled audio model, as the lab's pickers need it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioLabModel {
    /// The alias to send as `model`: the audio prefix and the model id.
    pub alias: String,
    pub model_id: String,
    /// The engine family the model runs in (audio.cpp's own name for it).
    pub family: String,
    /// What the model does: `tts`, `asr`, `align` or another audio.cpp task.
    /// Decides which panel the lab offers.
    pub task: String,
    /// `offline` or `streaming`; only a streaming model can stream speech.
    pub mode: String,
}

/// `GET /audio-lab/api/models`. `R` is the shape of a container row: the
/// gateway fills in its own runtime view, a reader (and the API document)
/// uses [`RuntimeStatus`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "AudioLabModels"))]
pub struct AudioLabModels<R = RuntimeStatus> {
    /// The enabled audio models.
    #[serde(default)]
    pub models: Vec<AudioLabModel>,
    /// One row per running or starting audio container, so a client can say
    /// "not running" before a request fails. Matched to a model by `model_id`.
    #[serde(default)]
    pub runtime: Vec<R>,
    /// The voice library's directory on the gateway's host; `null` while no
    /// audio models directory is set.
    #[serde(default)]
    pub voices_dir: Option<String>,
    /// The same directory as the model container sees it: what goes into a
    /// request's `voice_ref`.
    #[serde(default)]
    pub container_voices_dir: String,
    /// Whether the audio class's voice directory is this library. When it is,
    /// a clip's name (without extension) is also a `voice` every text-to-speech
    /// model answers to; when not, a clip is only a `voice_ref` path.
    #[serde(default)]
    pub voice_library_active: bool,
}

/// `GET /audio-lab/api/voices`'s query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabVoicesQuery {
    /// The model alias to list voices for.
    pub model: String,
    /// `1`, `true`, `yes` or `on` starts the model when it is not running and
    /// asks its own server. Without it only a container that is already up is
    /// read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
}

/// The answer to `GET /audio-lab/api/voices` for a local model that is not
/// running: nothing was started and nothing is known.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoicesNotRunning {
    /// Always empty.
    pub voices: Vec<String>,
    /// Always `false`.
    pub running: bool,
}

/// One reference clip of the voice library.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Clip {
    /// The file name, extension included.
    pub name: String,
    /// Bytes.
    pub size: u64,
    /// The clip's path inside the model container: what goes into `voice_ref`
    /// or `audio`.
    pub server_path: String,
    /// What a request's `voice` names once the audio class's voice directory is
    /// this library: the file name without its extension.
    pub voice: String,
    /// What the clip says (its line in the library's `prompt_text` index);
    /// empty while it has none. A cloning model that needs a reference text
    /// reads it from here when a request names the clip as `voice`.
    pub transcript: String,
}

/// `GET /audio-lab/api/refs`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClipList {
    /// The library's clips by file name; empty without an audio models
    /// directory.
    pub clips: Vec<Clip>,
    /// Present, with a 200, when no audio models directory is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Who wrote a transcript, as every route that records one says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TranscriptProvenance {
    /// `asr:<model>`: the model the transcript came from. When a fallback
    /// answered, that model's name.
    pub transcript_source: String,
    /// The fallback alias that answered in place of the one asked, else `null`.
    pub answered_by: Option<String>,
    /// Why the fallback answered (`hold`, `external_vram`, ...), else `null`.
    pub fallback_reason: Option<String>,
    /// Who wrote it, in a sentence.
    pub by: String,
}

/// A clip an upload transcribed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClipWritten {
    /// The clip's file name.
    pub clip: String,
    #[serde(flatten)]
    pub provenance: TranscriptProvenance,
}

/// A clip an upload could not transcribe; the upload itself stands.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClipFailed {
    /// The clip's file name.
    pub clip: String,
    pub transcribe_error: String,
}

/// One clip of an upload, transcribed or not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ClipTranscription {
    Written(ClipWritten),
    Failed(ClipFailed),
}

/// `POST /audio-lab/api/refs`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClipsUploaded {
    /// The file names stored, in upload order.
    pub saved: Vec<String>,
    /// One entry per stored clip when no transcript was typed for it and the
    /// audio settings name a transcription model; empty otherwise.
    pub transcribed: Vec<ClipTranscription>,
    /// The library after the upload.
    pub clips: Vec<Clip>,
}

/// The answer to a change of the library: a delete or a transcript set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClipsAck {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The library after the change.
    pub clips: Vec<Clip>,
}

/// `POST /audio-lab/api/refs/{name}/text`'s body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SetRefText {
    /// The clip's transcript. A newline is stored as a space; empty drops the
    /// clip's transcript.
    pub transcript: String,
}

/// `POST /audio-lab/api/refs/{name}/transcribe`'s body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TranscribeRef {
    /// The speech-to-text alias to use. Blank or absent: the audio settings'
    /// transcription model.
    pub alias: Option<String>,
}

/// The answer to `POST /audio-lab/api/refs/{name}/transcribe`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ClipTranscribed {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The clip as asked (a file name or a voice name).
    pub clip: String,
    /// The transcript recorded: one line, replacing the clip's earlier one.
    pub transcript: String,
    /// The library after the change.
    pub clips: Vec<Clip>,
    /// A sentence saying who transcribed it, a fallback that answered named
    /// as one.
    pub message: String,
    #[serde(flatten)]
    pub provenance: TranscriptProvenance,
}

/// How this plane answers a failure: `{"error": "..."}`, plus the `code` of
/// a refusal that has one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabError {
    pub error: String,
    /// A machine-readable word, on the refusals that have one (for example
    /// `dev_shared_models_dir`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// `POST /audio-lab/api/transcriptions`'s query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DetailsQuery {
    /// `1`, `true`, `yes` or `on` sends the request to
    /// `/v1/audio/transcriptions/details` instead, which also answers with the
    /// word timings, segments and speaker turns the model produced.
    #[serde(default, deserialize_with = "de_flag")]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
    pub details: bool,
}

/// `POST /audio-lab/api/tasks/run`'s query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskRunQuery {
    /// `1`, `true`, `yes` or `on` selects `/v1/tasks/stream`, which a
    /// streaming-mode model answers with its buffered event list.
    #[serde(default, deserialize_with = "de_flag")]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
    pub stream: bool,
}

/// `?flag=1` / `?flag=true` / absent: a bare query flag, which serde's bool
/// parser alone would reject for `1`.
fn de_flag<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let s = String::deserialize(d)?;
    Ok(matches!(s.as_str(), "1" | "true" | "yes" | "on"))
}

/// `voice_transcribe`: the clips a speech-to-text model wrote transcripts
/// for. The answer names clips and lengths, never the text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceTranscribed {
    /// Always `true`; a clip that failed is listed in `failed`.
    pub ok: bool,
    /// The clips transcribed, with who wrote each.
    pub transcribed: Vec<TranscribedClip>,
    /// The clips that could not be.
    pub failed: Vec<VoiceClipFailed>,
    /// For the run over every untranscribed clip: how many already had a
    /// transcript and were left alone. Absent when one clip was named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "schema",
        schemars(transform = crate::openapi_ext::non_null)
    )]
    pub already: Option<usize>,
    /// The outcome in a sentence.
    pub message: String,
}

/// One clip `voice_transcribe` wrote a transcript for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TranscribedClip {
    /// The clip's file name.
    pub clip: String,
    /// The transcript's length in characters.
    pub chars: usize,
    #[serde(flatten)]
    pub provenance: TranscriptProvenance,
}

/// One clip `voice_transcribe` could not transcribe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceClipFailed {
    pub clip: String,
    /// Why.
    pub error: String,
}
