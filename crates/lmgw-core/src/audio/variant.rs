//! A package's variant against its row's task (audio-class gap 2).
//!
//! Qwen3-TTS ships three variants under one family, told apart only by the
//! `tts_model_type` of the `config.json` its GGUF embeds: CustomVoice and
//! Base run the `tts` task, VoiceDesign only `vdes`
//! (`qwen3_tts/session.cpp` refuses the other, and audio.cpp runs the row's
//! task whatever a request asks). A row whose task does not match its
//! package fails every request, so lmgw refuses to save one, refuses a
//! speech request to one already saved before anything is started
//! ([`super::preflight`]), and says so in its `/v1/models` notes — with
//! the fix spelled out. It never changes the task itself.

use std::path::Path;

use serde_json::Value;

use super::{families, files};
use crate::gguf::embedded::read_embedded_index;

/// The package's variant (`voice_design`) of a row of `family` on `root`
/// (under `models_dir`), when the family keeps one and its GGUF says.
/// Blocking: reads the GGUF header and one embedded file.
pub fn package_variant(
    models_dir: &Path,
    root: &Path,
    family: &str,
    weight_id: Option<&str>,
) -> Option<String> {
    if !families::reads_qwen3_config(family) {
        return None;
    }
    let (gguf, _) = files::row_gguf(models_dir, root, weight_id, family)?;
    let index = read_embedded_index(&gguf).ok()?;
    let raw = index.file("config.json").ok()??;
    let cfg: Value = serde_json::from_slice(&raw).ok()?;
    cfg.get("tts_model_type")?.as_str().map(str::to_string)
}

/// The task `variant` runs, when lmgw knows it.
pub fn variant_task(variant: &str) -> Option<&'static str> {
    families::qwen3_variant_task(variant)
}

fn label(variant: &str) -> &str {
    match variant {
        "custom_voice" => "CustomVoice",
        "voice_design" => "VoiceDesign",
        "base" => "Base",
        other => other,
    }
}

/// Why a row `model_id` of `task` cannot run its package's `variant`, with
/// the fix; `None` when it can (or lmgw does not know the variant's task).
pub fn mismatch(model_id: &str, task: &str, variant: &str) -> Option<String> {
    let runs = variant_task(variant)?;
    if runs == task {
        return None;
    }
    let design = if runs == "vdes" {
        ", and give it a voice description under its default request options (instruct) or \
         send instructions with every request"
    } else {
        ""
    };
    Some(format!(
        "audio model '{model_id}' has task '{task}', but its package is a Qwen3-TTS {} model, \
         which runs only task '{runs}' — set the row's task to {runs}{design}",
        label(variant)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_names_the_task_to_set() {
        assert_eq!(mismatch("d", "vdes", "voice_design"), None);
        assert_eq!(mismatch("c", "tts", "custom_voice"), None);
        let m = mismatch("design", "tts", "voice_design").unwrap();
        assert!(
            m.contains("VoiceDesign") && m.contains("set the row's task to vdes"),
            "{m}"
        );
        let m = mismatch("base", "vdes", "base").unwrap();
        assert!(m.ends_with("set the row's task to tts"), "{m}");
        assert_eq!(mismatch("x", "tts", "something_new"), None);
    }
}
