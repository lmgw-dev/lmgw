//! Which catalog package an audio row actually loads.
//!
//! A row gives audio.cpp a model **directory** plus an optional `weight_id`
//! (`runtime/audio.rs`) — or the one GGUF of several there that `weight_id`
//! picks, which audio.cpp needs (`audio::files::direct`) — and one directory
//! often holds several packages of a family — `Parakeet-TDT-0.6B-v3-GGUF/` carries both the q8_0 and the f16
//! file. Matching a row to a package by that directory marked every package
//! there as serving, downloaded or not. The weights file is what tells them
//! apart, and lmgw has one rule for which file a row loads:
//! [`files::row_ggufs`] (the GGUFs under the root, narrowed by `weight_id`).
//! A package serves when it is the only one that holds what that rule picks;
//! when the pick spans packages, or several ship the same file, the row's
//! variant is unclear and the catalog says so instead of guessing.
//!
//! Only enabled rows count — a disabled row is not served from anywhere.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::audio::{files, ModelSpec, SpecPackage};
use crate::config::AudioModel;

/// One enabled audio row and the weights it would load, read from disk.
#[derive(Debug, Clone, Default)]
pub(crate) struct RowWeights {
    pub model_id: String,
    pub family: String,
    /// The row's model root relative to the models dir, no `/` at either end.
    pub path: String,
    pub weight_id: Option<String>,
    /// What [`files::row_ggufs`] picks from under that root, relative to the
    /// models dir (the layout `hf::dest_rel_path` downloads into).
    pub candidates: Vec<String>,
    /// Any GGUF under the root at all, before `weight_id` narrowed it: a
    /// `weight_id` that matches nothing marks nothing, while a root with no
    /// GGUF is what the directory fallback is for.
    pub any_gguf: bool,
}

/// The weights of every **enabled** row. Blocking: walks each row's root.
pub(crate) fn gather(models_dir: &str, rows: &[AudioModel]) -> Vec<RowWeights> {
    let base = Path::new(models_dir);
    rows.iter()
        .filter(|m| m.enabled)
        .map(|m| {
            let root = files::row_root(models_dir, m);
            let rel = |p: &Path| p.strip_prefix(base).unwrap_or(p).display().to_string();
            let mut candidates: Vec<String> =
                files::row_ggufs(base, &root, m.weight_id.as_deref(), &m.family)
                    .iter()
                    .map(|(p, _)| rel(p))
                    .collect();
            candidates.sort();
            let any_gguf = !candidates.is_empty()
                || !files::row_ggufs(base, &root, None, &m.family).is_empty();
            RowWeights {
                model_id: m.model_id.clone(),
                family: m.family.clone(),
                path: m.path.trim_matches('/').to_string(),
                weight_id: m.weight_id.clone(),
                candidates,
                any_gguf,
            }
        })
        .collect()
}

/// What the rows say of one package.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Serving {
    /// The rows that load this package's weights.
    pub served_by: Vec<String>,
    /// One sentence per row that points here without saying which package.
    pub unclear: Vec<String>,
}

/// What the rows say of one family: per package, and the rows that load
/// nothing of it at all.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct FamilyServing {
    pub packages: HashMap<String, Serving>,
    /// One sentence per enabled row of the family that matches no package:
    /// an empty or missing root, a `weight_id` that picks no file, GGUFs no
    /// package ships. Without it such a row was only visible as a family
    /// "serving" chip with nothing underneath to explain it.
    pub unmatched: Vec<String>,
}

/// A package whose weights are GGUF — the files [`files::row_ggufs`] sees.
fn is_gguf(pkg: &SpecPackage) -> bool {
    pkg.files
        .iter()
        .any(|f| f.to_ascii_lowercase().ends_with(".gguf"))
}

fn name(pkg: &SpecPackage) -> &str {
    match pkg.display_name.is_empty() {
        true => &pkg.id,
        false => &pkg.display_name,
    }
}

/// Whether `file` is one of a package's `dests`. A split GGUF counts by its
/// whole set: a spec may list one shard, and the download fetches every
/// sibling (`hf::expand_parts`), which then sit under the root too.
fn ships(dests: &[String], file: &str) -> bool {
    dests.iter().any(|d| crate::hf::same_split_set(d, file))
}

