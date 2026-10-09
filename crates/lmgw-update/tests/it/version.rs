//! The version order `is_newer` decides an update by.

use lmgw_update::is_newer;

#[test]
fn newer_compares_numerically_not_lexically() {
    assert!(is_newer("0.1.42", "0.1.0"));
    assert!(is_newer("0.1.10", "0.1.9")); // would fail under string compare
    assert!(is_newer("0.2.0", "0.1.99"));
    assert!(is_newer("1.0.0", "0.9.9"));
}

#[test]
fn equal_or_older_is_not_newer() {
    assert!(!is_newer("0.1.0", "0.1.0"));
    assert!(!is_newer("0.1.0", "0.1.5"));
    assert!(!is_newer("0.1.0", "0.2.0"));
}

#[test]
fn suffixes_are_ignored_and_bad_input_is_not_newer() {
    assert!(!is_newer("0.1.0-dev.3", "0.1.0"));
    assert!(!is_newer("garbage", "0.1.0"));
    assert!(is_newer("0.1.5+abc", "0.1.0"));
    assert!(!is_newer("0.1.0+abc", "0.1.0"));
}

#[test]
fn private_builds_sort_between_releases() {
    assert!(is_newer("0.3.0+1", "0.3.0"));
    assert!(is_newer("0.3.0+107", "0.3.0+99")); // numeric, not lexical
    assert!(is_newer("0.3.1", "0.3.0+500"));
    assert!(!is_newer("0.3.0", "0.3.0+5"));
    assert!(!is_newer("0.3.0+5", "0.3.0+5"));
    assert!(is_newer("0.3.0+100", "0.2.99")); // the old counter scheme
}
