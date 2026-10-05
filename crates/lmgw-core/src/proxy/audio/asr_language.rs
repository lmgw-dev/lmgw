//! A transcription's `language` in the speech-to-text row's own spelling
//! (chat-voice design §2.1, changed 2026-10-04): on the route that answers,
//! for every caller — a client's `POST /v1/audio/transcriptions`, a
//! realtime turn, the Chat's dictation.
//!
//! audio.cpp hands the field to the engine as `text_input.language`, and a
//! row that declares its languages takes only its own keys: Nemotron ASR
//! looks it up in its package's prompts and refuses a miss
//! ([`crate::audio::families::reads_prompt_dictionary`]). So a code that
//! names exactly one of them goes up as that key (`id` → `id-ID`), and
//! anything else as it came — the engine stays the judge, and the Chat says
//! beforehand where its configured language would be refused
//! (`web::chat_voice::language`). A route that is no lmgw audio row, or a
//! row without a vocabulary, is sent the field untouched.

use super::super::multipart::MultipartField;
use super::AudioUpload;
use crate::config::Route;
use crate::state::SharedState;

/// `fields` with their `language` respelled for `route`'s row, when the row
/// spells it otherwise; `None` to send them as they are.
pub(super) async fn respelled(
    state: &SharedState,
    route: &Route,
    which: AudioUpload,
    fields: &[MultipartField],
) -> Option<Vec<MultipartField>> {
    if !matches!(
        which,
        AudioUpload::Transcription | AudioUpload::TranscriptionDetails
    ) {
        return None;
    }
    let asked = fields.iter().find_map(|f| match f {
        MultipartField::Text(name, value) if name == "language" => Some(value.as_str()),
        _ => None,
    })?;
    let snap = state.snapshot();
    let row = crate::audio::voices::row_of_route(&snap, route)?;
    let profile = crate::audio::voices::row_profile(state, row).await;
    let to = crate::audio::language::map_language(asked, &profile.family, &profile.language_vocab)?;
    tracing::debug!(
        "transcription on '{}': language '{asked}' sent as '{to}', the row's own spelling",
        row.model_id
    );
    Some(
        fields
            .iter()
            .map(|f| match f {
                MultipartField::Text(name, _) if name == "language" => {
                    MultipartField::Text(name.clone(), to.clone())
                }
                other => other.clone(),
            })
            .collect(),
    )
}