/// The file names of `paths`, for a sentence.
fn file_names(paths: &[&String]) -> String {
    paths
        .iter()
        .map(|p| p.rsplit('/').next().unwrap_or(p))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The rows of `spec`'s family against its packages, by package id.
/// `installed` are the ids of packages with every file on disk (only those
/// can be found by directory).
pub(crate) fn family_serving(
    spec: &ModelSpec,
    rows: &[RowWeights],
    installed: &HashSet<String>,
) -> FamilyServing {
    // Each downloadable package with its files where the download put them.
    let packages: Vec<(&SpecPackage, Vec<String>)> = spec
        .packages
        .iter()
        .filter_map(|p| {
            let repo = spec.package_repo(p)?;
            let dests = p
                .files
                .iter()
                .filter_map(|f| crate::hf::dest_rel_path(repo, f).ok())
                .collect();
            Some((p, dests))
        })
        .collect();
    let mut out = FamilyServing::default();
    for r in rows.iter().filter(|r| r.family == spec.family) {
        if !r.candidates.is_empty() {
            let gguf = || packages.iter().filter(|(p, _)| is_gguf(p));
            // What the row picks up that no package of this family ships: a
            // leftover from an older spec, another model's file. lmgw never
            // deletes those, and they are what makes the pick ambiguous.
            let stray: Vec<&String> = r
                .candidates
                .iter()
                .filter(|c| !gguf().any(|(_, d)| ships(d, c)))
                .collect();
            let whole: Vec<&SpecPackage> = gguf()
                .filter(|(_, d)| r.candidates.iter().all(|c| ships(d, c)))
                .map(|(p, _)| *p)
                .collect();
            if let [only] = whole.as_slice() {
                out.packages
                    .entry(only.id.clone())
                    .or_default()
                    .served_by
                    .push(r.model_id.clone());
                continue;
            }
            let owners: Vec<&SpecPackage> = gguf()
                .filter(|(_, d)| r.candidates.iter().any(|c| ships(d, c)))
                .map(|(p, _)| *p)
                .collect();
            if owners.is_empty() {
                out.unmatched.push(format!(
                    "audio model '{}' loads {} under {}, which no package of this family ships",
                    r.model_id,
                    file_names(&stray),
                    r.path
                ));
                continue;
            }
            let names = owners
                .iter()
                .map(|p| name(p))
                .collect::<Vec<_>>()
                .join(" / ");
            for p in &owners {
                let sentence = match (stray.is_empty(), whole.is_empty(), r.weight_id.as_deref()) {
                    // The pick reaches past the packages: name what is
                    // extra, which is what has to go (or be left out).
                    (false, _, w) => format!(
                        "audio model '{}' points at {}{} and also picks up {}, which no package \
                         of this family ships — remove {} or set a weight_id that leaves {} out",
                        r.model_id,
                        r.path,
                        w.map(|w| format!(" with weight_id '{w}'"))
                            .unwrap_or_default(),
                        file_names(&stray),
                        if stray.len() == 1 { "it" } else { "them" },
                        if stray.len() == 1 { "it" } else { "them" },
                    ),
                    // Several packages ship the very file the row loads: no
                    // weight_id separates them.
                    (true, false, _) => format!(
                        "audio model '{}' loads a GGUF under {} that {names} all ship — which of \
                         them it serves cannot be told from the files",
                        r.model_id, r.path
                    ),
                    (true, true, None) => format!(
                        "audio model '{}' points at {} with no weight_id that tells {names} \
                         apart — set weight_id{}",
                        r.model_id,
                        r.path,
                        example(p)
                    ),
                    (true, true, Some(w)) => format!(
                        "audio model '{}' points at {} and its weight_id '{w}' does not tell \
                         {names} apart — set one that does{}",
                        r.model_id,
                        r.path,
                        example(p)
                    ),
                };
                out.packages
                    .entry(p.id.clone())
                    .or_default()
                    .unclear
                    .push(sentence);
            }
        } else if r.any_gguf {
            // GGUFs under the root, none of them picked.
            out.unmatched.push(format!(
                "audio model '{}' has weight_id '{}', which matches no GGUF under {}",
                r.model_id,
                r.weight_id.as_deref().unwrap_or_default(),
                r.path
            ));
        } else {
            // No GGUF under the root at all (a safetensors family): the
            // directory is all there is to go by. An installed package with
            // every file under the row's root is what it loads, if it is the
            // only one there.
            let prefix = match r.path.is_empty() {
                true => String::new(),
                false => format!("{}/", r.path),
            };
            let there: Vec<&SpecPackage> = packages
                .iter()
                .filter(|(p, d)| {
                    !is_gguf(p)
                        && installed.contains(&p.id)
                        && !d.is_empty()
                        && d.iter().all(|f| f.starts_with(&prefix))
                })
                .map(|(p, _)| *p)
                .collect();
            match there.as_slice() {
                [] => out.unmatched.push(format!(
                    "audio model '{}' points at {}, where no downloaded package of this family \
                     is (an empty or missing directory loads nothing)",
                    r.model_id, r.path
                )),
                [only] => out
                    .packages
                    .entry(only.id.clone())
                    .or_default()
                    .served_by
                    .push(r.model_id.clone()),
                several => {
                    let names = several
                        .iter()
                        .map(|p| name(p))
                        .collect::<Vec<_>>()
                        .join(" / ");
                    for p in several {
                        out.packages
                            .entry(p.id.clone())
                            .or_default()
                            .unclear
                            .push(format!(
                            "audio model '{}' points at {}, which holds {names} — point it at one \
                             package's directory",
                            r.model_id, r.path
                        ));
                    }
                }
            }
        }
    }
    out
}

/// ` (e.g. q8_0)` from the package's precision, the usual part of its file
/// name a `weight_id` matches; nothing when the spec names none.
fn example(p: &SpecPackage) -> String {
    match p.precision.is_empty() {
        true => String::new(),
        false => format!(" (e.g. {})", p.precision),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DIR: &str = "audio-cpp/audio.cpp-gguf/Parakeet-TDT-0.6B-v3-GGUF";

    /// Two GGUF packages in one directory (today's Parakeet), plus a
    /// safetensors pair for the directory fallback.
    fn spec() -> ModelSpec {
        crate::audio::parse_spec(&json!({
            "family": "parakeet_tdt",
            "display_name": "Parakeet TDT",
            "package_defaults": { "download": { "kind": "huggingface_snapshot", "repo": "audio-cpp/audio.cpp-gguf" } },
            "packages": [
                {
                    "id": "parakeet_q8_0", "display_name": "q8_0", "format": "gguf", "precision": "q8_0",
                    "files": ["Parakeet-TDT-0.6B-v3-GGUF/parakeet-tdt-0.6b-v3-q8_0.gguf"],
                },
                {
                    "id": "parakeet_f16", "display_name": "f16", "format": "gguf", "precision": "f16",
                    "files": ["Parakeet-TDT-0.6B-v3-GGUF/parakeet-tdt-0.6b-v3-f16.gguf"],
                },
                {
                    "id": "parakeet_st_a", "display_name": "st a", "format": "safetensors",
                    "files": ["st/a/model.safetensors", "st/a/config.json"],
                },
                {
                    "id": "parakeet_st_b", "display_name": "st b", "format": "safetensors",
                    "files": ["st/b/model.safetensors"],
                },
            ],
        }))
    }

    fn row(id: &str, path: &str, weight_id: Option<&str>, candidates: &[&str]) -> RowWeights {
        RowWeights {
            model_id: id.into(),
            family: "parakeet_tdt".into(),
            path: path.into(),
            weight_id: weight_id.map(String::from),
            candidates: candidates.iter().map(|c| format!("{path}/{c}")).collect(),
            any_gguf: !candidates.is_empty(),
        }
    }

    const Q8: &str = "parakeet-tdt-0.6b-v3-q8_0.gguf";
    const F16: &str = "parakeet-tdt-0.6b-v3-f16.gguf";

    fn family(rows: &[RowWeights], installed: &[&str]) -> FamilyServing {
        let installed = installed.iter().map(|s| s.to_string()).collect();
        family_serving(&spec(), rows, &installed)
    }

    fn serving(rows: &[RowWeights], installed: &[&str]) -> HashMap<String, Serving> {
        family(rows, installed).packages
    }

    #[test]
    fn the_weights_file_decides_not_the_directory() {
        // Both on disk, weight_id q8_0: only q8 serves. The f16 package in
        // the same directory is not served — that was the bug.
        let s = serving(&[row("asr", DIR, Some("q8_0"), &[Q8])], &[]);
        assert_eq!(s["parakeet_q8_0"].served_by, ["asr"]);
        assert!(!s.contains_key("parakeet_f16"), "{s:?}");

        // Only q8 on disk and no weight_id: still exactly one file, q8.
        let s = serving(&[row("asr", DIR, None, &[Q8])], &[]);
        assert_eq!(s["parakeet_q8_0"].served_by, ["asr"]);
        assert!(!s.contains_key("parakeet_f16"));
    }

    #[test]
    fn a_pick_across_packages_is_unclear_on_each() {
        let s = serving(&[row("asr", DIR, None, &[Q8, F16])], &[]);
        for (id, precision) in [("parakeet_q8_0", "q8_0"), ("parakeet_f16", "f16")] {
            let p = &s[id];
            assert!(p.served_by.is_empty(), "{p:?}");
            assert_eq!(p.unclear.len(), 1);
            assert!(
                p.unclear[0].contains("no weight_id that tells q8_0 / f16 apart"),
                "{}",
                p.unclear[0]
            );
            assert!(p.unclear[0].contains(&format!("(e.g. {precision})")));
        }
        // A weight_id that matches both says so by name.
        let s = serving(&[row("asr", DIR, Some("parakeet"), &[Q8, F16])], &[]);
        assert!(s["parakeet_f16"].unclear[0].contains("weight_id 'parakeet' does not"));
    }

    #[test]
    fn a_file_several_packages_ship_is_unclear() {
        let mut spec = spec();
        // A shared codec: both packages list the same GGUF.
        spec.packages[1].files = spec.packages[0].files.clone();
        let rows = [row("asr", DIR, None, &[Q8])];
        let s = family_serving(&spec, &rows, &HashSet::new()).packages;
        assert!(s["parakeet_q8_0"].served_by.is_empty());
        assert!(s["parakeet_f16"].unclear[0].contains("all ship"), "{s:?}");
    }

    #[test]
    fn other_families_and_unmatched_weight_ids_mark_nothing() {
        // Another family's row in the same directory.
        let mut other = row("tts", DIR, None, &[Q8]);
        other.family = "pocket_tts".into();
        assert!(serving(&[other], &[]).is_empty());
        // A weight_id that matches nothing: no candidates though GGUFs are
        // there — nothing is marked, not even by directory.
        let mut miss = row("asr", DIR, Some("q4_k"), &[]);
        miss.any_gguf = true;
        assert!(serving(&[miss], &["parakeet_q8_0"]).is_empty());
    }

    #[test]
    fn a_root_without_ggufs_falls_back_to_the_directory() {
        let prefix = "audio-cpp/audio.cpp-gguf/st";
        // Installed and alone under the row's root: served.
        let s = serving(
            &[row("st", &format!("{prefix}/a"), None, &[])],
            &["parakeet_st_a"],
        );
        assert_eq!(s["parakeet_st_a"].served_by, ["st"]);
        // Not installed: nothing, however the directory reads.
        let s = serving(&[row("st", &format!("{prefix}/a"), None, &[])], &[]);
        assert!(s.is_empty());
        // Two installed packages under one root: unclear on both.
        let s = serving(
            &[row("st", prefix, None, &[])],
            &["parakeet_st_a", "parakeet_st_b"],
        );
        assert!(
            s["parakeet_st_a"].unclear[0].contains("st a / st b"),
            "{s:?}"
        );
        assert!(s["parakeet_st_b"].served_by.is_empty());
        // A GGUF package never falls back, even installed under the root.
        let s = serving(&[row("asr", DIR, None, &[])], &["parakeet_q8_0"]);
        assert!(s.is_empty());
    }

    /// Two enabled rows of one family, as a box serving both precisions (or
    /// the same one twice) has them.
    #[test]
    fn two_rows_in_one_family_are_each_their_own() {
        // Different packages: each served by its own row.
        let s = serving(
            &[
                row("asr-q8", DIR, Some("q8_0"), &[Q8]),
                row("asr-f16", DIR, Some("f16"), &[F16]),
            ],
            &[],
        );
        assert_eq!(s["parakeet_q8_0"].served_by, ["asr-q8"]);
        assert_eq!(s["parakeet_f16"].served_by, ["asr-f16"]);
        assert!(s.values().all(|p| p.unclear.is_empty()), "{s:?}");

        // The same package: both rows serve it.
        let s = serving(
            &[
                row("asr-a", DIR, Some("q8_0"), &[Q8]),
                row("asr-b", DIR, Some("q8"), &[Q8]),
            ],
            &[],
        );
        assert_eq!(s["parakeet_q8_0"].served_by, ["asr-a", "asr-b"]);

        // One serves it, one cannot be told apart: both say so on it.
        let s = serving(
            &[
                row("asr-a", DIR, Some("q8_0"), &[Q8]),
                row("asr-b", DIR, None, &[Q8, F16]),
            ],
            &[],
        );
        let q8 = &s["parakeet_q8_0"];
        assert_eq!(q8.served_by, ["asr-a"]);
        assert_eq!(q8.unclear.len(), 1);
        assert!(q8.unclear[0].starts_with("audio model 'asr-b'"), "{q8:?}");
        assert!(s["parakeet_f16"].served_by.is_empty());
    }

    /// A GGUF under the root that no package ships (a leftover from an older
    /// spec) is what makes the pick ambiguous, and the sentence names it —
    /// not "set the weight_id you already set".
    #[test]
    fn a_stray_file_under_the_root_is_named() {
        let stray = "parakeet-tdt-0.6b-v2-q8_0.gguf";
        let f = family(&[row("asr", DIR, Some("q8_0"), &[Q8, stray])], &[]);
        let why = &f.packages["parakeet_q8_0"].unclear[0];
        assert!(
            why.contains(&format!(
                "also picks up {stray}, which no package of this family ships"
            )),
            "{why}"
        );
        assert!(why.contains("with weight_id 'q8_0'"), "{why}");
        assert!(!why.contains("does not tell"), "{why}");
        assert!(f.packages["parakeet_q8_0"].served_by.is_empty());

        // Only strays: no package is touched, and the family says so.
        let f = family(&[row("asr", DIR, Some("v2"), &[stray])], &[]);
        assert!(f.packages.is_empty(), "{f:?}");
        assert_eq!(
            f.unmatched,
            [format!(
                "audio model 'asr' loads {stray} under {DIR}, which no package of this family ships"
            )]
        );
    }

    /// A spec that lists one shard of a split GGUF: the download fetches all
    /// of them, and all of them belong to the package.
    #[test]
    fn a_split_gguf_counts_by_its_whole_set() {
        let mut spec = spec();
        spec.packages[0].files = vec!["Parakeet-TDT-0.6B-v3-GGUF/big-00001-of-00002.gguf".into()];
        let rows = [row(
            "asr",
            DIR,
            None,
            &["big-00001-of-00002.gguf", "big-00002-of-00002.gguf"],
        )];
        let s = family_serving(&spec, &rows, &HashSet::new());
        assert_eq!(s.packages["parakeet_q8_0"].served_by, ["asr"], "{s:?}");
        // Another split set is not the same file.
        assert!(!ships(
            &[format!("{DIR}/big-00001-of-00002.gguf")],
            &format!("{DIR}/big-00001-of-00003.gguf")
        ));
    }

    /// A row that loads nothing of its family is said on the family, not
    /// shown as a bare "serving".
    #[test]
    fn a_row_that_matches_no_package_is_a_family_note() {
        let mut miss = row("asr", DIR, Some("q4_k"), &[]);
        miss.any_gguf = true;
        let f = family(&[miss], &[]);
        assert_eq!(
            f.unmatched,
            [format!(
                "audio model 'asr' has weight_id 'q4_k', which matches no GGUF under {DIR}"
            )]
        );
        // An empty or deleted root.
        let f = family(&[row("asr", "gone/dir", None, &[])], &[]);
        assert!(f.unmatched[0].contains("where no downloaded package of this family is"));
        // A served row is not a note.
        assert!(family(&[row("asr", DIR, None, &[Q8])], &[])
            .unmatched
            .is_empty());
    }

    #[test]
    fn gather_reads_enabled_rows_only_relative_to_the_models_dir() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join(DIR);
        std::fs::create_dir_all(&dir).unwrap();
        for f in [Q8, F16] {
            std::fs::write(dir.join(f), b"GGUF").unwrap();
        }
        let model = |id: &str, enabled: bool, weight_id: Option<&str>| AudioModel {
            id: 1,
            model_id: id.into(),
            family: "parakeet_tdt".into(),
            path: format!("/{DIR}/"),
            task: "asr".into(),
            mode: "offline".into(),
            lazy: None,
            busy_timeout_ms: None,
            backend: None,
            threads: None,
            load_options: Default::default(),
            session_options: Default::default(),
            default_request_options: Default::default(),
            model_spec_override: None,
            config_id: None,
            weight_id: weight_id.map(String::from),
            voice_presets: Default::default(),
            default_voice_preset: None,
            enabled,
            image: None,
            extra_run_args: None,
            warm_start: false,
            hold_fallback_mode: Default::default(),
            hold_fallback: None,
            residency: None,
        };
        let models_dir = d.path().display().to_string();
        let rows = gather(
            &models_dir,
            &[
                model("on", true, Some("q8_0")),
                model("off", false, Some("q8_0")),
                model("miss", true, Some("q4_k")),
            ],
        );
        assert_eq!(rows.len(), 2, "the disabled row is not read");
        assert_eq!(rows[0].path, DIR);
        assert_eq!(rows[0].candidates, [format!("{DIR}/{Q8}")]);
        assert!(rows[1].candidates.is_empty() && rows[1].any_gguf);
        // The gathered row against the spec: the q8 package serves.
        let s = family_serving(&spec(), &rows, &HashSet::new()).packages;
        assert_eq!(s["parakeet_q8_0"].served_by, ["on"]);
        assert!(!s.contains_key("parakeet_f16"));
    }
}
