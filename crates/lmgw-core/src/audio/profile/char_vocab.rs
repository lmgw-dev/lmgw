//! The characters a row's engine says, for a family that refuses one its
//! package lacks ([`families::char_vocabulary`]). The file is the one the
//! package's own embedded spec names for it (Supertonic 3: `unicode_indexer`
//! → `model:config/unicode_indexer.json`), read from the files the package
//! GGUF embeds, else from beside the GGUF — a `model:` source is a path in
//! the package. A row without a package GGUF, and a package without the
//! file, is a problem of the profile's (logged once per computation): its
//! `input` then goes as it came, and the engine refuses what it lacks. Text
//! the engine writes itself that the vocabulary lacks
//! ([`CharVocab::engine_misses`]) is only a note: the vocabulary is used.

use std::path::{Component, Path};
use std::sync::Arc;

use serde_json::Value;

use super::SpeechProfile;
use crate::audio::charset::CharVocab;
use crate::audio::families;
use crate::gguf::embedded::{EmbeddedIndex, MAX_EMBEDDED_FILE};

/// Fill `p.char_vocab` from the package: `spec` is the spec its GGUF
/// embeds, `index` that GGUF's embedded files.
pub(super) fn read(
    p: &mut SpeechProfile,
    spec: Option<&Value>,
    index: Option<&EmbeddedIndex>,
    gguf: Option<&Path>,
) {
    let Some(fact) = families::char_vocabulary(&p.family) else {
        return;
    };
    let unread = |why: String| {
        format!(
            "{}: {why}, so input characters the engine lacks are sent as they came",
            fact.resource
        )
    };
    // A package whose GGUF could not be read is a problem of its own:
    // nothing of it is known, this included.
    let Some(gguf) = gguf else {
        p.problems.push(unread(
            "the row has no package GGUF, which lmgw reads the vocabulary from".into(),
        ));
        return;
    };
    let Some(index) = index else {
        return;
    };
    let Some(rel) = spec.and_then(|s| source(s, fact.resource)) else {
        p.problems
            .push(unread("the package's spec names no such file".into()));
        return;
    };
    let raw = match package_file(p, index, gguf, &rel) {
        Ok(Some(raw)) => raw,
        Ok(None) => {
            p.problems.push(unread(format!("the package has no {rel}")));
            return;
        }
        Err(e) => {
            p.problems.push(unread(e));
            return;
        }
    };
    match CharVocab::from_indexer(&rel, &raw, &fact) {
        Ok(v) => {
            p.sources.push(format!(
                "the package's {rel} ({} characters the engine says)",
                v.len()
            ));
            let misses = v.engine_misses(&fact, &p.spec_languages);
            p.notes.extend(
                misses
                    .into_iter()
                    .map(|m| format!("{}: {m}", fact.resource)),
            );
            p.char_vocab = Some(Arc::new(v));
        }
        Err(e) => p.problems.push(unread(e)),
    }
}

/// The package path the spec's sources give `resource` (`files` or
/// `optional_files`, `model:<path>`), normalised as audio.cpp normalises an
/// embedded name; `None` when no source names it, or names something other
/// than a relative path in the package.
fn source(spec: &Value, resource: &str) -> Option<String> {
    let sources = spec.get("sources")?.as_array()?;
    let named = sources.iter().find_map(|src| {
        ["files", "optional_files"]
            .iter()
            .find_map(|g| src.get(g)?.get(resource)?.as_str())
    })?;
    let rel = Path::new(named.strip_prefix("model:")?);
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(n) => parts.push(n.to_str()?),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The package file `rel`: embedded in the GGUF, else beside it — a path
/// the profile then notes in `p.beside`, so the cache sees a change to it.
/// `Err` for one lmgw does not read (over [`MAX_EMBEDDED_FILE`], or
/// unreadable).
fn package_file(
    p: &mut SpeechProfile,
    index: &EmbeddedIndex,
    gguf: &Path,
    rel: &str,
) -> Result<Option<Vec<u8>>, String> {
    if let Some(raw) = index.file(rel).map_err(|e| e.to_string())? {
        return Ok(Some(raw));
    }
    let Some(path) = gguf.parent().map(|d| d.join(rel)) else {
        return Ok(None);
    };
    p.note_beside(path.clone());
    let Ok(meta) = std::fs::metadata(&path) else {
        return Ok(None);
    };
    if meta.len() > MAX_EMBEDDED_FILE {
        return Err(format!(
            "{}: {} bytes, more than the {MAX_EMBEDDED_FILE} lmgw reads for a profile",
            path.display(),
            meta.len()
        ));
    }
    std::fs::read(&path)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests;
