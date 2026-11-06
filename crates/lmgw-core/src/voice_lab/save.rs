//! Saving a candidate to the voice library (`voice_sample_save`,
//! `POST /voice-lab/api/samples`).
//!
//! The candidate is written to `<audio models dir>/voices/<name>.wav` — the
//! Audio lab's library, which the containers read as `/models/voices` — with
//! its leading and trailing silence cut ([`super::wav`]; loudness
//! untouched), and the text the engine was sent (the round's `sent_text`
//! when shaping changed it, else its text) becomes its line in the
//! library's `prompt_text`: the exact transcript a cloning model needs, no
//! speech-to-text involved. How it was made goes to the lab's
//! [`super::provenance`].
//!
//! A name the library already has — an audio clip of any extension whose
//! voice name it is ([`is_audio`]; `anna.txt` is not one) — is refused
//! (`409 name_taken`) unless the save says `overwrite`: then the new clip
//! is written first, and only then do the other clips of that name go,
//! with their records. The whole save holds [`super::library_write`], so a
//! second save, or an upload, cannot slip in between the name check and
//! the write. A records file (`samples.json`) lmgw cannot read refuses the
//! save before anything is written.

use std::path::Path;

use axum::http::StatusCode;
use lmgw_api_types::voice_lab::{
    VoiceProvenance, VoiceRound, VoiceSampleSaveArgs, VoiceSampleSaved,
};

use super::{lab_dir, provenance, rounds, wav, LabFail};
use crate::state::SharedState;
use crate::web::audio_lab::{
    clip_entries, read_prompt_text, safe_clip_name, voices_dir, write_prompt_text,
    CONTAINER_VOICES_DIR,
};

/// The voice name a save asks for: trimmed, a trailing `.wav` dropped, and
/// only the characters a library clip's name may hold.
pub fn check_name(name: &str) -> Result<String, LabFail> {
    let name = name.trim();
    let name = match name.len().checked_sub(4) {
        Some(cut) if name.is_char_boundary(cut) && name[cut..].eq_ignore_ascii_case(".wav") => {
            name[..cut].trim_end()
        }
        _ => name,
    };
    if name.is_empty() {
        return Err(LabFail::bad("give the sample a name"));
    }
    safe_clip_name(name).map_err(|e| LabFail::bad(e.replace("file name", "name")))
}

/// The refusal of a taken name, naming the files it would replace.
pub fn name_taken(voice: &str, files: &[String]) -> LabFail {
    LabFail {
        status: StatusCode::CONFLICT,
        code: "name_taken",
        message: format!(
            "the voice library already has '{voice}' ({}): saving under that name replaces it — \
             confirm, or pick another name",
            files.join(", ")
        ),
    }
}

/// Whether the library file `name` is audio by its extension (`.wav`,
/// `.mp3`, `.flac`, …): what a voice name can mean. Anything else beside
/// the clips (`anna.txt`, `anna.json`) is never replaced by a save.
pub fn is_audio(name: &str) -> bool {
    mime_guess::from_path(name)
        .iter()
        .any(|m| m.type_() == mime_guess::mime::AUDIO)
}

/// The library's audio clips whose voice name is `voice`.
pub fn files_named(voices: &Path, voice: &str) -> Vec<String> {
    clip_entries(voices)
        .into_iter()
        .filter(|c| c.voice == voice && is_audio(&c.name))
        .map(|c| c.name)
        .collect()
}

