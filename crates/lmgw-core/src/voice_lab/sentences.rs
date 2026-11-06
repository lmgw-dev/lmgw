//! The sentence a round's candidates say, per language.
//!
//! A cloning model learns a voice from its reference clip, so the clip
//! should cover the language's sounds: the built-in sentence per language
//! is the first two sentences of "The North Wind and the Sun", the passage
//! phoneticians use to illustrate a language (the IPA Handbook's), about ten
//! seconds of speech. The owner may edit it; the edited sentence is saved
//! for its language (`<data_dir>/voice-lab/sentences.json`) and becomes the
//! prefill. Each entry also has the short line the cloning check speaks.
//!
//! **Adding a language is one entry in [`TABLE`]** and nothing else: the
//! page, the API and the tools list it from there. A language without an
//! entry starts with an empty box, and appears in the list once the owner
//! saved a sentence for it.

use std::collections::BTreeMap;
use std::path::Path;

use lmgw_api_types::voice_lab::{VoiceSentence, VoiceSentences};

use super::LabFail;

/// One language of the built-in table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// ISO 639-1 code, lowercase.
    pub language: &'static str,
    /// What a round's candidates say.
    pub sentence: &'static str,
    /// What the cloning check says.
    pub check_line: &'static str,
}

/// The built-in sentences, one entry per language.
pub const TABLE: &[Entry] = &[
    Entry {
        language: "en",
        sentence: "The North Wind and the Sun were disputing which was the stronger, when a \
                   traveler came along wrapped in a warm cloak. They agreed that the one who \
                   first succeeded in making the traveler take his cloak off should be \
                   considered stronger than the other.",
        check_line: "Hello! This is a short test of the new voice. Does it still sound like \
                     the sample, even in a question?",
    },
    Entry {
        language: "de",
        sentence: "Einst stritten sich Nordwind und Sonne, wer von ihnen beiden wohl der \
                   Stärkere wäre, als ein Wanderer, der in einen warmen Mantel gehüllt war, \
                   des Weges daherkam. Sie wurden einig, dass derjenige für den Stärkeren \
                   gelten sollte, der den Wanderer zwingen würde, seinen Mantel abzunehmen.",
        check_line: "Hallo! Das ist ein kurzer Test der neuen Stimme. Klingt sie noch wie die \
                     Vorlage, auch bei einer Frage?",
    },
];

/// The owner's sentences, in the lab's directory.
const SAVED_FILE: &str = "sentences.json";

/// A language tag as the table keys it: trimmed, lowercase, `_` read as
/// `-` (`de`, `pt-br`). Refused when it is not a tag at all.
pub fn normalize(language: &str) -> Result<String, String> {
    let tag = language.trim().to_ascii_lowercase().replace('_', "-");
    let mut parts = tag.split('-');
    let primary = parts.next().unwrap_or_default();
    let primary_ok =
        (2..=3).contains(&primary.len()) && primary.chars().all(|c| c.is_ascii_lowercase());
    let rest_ok =
        parts.all(|p| (1..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()));
    if primary_ok && rest_ok {
        Ok(tag)
    } else {
        Err(format!(
            "language '{}' is not a language code — use an ISO 639-1 code such as en or de",
            language.trim()
        ))
    }
}

/// The built-in entry for `language` (normalized): its own, else its
/// primary subtag's (`de-at` reads `de`).
pub fn builtin(language: &str) -> Option<&'static Entry> {
    let primary = language.split('-').next().unwrap_or_default();
    TABLE
        .iter()
        .find(|e| e.language == language)
        .or_else(|| TABLE.iter().find(|e| e.language == primary))
}

/// The owner's saved sentences in `dir`, by language: none when there is
/// no file. A file that is there and cannot be read is `Err` naming it: it
/// is the owner's own, edited by hand perhaps, so it is never written over
/// ([`save`] refuses), and a reader shows the built-in sentences and says
/// why ([`table`]'s `error`).
pub fn read_saved(dir: &Path) -> Result<BTreeMap<String, String>, String> {
    super::read_own_json(&dir.join(SAVED_FILE)).map(Option::unwrap_or_default)
}

/// [`read_saved`] for a reader that goes on with the built-in sentences:
/// the error is logged.
fn saved_or_logged(dir: &Path) -> BTreeMap<String, String> {
    read_saved(dir).unwrap_or_else(|why| {
        tracing::warn!("voice lab: the owner's sentences are not used: {why}");
        BTreeMap::new()
    })
}

/// Save the owner's sentence for `language` in `dir` (an empty `text`
/// drops it, so the built-in one is the prefill again). Returns the
/// normalized language. Refused, writing nothing, when the file there
/// cannot be read ([`read_saved`]). The caller holds
/// [`super::library_write`].
pub fn save(dir: &Path, language: &str, text: &str) -> Result<String, LabFail> {
    let language = normalize(language).map_err(LabFail::bad)?;
    let mut saved = read_saved(dir).map_err(super::unreadable)?;
    let text = text.trim();
    if text.is_empty() {
        saved.remove(&language);
    } else {
        saved.insert(language.clone(), text.to_string());
    }
    let body = serde_json::to_vec_pretty(&saved).map_err(|e| LabFail::internal(e.to_string()))?;
    std::fs::create_dir_all(dir)
        .and_then(|()| super::write_atomic(&dir.join(SAVED_FILE), &body))
        .map_err(|e| LabFail::internal(format!("writing {SAVED_FILE}: {e}")))?;
    Ok(language)
}

/// The sentence in effect for `language`: the owner's, else the built-in
/// one; `None` when there is neither.
pub fn sentence_for(dir: &Path, language: &str) -> Option<String> {
    let language = normalize(language).ok()?;
    saved_or_logged(dir)
        .remove(&language)
        .or_else(|| builtin(&language).map(|e| e.sentence.to_string()))
}

/// The cloning check's line for `language`, when the table has one.
pub fn check_line_for(language: &str) -> Option<&'static str> {
    let language = normalize(language).ok()?;
    builtin(&language).map(|e| e.check_line)
}

/// Every language with a built-in or a saved sentence: the built-in ones in
/// table order, then the others the owner saved, by code. A saved file that
/// cannot be read leaves the built-in ones, and `error` says why.
pub fn table(dir: &Path) -> VoiceSentences {
    let (mut saved, error) = match read_saved(dir) {
        Ok(s) => (s, None),
        Err(why) => {
            tracing::warn!("voice lab: the owner's sentences are not shown: {why}");
            (BTreeMap::new(), Some(why))
        }
    };
    let mut languages: Vec<VoiceSentence> = TABLE
        .iter()
        .map(|e| {
            let own = saved.remove(e.language);
            VoiceSentence {
                language: e.language.to_string(),
                name: crate::audio::language::display_name(e.language),
                saved: own.is_some(),
                text: own.unwrap_or_else(|| e.sentence.to_string()),
                builtin: Some(e.sentence.to_string()),
                check_line: e.check_line.to_string(),
            }
        })
        .collect();
    for (language, text) in saved {
        let entry = builtin(&language);
        languages.push(VoiceSentence {
            name: crate::audio::language::display_name(&language),
            builtin: entry.map(|e| e.sentence.to_string()),
            check_line: entry.map(|e| e.check_line.to_string()).unwrap_or_default(),
            language,
            text,
            saved: true,
        });
    }
    VoiceSentences { languages, error }
}
