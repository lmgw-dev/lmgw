//! A name matches by its words, counted (find-a-model design D13, D18).
//!
//! The query and each repo name are split into words at spaces, `-`, `_`,
//! `/`, and where a letter is followed by a digit ("qwen3.6" → qwen, 3.6;
//! "27b" stays one word), lower case. Each query word is exact (one of the
//! name's words), loose (it only starts one) or missing. Names are searched
//! as typed (D18): no word stands for another.

/// The words of `s` (module doc).
pub fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in s
        .to_lowercase()
        .split(|c: char| c.is_whitespace() || matches!(c, '-' | '_' | '/'))
    {
        let mut word = String::new();
        let mut last_letter = false;
        for c in part.chars() {
            if c.is_ascii_digit() && last_letter && !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            last_letter = c.is_alphabetic();
            word.push(c);
        }
        if !word.is_empty() {
            out.push(word);
        }
    }
    out
}

/// How a name matches the query's words.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Match {
    /// Query words that are one of the name's words.
    pub exact: usize,
    /// Query words that only start one of them.
    pub loose: usize,
    /// Query words that are neither.
    pub missing: usize,
}

impl Match {
    /// Every query word exact or loose: the name can be a card.
    pub fn full(&self) -> bool {
        self.missing == 0 && self.exact + self.loose > 0
    }

    /// How many query words matched at all.
    pub fn matched(&self) -> usize {
        self.exact + self.loose
    }
}

/// How `name` (a repo's name, without its owner) matches `query`'s words.
pub fn matches(query: &[String], name: &str) -> Match {
    let n = words(name);
    let mut m = Match::default();
    for q in query {
        if n.iter().any(|w| w == q) {
            m.exact += 1;
        } else if n.iter().any(|w| w.starts_with(q.as_str())) {
            m.loose += 1;
        } else {
            m.missing += 1;
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_split_as_d13_says() {
        assert_eq!(words("qwen3.6"), ["qwen", "3.6"]);
        assert_eq!(words("Qwen 3.6"), ["qwen", "3.6"]);
        assert_eq!(words("llama 3 8b"), ["llama", "3", "8b"]);
        assert_eq!(words("Qwen3.6-27B"), ["qwen", "3.6", "27b"]);
        assert_eq!(words("gemma-4-E4B-it"), ["gemma", "4", "e", "4b", "it"]);
        assert_eq!(words("DeepSeek_V4/Flash"), ["deepseek", "v", "4", "flash"]);
        assert_eq!(words("  "), Vec::<String>::new());
    }

    #[test]
    fn a_word_is_exact_loose_or_missing() {
        let q = words("gemma 4");
        assert_eq!(
            matches(&q, "gemma-4-31B-it"),
            Match {
                exact: 2,
                loose: 0,
                missing: 0
            }
        );
        // "4" only starts "4b": loose.
        assert_eq!(
            matches(&q, "gemma-3-4b-it"),
            Match {
                exact: 1,
                loose: 1,
                missing: 0
            }
        );
        // D18: a number word is a word like any other.
        let four = words("gemma four");
        assert!(!matches(&four, "gemma-4-31B-it").full());
        assert!(matches(&four, "nous-gemma-four").full());
    }
}
