//! A TTS row's voices as lmgw knows them — what `GET /v1/audio/voices`
//! answers for a local row without starting it, what a realtime session
//! resolves a voice name against, and what speech shaping ([`super::shape`])
//! decides by.
//!
//! audio.cpp's own list (`handle_voices`) is the row's presets, the
//! `<root>/embeddings/*.safetensors` stems and the `voice_dir` clips — all
//! three lmgw can read itself. It lacks the voices a package ships inside
//! its GGUF or names in its spec (Magpie's `Jason`, Supertonic's `F1`,
//! Qwen3's `ryan`), which is why a client could not find them; lmgw adds
//! them from the [`super::profile`].
//!
//! One entry per name, by what the engine does with that name in a
//! request's `voice` (`select_voice_preset` and `build_speech_request`):
//! a **preset** wins, then a **library** clip, then the name as a voice id —
//! a **native** voice or an **embedding**.

use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use super::profile::{SpeechProfile, VoiceField};
use crate::config::AudioModel;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceKind {
    /// A preset of the row (`voice_presets`).
    Preset,
    /// A clip of the voice library (the class's `voice_dir`).
    Library,
    /// A voice the package ships, by the spec or its GGUF.
    Native,
    /// `<root>/embeddings/<name>.safetensors`.
    Embedding,
}

/// One voice name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VoiceEntry {
    pub id: String,
    pub kind: VoiceKind,
    /// Where lmgw puts the name in the engine's request: `voice`, or
    /// `options.voice_id` for a native voice of a family that reads that.
    pub send: &'static str,
    /// For a library clip: whether its transcript (`prompt_text`) is
    /// recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript: Option<bool>,
}

/// A clip of the voice library, as [`row_voices`] needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryClip {
    pub voice: String,
    pub transcript: bool,
}

/// Every voice name a row answers to (module doc).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RowVoices {
    /// Sorted by id.
    pub entries: Vec<VoiceEntry>,
    /// What speaks when a request names no voice: the row's default preset
    /// (its name, or the voice id or clip of an inline one), else the
    /// family's default voice.
    pub default: Option<String>,
    /// Native voices whose file the package lacks — named by the spec, not
    /// on disk (`<root>/embeddings/<name>.safetensors`).
    pub missing: Vec<String>,
}

impl RowVoices {
    /// The flat, sorted name list — audio.cpp's `{"voices": [...]}`.
    pub fn names(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.id.clone()).collect()
    }

    /// The entry named exactly `name`.
    pub fn exact(&self, name: &str) -> Option<&VoiceEntry> {
        self.entries.iter().find(|e| e.id == name)
    }
}

/// The row's voices, from its profile, the embeddings on disk and the
/// voice library (`None` when the class's `voice_dir` is not the library,
/// so no clip is a name the engine answers to).
pub fn row_voices(
    profile: &SpeechProfile,
    row: &AudioModel,
    embeddings: &[String],
    library: Option<&[LibraryClip]>,
) -> RowVoices {
    let mut entries: Vec<VoiceEntry> = Vec::new();
    let mut add = |id: &str, kind, send, transcript| {
        if !entries.iter().any(|e: &VoiceEntry| e.id == id) {
            entries.push(VoiceEntry {
                id: id.to_string(),
                kind,
                send,
                transcript,
            });
        }
    };
    for name in row.voice_presets.keys() {
        add(name, VoiceKind::Preset, "voice", None);
    }
    for clip in library.unwrap_or_default() {
        add(
            &clip.voice,
            VoiceKind::Library,
            "voice",
            Some(clip.transcript),
        );
    }
    let mut missing = Vec::new();
    for name in &profile.native_voices {
        if profile.file_backed.contains(name) && !embeddings.contains(name) {
            missing.push(name.clone());
            continue;
        }
        let send = match profile.voice_field {
            VoiceField::OptionsVoiceId => VoiceField::OptionsVoiceId.as_str(),
            VoiceField::Voice => VoiceField::Voice.as_str(),
        };
        add(name, VoiceKind::Native, send, None);
    }
    for name in embeddings {
        add(name, VoiceKind::Embedding, "voice", None);
    }
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    let default = match &row.default_voice_preset {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Some(Value::Object(o)) => o
            .get("voice_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                o.get("voice_ref").and_then(Value::as_str).map(|r| {
                    let file = r.rsplit('/').next().unwrap_or(r);
                    file.rsplit_once('.').map_or(file, |(n, _)| n).to_string()
                })
            }),
        _ => profile
            .default_voice
            .clone()
            .filter(|d| !missing.contains(d)),
    };
    RowVoices {
        entries,
        default,
        missing,
    }
}

