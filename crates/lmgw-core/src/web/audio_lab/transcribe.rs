//! Voice-library clip transcripts (audio-class gap 5).
//!
//! A cloning model that declares `reference_text` (Fish, CosyVoice3) clones
//! well only with the clip's transcript, which audio.cpp injects from the
//! library's `prompt_text` when a request's `voice` names the clip. Typing
//! one per clip was the only way to get it there. Here a speech-to-text
//! model writes it:
//! - per clip (`POST /audio-lab/api/refs/{name}/transcribe`), overwriting;
//! - for every clip without one (the `voice_transcribe` op and its
//!   `lmgw__voice_transcribe` tool);
//! - on upload, when the owner set `audio.voice_transcribe_alias` — empty by
//!   default, so nothing is transcribed unless the owner chose a model.
//!
//! Always through [`proxy::transcribe_voice_clip`]: the model that answers
//! must transcribe, and a configured fallback answers as for any request
//! (changed 2026-10-06, the owner's ruling; until then the clip went only
//! to a local speech-to-text row, never to the GPU hold's fallback).

use std::path::Path;

use bytes::Bytes;
use lmgw_api_types::audio_lab::TranscriptProvenance;

use super::{clip_entries, read_prompt_text, safe_clip_name, voice_name, voices_dir};
use super::{write_prompt_text, PROMPT_TEXT_FILE};
use crate::proxy;
use crate::state::SharedState;

/// The alias to transcribe with: the one asked for, else the setting.
pub(crate) fn alias_for(state: &SharedState, asked: Option<&str>) -> Result<String, String> {
    if let Some(a) = asked.map(str::trim).filter(|a| !a.is_empty()) {
        return Ok(a.to_string());
    }
    let set = state
        .snapshot()
        .settings
        .audio
        .voice_transcribe_alias
        .trim()
        .to_string();
    if set.is_empty() {
        return Err(
            "no transcription model: name a speech-to-text model (alias), or set one under \
             Settings → Runtimes → Audio → Clip transcripts"
                .into(),
        );
    }
    Ok(set)
}

/// What [`transcribe_clip`] wrote.
#[derive(Debug)]
pub(crate) struct Written {
    pub transcript: String,
    /// `asr:<model>` — where the transcript came from: the alias asked
    /// for, or the fallback that answered in its place (review V1).
    pub source: String,
    /// The fallback that answered, when one did: its name and the gate's
    /// reason (`hold`, `external_vram`, …).
    pub answered_by: Option<(String, &'static str)>,
    /// Who wrote it, in a sentence's words ([`proxy::answered_line`]).
    pub by: String,
}

impl Written {
    /// The answer's fields about who wrote the transcript.
    pub(crate) fn provenance(&self) -> TranscriptProvenance {
        TranscriptProvenance {
            transcript_source: self.source.clone(),
            answered_by: self.answered_by.as_ref().map(|(f, _)| f.clone()),
            fallback_reason: self.answered_by.as_ref().map(|(_, r)| r.to_string()),
            by: self.by.clone(),
        }
    }
}

/// Why a clip was not transcribed.
#[derive(Debug)]
pub(crate) enum ClipError {
    /// The model may not hear any clip now: the hold or a benchmark with no
    /// fallback, an alias that is no speech-to-text model.
    Refused(String),
    /// This clip: unreadable, no speech in it, the model failed on it.
    Clip(String),
}

impl std::fmt::Display for ClipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(why) | Self::Clip(why) => f.write_str(why),
        }
    }
}

