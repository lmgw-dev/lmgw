//! [`PlanFiles`] from Hugging Face (find-a-model design §2.3): the tree
//! listing's sizes and names, and the remote headers of the weights (every
//! part of a split file, D131), the picked projector (D121) and the first
//! drafter, read before anything is downloaded ([`crate::hf::remote_header`],
//! cached by content hash for the life of the process, D4, D43).
//!
//! The paths are where `hf_add` puts the files (`<owner>/<name>/<file>`), so
//! the plan's `gguf_path`, `mmproj_path` and `draft_gguf_path` are what the
//! same plan says once the files are on disk.

use crate::hf::{self, HfFile, ReadError};
use crate::runtime::Class;
use crate::state::SharedState;

use super::{later_parts, parent_of, set_mismatch, Companion, PlanFiles, Source, Whole};

/// What one header read gave the plan, and what it cost: nothing for a
/// header the process had read before (D4), which this plan didn't fetch.
struct Read {
    summary: crate::gguf::ModelSummary,
    received: u64,
    secs: f64,
    oid: Option<String>,
    commit: Option<String>,
}

async fn read(state: &SharedState, repo: &str, entry: &HfFile) -> Result<Read, ReadError> {
    let (h, fetched) = hf::remote::remote_header_fetched(state, repo, entry).await?;
    Ok(Read {
        summary: (*h.summary).clone(),
        received: if fetched { h.received_bytes } else { 0 },
        secs: if fetched { h.read_secs } else { 0.0 },
        oid: h.oid.clone(),
        commit: h.commit.clone(),
    })
}

/// The listing's entry for `path`.
fn entry<'a>(listing: &'a [HfFile], path: &str) -> Option<&'a HfFile> {
    listing.iter().find(|f| f.path == path)
}

/// What the listing says of a file's bytes (every part of a split file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    /// The bytes of the parts the listing has with a size.
    pub present: u64,
    /// Every part's bytes, when the listing has every part with its size;
    /// `None` when a part is missing or sizeless, so the bytes are unknown.
    pub bytes: Option<u64>,
    /// The sentence for parts the listing doesn't have, when some are
    /// missing.
    pub missing: Option<String>,
}

/// The listing's bytes of `file` ([`Listed`]).
pub(crate) fn listed_bytes(listing: &[HfFile], file: &str) -> Listed {
    let Some((prefix, _, total)) = hf::split_part_name(file).filter(|(_, _, t)| *t > 0) else {
        let size = entry(listing, file).and_then(|f| f.size);
        return Listed {
            present: size.unwrap_or(0),
            bytes: size,
            missing: None,
        };
    };
    let mut present = 0u64;
    let mut sizeless = false;
    let mut missing: Vec<u32> = Vec::new();
    for i in 1..=total {
        let part = format!("{prefix}-{i:05}-of-{total:05}.gguf");
        match entry(listing, &part) {
            Some(f) => match f.size {
                Some(b) => present = present.saturating_add(b),
                None => sizeless = true,
            },
            None => missing.push(i),
        }
    }
    if missing.is_empty() {
        return Listed {
            present,
            bytes: (!sizeless).then_some(present),
            missing: None,
        };
    }
    let numbers: Vec<String> = missing.iter().map(u32::to_string).collect();
    Listed {
        present,
        bytes: None,
        missing: Some(format!(
            "'{file}' is one part of a GGUF split into {total} files, and the repo's file list \
             has no part {}: llama-server cannot load the model without every part, so only \
             the parts listed are counted ({})",
            numbers.join(", "),
            hf::fmt_bytes(present)
        )),
    }
}

