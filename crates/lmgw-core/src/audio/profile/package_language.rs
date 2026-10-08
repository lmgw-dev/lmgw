//! The one language of a package that speaks or hears only its own
//! ([`families::package_language`]): Pocket TTS's from the row's `language`
//! load option or the package's name, SanoTTS's and Kroko ASR's from the
//! package's `config.json` — the one its GGUF embeds, else the file beside
//! it (their specs source it as `model:config.json`). What the conversation
//! language is compared with: no note when they match
//! ([`crate::audio::language`]).

use std::path::Path;

use serde_json::Value;

use super::SpeechProfile;
use crate::audio::families::{self, PackageLanguage};
use crate::audio::language::code_of_name;
use crate::config::AudioModel;
use crate::gguf::embedded::{EmbeddedIndex, MAX_EMBEDDED_FILE};

/// The row's `language` load option, when it is a non-empty string.
pub(super) fn load_option(row: &AudioModel) -> Option<&str> {
    row.load_options
        .get("language")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|l| !l.is_empty())
}

/// Fill `p.package_language` for a family whose package has one language.
pub(super) fn read(
    p: &mut SpeechProfile,
    row: &AudioModel,
    index: Option<&EmbeddedIndex>,
    gguf: Option<&Path>,
) {
    let Some(how) = families::package_language(&row.family) else {
        return;
    };
    p.package_language = match how {
        PackageLanguage::LoadOptionOrName => load_option(row)
            .and_then(pocket_language)
            .or_else(|| named_in(&row.path))
            .or_else(|| {
                gguf.and_then(|g| g.file_name()?.to_str())
                    .and_then(named_in)
            }),
        PackageLanguage::SanoConfig => config(p, index, gguf).and_then(|c| {
            match c.get("graph").and_then(Value::as_str).unwrap_or("nano") {
                "nano" => Some("en".to_string()),
                "piperlite" => iso(c.get("language")),
                _ => None,
            }
        }),
        PackageLanguage::KrokoConfig => {
            config(p, index, gguf).and_then(|c| iso(c.get("language").and_then(|l| l.get("iso"))))
        }
    };
    if p.package_language.is_some() {
        p.sources.push("the package's own language".into());
    }
}

/// A Pocket TTS language name (`german`, `english_2026-04`, `french_24l`):
/// the name before any `_` suffix, as its ISO 639-1 code.
fn pocket_language(name: &str) -> Option<String> {
    let base = name.split('_').next().unwrap_or_default();
    code_of_name(&base.to_ascii_lowercase()).map(str::to_string)
}

/// The first word of `name` (a path or a file name, split at `/ - _ .`)
/// that is a language's English name, as its code.
fn named_in(name: &str) -> Option<String> {
    name.split(['/', '\\', '-', '_', '.'])
        .find_map(|w| code_of_name(&w.to_ascii_lowercase()))
        .map(str::to_string)
}

/// An ISO code as written in a package (`de`, `de_DE`): its primary subtag,
/// lowercase.
fn iso(v: Option<&Value>) -> Option<String> {
    let s = v?.as_str()?.trim().to_ascii_lowercase();
    let primary = s.split(['-', '_']).next().unwrap_or_default();
    (2..=3)
        .contains(&primary.len())
        .then(|| primary.to_string())
}

