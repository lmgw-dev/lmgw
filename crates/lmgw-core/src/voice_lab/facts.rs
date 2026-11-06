//! What a text-to-speech alias takes, as the Voices lab's form adapts to it
//! (`GET /voice-lab/api/model`): a voice description for a model that
//! designs or styles its voice, a voice for one that speaks a voice it is
//! told, a seed where the engine reads one, the languages it declares — and
//! whether it clones a library clip, for the cloning check.
//!
//! Read from lmgw's own catalog (an audio row's speech profile, a cloud
//! alias's `capabilities.speech` override), starting nothing. The gate is
//! consulted only for routing — whether the GPU hold or a busy card hands
//! the alias to its fallback now — never for admission.

use std::path::Path;

use lmgw_api_types::voice_lab::{VoiceDescriptionMode, VoiceModelFacts, VoiceRow};
use lmgw_api_types::ModelLicence;

use super::LabFail;
use crate::audio::profile::{InstructionsMode, SpeechProfile, Unvoiced};
use crate::audio::voices::{row_of_route, row_profile};
use crate::config::AudioModel;
use crate::error::GatewayError;
use crate::gate::{FallbackReason, RouteCheck};
use crate::state::SharedState;

/// The published word as the lab's mode.
pub fn description_mode(m: InstructionsMode) -> VoiceDescriptionMode {
    match m {
        InstructionsMode::None => VoiceDescriptionMode::None,
        InstructionsMode::Style => VoiceDescriptionMode::Style,
        InstructionsMode::VoiceDesign => VoiceDescriptionMode::VoiceDesign,
        InstructionsMode::Passthrough => VoiceDescriptionMode::Passthrough,
    }
}

/// The row as the lab names it.
pub fn row_ref(row: &AudioModel, profile: &SpeechProfile) -> VoiceRow {
    VoiceRow {
        model_id: row.model_id.clone(),
        family: row.family.clone(),
        variant: profile.variant.clone(),
    }
}

/// The licence `row`'s package GGUF states, when it states one.
pub async fn row_licence(state: &SharedState, row: &AudioModel) -> Option<ModelLicence> {
    let models_dir = state
        .snapshot()
        .settings
        .audio
        .models_dir
        .trim()
        .to_string();
    if models_dir.is_empty() {
        return None;
    }
    let root = crate::audio::files::row_root(&models_dir, row);
    let (gguf, _) = crate::audio::files::row_gguf(
        Path::new(&models_dir),
        &root,
        row.weight_id.as_deref(),
        &row.family,
    )?;
    let summary = state.gguf_cache.summarize_cached(&gguf).await.ok()?;
    summary.licence.as_ref().map(ModelLicence::from)
}

/// Whether lmgw knows the row's engine clones reference audio — it takes a
/// clip's transcript, refuses to clone without one, or refuses to speak
/// without a reference at all.
pub fn clones(profile: &SpeechProfile) -> bool {
    profile.needs_reference_text
        || profile.requires_reference_text
        || profile.unvoiced == Unvoiced::NeedsReference
}

/// The form's facts for `alias` (module doc); `language` and `voice` are
/// the form's, for what happens to the language.
pub async fn facts(
    state: &SharedState,
    alias: &str,
    language: Option<&str>,
    voice: Option<&str>,
) -> Result<VoiceModelFacts, LabFail> {
    let alias = alias.trim();
    if alias.is_empty() {
        return Err(LabFail::bad("name a text-to-speech model (alias)"));
    }
    let snap = state.snapshot();
    let route = snap.resolve(alias).map_err(|e| LabFail::gateway(&e))?;
    crate::proxy::require_openai_audio(&route, alias).map_err(|e| LabFail::gateway(&e))?;
    let (held, fallback) = routing(state, alias).await?;
    let language = language
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(super::sentences::normalize)
        .transpose()
        .map_err(LabFail::bad)?;
    let voice = voice.map(str::trim).filter(|v| !v.is_empty());
    let mut f = VoiceModelFacts {
        alias: alias.to_string(),
        held,
        fallback,
        ..VoiceModelFacts::default()
    };
    match row_of_route(&snap, &route).cloned() {
        Some(row) => {
            let profile = row_profile(state, &row).await;
            let mode = description_mode(profile.instructions);
            let describes = crate::audio::preflight::row_describes(&row);
            f.local = true;
            f.row = Some(row_ref(&row, &profile));
            f.description = mode;
            f.description_default = describes;
            f.description_required = mode == VoiceDescriptionMode::VoiceDesign && !describes;
            f.takes_voice = mode != VoiceDescriptionMode::VoiceDesign;
            f.default_voice = crate::audio::voices::row_speech(state, &row)
                .await
                .voices
                .default;
            f.reads_seed = profile.reads_seed;
            f.row_pins_seed = row.default_request_options.contains_key("seed");
            f.languages = crate::audio::language::iso_languages(&profile);
            f.language_note = language.as_deref().and_then(|code| {
                crate::audio::language::tts_fit(code, &profile, voice)
                    .note
                    .map(|n| format!("{alias} {n}"))
            });
            f.clones = clones(&profile);
            f.clone_note = clone_note(state, alias, &profile);
            f.licence = row_licence(state, &row).await;
        }
        None => {
            let rules = crate::proxy::remote_rules(&snap, alias);
            let mode = description_mode(rules.instructions);
            f.description = mode;
            f.description_required = mode == VoiceDescriptionMode::VoiceDesign;
            f.takes_voice = mode != VoiceDescriptionMode::VoiceDesign;
            f.languages = snap
                .aliases
                .get(alias)
                .and_then(|a| a.capabilities_override.as_ref())
                .and_then(|o| o.pointer("/capabilities/languages"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            f.language_note = language.map(|_| {
                format!(
                    "{alias} is a cloud model: lmgw sends it no language, it reads the text as \
                     written"
                )
            });
            f.clone_note = Some(format!(
                "{alias} is a cloud model: it is sent the sample's name, which its provider does \
                 not know — a voice-library clip clones on a local audio row"
            ));
        }
    }
    Ok(f)
}

/// What a cloning check with this row does with a sample.
fn clone_note(state: &SharedState, alias: &str, profile: &SpeechProfile) -> Option<String> {
    let library = state.snapshot().settings.audio.voice_dir.trim()
        == crate::web::audio_lab::CONTAINER_VOICES_DIR;
    let how = if library {
        "the sample is named as its voice, as any client and a realtime session name it"
    } else {
        "the voice library is not the class's voice directory, so the sample is sent as a \
         voice_ref path with its transcript"
    };
    if clones(profile) {
        Some(format!("{alias} clones a clip: {how}"))
    } else {
        Some(format!(
            "lmgw does not know whether {alias} ({}) clones a clip — try it: {how}",
            profile.family
        ))
    }
}

/// Whether the GPU hold (or a benchmark's lease) pauses the alias now, and
/// the alias that answers in its place — routing only, no admission.
async fn routing(state: &SharedState, alias: &str) -> Result<(bool, Option<String>), LabFail> {
    match crate::gate::resolve(
        state,
        alias,
        RouteCheck::Audio,
        crate::runtime::registry::Origin::Owner,
    )
    .await
    {
        Ok(r) => {
            let held = matches!(
                r.headers().fallback_reason(),
                Some(FallbackReason::Hold | FallbackReason::Benchmark)
            );
            Ok((held, r.headers().fallback().map(str::to_string)))
        }
        Err(f) => match f.error {
            GatewayError::GpuHold { .. } | GatewayError::GpuBenchmark { .. } => Ok((true, None)),
            e => Err(LabFail::gateway(&e)),
        },
    }
}