/// The plan's input for `file` in `repo` at `main`, from its tree listing
/// `listing` and the remote headers (module doc), for the class `target`
/// (where `hf_add` with that target puts it), else the header's. The error
/// is the weights' header read's, a companion's or a later part's that the
/// hub refused as it would every read ([`ReadError::hub`]), or why the file
/// can't be planned; it counts every byte received up to it.
///
/// A split file's later parts are read only when the listing has every part
/// and the first part's header declares as many as its name says: else the
/// set can't load as it is, the fit is by its size alone with the reason,
/// and nothing more is read.
pub async fn from_hub(
    state: &SharedState,
    repo: &str,
    file: &str,
    listing: &[HfFile],
    target: Option<Class>,
) -> Result<PlanFiles, ReadError> {
    let rel = hf::dest_rel_path(repo, file).map_err(ReadError::file)?;
    let weights = entry(listing, file).ok_or_else(|| {
        ReadError::file(format!(
            "{file} is not in {repo}'s file list at main — lmgw__hf_repo lists what is"
        ))
    })?;
    if !file.to_ascii_lowercase().ends_with(".gguf") {
        return Err(ReadError::file(format!(
            "{file} is not a GGUF: only a GGUF has a header the plan can read"
        )));
    }
    let head = read(state, repo, weights).await?;
    let listed = listed_bytes(listing, file);
    let mut received = head.received;
    // A refusal every read would meet ends the plan, with what came before
    // it counted; a file's own failure is noted and the plan goes on.
    let stop = |e: ReadError, received: u64| ReadError {
        received: received.saturating_add(e.received),
        ..e
    };
    let mut secs = head.secs;
    let mut notes = Vec::new();

    // The later parts of a split file, for the whole count and width, when
    // they are the set the first declares (doc).
    let mismatch = set_mismatch(&head.summary, file);
    notes.extend(mismatch.clone());
    let mut parts = Vec::new();
    if mismatch.is_none() && listed.missing.is_none() {
        for part in later_parts(file) {
            let r = match entry(listing, &part) {
                Some(e) => read(state, repo, e).await,
                None => Err(ReadError::file(format!(
                    "{part} is not in the repo's file list"
                ))),
            };
            match &r {
                Ok(r) => {
                    received = received.saturating_add(r.received);
                    secs += r.secs;
                }
                Err(e) if e.hub => return Err(stop(e.clone(), received)),
                Err(e) => received = received.saturating_add(e.received),
            }
            parts.push(r.map(|r| r.summary).map_err(String::from));
        }
    } else {
        parts.push(Err(String::new()));
    }
    let whole = Whole::of(&head.summary, &parts);
    let summary = super::set_summary(head.summary, &parts);
    let found_in = target.unwrap_or_else(|| match super::super::class_for(&summary).0 {
        "aux" => Class::Aux,
        _ => Class::Chat,
    });

    // The siblings by name: the parts of this file aren't its companions.
    let parent = parent_of(file);
    let siblings: Vec<&HfFile> = listing
        .iter()
        .filter(|f| f.path != file && parent_of(&f.path) == parent)
        .filter(|f| !hf::same_split_set(&f.path, file))
        .collect();
    let named = |role: &str| -> Vec<&HfFile> {
        let mut v: Vec<&HfFile> = siblings
            .iter()
            .copied()
            .filter(|f| crate::ops::classify_repo_file(&f.path, "chat") == role)
            .collect();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        v
    };
    let projectors = named("mmproj");
    let pick = hf::pick_projector(
        projectors
            .iter()
            .map(|f| (f.path.as_str(), f.size.unwrap_or(u64::MAX))),
    )
    .map(str::to_string);
    let wanted: [(Option<HfFile>, &str); 2] = [
        (
            projectors
                .iter()
                .find(|f| Some(&f.path) == pick.as_ref())
                .map(|f| (*f).clone()),
            "mmproj",
        ),
        (named("drafter").first().map(|f| (*f).clone()), "drafter"),
    ];
    let mut companions: Vec<Option<Companion>> = Vec::new();
    for (found, role) in wanted {
        let Some(found) = found else {
            companions.push(None);
            continue;
        };
        match read(state, repo, &found).await {
            Ok(r) if super::super::role_of(&r.summary) == role => {
                received = received.saturating_add(r.received);
                secs += r.secs;
                let listed = listed_bytes(listing, &found.path);
                companions.push(Some(Companion {
                    path: hf::dest_rel_path(repo, &found.path).map_err(ReadError::file)?,
                    bytes: listed.present,
                    missing: listed.missing,
                    summary: r.summary,
                }));
            }
            Ok(r) => {
                received = received.saturating_add(r.received);
                secs += r.secs;
                notes.push(format!(
                    "{} is named like a {} but its header reads as a '{}', so the plan names \
                     no {} from Hugging Face's listing",
                    found.path,
                    role_word(role),
                    super::super::role_of(&r.summary),
                    role_word(role)
                ));
                companions.push(None);
            }
            Err(e) if e.hub => return Err(stop(e, received)),
            Err(e) => {
                received = received.saturating_add(e.received);
                notes.push(format!(
                    "the header of the {} {} could not be read from Hugging Face ({e}), so the \
                     plan names none; plan again once it can be read, or after the download",
                    role_word(role),
                    found.path
                ));
                companions.push(None);
            }
        }
    }
    let drafter = companions.pop().flatten();
    let projector = companions.pop().flatten();

    // The location rule's look into the aux dir, as the disk plan makes it
    // once the file is where `hf_add` puts it.
    let aux_dir = super::super::models_dir_of(&state.snapshot(), Class::Aux);
    let in_aux_dir = {
        let rel = rel.clone();
        tokio::task::spawn_blocking(move || super::in_aux_dir(found_in, &aux_dir, &rel))
            .await
            .unwrap_or(false)
    };
    Ok(PlanFiles {
        rel,
        summary,
        weights_bytes: listed.present,
        size_only: mismatch.or_else(|| listed.missing.clone()),
        weights_missing: listed.missing,
        listed_bytes: listed.bytes,
        projector,
        drafter,
        parameters: whole.parameters,
        layer_bits: whole.layer_bits,
        layer_bits_estimated: whole.estimated,
        layer_elements: whole.layer_elements,
        host_input_elements: whole.host_input_elements,
        overhead_bytes: whole.overhead_bytes,
        notes,
        found_in,
        in_aux_dir,
        source: Source::Hub {
            repo: repo.to_string(),
            file: file.to_string(),
            oid: head.oid.or_else(|| weights.oid().map(str::to_string)),
            commit: head.commit,
            header_bytes_read: received,
            read_secs: secs,
        },
    })
}