/// Transcribe the library clip `name` (a file name) with `alias` and record
/// it as the clip's `prompt_text` line, replacing one it had.
pub(crate) async fn transcribe_clip(
    state: &SharedState,
    name: &str,
    alias: &str,
) -> Result<Written, ClipError> {
    let dir = voices_dir(state).ok_or_else(|| {
        ClipError::Refused(
            "audio models dir is not configured (Settings → Runtimes → Audio)".into(),
        )
    })?;
    // Before the model hears anything: the transcript is a write into the
    // library, which a dev instance may not make outside its data dir.
    state
        .refuse_shared_models_dir(&dir)
        .map_err(ClipError::Refused)?;
    let name = clip_file(&dir, name)?;
    let path = dir.join(&name);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| ClipError::Clip(format!("reading {name}: {e}")))?;
    let mime = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .as_ref()
        .to_string();
    let heard = proxy::transcribe_voice_clip(state, alias, Bytes::from(bytes), &name, &mime)
        .await
        .map_err(|e| match e.kind() {
            "gpu_hold" | "gpu_benchmark" | proxy::ASR_REQUIRED | "unknown_alias" => {
                ClipError::Refused(e.to_string())
            }
            _ => ClipError::Clip(e.to_string()),
        })?;
    let by = proxy::answered_line(alias, heard.answered_by.as_ref());
    // One line per clip in the index.
    let text = heard.text.replace(['\n', '\r'], " ").trim().to_string();
    if text.is_empty() {
        return Err(ClipError::Clip(format!(
            "{by} heard no speech in {name} — nothing was recorded"
        )));
    }
    let mut texts = read_prompt_text(&dir);
    texts.insert(voice_name(&name).to_string(), text.clone());
    write_prompt_text(&dir, &texts)
        .map_err(|e| ClipError::Clip(format!("writing {PROMPT_TEXT_FILE}: {e}")))?;
    Ok(Written {
        transcript: text,
        source: format!("asr:{}", heard.by(alias)),
        answered_by: heard.answered_by.map(|(f, r)| (f, r.as_str())),
        by,
    })
}

/// The clip `name` names: its file name (`me.wav`), or the voice name a
/// request uses (`me`).
fn clip_file(dir: &Path, name: &str) -> Result<String, ClipError> {
    let name = safe_clip_name(name).map_err(ClipError::Clip)?;
    if dir.join(&name).is_file() {
        return Ok(name);
    }
    clip_entries(dir)
        .iter()
        .find(|e| e.voice == name)
        .map(|e| e.name.clone())
        .ok_or_else(|| ClipError::Clip(format!("no clip named {name} in the voice library")))
}

/// The library's clips without a transcript, by file name.
pub(crate) fn clips_without_transcript(dir: &Path) -> Vec<String> {
    clip_entries(dir)
        .iter()
        .filter(|e| e.transcript.trim().is_empty())
        .map(|e| e.name.clone())
        .collect()
}

/// What [`transcribe_missing`] did.
#[derive(Debug, Default)]
pub(crate) struct Bulk {
    /// `(clip, characters, what wrote it)` per clip transcribed.
    pub transcribed: Vec<(String, usize, Written)>,
    /// `(clip, why)` per clip that failed.
    pub failed: Vec<(String, String)>,
    /// Clips that had a transcript already.
    pub kept: usize,
    /// Why the run stopped before the last clip: a refusal every clip would
    /// get (module doc).
    pub stopped: Option<String>,
}

/// Transcribe every clip of the library that has no transcript, one at a
/// time, with `alias`. A refusal that would refuse every clip the same way
/// (the hold with no fallback, an alias that is no speech-to-text model) ends the run
/// where it happens rather than repeating itself; what was transcribed
/// before it stays recorded.
pub(crate) async fn transcribe_missing(state: &SharedState, alias: &str) -> Result<Bulk, String> {
    let dir = voices_dir(state)
        .ok_or("audio models dir is not configured (Settings → Runtimes → Audio)")?;
    state.refuse_shared_models_dir(&dir)?;
    let missing = clips_without_transcript(&dir);
    let mut bulk = Bulk {
        kept: clip_entries(&dir).len().saturating_sub(missing.len()),
        ..Default::default()
    };
    for clip in missing {
        match transcribe_clip(state, &clip, alias).await {
            Ok(w) => bulk
                .transcribed
                .push((clip, w.transcript.chars().count(), w)),
            Err(ClipError::Refused(why)) => {
                bulk.stopped = Some(why);
                break;
            }
            Err(ClipError::Clip(why)) => bulk.failed.push((clip, why)),
        }
    }
    Ok(bulk)
}
