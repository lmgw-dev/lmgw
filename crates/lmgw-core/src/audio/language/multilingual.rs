//! `capabilities.multilingual`: a family whose spec says it covers many
//! languages without naming them. The audio catalog writes that as
//! `multilingual`, or a count — `80+ languages`, `600+ languages`,
//! `51 languages` — in the spec's `languages`, where a family that does list
//! its languages has codes or names. Entries that say nothing about
//! coverage (`auto`, `language_agnostic`, `model_dependent`, `zh dialects`)
//! do not count.

use super::SpeechProfile;

/// Whether one `languages` entry is a "many languages" statement.
fn says_many(entry: &str) -> bool {
    let entry = entry.trim().to_ascii_lowercase();
    if entry == "multilingual" {
        return true;
    }
    let Some(count) = entry.strip_suffix(" languages") else {
        return false;
    };
    let digits = count.strip_suffix('+').unwrap_or(count);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// `Some(true)` when the row's spec says the model is multilingual without
/// listing codes; `None` otherwise (never `Some(false)`: a single-language
/// model is told by its one code, and an unknown stays unknown).
pub fn multilingual(profile: &SpeechProfile) -> Option<bool> {
    profile
        .spec_languages
        .iter()
        .any(|e| says_many(e))
        .then_some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(languages: &[&str]) -> SpeechProfile {
        SpeechProfile {
            spec_languages: languages.iter().map(|l| l.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_family_that_only_counts_its_languages_is_multilingual() {
        for entry in [
            "multilingual",
            "80+ languages",
            "600+ languages",
            "51 languages",
        ] {
            assert_eq!(multilingual(&profile(&[entry])), Some(true), "{entry}");
        }
        assert_eq!(
            multilingual(&profile(&["auto", "50+ languages"])),
            Some(true)
        );
    }

    #[test]
    fn a_family_that_lists_codes_or_says_nothing_about_coverage_is_not_told() {
        for entries in [
            &["en", "de"][..],
            &["en"],
            &["auto"],
            &["language_agnostic"],
            &["zh", "zh dialects"],
            &[],
        ] {
            assert_eq!(multilingual(&profile(entries)), None, "{entries:?}");
        }
    }
}
