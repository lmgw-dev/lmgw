//! The audio-model editor's prefill for a catalog package: which task a row
//! for it runs, and in which mode.

use super::AUDIO_TASKS;
use crate::audio::{ModelSpec, SpecPackage};

/// The audio.cpp server task (the `--task` values in [`AUDIO_TASKS`]) a row
/// for `pkg` of `spec` runs.
///
/// A family's packages can be different variants of one model, and the
/// package says which: Qwen3-TTS's VoiceDesign package runs only `vdes`
/// while its CustomVoice and Base packages run `tts` (audio-class gap 2,
/// [`crate::audio::variant`]). So a package whose id or name says it is a
/// voice-design one, of a family that lists `design`, suggests `vdes`.
///
/// Otherwise the family's first task tag that maps to one. The aliases are
/// upstream's own, from the `kVocabulary` table: a spec says what the audio
/// is *for* (`music`, `sfx`, `edit`, `audio_generation`), the runtime has
/// one generation kind (`gen`). Guessing them separately is what put
/// `design` and `sfx` families into the editor as `tts` rows.
pub(super) fn suggest_task(spec: &ModelSpec, pkg: &SpecPackage) -> String {
    let named = format!("{} {}", pkg.id, pkg.display_name)
        .to_ascii_lowercase()
        .replace([' ', '-'], "_");
    let design_package = ["voicedesign", "voice_design", "vdes"]
        .iter()
        .any(|w| named.contains(w));
    if design_package && spec.tasks.iter().any(|t| t == "design") {
        return "vdes".into();
    }
    for t in &spec.tasks {
        let t = t.as_str();
        if AUDIO_TASKS.contains(&t) {
            return t.to_string();
        }
        match t {
            "clone" => return "clon".into(),
            "music" | "sfx" | "edit" | "audio_generation" => return "gen".into(),
            "design" => return "vdes".into(),
            "speaker" => return "spk".into(),
            _ => {}
        }
    }
    "tts".into()
}

/// The mode a row for `spec` runs in: `streaming` whenever the family
/// lists it (audio-class gap 8) — a streaming row answers a plain request
/// with one WAV as well (audio.cpp's `handle_speech`), and only a
/// streaming row can stream; otherwise the spec's first mode, else
/// `offline`.
pub(super) fn suggest_mode(spec: &ModelSpec) -> String {
    if spec.modes.iter().any(|m| m == "streaming") {
        return "streaming".into();
    }
    spec.modes
        .first()
        .cloned()
        .unwrap_or_else(|| "offline".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn a_voice_design_package_runs_vdes_and_its_siblings_tts() {
        let spec = crate::audio::parse_spec(&json!({
            "family": "qwen3_tts", "tasks": ["tts", "clone", "design"],
            "packages": [
                {"id": "qwen3_tts_1_7b_customvoice_q8_0",
                 "display_name": "Qwen3 TTS 12Hz 1.7B CustomVoice Q8_0 GGUF", "files": ["a"]},
                {"id": "qwen3_tts_1_7b_voicedesign_q8_0",
                 "display_name": "Qwen3 TTS 12Hz 1.7B VoiceDesign Q8_0 GGUF", "files": ["b"]},
                {"id": "qwen3_tts_1_7b_base_q8_0",
                 "display_name": "Qwen3 TTS 12Hz 1.7B Base Q8_0 GGUF", "files": ["c"]},
            ]
        }));
        let tasks: Vec<String> = spec
            .packages
            .iter()
            .map(|p| suggest_task(&spec, p))
            .collect();
        assert_eq!(tasks, ["tts", "vdes", "tts"]);

        // A family that cannot design keeps its own task whatever the name.
        let other = crate::audio::parse_spec(&json!({
            "family": "x", "tasks": ["tts"],
            "packages": [{"id": "x_voicedesign", "display_name": "", "files": ["a"]}]
        }));
        assert_eq!(suggest_task(&other, &other.packages[0]), "tts");
    }

    #[test]
    fn a_family_that_streams_is_suggested_streaming() {
        let mode = |modes: Value| {
            suggest_mode(&crate::audio::parse_spec(
                &json!({"family": "f", "modes": modes}),
            ))
        };
        assert_eq!(mode(json!(["offline", "streaming"])), "streaming");
        assert_eq!(mode(json!(["offline"])), "offline");
        assert_eq!(mode(json!([])), "offline");
    }
}
