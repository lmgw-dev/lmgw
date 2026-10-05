//! Spec files a refresh could not load keep their families.
//!
//! A refresh fetches ~100 spec files one by one; on a flaky link, or with the
//! raw host rate-limiting, a few of them time out while the rest load. Those
//! families used to drop out of the new snapshot — installed or served ones
//! included — and with them their repos' listings, until a later refresh
//! happened to load them. Now a failed file's family is kept as the previous
//! snapshot had it, and the warning says from when.

use super::{CatalogSnapshot, ModelSpec};

/// Carry each failed spec file's family over from `previous` into `specs`,
/// and return the warning each failure becomes. `failed` is `(path, why)`.
///
/// A previous spec is matched by the file it came from; one saved before
/// that was recorded is matched by its family against the file's stem, which
/// is how audio.cpp names them.
pub(super) fn carry_failed(
    specs: &mut Vec<ModelSpec>,
    failed: &[(String, String)],
    previous: Option<&CatalogSnapshot>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for (path, why) in failed {
        let stem = path
            .rsplit('/')
            .next()
            .and_then(|f| f.strip_suffix(".json"))
            .unwrap_or_default();
        let kept = previous.and_then(|p| {
            let spec = p.specs.iter().find(|s| match s.source.is_empty() {
                false => s.source == *path,
                true => !stem.is_empty() && s.family == stem,
            })?;
            Some((spec, p))
        });
        // A family another file now declares wins over the kept copy.
        let kept = kept.filter(|(spec, _)| !specs.iter().any(|s| s.family == spec.family));
        match kept {
            Some((spec, p)) => {
                warnings.push(format!(
                    "audio.cpp spec {path}: {why} — its family is kept as it was in the catalog \
                     fetched {}",
                    p.fetched_at.get(..10).unwrap_or(&p.fetched_at)
                ));
                let mut spec = spec.clone();
                spec.source = path.clone();
                specs.push(spec);
            }
            None => warnings.push(format!(
                "audio.cpp spec {path}: {why} — its family is left out of this catalog"
            )),
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(family: &str, source: &str) -> ModelSpec {
        let mut s = crate::audio::parse_spec(&json!({ "family": family, "packages": [] }));
        s.source = source.into();
        s
    }

    fn previous(specs: Vec<ModelSpec>) -> CatalogSnapshot {
        CatalogSnapshot {
            fetched_at: "2026-09-30T08:00:00Z".into(),
            specs,
            listings: Default::default(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn a_failed_file_keeps_its_family_from_the_previous_catalog() {
        let prev = previous(vec![
            spec("parakeet_tdt", "model_specs/parakeet_tdt.json"),
            spec("pocket_tts", "model_specs/pocket_tts.json"),
        ]);
        let mut specs = vec![spec("pocket_tts", "model_specs/pocket_tts.json")];
        let failed = [(
            "model_specs/parakeet_tdt.json".to_string(),
            "HTTP 429 Too Many Requests".to_string(),
        )];
        let warnings = carry_failed(&mut specs, &failed, Some(&prev));
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[1].family, "parakeet_tdt");
        assert_eq!(
            warnings,
            [
                "audio.cpp spec model_specs/parakeet_tdt.json: HTTP 429 Too Many Requests — its \
              family is kept as it was in the catalog fetched 2026-09-30"
            ]
        );
    }

    #[test]
    fn with_nothing_to_keep_the_family_is_left_out_and_said() {
        let mut specs = vec![];
        let failed = [("model_specs/new.json".to_string(), "timed out".to_string())];
        let warnings = carry_failed(&mut specs, &failed, None);
        assert!(specs.is_empty());
        assert!(warnings[0].ends_with("its family is left out of this catalog"));
        let prev = previous(vec![spec("other", "model_specs/other.json")]);
        carry_failed(&mut specs, &failed, Some(&prev));
        assert!(specs.is_empty(), "no spec came from that file");
    }

    /// A snapshot saved before sources were recorded: the file's stem names
    /// the family.
    #[test]
    fn an_old_snapshot_is_matched_by_the_file_stem() {
        let prev = previous(vec![spec("qwen3_asr", "")]);
        let mut specs = vec![];
        let failed = [("model_specs/qwen3_asr.json".to_string(), "x".to_string())];
        carry_failed(&mut specs, &failed, Some(&prev));
        assert_eq!(specs[0].family, "qwen3_asr");
        assert_eq!(specs[0].source, "model_specs/qwen3_asr.json");
    }
}