/// A row's profile and voices together — what every caller wants.
#[derive(Debug, Clone)]
pub struct RowSpeech {
    pub profile: Arc<SpeechProfile>,
    pub voices: RowVoices,
}

/// An lmgw audio row's speech profile alone, from its cache: what a caller
/// that needs only the row's speech facts reads — no listing of its voices
/// (WP11 server review m3: the thread JSON and every speech plan read the
/// facts, and threw the voice lists away).
pub async fn row_profile(
    state: &crate::state::SharedState,
    row: &AudioModel,
) -> Arc<SpeechProfile> {
    let snap = state.snapshot();
    let models_dir = snap.settings.audio.models_dir.trim().to_string();
    let catalog = crate::web::audio::catalog_cached(state)
        .await
        .and_then(|c| {
            let at = c.fetched_at.clone();
            c.specs
                .into_iter()
                .find(|s| s.family == row.family)
                .map(|s| (at, s))
        });
    super::profile::profile_of(&state.audio_profiles, &models_dir, row, catalog).await
}

/// [`row_voices`] for an lmgw audio row: the cached profile, plus the
/// embeddings dir and the voice library read now (both change without the
/// row changing), on the blocking pool.
pub async fn row_speech(state: &crate::state::SharedState, row: &AudioModel) -> RowSpeech {
    let profile = row_profile(state, row).await;
    let models_dir = state
        .snapshot()
        .settings
        .audio
        .models_dir
        .trim()
        .to_string();
    let st = state.clone();
    let r = row.clone();
    let p = profile.clone();
    let voices = tokio::task::spawn_blocking(move || {
        let root = super::files::row_root(&models_dir, &r);
        let embeddings = super::files::embedding_voices(&root);
        let library = crate::web::library_clips(&st);
        row_voices(&p, &r, &embeddings, library.as_deref())
    })
    .await
    .unwrap_or_default();
    RowSpeech { profile, voices }
}

/// The lmgw audio row `route` lands on, if it lands on one.
pub fn row_of_route<'a>(
    snap: &'a crate::config::Snapshot,
    route: &crate::config::Route,
) -> Option<&'a AudioModel> {
    let target = crate::vram::classify(route)?;
    if target.class != crate::runtime::Class::Audio {
        return None;
    }
    snap.audio_models
        .iter()
        .find(|m| m.model_id == target.model_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::profile::VoiceField;

    fn row() -> AudioModel {
        serde_json::from_value(serde_json::json!({
            "id": 1, "model_id": "m", "family": "f", "path": "p", "task": "tts",
            "mode": "offline", "load_options": {}, "session_options": {},
            "voice_presets": {"narrator": {"voice_id": "Jason"}},
            "default_voice_preset": null, "enabled": true, "image": null,
            "extra_run_args": null, "warm_start": false
        }))
        .unwrap()
    }

    #[test]
    fn presets_win_then_clips_then_natives_and_a_missing_file_is_missing() {
        let profile = SpeechProfile {
            native_voices: vec!["Jason".into(), "alba".into(), "narrator".into()],
            file_backed: vec!["alba".into()],
            voice_field: VoiceField::OptionsVoiceId,
            default_voice: Some("alba".into()),
            ..Default::default()
        };
        let clips = [LibraryClip {
            voice: "me".into(),
            transcript: false,
        }];
        let v = row_voices(&profile, &row(), &[], Some(&clips));
        assert_eq!(v.names(), ["Jason", "me", "narrator"]);
        assert_eq!(v.exact("narrator").unwrap().kind, VoiceKind::Preset);
        assert_eq!(v.exact("Jason").unwrap().send, "options.voice_id");
        assert_eq!(v.exact("me").unwrap().transcript, Some(false));
        assert_eq!(v.missing, ["alba"]);
        assert_eq!(v.default, None, "a missing default is no default");

        let v = row_voices(&profile, &row(), &["alba".into()], None);
        assert_eq!(v.names(), ["Jason", "alba", "narrator"]);
        assert_eq!(v.exact("alba").unwrap().kind, VoiceKind::Native);
        assert_eq!(v.default.as_deref(), Some("alba"));
    }
}
