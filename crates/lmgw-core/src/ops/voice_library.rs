//! `voice_transcribe` (audio-class gap 5), shared by `/api/op/voice_transcribe`
//! and `lmgw__voice_transcribe`: voice-library clip transcripts written by a
//! local speech-to-text model ([`crate::web::audio_lab::transcribe`]).
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
/// `alias`, else the setting `audio.voice_transcribe_alias`. The model must
/// be a local speech-to-text row, and is refused under the GPU hold rather
/// than answered by a fallback.
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
        return Ok(json!({
            "ok": true,
            "transcribed": [{"clip": clip, "chars": chars, "transcript_source": w.source}],
            "failed": [],
            "message": format!(
                "{clip} transcribed by '{alias}' ({chars} characters) and recorded as its \
                 transcript — the Audio lab's voice library shows it"
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
        .map(|(clip, chars, source)| {
            json!({"clip": clip, "chars": chars, "transcript_source": source})
        })
        .collect();
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
        (n, 0) => format!("{n} clip(s) transcribed by '{alias}'"),
        (n, f) => format!("{n} clip(s) transcribed by '{alias}', {f} failed"),
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
