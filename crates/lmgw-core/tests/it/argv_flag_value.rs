//! `runtime::argv::flag_value`: the one reader of a llama-server flag's value
//! in a row's freeform args, under every spelling the renderer's alias table
//! knows, so what lmgw reports, charges and checks is what the container is
//! started with.

use lmgw_core::runtime::argv::flag_value;

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[test]
fn every_spelling_of_a_flag_answers_its_long_name() {
    for a in [
        argv(&["--mmproj", "/models/p.gguf"]),
        argv(&["-mm", "/models/p.gguf"]),
        argv(&["--mmproj=/models/p.gguf"]),
        argv(&["-mm=/models/p.gguf"]),
    ] {
        assert_eq!(flag_value(&a, "mmproj"), Some("/models/p.gguf"), "{a:?}");
    }
    let a = argv(&["-ub", "8192", "-b", "8192", "-t", "8"]);
    assert_eq!(flag_value(&a, "ubatch-size"), Some("8192"));
    assert_eq!(flag_value(&a, "batch-size"), Some("8192"));
    assert_eq!(flag_value(&a, "threads"), Some("8"));
    assert_eq!(flag_value(&a, "mmproj"), None);
}

#[test]
fn the_last_occurrence_wins_as_in_llama_server() {
    let a = argv(&["--mmproj", "/models/old.gguf", "-mm", "/models/new.gguf"]);
    assert_eq!(flag_value(&a, "mmproj"), Some("/models/new.gguf"));
    let a = argv(&["-ub", "512", "--ubatch-size=4096"]);
    assert_eq!(flag_value(&a, "ubatch-size"), Some("4096"));
}

#[test]
fn a_flag_followed_by_an_option_has_no_value_and_a_negative_number_is_one() {
    assert_eq!(flag_value(&argv(&["--mmproj", "--jinja"]), "mmproj"), None);
    assert_eq!(
        flag_value(&argv(&["-ngl", "-1"]), "n-gpu-layers"),
        Some("-1")
    );
    // A value that merely looks like the flag is not read as one.
    assert_eq!(flag_value(&argv(&["--alias", "mm"]), "mmproj"), None);
}