fn role_word(role: &str) -> &'static str {
    match role {
        "mmproj" => "projector",
        _ => "drafter",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(path: &str, size: u64) -> HfFile {
        HfFile {
            path: path.into(),
            size: Some(size),
            kind: "file".into(),
            lfs: None,
        }
    }

    #[test]
    fn a_split_files_bytes_are_its_listed_parts() {
        let listing = [
            f("q/m-00001-of-00002.gguf", 100),
            f("q/m-00002-of-00002.gguf", 50),
            f("q/single.gguf", 7),
        ];
        let whole = listed_bytes(&listing, "q/m-00001-of-00002.gguf");
        assert_eq!(
            (whole.present, whole.bytes, whole.missing),
            (150, Some(150), None)
        );
        assert_eq!(listed_bytes(&listing, "q/single.gguf").bytes, Some(7));
        let short = listed_bytes(&listing[..1], "q/m-00001-of-00002.gguf");
        assert_eq!(
            (short.present, short.bytes),
            (100, None),
            "unknown, not 100"
        );
        let why = short.missing.unwrap();
        assert!(
            why.contains("has no part 2") && why.contains("cannot load"),
            "{why}"
        );
        // Not listed, or listed without a size: unknown.
        assert_eq!(listed_bytes(&listing, "q/other.gguf").bytes, None);
        let mut sizeless = f("q/s.gguf", 0);
        sizeless.size = None;
        assert_eq!(listed_bytes(&[sizeless], "q/s.gguf").bytes, None);
    }
}
