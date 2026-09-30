//! Regression coverage for `config::hoist_promoted_args_into` (review
//! finding X2, final cross-phase review).
//!
//! Phase 1 (unified KV) taught the hoist to canonicalize `-kvu`/`-no-kvu`
//! before checking `PROMOTED_ARGS`, by routing the lookup through
//! `runtime::argv::canonical_key`'s *full* `SHORT_ALIASES` table. That table
//! also carries short spellings with no promoted twin at all — `-ub`, `-n` /
//! `-predict`, `-cram`, `-mm`, `-rea` — which pre-date phase 1 and were never
//! hoisted before it: before it the hoist matched only the literal long
//! flag. Reusing the whole table silently started hoisting those on every
//! row's next load: argv gained a new spelling and position (failing the
//! post-upgrade `Config.Cmd` adoption match), `-mm` pulled a projector into
//! the VRAM plan and published vision capability, and `-n`/`-rea` changed
//! `/v1/models`' `max_output_tokens` and the published reasoning shape — all
//! on rows that never touched the unified-KV feature.
//!
//! This is exactly the class of regression ladder §12 entry 75 reverted for
//! `spec-type`/`model-draft` (see `tests/it/ladder_models.rs`'s
//! `a_non_ladder_row_with_freeform_spec_type_is_unhoisted_and_unchanged`);
//! these tests pin the same guarantee for the older promoted flags phase 1
//! touched.

use lmgw_core::config::{hoist_promoted_args_into, LlamaParams};

fn hoist(args: &[&str]) -> (LlamaParams, Vec<String>) {
    let mut params = LlamaParams::default();
    let mut args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    hoist_promoted_args_into(&mut params, &mut args);
    (params, args)
}

/// `-ub` has a dedicated field (`ubatch_size`) but no short alias existed in
/// the hoist before phase 1 — it must still not be recognized.
#[test]
fn short_ubatch_size_is_not_hoisted() {
    let (params, args) = hoist(&["-ub", "2048"]);
    assert_eq!(params.ubatch_size, None);
    assert_eq!(args, vec!["-ub", "2048"]);
}

/// Both `-n` and its `-predict` twin map to `n-predict` in the renderer's
/// dedup table, but neither hoisted before phase 1.
#[test]
fn short_n_predict_spellings_are_not_hoisted() {
    let (params, args) = hoist(&["-n", "4096"]);
    assert_eq!(params.n_predict, None);
    assert_eq!(args, vec!["-n", "4096"]);

    let (params, args) = hoist(&["--predict", "4096"]);
    assert_eq!(params.n_predict, None);
    assert_eq!(args, vec!["--predict", "4096"]);
}

#[test]
fn short_cache_ram_is_not_hoisted() {
    let (params, args) = hoist(&["-cram", "0"]);
    assert_eq!(params.cache_ram, None);
    assert_eq!(args, vec!["-cram", "0"]);
}

/// The highest-stakes case: a hoisted `-mm` joins the VRAM plan (projector
/// footprint) and flips the published vision capability.
#[test]
fn short_mmproj_is_not_hoisted() {
    let (params, args) = hoist(&["-mm", "/models/x.mmproj"]);
    assert_eq!(params.mmproj_path, None);
    assert_eq!(args, vec!["-mm", "/models/x.mmproj"]);
}

#[test]
fn short_reasoning_is_not_hoisted() {
    let (params, args) = hoist(&["-rea", "on"]);
    assert_eq!(params.reasoning, None);
    assert_eq!(args, vec!["-rea", "on"]);
}

/// A row mixing several of the older short flags together, exactly the
/// "plain row loaded after upgrade" scenario X2 describes: argv must survive
/// untouched, in the same order, with every typed field still unset.
#[test]
fn a_plain_row_with_several_old_short_flags_is_completely_unhoisted() {
    let original = vec![
        "-ub".to_string(),
        "2048".to_string(),
        "-n".to_string(),
        "4096".to_string(),
        "-cram".to_string(),
        "0".to_string(),
        "-mm".to_string(),
        "/models/x.mmproj".to_string(),
        "-rea".to_string(),
        "on".to_string(),
    ];
    let (params, args) = hoist(&original.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(args, original, "argv order and spelling must be unchanged");
    assert_eq!(params.ubatch_size, None);
    assert_eq!(params.n_predict, None);
    assert_eq!(params.cache_ram, None);
    assert_eq!(params.mmproj_path, None);
    assert_eq!(params.reasoning, None);
}

/// The phase-1 flags this canonicalization exists for must still hoist —
/// only the *unrelated* short aliases must stay freeform.
#[test]
fn short_kv_unified_aliases_still_hoist() {
    let (params, args) = hoist(&["-kvu"]);
    assert_eq!(params.kv_unified, Some(true));
    assert!(args.is_empty());

    let (params, args) = hoist(&["-no-kvu"]);
    assert_eq!(params.kv_unified, Some(false));
    assert!(args.is_empty());
}

/// The long spellings of the older promoted flags must keep hoisting exactly
/// as before phase 1 — only the short-alias canonicalization changed.
#[test]
fn long_spellings_of_older_promoted_flags_still_hoist() {
    let (params, args) = hoist(&["--ubatch-size", "2048"]);
    assert_eq!(params.ubatch_size, Some(2048));
    assert!(args.is_empty());

    let (params, args) = hoist(&["--mmproj", "x.mmproj"]);
    assert_eq!(params.mmproj_path, Some("x.mmproj".to_string()));
    assert!(args.is_empty());
}
