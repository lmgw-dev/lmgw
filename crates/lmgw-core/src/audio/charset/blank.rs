//! A text the engine's own rewrites leave blank (review TC-13).

use super::CharVocab;

impl CharVocab {
    /// Nothing is left of `s` once the engine has rewritten it: white space
    /// and the characters it writes as nothing or as white space
    /// (`♥ ☆ ♡ © \` and `# _ [ ] | / → ←`) only. The engine then trims
    /// and says a short blip for a "." of its own.
    pub fn blank_once_written(&self, s: &str) -> bool {
        s.chars()
            .all(|c| c.is_whitespace() || self.blank_rewrites.contains(&c))
    }
}
