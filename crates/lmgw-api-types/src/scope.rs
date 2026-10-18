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

/// The text of an allow `pattern` before its first `*` (the whole pattern when
/// it has none), folded: the part that fixes the names it can match.
fn head(pattern: &str) -> String {
    fold(pattern.trim().split('*').next().unwrap_or_default())
}

/// Does the allow `pattern` name the device-hosted `label`: is the text before
/// its first `*` `<label>__…`, case-insensitively (`kai__*`, `kai__notify`)?
/// Under the device-tools grant (client-apps design L16, 2026-10-15) such a
/// pattern only filters the label's tools; the grant is what reaches them.
pub fn names_label(pattern: &str, label: &str) -> bool {
    head(pattern).starts_with(&fold(&format!("{}__", label.trim())))
}

/// Could the allow `pattern` match a tool of `label` without naming it: has a
/// `*`, does not name the label, and the text before that `*` is a prefix of
/// `<label>__` (`*`, `k*`, `ka*`, `*__search`)?
pub fn broad_for_label(pattern: &str, label: &str) -> bool {
    let ns = fold(&format!("{}__", label.trim()));
    let h = head(pattern);
    pattern.contains('*') && !h.starts_with(&ns) && ns.starts_with(&h)
}

/// Whether migration 0082 grants `label` to a key whose `allow` list is
/// `patterns`: a line names the label and no other line could match its
/// tools. Granting a mixed list (`*` and `kai__notify`) would let its broad
/// line admit every tool of the label, where before only the naming line
/// filtered them; such a key is left without the grant, so it loses reach
/// rather than gaining it. The SQL in `0082_key_device_tools.sql` implements
/// this rule; keep them equal.
pub fn migration_grants(patterns: &str, label: &str) -> bool {
    let lines = || patterns.lines().map(str::trim).filter(|p| !p.is_empty());
    lines().any(|p| names_label(p, label)) && !lines().any(|p| broad_for_label(p, label))
}

/// The labels among `hosted` that the allow `patterns` name and `grant` does
/// not hold: the tool scope filters a label's tools, but only the grant
/// reaches them, so such a pattern does nothing until the grant is given.
pub fn ungranted_named<'a>(patterns: &str, grant: &[String], hosted: &'a [String]) -> Vec<&'a str> {
    hosted
        .iter()
        .map(String::as_str)
        .filter(|l| !grant.iter().any(|g| fold(g.trim()) == fold(l.trim())))
        .filter(|l| patterns.lines().any(|p| names_label(p, l)))
        .collect()
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

    #[test]
    fn the_migration_grants_a_label_only_to_a_list_no_broad_line_could_widen() {
        assert!(migration_grants("kai__notify", "kai"));
        assert!(migration_grants("docs__*\n  KAI__x*  ", "kai"));
        assert!(!migration_grants("*\nkai__notify", "kai"));
        assert!(!migration_grants("docs__*\nkai__echo\n*__search", "kai"));
        assert!(!migration_grants("k*\nkai__x", "kai"));
        assert!(!migration_grants("ka*\nkai__x", "kai"));
        // Other lines that cannot reach the label's names do not matter.
        assert!(migration_grants("d*\nkai__x\nkai_b__y*", "kai"));
        assert!(!migration_grants("d*\nkai_b__y*", "kai"), "names nothing");
    }

    #[test]
    fn a_pattern_naming_a_label_is_flagged_until_the_grant_holds_it() {
        let hosted = vec!["kai".to_string(), "desk".to_string()];
        assert_eq!(
            ungranted_named("kai__*\nDESK__x", &[], &hosted),
            ["kai", "desk"]
        );
        assert_eq!(
            ungranted_named("kai__*\nDESK__x", &["KAI".to_string()], &hosted),
            ["desk"]
        );
        assert!(ungranted_named("k*\n*", &[], &hosted).is_empty());
    }
}
