//! Pure, snapshot-free save-time rules (candidate-aliases design §4.1, §4.6)
//! — the part of `ops::candidate_alias`'s validation that does not need a
//! database or a GGUF read, split out so it is unit-testable on hand-built
//! [`FacetSet`]s rather than only through a full save.

use std::collections::HashMap;

use super::FacetSet;
use crate::candidates::Facet;

/// §4.1's list refusals: empty, or a duplicate entry.
pub fn refuse_empty_or_duplicate(candidates: &[String]) -> Result<(), String> {
    if candidates.is_empty() {
        return Err(
            "candidates must not be empty — the first entry is the primary, at least one is \
             required"
                .to_string(),
        );
    }
    let mut seen: Vec<&str> = Vec::with_capacity(candidates.len());
    for id in candidates {
        if seen.contains(&id.as_str()) {
            return Err(format!("'{id}' is listed more than once in candidates"));
        }
        seen.push(id);
    }
    Ok(())
}

/// §4.6's capability save rules: what ends up enabled, and what gets stored
/// as `capabilities_disabled` — `(enabled, disabled_to_store)`.
///
/// `disabled` is this save's full, stated `capabilities_disabled` — the
/// owner's explicit switches only, **never auto-filled** (`ops::
/// candidate_alias` resolves it: an untouched `update` carries the row's own
/// stored value forward unchanged, a fresh `create` that never named the
/// field is `FacetSet::EMPTY`). `enabled` is always `common.minus(disabled)`,
/// and `disabled` is stored back exactly as given — nothing here ever
/// fabricates a value in either direction (§12 entry 49).
///
/// `previously_saved` is the row's last stored `(capabilities_enabled,
/// capabilities_disabled)` — `None` only on a fresh `create`, where nothing
/// has been decided yet and so nothing can be refused: an uncommon facet is
/// simply left off, and turns itself on the moment it becomes common, unless
/// the owner names it in `disabled` (rule 2). With a previous save to compare
/// against, a facet neither common now nor in `disabled` refuses the save
/// (naming it and `unsupported_by`'s candidates) when either:
/// - it was in the *previous* enabled set (rule 3: a candidate dropped
///   support and the owner did not switch it off in this save — "switch the
///   facet off first, then add the candidate"); or
/// - it was in the *previous* disabled set and this save no longer names it
///   (rule 1: the owner is switching it on, and no candidate supports it).
///
/// A facet the previous save already left off, and this save still leaves
/// off, is never refused — that is "removing the odd one out re-enables a
/// facet only if it was not switched off" falling out for free: nothing
/// switched, nothing to refuse.
pub fn resolve_enabled(
    disabled: FacetSet,
    previously_saved: Option<(FacetSet, FacetSet)>,
    common: FacetSet,
    unsupported_by: &HashMap<String, Vec<String>>,
) -> Result<(FacetSet, FacetSet), String> {
    if let Some((prev_enabled, prev_disabled)) = previously_saved {
        for f in Facet::ALL {
            if common.contains(f) || disabled.contains(f) {
                continue;
            }
            if !prev_enabled.contains(f) && !prev_disabled.contains(f) {
                continue;
            }
            let lacking = unsupported_by
                .get(f.as_str())
                .map(|v| v.join(", "))
                .unwrap_or_default();
            return Err(format!(
                "'{}' cannot be enabled: not supported by {lacking}. Add it to \
                 capabilities_disabled, or drop/replace the candidate(s) named.",
                f.as_str()
            ));
        }
    }
    Ok((common.minus(disabled), disabled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_duplicate_are_refused() {
        assert!(refuse_empty_or_duplicate(&[]).is_err());
        assert!(refuse_empty_or_duplicate(&["a".into(), "a".into()]).is_err());
        assert!(refuse_empty_or_duplicate(&["a".into(), "b".into()]).is_ok());
    }

    #[test]
    fn a_fresh_create_enables_common_and_never_refuses_an_uncommon_facet() {
        // Nothing stored before (`previously_saved: None`): `audio` is
        // uncommon and unnamed in `disabled`, and that is never refused on a
        // create — there is nothing yet to have "switched on".
        let common = FacetSet::from_names(["vision", "reasoning"]).unwrap();
        let mut unsupported = HashMap::new();
        unsupported.insert("audio".to_string(), vec!["m2".to_string()]);
        let (enabled, disabled) =
            resolve_enabled(FacetSet::EMPTY, None, common, &unsupported).unwrap();
        assert!(enabled.contains(Facet::Vision));
        assert!(enabled.contains(Facet::Reasoning));
        assert!(!enabled.contains(Facet::Audio));
        // Stored back exactly as stated — empty, never materialized into
        // common's complement.
        assert_eq!(disabled, FacetSet::EMPTY);
    }

    #[test]
    fn a_fresh_create_stores_exactly_the_owners_stated_disabled_list() {
        let common = FacetSet::from_names(["vision", "audio"]).unwrap();
        let disabled = FacetSet::from_names(["vision"]).unwrap();
        let (enabled, stored) = resolve_enabled(disabled, None, common, &HashMap::new()).unwrap();
        assert!(!enabled.contains(Facet::Vision));
        assert!(enabled.contains(Facet::Audio));
        assert_eq!(stored, disabled);
    }

    #[test]
    fn a_facet_previously_enabled_that_a_candidate_now_lacks_is_refused() {
        // `vision` used to be common and enabled; a new candidate dropped it.
        // This save's `disabled` does not name it, so rule 3 fires: "switch
        // the facet off first, then add the candidate".
        let prev_enabled = FacetSet::from_names(["vision"]).unwrap();
        let prev_disabled = FacetSet::EMPTY;
        let common = FacetSet::EMPTY;
        let mut unsupported = HashMap::new();
        unsupported.insert("vision".to_string(), vec!["new-candidate".to_string()]);
        let err = resolve_enabled(
            FacetSet::EMPTY,
            Some((prev_enabled, prev_disabled)),
            common,
            &unsupported,
        )
        .unwrap_err();
        assert!(err.contains("vision"), "{err}");
        assert!(err.contains("new-candidate"), "{err}");
    }

    #[test]
    fn switching_on_a_facet_no_candidate_supports_is_refused() {
        // `audio` was explicitly switched off before (in the previously
        // stored disabled set); this save removes it from `disabled` —
        // switching it on — while no candidate supports it, so rule 1
        // fires.
        let prev_enabled = FacetSet::EMPTY;
        let prev_disabled = FacetSet::from_names(["audio"]).unwrap();
        let common = FacetSet::EMPTY;
        let mut unsupported = HashMap::new();
        unsupported.insert("audio".to_string(), vec!["m2".to_string()]);
        let err = resolve_enabled(
            FacetSet::EMPTY,
            Some((prev_enabled, prev_disabled)),
            common,
            &unsupported,
        )
        .unwrap_err();
        assert!(err.contains("audio"), "{err}");
        assert!(err.contains("m2"), "{err}");
    }

    #[test]
    fn a_facet_left_off_all_along_is_never_refused() {
        // `audio` was uncommon and unnamed at the previous save too (the
        // create-lenient default carried forward) — neither "previously
        // enabled" nor "previously disabled" applies, so nothing switched
        // and there is nothing to refuse.
        let prev_enabled = FacetSet::from_names(["vision"]).unwrap();
        let prev_disabled = FacetSet::EMPTY;
        let common = FacetSet::from_names(["vision"]).unwrap();
        let (enabled, disabled) = resolve_enabled(
            FacetSet::EMPTY,
            Some((prev_enabled, prev_disabled)),
            common,
            &HashMap::new(),
        )
        .unwrap();
        assert!(!enabled.contains(Facet::Audio));
        assert_eq!(disabled, FacetSet::EMPTY);
    }

    #[test]
    fn removing_the_odd_one_out_reenables_a_facet_only_if_not_switched_off() {
        // `audio` was uncommon and unnamed (never switched off) at the
        // previous save; the odd candidate lacking it was just removed, so
        // every facet is common now — it turns back on by itself.
        let prev_enabled = FacetSet::EMPTY;
        let prev_disabled = FacetSet::EMPTY;
        let common = FacetSet::everything();
        let (enabled, _) = resolve_enabled(
            FacetSet::EMPTY,
            Some((prev_enabled, prev_disabled)),
            common,
            &HashMap::new(),
        )
        .unwrap();
        assert!(enabled.contains(Facet::Audio));

        // The owner had separately switched it off (in the previously
        // stored disabled set) and this save still names it — it stays off
        // even though every candidate now supports it.
        let prev_disabled_off = FacetSet::from_names(["audio"]).unwrap();
        let (still_off, _) = resolve_enabled(
            FacetSet::from_names(["audio"]).unwrap(),
            Some((prev_enabled, prev_disabled_off)),
            common,
            &HashMap::new(),
        )
        .unwrap();
        assert!(!still_off.contains(Facet::Audio));
    }
}
