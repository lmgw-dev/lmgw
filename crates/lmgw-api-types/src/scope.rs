//! A key's scope lists — the `*`-glob allow/deny lists a key carries for the
//! aliases it may ask for and for the MCP tools it may see.
//!
//! It lives in the shared crate for the reason [`image_lab`](crate::image_lab)
//! does: both halves need the *same* answer. The gate reads it on every
//! request, and the Keys dialog previews which tools a list admits before it is
//! saved. A preview computed by a second matcher would be the one place a
//! `deny` list looked tighter than the gate it describes.

/// `*`-only glob match (no `?`, no character classes): the patterns are alias
/// and tool names, where `claude-*`, `*-mini` and `github__*` are the whole
/// vocabulary anyone needs, and a regex would be a footgun in a text box that
/// refuses requests.
///
/// **Backtracking matters here.** A greedy left-to-right scan that takes the
/// first occurrence of each literal and never retries gets `*-5` against
/// `claude-5-opus-5` wrong: it consumes the leading `-5`, finds the value does
/// not end there, and gives up. In `deny` mode that is a *fail-open* — the key
/// the owner fenced off from the `-5` generation is admitted. So this is the
/// standard two-pointer wildcard match with a restart point: linear, and right
/// on every case where a literal recurs.
pub fn glob_match(pattern: &str, value: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    let (mut pi, mut vi) = (0usize, 0usize);
    // Where to resume when the last `*` turns out to have matched too little.
    let (mut star, mut resume) = (None::<usize>, 0usize);

    while vi < v.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            resume = vi;
            pi += 1;
        } else if pi < p.len() && p[pi] == v[vi] {
            pi += 1;
            vi += 1;
        } else if let Some(sp) = star {
            // Let that `*` swallow one more character and try again.
            pi = sp + 1;
            resume += 1;
            vi = resume;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Case-fold for matching, one character at a time.
///
/// Not `str::to_lowercase`: that lowercases a capital sigma by what follows
/// it, so `ΑΣ*` folds to `ας*` while `ΑΣΑ` folds to `ασα`, and a `deny`
/// pattern would stop matching the very name it was written from. Folding each
/// character alone gives a pattern and a name the same spelling.
pub fn fold(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// Does any line of `patterns` match `value`?
///
/// One glob per line, trimmed, blanks skipped — the textarea's own shape.
/// Case-insensitive: aliases resolve case-insensitively everywhere else, and
/// for a `deny` list the looser match is the one that fails closed.
pub fn patterns_match(patterns: &str, value: &str) -> bool {
    let v = fold(value);
    patterns
        .lines()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .any(|p| glob_match(&fold(p), &v))
}

/// A scope list's verdict on `value`, with the mode in its wire spelling
/// (`all` | `allow` | `deny`). An unknown mode reads as `all`, which is what
/// the server's own parser does with one.
pub fn admits(mode: &str, patterns: &str, value: &str) -> bool {
    match mode {
        "allow" => patterns_match(patterns, value),
        "deny" => !patterns_match(patterns, value),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_prefix_glob_matches_that_server_and_nothing_else() {
        assert!(glob_match("github__*", "github__search"));
        assert!(!glob_match("github__*", "gitlab__search"));
        assert!(patterns_match(
            "docs__*\n\n  github__search  \n",
            "GitHub__Search"
        ));
    }

    #[test]
    fn a_deny_pattern_matches_the_name_it_was_written_from_in_any_script() {
        // Final-sigma folding would turn the pattern's Σ into ς and the
        // name's into σ, and the deny list would fail open.
        assert!(!admits("deny", "gh__ΑΣ*", "gh__ΑΣΑ"));
    }

    #[test]
    fn the_modes_read_the_way_the_select_says() {
        assert!(admits("all", "", "anything"));
        assert!(admits("allow", "docs__*", "docs__query"));
        assert!(!admits("allow", "docs__*", "github__search"));
        assert!(
            !admits("allow", "", "docs__query"),
            "an empty allow list admits nothing"
        );
        assert!(!admits("deny", "github__*", "github__search"));
        assert!(
            admits("deny", "", "github__search"),
            "an empty deny list denies nothing"
        );
    }
}