/// Save (module doc).
pub async fn save(
    state: &SharedState,
    args: &VoiceSampleSaveArgs,
) -> Result<VoiceSampleSaved, LabFail> {
    let voices = voices_dir(state)
        .ok_or_else(|| LabFail::bad(crate::web::audio_lab::no_audio_models_dir()))?;
    // A dev instance may not write into a library outside its data dir.
    state
        .refuse_shared_models_dir(&voices)
        .map_err(|why| LabFail {
            status: StatusCode::BAD_REQUEST,
            code: crate::config::dev_models_dir_code(&why).unwrap_or("bad_request"),
            message: why,
        })?;
    let voice = check_name(&args.name)?;
    let lab = lab_dir(state);
    let library_active = state.snapshot().settings.audio.voice_dir.trim() == CONTAINER_VOICES_DIR;
    let overwrite = args.overwrite;
    let (round_id, candidate) = (args.round.clone(), args.candidate);
    // Held by the file work itself: a caller that goes away mid-save does
    // not let the next writer in while this one still writes.
    let lock = super::library_write().await;
    tokio::task::spawn_blocking(move || {
        let _lock = lock;
        let round = rounds::read_round(&lab, &round_id)?;
        let (bytes, mime) = rounds::read_candidate(&lab, &round.id, candidate)?;
        write(
            &lab, &voices, &round, candidate, &voice, overwrite, &bytes, &mime,
        )
        .map(|(clip, replaced, provenance)| {
            let trim = &provenance.trim;
            let cut = match &trim.note {
                Some(n) => format!("not trimmed: {n}"),
                None => format!(
                    "{} ms of silence cut before, {} ms after, {:.1} s kept",
                    trim.lead_ms,
                    trim.tail_ms,
                    trim.kept_ms as f64 / 1000.0
                ),
            };
            let reach = if library_active {
                format!("\"voice\": \"{voice}\" clones it on every cloning model")
            } else {
                format!(
                    "the class's voice directory is not this library, so it is reachable as \
                         voice_ref {CONTAINER_VOICES_DIR}/{clip} only"
                )
            };
            VoiceSampleSaved {
                ok: true,
                message: format!("saved as {clip} with its transcript ({cut}); {reach}"),
                transcript: provenance.text.clone(),
                clip,
                voice,
                replaced,
                provenance,
                library_active,
            }
        })
    })
    .await
    .map_err(|e| LabFail::internal(e.to_string()))?
}

/// The file work of a save, on the blocking pool, under the library's
/// lock: the name check against the library, the trimmed clip, its
/// transcript line, its record.
#[allow(clippy::too_many_arguments)]
fn write(
    lab: &Path,
    voices: &Path,
    round: &VoiceRound,
    candidate: u32,
    voice: &str,
    overwrite: bool,
    bytes: &[u8],
    mime: &str,
) -> Result<(String, Vec<String>, VoiceProvenance), LabFail> {
    let existing = files_named(voices, voice);
    if !existing.is_empty() && !overwrite {
        return Err(name_taken(voice, &existing));
    }
    // Before anything is written: a records file lmgw cannot read is the
    // owner's, and the save would have to write over it.
    provenance::read(lab).map_err(super::unreadable)?;
    let (clip_bytes, trim) = wav::trim_silence(bytes);
    let ext = rounds::candidate_file(0, mime)
        .rsplit_once('.')
        .map(|(_, e)| e.to_string())
        .unwrap_or_else(|| "wav".into());
    let clip = format!("{voice}.{ext}");
    std::fs::create_dir_all(voices)
        .map_err(|e| LabFail::internal(format!("creating {}: {e}", voices.display())))?;
    // The new clip first: a failure from here on leaves the library with
    // it, never with the voice gone.
    super::write_atomic(&voices.join(&clip), &clip_bytes)
        .map_err(|e| LabFail::internal(format!("writing {clip}: {e}")))?;
    for f in existing.iter().filter(|f| **f != clip) {
        std::fs::remove_file(voices.join(f))
            .map_err(|e| LabFail::internal(format!("replacing {f}: {e}")))?;
    }
    // One line per clip in the index: what the engine was sent.
    let sent = round.sent_text.as_deref().unwrap_or(&round.text);
    let transcript = sent.replace(['\n', '\r'], " ").trim().to_string();
    let mut texts = read_prompt_text(voices);
    texts.insert(voice.to_string(), transcript.clone());
    write_prompt_text(voices, &texts)
        .map_err(|e| LabFail::internal(format!("writing the transcript: {e}")))?;
    let c = round.candidates.iter().find(|c| c.index == candidate);
    let p = VoiceProvenance {
        alias: round.alias.clone(),
        answered_by: round.answered_by.clone(),
        fallback_reason: round.fallback_reason.clone(),
        upstream: round.upstream.clone(),
        upstream_model: round.upstream_model.clone(),
        row: round.row.clone(),
        description: round.description.clone(),
        voice: round.voice.clone(),
        seed: c.and_then(|c| c.seed),
        language: round.language.clone(),
        text: transcript,
        asked_text: round.sent_text.as_ref().map(|_| round.text.clone()),
        generated_at: round.created_at.clone(),
        saved_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        licence: round.licence.clone(),
        round: round.id.clone(),
        candidate,
        trim,
        sha256: provenance::sha256(&clip_bytes),
    };
    provenance::record(lab, &clip, &p, &existing)?;
    Ok((clip, existing, p))
}
