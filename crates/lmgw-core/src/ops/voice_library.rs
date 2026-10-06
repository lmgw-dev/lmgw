//! `voice_transcribe` (audio-class gap 5), shared by `/api/op/voice_transcribe`
//! and `lmgw__voice_transcribe`: voice-library clip transcripts written by a
//! speech-to-text model ([`crate::web::audio_lab::transcribe`]).
//!
//! The answer says which clips were transcribed and how long each
//! transcript is, never the text: the clips are the owner's own recordings,
//! and a tool caller is not the place their words go. The dashboard's Audio
//! lab shows them.

use serde_json::{json, Value};

use crate::state::SharedState;
use crate::web::audio_lab::transcribe::{alias_for, transcribe_clip, transcribe_missing};

/// Transcribe `clip` (a file or voice name; it is replaced if it had one),
/// or with no clip every clip of the library that has no transcript — with
/// `alias`, else the setting `audio.voice_transcribe_alias`. The model that
/// answers must be a speech-to-text model, wherever it runs: the alias, or
/// the fallback the GPU hold or an outside-VRAM verdict hands it to.
pub async fn voice_transcribe(
    state: &SharedState,
    clip: Option<&str>,
    alias: Option<&str>,
) -> Result<Value, String> {
    let alias = alias_for(state, alias)?;
    if let Some(clip) = clip.map(str::trim).filter(|c| !c.is_empty()) {
        let w = transcribe_clip(state, clip, &alias)
            .await
            .map_err(|e| e.to_string())?;
        let chars = w.transcript.chars().count();
        let mut entry = w.provenance();
        entry["clip"] = json!(clip);
        entry["chars"] = json!(chars);
        return Ok(json!({
            "ok": true,
            "transcribed": [entry],
            "failed": [],
            "message": format!(
                "{clip} transcribed by {} ({chars} characters) and recorded as its \
                 transcript — the Audio lab's voice library shows it",
                w.by
            ),
        }));
    }
    let bulk = transcribe_missing(state, &alias).await?;
    if bulk.transcribed.is_empty() {
        if let Some(why) = bulk.stopped {
            return Err(why);
        }
    }
    let transcribed: Vec<Value> = bulk
        .transcribed
        .iter()
        .map(|(clip, chars, w)| {
            let mut entry = w.provenance();
            entry["clip"] = json!(clip);
            entry["chars"] = json!(chars);
            entry
        })
        .collect();
    // Who wrote them: one line per model, a fallback named as one.
    let mut by: Vec<&str> = bulk
        .transcribed
        .iter()
        .map(|(_, _, w)| w.by.as_str())
        .collect();
    by.sort_unstable();
    by.dedup();
    let by = match by.as_slice() {
        [] => format!("'{alias}'"),
        [one] => one.to_string(),
        many => many.join(" and "),
    };
    let failed: Vec<Value> = bulk
        .failed
        .iter()
        .map(|(clip, why)| json!({"clip": clip, "error": why}))
        .collect();
    let mut message = match (transcribed.len(), failed.len()) {
        (0, 0) => format!(
            "every clip has a transcript already ({}) — nothing to do",
            bulk.kept
        ),
        (n, 0) => format!("{n} clip(s) transcribed by {by}"),
        (n, f) => format!("{n} clip(s) transcribed by {by}, {f} failed"),
    };
    if let Some(why) = &bulk.stopped {
        message.push_str(&format!("; stopped before the rest: {why}"));
    }
    Ok(json!({
        "ok": true,
        "transcribed": transcribed,
        "failed": failed,
        "already": bulk.kept,
        "message": message,
    }))
}
