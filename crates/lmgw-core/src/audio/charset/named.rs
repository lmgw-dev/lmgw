//! Codepoints as a message names them.

use unicode_normalization::char::is_combining_mark;

/// `U+201E „, U+2028`: each codepoint, with the character beside it where
/// it shows (not a control, a space or a combining mark).
pub fn named(cps: &[u32]) -> String {
    cps.iter()
        .map(|&cp| match char::from_u32(cp) {
            Some(c) if !(c.is_control() || c.is_whitespace() || is_combining_mark(c)) => {
                format!("U+{cp:04X} {c}")
            }
            _ => format!("U+{cp:04X}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_character_that_shows_is_named_beside_its_codepoint() {
        assert_eq!(
            named(&[0x201E, 0x2028, 0x0301, 0x1F60A]),
            "U+201E \u{201E}, U+2028, U+0301, U+1F60A \u{1F60A}"
        );
        assert_eq!(named(&[]), "");
    }
}