/// The package's `config.json`: embedded, else beside the GGUF. A file
/// that cannot be read is a problem of the profile's; one that is missing
/// says nothing.
fn config(
    p: &mut SpeechProfile,
    index: Option<&EmbeddedIndex>,
    gguf: Option<&Path>,
) -> Option<Value> {
    let embedded = match index.map(|i| i.file("config.json")) {
        Some(Ok(raw)) => raw,
        Some(Err(e)) => {
            p.problems.push(format!("config.json: {e}"));
            None
        }
        None => None,
    };
    let raw = match embedded {
        Some(raw) => raw,
        None => {
            let path = gguf?.parent()?.join("config.json");
            p.note_beside(path.clone());
            let len = std::fs::metadata(&path).ok()?.len();
            if len > MAX_EMBEDDED_FILE {
                p.problems.push(format!(
                    "{}: {len} bytes, more than the {MAX_EMBEDDED_FILE} lmgw reads for a profile",
                    path.display()
                ));
                return None;
            }
            std::fs::read(&path).ok()?
        }
    };
    match serde_json::from_slice(&raw) {
        Ok(v) => Some(v),
        Err(_) => {
            p.problems.push("config.json: not JSON".into());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(family: &str, path: &str, language: Option<&str>) -> AudioModel {
        let load: Value = match language {
            Some(l) => serde_json::json!({ "language": l }),
            None => serde_json::json!({}),
        };
        serde_json::from_value(serde_json::json!({
            "id": 1, "model_id": "r", "family": family, "path": path, "task": "tts",
            "mode": "offline", "load_options": load, "session_options": {},
            "voice_presets": {}, "default_voice_preset": null, "enabled": true,
            "image": null, "extra_run_args": null, "warm_start": false
        }))
        .unwrap()
    }

    #[test]
    fn pocket_speaks_its_load_option_else_its_package_s_name() {
        let mut p = SpeechProfile::default();
        read(
            &mut p,
            &row("pocket_tts", "PocketTTS-GGUF/german", None),
            None,
            None,
        );
        assert_eq!(p.package_language.as_deref(), Some("de"));
        let mut p = SpeechProfile::default();
        let gguf = Path::new("/m/pocket/pocket-tts-italian-q8_0.gguf");
        read(&mut p, &row("pocket_tts", "pocket", None), None, Some(gguf));
        assert_eq!(p.package_language.as_deref(), Some("it"));
        let mut p = SpeechProfile::default();
        read(
            &mut p,
            &row(
                "pocket_tts",
                "PocketTTS-GGUF/german",
                Some("english_2026-04"),
            ),
            None,
            None,
        );
        assert_eq!(p.package_language.as_deref(), Some("en"), "the option wins");
        let mut p = SpeechProfile::default();
        read(&mut p, &row("pocket_tts", "voices/x", None), None, None);
        assert_eq!(p.package_language, None, "nothing says");
    }

    /// The `config.json` read from beside the GGUF is an input of the
    /// profile cache (review TC-7): noted with a stamp (TC-17: the stamp
    /// order is pinned by `profile::tests`).
    #[test]
    fn sanotts_and_kroko_track_the_config_beside_the_gguf() {
        let d = tempfile::tempdir().unwrap();
        let gguf = d.path().join("m.gguf");
        let config = d.path().join("config.json");
        for (family, json) in [
            ("sanotts", r#"{"architecture":"sanotts","language":"de"}"#),
            (
                "kroko_asr",
                r#"{"model_type":"zipformer2","language":{"iso":"en"}}"#,
            ),
        ] {
            let _ = std::fs::remove_file(&config);
            let mut p = SpeechProfile::default();
            read(&mut p, &row(family, "x", None), None, Some(&gguf));
            assert_eq!(p.beside.paths(), [config.as_path()], "{family}: looked for");
            assert_eq!(
                p.beside.stamp_of(&config),
                Some(None),
                "{family}: not there"
            );

            std::fs::write(&config, json).unwrap();
            let mut p = SpeechProfile::default();
            read(&mut p, &row(family, "x", None), None, Some(&gguf));
            assert_eq!(p.beside.paths(), [config.as_path()], "{family}");
            let stamp = p.beside.stamp_of(&config).unwrap();
            assert_eq!(stamp.map(|s| s.0), Some(json.len() as u64));
        }
    }

    #[test]
    fn sanotts_and_kroko_read_their_config_beside_the_gguf() {
        let d = tempfile::tempdir().unwrap();
        let gguf = d.path().join("de-f32.gguf");
        std::fs::write(
            d.path().join("config.json"),
            r#"{"architecture":"sanotts","graph":"piperlite","language":"de"}"#,
        )
        .unwrap();
        let mut p = SpeechProfile::default();
        read(&mut p, &row("sanotts", "de", None), None, Some(&gguf));
        assert_eq!(p.package_language.as_deref(), Some("de"));

        std::fs::write(
            d.path().join("config.json"),
            r#"{"architecture":"sanotts"}"#,
        )
        .unwrap();
        let mut p = SpeechProfile::default();
        read(&mut p, &row("sanotts", "heart", None), None, Some(&gguf));
        assert_eq!(
            p.package_language.as_deref(),
            Some("en"),
            "nano speaks English"
        );

        std::fs::write(
            d.path().join("config.json"),
            r#"{"model_type":"zipformer2","language":{"iso":"en_US"}}"#,
        )
        .unwrap();
        let mut p = SpeechProfile::default();
        read(&mut p, &row("kroko_asr", "k", None), None, Some(&gguf));
        assert_eq!(p.package_language.as_deref(), Some("en"));
        assert!(p.problems.is_empty(), "{:?}", p.problems);

        let mut p = SpeechProfile::default();
        read(&mut p, &row("supertonic", "s", None), None, Some(&gguf));
        assert_eq!(p.package_language, None, "not a one-language family");
    }
}
