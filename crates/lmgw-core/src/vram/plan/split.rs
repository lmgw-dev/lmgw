//! What a split GGUF (`…-00001-of-0000N.gguf`) weighs on disk.
//!
//! A row names one part, the first, and llama-server opens the others from
//! it, so the model it loads is every part. The download already fetches
//! the whole set ([`crate::hf::expand_parts`]); the estimate counts the
//! whole set too, by the same naming rule ([`crate::hf::split_part_name`]).
//! Before this, a split row was charged its first part alone: a Qwen BF16 in
//! three parts was about 3.8 GB under (find-a-model design review, finding
//! F1).
//!
//! A part that is not on disk is not counted as 0 in silence: it is an
//! error naming the part, and llama-server cannot load the model until it
//! is there. Callers that still need a figure take the parts on disk
//! ([`MissingParts::present_bytes`]) and surface the message beside it.

use std::path::Path;

use super::file_size;

/// A split GGUF with parts missing from the models dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MissingParts {
    /// What the parts on disk add up to.
    pub present_bytes: u64,
    /// A plain sentence naming the missing parts.
    pub message: String,
}

/// The bytes on disk of the GGUF at `rel` under `root`: every part's for a
/// split GGUF, the file's own otherwise (0 when it is absent, which the
/// header read then reports as unreadable). An error when a part of a split
/// set is missing.
pub(crate) fn gguf_bytes(root: &Path, rel: &str) -> Result<u64, MissingParts> {
    let rel = rel.trim_start_matches('/');
    let Some((prefix, _, total)) = crate::hf::split_part_name(rel).filter(|(_, _, t)| *t > 0)
    else {
        return Ok(file_size(&root.join(rel)));
    };
    let mut present = 0u64;
    let mut on_disk = 0u32;
    let mut missing: Vec<(u32, String)> = Vec::new();
    for i in 1..=total {
        let part = format!("{prefix}-{i:05}-of-{total:05}.gguf");
        match std::fs::metadata(root.join(&part)) {
            Ok(m) if m.is_file() => {
                present = present.saturating_add(m.len());
                on_disk += 1;
            }
            _ => missing.push((i, part)),
        }
    }
    if missing.is_empty() {
        return Ok(present);
    }
    let numbers: Vec<String> = missing.iter().map(|(i, _)| i.to_string()).collect();
    let names: Vec<String> = missing.iter().map(|(_, p)| format!("'{p}'")).collect();
    let which = match missing.len() {
        1 => format!("part {} ({}) is", numbers[0], names[0]),
        _ => format!("parts {} ({}) are", numbers.join(", "), names.join(", ")),
    };
    Err(MissingParts {
        present_bytes: present,
        message: format!(
            "'{rel}' is one part of a GGUF split into {total} files, and {which} not in the \
             models dir: llama-server cannot load the model until every part is there, so \
             only the {on_disk} part(s) on disk are counted ({})",
            crate::hf::fmt_bytes(present)
        ),
    })
}

/// Blocking: the header of the GGUF at `rel` under `root`, a split set's as
/// the whole set's (find-a-model design D3, owner ruling 2026-11-04): part
/// 1's, with every later part's tensors taken in when every part is on disk
/// and part 1 declares the count its name says, the rule the plan reads
/// them by ([`crate::modelinfo::plan_files::set_summary`]). Else, and for a
/// file that isn't split, the file's own header. A later part's header is
/// its descriptors alone, kilobytes.
pub(crate) fn summarize(root: &Path, rel: &str) -> Result<crate::gguf::ModelSummary, String> {
    let rel = rel.trim_start_matches('/');
    let first = crate::gguf::summarize(&root.join(rel)).map_err(|e| e.to_string())?;
    Ok(whole_set(root, rel, first))
}

/// Blocking: `first`, the header of the GGUF at `rel` under `root`, as the
/// whole set's ([`summarize`], which reads `first` itself).
pub(crate) fn whole_set(
    root: &Path,
    rel: &str,
    first: crate::gguf::ModelSummary,
) -> crate::gguf::ModelSummary {
    use crate::modelinfo::plan_files::{later_parts, set_mismatch, set_summary};
    let rel = rel.trim_start_matches('/');
    let later = later_parts(rel);
    if later.is_empty() || set_mismatch(&first, rel).is_some() || gguf_bytes(root, rel).is_err() {
        return first;
    }
    let parts: Vec<Result<crate::gguf::ModelSummary, String>> = later
        .iter()
        .map(|p| crate::gguf::summarize(&root.join(p)).map_err(|e| e.to_string()))
        .collect();
    set_summary(first, &parts)
}

/// [`gguf_bytes`], with a missing part's message pushed onto `missing` and
/// the parts on disk returned in its place.
pub(crate) fn charged(root: &Path, rel: &str, missing: &mut Vec<String>) -> u64 {
    gguf_bytes(root, rel).unwrap_or_else(|m| {
        missing.push(m.message);
        m.present_bytes
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, len: usize) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![0u8; len]).unwrap();
    }

    #[test]
    fn a_split_gguf_weighs_every_part() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "q/m-00001-of-00003.gguf", 100);
        write(dir.path(), "q/m-00002-of-00003.gguf", 200);
        write(dir.path(), "q/m-00003-of-00003.gguf", 300);
        assert_eq!(gguf_bytes(dir.path(), "q/m-00001-of-00003.gguf"), Ok(600));
        // Whichever part is named, the set is the same.
        assert_eq!(gguf_bytes(dir.path(), "/q/m-00002-of-00003.gguf"), Ok(600));
    }

    #[test]
    fn a_missing_part_is_an_error_that_names_it() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "q/m-00001-of-00003.gguf", 100);
        write(dir.path(), "q/m-00003-of-00003.gguf", 300);
        let err = gguf_bytes(dir.path(), "q/m-00001-of-00003.gguf").unwrap_err();
        assert_eq!(err.present_bytes, 400);
        assert!(
            err.message
                .contains("part 2 ('q/m-00002-of-00003.gguf') is not in the models dir")
                && err.message.contains("cannot load the model")
                && err
                    .message
                    .contains("only the 2 part(s) on disk are counted"),
            "{}",
            err.message
        );

        let mut missing = Vec::new();
        assert_eq!(
            charged(dir.path(), "q/m-00001-of-00003.gguf", &mut missing),
            400
        );
        assert_eq!(missing, vec![err.message]);

        std::fs::remove_file(dir.path().join("q/m-00003-of-00003.gguf")).unwrap();
        let err = gguf_bytes(dir.path(), "q/m-00001-of-00003.gguf").unwrap_err();
        assert!(err.message.contains("parts 2, 3 ("), "{}", err.message);
    }

    #[test]
    fn a_single_file_is_its_own_size() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "q/m.gguf", 123);
        // Named like a part, but not one (no five-digit counter).
        write(dir.path(), "q/m-1-of-3.gguf", 7);
        let mut missing = Vec::new();
        assert_eq!(charged(dir.path(), "q/m.gguf", &mut missing), 123);
        assert_eq!(gguf_bytes(dir.path(), "q/m-1-of-3.gguf"), Ok(7));
        // An absent single file is 0 here; the header read says why.
        assert_eq!(gguf_bytes(dir.path(), "q/gone.gguf"), Ok(0));
        assert!(missing.is_empty());
    }
}
