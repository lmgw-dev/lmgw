//! `voice_generate` and `voice_sample_save`, shared by `/api/op/*` and the
//! `lmgw__voice_generate` / `lmgw__voice_sample_save` tools: the Voices
//! lab's round and save ([`crate::voice_lab`]), the same functions its
//! routes call.

use lmgw_api_types::voice_lab::{
    VoiceGenerateArgs, VoiceRound, VoiceSampleSaveArgs, VoiceSampleSaved,
};

use crate::state::SharedState;
use crate::voice_lab::{generate, save, LabFail};

/// A refusal as an op's error: the sentence, with the code a caller acts
/// on (`name_taken`) in front.
fn said(f: LabFail) -> String {
    match f.code {
        "bad_request" | "internal" => f.message,
        "name_taken" => format!(
            "name_taken: {} (pass overwrite: true to replace it, once the owner confirmed)",
            f.message
        ),
        code => format!("{code}: {}", f.message),
    }
}

/// Render a round of candidates with a text-to-speech alias.
pub async fn voice_generate(
    state: &SharedState,
    args: VoiceGenerateArgs,
) -> Result<VoiceRound, String> {
    generate::generate(state, &args).await.map_err(said)
}

/// Save a round's candidate to the voice library.
pub async fn voice_sample_save(
    state: &SharedState,
    args: VoiceSampleSaveArgs,
) -> Result<VoiceSampleSaved, String> {
    save::save(state, &args).await.map_err(said)
}
