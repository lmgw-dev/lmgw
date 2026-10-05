//! Nemotron ASR's language prompts: the keys of the `prompt_dictionary` in
//! the `processor_config.json` its package GGUF embeds — what the engine
//! looks a transcription's `language` up in, exactly
//! ([`super::families::reads_prompt_dictionary`]). They become the row's
//! vocabulary, so a configured `de` goes up as the key the package has, and
//! one it has none for is said before a transcription is refused for it
//! ([`crate::audio::language`]).

use serde_json::Value;

use super::{LanguageVocab, SpeechProfile, VocabSource};
use crate::gguf::embedded::EmbeddedIndex;

/// The prompts of `index`'s `processor_config.json` into `p`, sorted; a
/// file that is missing leaves `p` alone, one that cannot be read is a
/// problem of the profile's.
pub(super) fn prompt_dictionary(p: &mut SpeechProfile, index: &EmbeddedIndex) {
    let raw = match index.file("processor_config.json") {
        Ok(Some(raw)) => raw,
        Ok(None) => return,
        Err(e) => {
            p.problems.push(format!("processor_config.json: {e}"));
            return;
        }
    };
    let Ok(cfg) = serde_json::from_slice::<Value>(&raw) else {
        p.problems.push("processor_config.json: not JSON".into());
        return;
    };
    let Some(prompts) = cfg.get("prompt_dictionary").and_then(Value::as_object) else {
        return;
    };
    let mut entries: Vec<String> = prompts.keys().cloned().collect();
    if entries.is_empty() {
        return;
    }
    entries.sort();
    p.language_vocab = LanguageVocab {
        source: VocabSource::Gguf,
        entries,
    };
    p.sources
        .push("the language prompts the package GGUF embeds".into());
}
