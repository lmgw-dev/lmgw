//! The licence a GGUF header states: `general.license` (an SPDX-like id or a
//! name), with the optional `general.license.name` and `general.license.link`
//! (decision-models design §1.5, §3.9). Information only: nothing reads it to
//! filter, rank or warn.

use serde::Serialize;

use super::GgufMeta;

/// What the header says about the model's licence. At least one field is set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Licence {
    /// `general.license`.
    pub id: Option<String>,
    /// `general.license.name`.
    pub name: Option<String>,
    /// `general.license.link`.
    pub link: Option<String>,
}

impl Licence {
    /// `None` when the header carries none of the three keys. A key whose
    /// value is blank counts as absent.
    pub fn from_meta(meta: &GgufMeta) -> Option<Self> {
        let read = |key: &str| {
            meta.string(key)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let l = Self {
            id: read("general.license"),
            name: read("general.license.name"),
            link: read("general.license.link"),
        };
        (l.id.is_some() || l.name.is_some() || l.link.is_some()).then_some(l)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{summarize, synth};

    fn read(h: &synth::Header) -> Option<super::Licence> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("m.gguf");
        h.write_to(&p);
        summarize(&p).unwrap().licence
    }

    #[test]
    fn all_three_keys_are_read() {
        let mut h = synth::chat("llama", 4096);
        h.str("general.license", "other")
            .str("general.license.name", "Gemma Terms of Use")
            .str("general.license.link", "https://ai.google.dev/gemma/terms");
        let l = read(&h).unwrap();
        assert_eq!(l.id.as_deref(), Some("other"));
        assert_eq!(l.name.as_deref(), Some("Gemma Terms of Use"));
        assert_eq!(l.link.as_deref(), Some("https://ai.google.dev/gemma/terms"));
    }

    #[test]
    fn the_id_alone_is_enough() {
        let mut h = synth::chat("llama", 4096);
        h.str("general.license", "apache-2.0");
        let l = read(&h).unwrap();
        assert_eq!(l.id.as_deref(), Some("apache-2.0"));
        assert_eq!((l.name, l.link), (None, None));
    }

    #[test]
    fn a_header_without_the_keys_has_no_licence() {
        assert_eq!(read(&synth::chat("llama", 4096)), None);
    }

    #[test]
    fn blank_values_count_as_absent() {
        let mut h = synth::chat("llama", 4096);
        h.str("general.license", "  ")
            .str("general.license.name", "");
        assert_eq!(read(&h), None);
    }
}
