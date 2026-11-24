//! Audio and video (document-conversion design §3, §4.2, WP7): transcribed
//! by the speech-to-text model chosen under Settings › Documents
//! (`documents.asr_alias`), a video's sound first taken out by ffmpeg
//! ([`ffmpeg`]) as 16 kHz mono WAV.
//!
//! - **Which model.** The setting, read once per request into the plan
//!   ([`Speech`]). It is empty until someone picks one (D5: with no pick a
//!   conversion never touches a model); an audio or video file then fails
//!   with `model_not_chosen`, which names the setting. The Chat's own
//!   speech-to-text model is not borrowed: a conversion's caller is an API
//!   key, and the owner picks what it may transcribe with.
//! - **How it is called** ([`asr`]): lmgw's in-process speech-to-text
//!   call (`proxy::transcribe_document`, beside Chat dictation's): the
//!   key's per-call check first (`key_check.rs`, the page-reading model's:
//!   expiry, scope, budget, tokens-per-minute; not the requests-per-minute
//!   window), then the gate (the GPU hold, fallbacks as configured,
//!   admission), the upload and one usage row of class `audio`, labelled
//!   `documents-asr`, under the calling key (a dropped call: a `499` row).
//!   A held model without a fallback is `model_held`; a key that may not
//!   use it, `model_not_allowed`; a fallback that answered is named in the
//!   file's warnings (`model_held`, with the alias that answered) and in
//!   `stats.models`. The call is part of the file's `document_timeout`.
//! - **An audio file** goes up as it is, named `audio.<its type>` (never
//!   the caller's file name), as Chat's audio attachments do (design §4.2:
//!   no ffmpeg for audio). The settings row's facts are [`settings`].
//! - **The transcript** is one part, its paragraphs (the model's own line
//!   breaks) as blocks marked `asr`; no timestamps (the design does not ask
//!   for them; `verbose_json` segments are not asked for yet).
//!
//! Model calls do not take a slot of the tool pool (D20); the ffmpeg run
//! does.

pub mod asr;
pub mod ffmpeg;
pub mod settings;

use lmgw_api_types::documents::convert::ErrorCode;

use crate::proxy::RequestCtx;
use crate::state::SharedState;

/// Who transcribes a request's audio and video, read when the request is
/// planned: the gateway, the calling key's context, and the chosen alias
/// (empty: none chosen).
#[derive(Clone)]
pub struct Speech {
    pub state: SharedState,
    pub ctx: RequestCtx,
    pub alias: String,
}

impl Speech {
    /// The request's speech-to-text model as the settings choose it now.
    pub fn of(state: &SharedState, ctx: &RequestCtx) -> Self {
        Self {
            state: state.clone(),
            ctx: ctx.clone(),
            alias: state
                .snapshot()
                .settings
                .documents
                .asr_alias
                .trim()
                .to_string(),
        }
    }
}

impl std::fmt::Debug for Speech {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Speech")
            .field("alias", &self.alias)
            .finish()
    }
}

/// Plans compare by their options; who transcribes is not one.
impl PartialEq for Speech {
    fn eq(&self, other: &Self) -> bool {
        self.alias == other.alias
    }
}

/// Why a file that needs the model is not transcribed when none is chosen.
pub fn not_chosen() -> (ErrorCode, String) {
    (
        ErrorCode::ModelNotChosen,
        "no speech-to-text model is chosen for conversions: choose one under Settings → \
         Documents (\"Transcribe audio with\", documents_asr_alias) to transcribe audio and \
         video files"
            .to_string(),
    )
}

/// What Settings › Documents says beside audio and video while no model is
/// chosen: the same check the conversion makes ([`not_chosen`]).
pub fn page_note(caps: &mut lmgw_api_types::documents::DocCaps, alias: &str) {
    use lmgw_api_types::documents::Quality;
    for row in &mut caps.formats {
        if !matches!(row.id.as_str(), "audio" | "video") || row.handler.is_none() {
            continue;
        }
        if alias.trim().is_empty() {
            row.quality = Quality::None;
            row.said = "Not transcribed: choose a speech-to-text model below.".to_string();
        } else {
            row.said = format!("{}: {}.", row.said.trim_end_matches('.'), alias.trim());
        }
    }
}

#[cfg(test)]
mod page_note_tests {
    use super::*;
    use crate::extract::convert::report;
    use crate::extract::convert::tools::ToolFacts;
    use lmgw_api_types::documents::InstallPlatform;

    /// The audio row's words are finished sentences, with and without a
    /// chosen model.
    #[test]
    fn the_audio_row_says_whether_a_model_transcribes_in_finished_sentences() {
        let row = |alias: &str| {
            let mut caps = report::doc_caps(&ToolFacts::all_missing(), InstallPlatform::Fedora);
            let audio = caps.formats.iter_mut().find(|f| f.id == "audio").unwrap();
            audio.handler = Some("audio.ffmpeg".into());
            page_note(&mut caps, alias);
            caps.formats.into_iter().find(|f| f.id == "audio").unwrap()
        };
        assert_eq!(
            row("").said,
            "Not transcribed: choose a speech-to-text model below."
        );
        let chosen = row(" example-asr ").said;
        assert!(chosen.starts_with("Transcribed by the speech-to-text model"));
        assert!(chosen.ends_with("chosen below: example-asr."), "{chosen}");
        assert!(!chosen.contains(".:"), "{chosen}");
    }
}
