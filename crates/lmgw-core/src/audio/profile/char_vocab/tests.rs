//! Where a package keeps its character vocabulary: the file its own spec
//! names, embedded in the GGUF or beside it.

use serde_json::json;

use super::*;
use crate::gguf::embedded::read_embedded_index;
use crate::gguf::synth;

fn spec(files: serde_json::Value) -> Value {
    json!({"family": "supertonic", "sources": [{"files": {"tts_config": "model:config/tts.json"}},
                                                 files]})
}

/// The profile of a `family` package at `dir/p.gguf` embedding `files`,
/// whose spec has `spec`'s sources.
fn profile(dir: &Path, family: &str, spec: &Value, files: &[(&str, &[u8])]) -> SpeechProfile {
    let g = dir.join("p.gguf");
    synth::audiocpp(family, &spec.to_string(), files).write_to(&g);
    let index = read_embedded_index(&g).unwrap();
    let mut p = SpeechProfile {
        family: family.into(),
        ..Default::default()
    };
    read(&mut p, Some(spec), Some(&index), Some(&g));
    p
}

/// The real Supertonic 3 indexer, trimmed: one that has what the engine
/// writes itself, so it is no problem.
const INDEXER: &[u8] =
    include_bytes!("../../../../tests/fixtures/audio/supertonic3_unicode_indexer_trimmed.json");

#[test]
fn the_file_the_spec_names_is_read_from_the_gguf_else_from_beside_it() {
    let d = tempfile::tempdir().unwrap();
    let named = spec(json!({"optional_files": {"unicode_indexer": "model:./config/ui.json"}}));
    let p = profile(
        d.path(),
        "supertonic",
        &named,
        &[("config/ui.json", INDEXER)],
    );
    let v = p.char_vocab.as_ref().unwrap();
    assert_eq!((v.file.as_str(), v.len()), ("config/ui.json", 661));
    assert!(p.problems.is_empty(), "{:?}", p.problems);
    assert!(p.sources[0].contains("config/ui.json"), "{:?}", p.sources);

    // Not embedded: the package directory's own file.
    std::fs::create_dir(d.path().join("config")).unwrap();
    std::fs::write(d.path().join("config/ui.json"), INDEXER).unwrap();
    let p = profile(d.path(), "supertonic", &named, &[]);
    assert!(p.char_vocab.as_ref().unwrap().says('A'));
    assert!(p.problems.is_empty(), "{:?}", p.problems);

    // One that lacks what the engine writes itself is read all the same,
    // and the profile says what is missing.
    std::fs::write(d.path().join("config/ui.json"), b"{\"65\": 4}").unwrap();
    let p = profile(d.path(), "supertonic", &named, &[]);
    assert!(p.char_vocab.as_ref().unwrap().says('A'));
    assert!(
        p.problems.is_empty()
            && !p.notes.is_empty()
            && p.notes
                .iter()
                .all(|m| m.starts_with("unicode_indexer: the engine ")),
        "{:?} {:?}",
        p.problems,
        p.notes
    );
}

#[test]
fn a_package_without_it_is_a_problem_and_its_input_goes_as_it_came() {
    let d = tempfile::tempdir().unwrap();
    let cases = [
        // The spec names none.
        (spec(json!({})), "names no such file"),
        // It names one the package lacks.
        (
            spec(json!({"files": {"unicode_indexer": "model:config/unicode_indexer.json"}})),
            "has no config/unicode_indexer.json",
        ),
        // A path out of the package is none.
        (
            spec(json!({"files": {"unicode_indexer": "model:../elsewhere.json"}})),
            "names no such file",
        ),
    ];
    for (named, why) in cases {
        let p = profile(d.path(), "supertonic", &named, &[]);
        assert!(p.char_vocab.is_none());
        assert_eq!(p.problems.len(), 1, "{:?}", p.problems);
        assert!(p.problems[0].contains(why), "{:?}", p.problems);
        assert!(
            p.problems[0].contains("sent as they came"),
            "{:?}",
            p.problems
        );
    }
    // Unreadable as an indexer.
    let named = spec(json!({"files": {"unicode_indexer": "model:ui.json"}}));
    let p = profile(d.path(), "supertonic", &named, &[("ui.json", b"[-1]")]);
    assert!(p.char_vocab.is_none());
    assert!(p.problems[0].contains("no entries"), "{:?}", p.problems);
}

#[test]
fn a_family_that_takes_any_character_has_none_and_no_problem() {
    let d = tempfile::tempdir().unwrap();
    let named = spec(json!({"files": {"unicode_indexer": "model:ui.json"}}));
    let p = profile(d.path(), "kokoro_tts", &named, &[("ui.json", INDEXER)]);
    assert!(p.char_vocab.is_none() && p.problems.is_empty());
}

/// A Supertonic row with no package GGUF is sent its input unshaped, and
/// the profile says so (review TC-6); one whose GGUF could not be read has
/// that problem already.
#[test]
fn a_row_without_a_package_gguf_says_it_goes_unshaped() {
    let mut p = SpeechProfile {
        family: "supertonic".into(),
        ..Default::default()
    };
    read(&mut p, None, None, None);
    assert!(p.char_vocab.is_none());
    assert_eq!(p.problems.len(), 1, "{:?}", p.problems);
    assert!(
        p.problems[0].contains("no package GGUF") && p.problems[0].contains("sent as they came"),
        "{:?}",
        p.problems
    );
    let mut p = SpeechProfile {
        family: "supertonic".into(),
        ..Default::default()
    };
    read(&mut p, None, None, Some(Path::new("/nowhere/p.gguf")));
    assert!(p.char_vocab.is_none() && p.problems.is_empty());
}

/// The file read from beside the GGUF is noted, so a change to it computes
/// the profile again (review TC-7); an embedded one is not.
#[test]
fn a_file_beside_the_gguf_is_noted_for_the_cache() {
    let d = tempfile::tempdir().unwrap();
    let named = spec(json!({"files": {"unicode_indexer": "model:config/ui.json"}}));
    let p = profile(
        d.path(),
        "supertonic",
        &named,
        &[("config/ui.json", INDEXER)],
    );
    assert!(p.beside.paths().is_empty(), "{:?}", p.beside);
    // Looked for even before it is there: adding it is a change too.
    let p = profile(d.path(), "supertonic", &named, &[]);
    assert_eq!(p.beside.paths(), [d.path().join("config/ui.json")]);
}
