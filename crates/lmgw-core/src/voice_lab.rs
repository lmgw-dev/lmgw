//! The Voices lab: fixed voices
//! for the cloning text-to-speech models.
//!
//! Voice-design models (Qwen3-TTS VoiceDesign) and OmniVoice without a
//! reference draw a new speaker per request, so they cannot speak a realtime
//! answer clause by clause in one voice. What the cloning models (OmniVoice,
//! CosyVoice3, Fish, Qwen3-TTS Base) need instead is a good reference clip
//! with its exact transcript. Here any text-to-speech alias renders a
//! **round** of candidates of one fixed sentence ([`generate`]), each with a
//! seed of its own where the engine takes one; the owner listens and saves
//! the chosen candidate to the voice library ([`save`]): the clip, its
//! leading and trailing silence trimmed ([`wav`]), its transcript in the
//! library's `prompt_text`, and how it was made ([`provenance`]). Every
//! cloning model can then speak with it by name, realtime included; the
//! cloning check ([`clone_check`]) has one speak a test line with it.
//!
//! What the lab keeps lives under `<data_dir>/voice-lab/`: the rounds
//! (`rounds/<id>/`), the owner's sentence per language (`sentences.json`)
//! and the samples' provenance (`samples.json`). The voice library itself is
//! the Audio lab's, `<audio models dir>/voices/`, which the containers read;
//! nothing of lmgw's own goes into that directory but the clip and its
//! `prompt_text` line, the engine's own format.

use std::path::PathBuf;

use axum::http::StatusCode;

use crate::error::GatewayError;
use crate::state::SharedState;

/// The cloning check: a cloning model speaks a test line with a sample.
pub mod clone_check;
/// What a text-to-speech alias takes: the form's facts.
pub mod facts;
/// A round: open the alias once, render the candidates.
pub mod generate;
/// How a sample was made, kept beside the library.
pub mod provenance;
/// The rounds on disk.
pub mod rounds;
/// Saving a candidate to the voice library.
pub mod save;
/// The fixed sentence per language, and the owner's own.
pub mod sentences;
/// WAV reading: the duration, and trimming silence.
pub mod wav;

/// The lab's own directory: `<data_dir>/voice-lab`.
pub fn lab_dir(state: &SharedState) -> PathBuf {
    state.data_dir.join("voice-lab")
}

/// Why a lab call failed: the HTTP status, a stable code and the sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabFail {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl LabFail {
    pub fn bad(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "bad_request",
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            message: message.into(),
        }
    }

    /// A gateway refusal (the hold with no fallback, an unknown alias, the
    /// engine's own error) with its own status and code.
    pub fn gateway(e: &GatewayError) -> Self {
        Self {
            status: e.http_status(),
            code: e.kind(),
            message: e.to_string(),
        }
    }
}

impl std::fmt::Display for LabFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// The lock every writer of the voice library's shared files takes: a
/// sample's save (the name check, the clip, its `prompt_text` line and its
/// `samples.json` record), the Audio lab's upload, delete and transcript
/// writes, and the owner's `sentences.json`. Each of those reads a file,
/// changes it and writes it back; two at once would lose one's change, and a
/// save's "is the name free" would be stale by its write. One process-wide
/// lock: these writes are rare and short, so nothing waits long.
pub(crate) async fn library_write() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

/// A temporary name beside `name` that no other writer uses: hidden, with
/// the process id and a random tail (`.anna.wav.4711-3fa2c1d0.part`).
pub fn temp_name(name: &str) -> String {
    format!(
        ".{name}.{}-{}.part",
        std::process::id(),
        hex::encode(rand::random::<[u8; 4]>())
    )
}

/// Write `bytes` to `path` whole: a temporary file of its own beside it
/// ([`temp_name`]), then a rename, so a reader never sees half of it. The
/// directory must exist: a writer never brings back a directory something
/// else removed (a round discarded while it rendered).
pub(crate) fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(temp_name(&name));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// A file the owner may edit by hand, read as JSON: `Ok(None)` when it is
/// not there, `Err` naming it when it is there and lmgw cannot read it.
/// Such a file is never written over (`sentences.json`, `samples.json`): a
/// write would replace what the owner has with what lmgw could make of it,
/// which is nothing.
pub(crate) fn read_own_json<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
) -> Result<Option<T>, String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{} cannot be read: {e}", path.display())),
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|e| {
        format!(
            "{} is not valid JSON ({e}): lmgw leaves it as it is — fix it, or move it away to \
             start a new one",
            path.display()
        )
    })
}

/// The refusal of a write into a file [`read_own_json`] could not read.
pub(crate) fn unreadable(why: String) -> LabFail {
    tracing::warn!("voice lab: {why}");
    LabFail {
        status: StatusCode::CONFLICT,
        code: "unreadable_file",
        message: format!("nothing was written: {why}"),
    }
}
