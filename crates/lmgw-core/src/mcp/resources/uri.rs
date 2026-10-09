//! A resource URI's namespace (L14): the server's tool prefix before the
//! URI's authority, `<prefix>__`, as a tool name carries it before the tool's
//! own. `ui://weather/card` from server `p` is `ui://p__weather/card`;
//! `file:///notes` is `file://p__/notes`. A URI without `scheme://` has no
//! authority to prefix and keeps its spelling.
//!
//! And RFC 6570 template matching, as far as routing a read needs it: does a
//! URI fit a server's `uriTemplate`?

/// `uri` as `/mcp` shows it for a server with tool prefix `prefix`.
pub fn namespaced(prefix: &str, uri: &str) -> String {
    let prefix = prefix.trim();
    match split(uri) {
        Some((scheme, rest)) if !prefix.is_empty() => format!("{scheme}://{prefix}__{rest}"),
        _ => uri.to_string(),
    }
}

/// The server's own spelling of `uri` when `uri` is in the namespace of
/// tool prefix `prefix`: [`namespaced`]'s inverse.
pub fn strip(prefix: &str, uri: &str) -> Option<String> {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return None;
    }
    let (scheme, rest) = split(uri)?;
    let own = rest.strip_prefix(prefix)?.strip_prefix("__")?;
    Some(format!("{scheme}://{own}"))
}

/// Does `uri` have an authority to prefix: `scheme://…`, with an RFC 3986
/// scheme?
pub fn has_authority(uri: &str) -> bool {
    split(uri).is_some()
}

/// `scheme://rest` → `(scheme, rest)`.
fn split(uri: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = uri.split_once("://")?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    let scheme_ok = first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    scheme_ok.then_some((scheme, rest))
}

/// Does `uri` fit `template`? Each `{…}` expression stands for any run of
/// characters, the literal text between them must be there as written —
/// enough to tell which server's template a URI was made from, which is all
/// a read needs. An unclosed `{` is literal text.
///
/// Linear in the URI per literal, never a backtracking search: the first
/// literal run must open the URI and the last close it, and each run
/// between them is taken at its leftmost place after the one before — a
/// later place never leaves more room for the runs after it.
pub fn template_matches(template: &str, uri: &str) -> bool {
    let runs = literal_runs(template);
    let Some((first, rest)) = runs.split_first() else {
        return uri.is_empty();
    };
    let Some((last, middle)) = rest.split_last() else {
        return uri == first;
    };
    let Some(mut open) = uri
        .strip_prefix(first.as_str())
        .and_then(|r| r.strip_suffix(last.as_str()))
    else {
        return false;
    };
    for run in middle {
        match open.find(run.as_str()) {
            Some(at) => open = &open[at + run.len()..],
            None => return false,
        }
    }
    true
}

/// May a server with tool prefix `prefix` use `template` to claim URIs? A
/// bare server, any; a prefixed server's only with a literal `scheme://`,
/// which its namespace then prefixes. One that opens with an expression
/// (`{uri}`) or has no authority (`urn:{id}`) would claim URIs no prefix
/// changes — every other server's `urn:…` and `data:…` — so it claims
/// nothing, and is not listed.
pub fn template_usable(prefix: &str, template: &str) -> bool {
    prefix.trim().is_empty() || has_authority(template)
}

/// `template`'s literal text, split at its expressions: one run more than
/// it has expressions, each possibly empty.
fn literal_runs(template: &str) -> Vec<String> {
    let mut runs = vec![String::new()];
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        runs.last_mut().unwrap().push_str(&rest[..open]);
        runs.push(String::new());
        rest = &rest[open + close + 1..];
    }
    runs.last_mut().unwrap().push_str(rest);
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prefix_goes_before_the_authority_and_comes_off_again() {
        assert_eq!(namespaced("p", "ui://weather/card"), "ui://p__weather/card");
        assert_eq!(namespaced("p", "file:///notes"), "file://p__/notes");
        assert_eq!(namespaced("", "ui://weather/card"), "ui://weather/card");
        assert_eq!(
            strip("p", "ui://p__weather/card").unwrap(),
            "ui://weather/card"
        );
        assert_eq!(strip("p", "file://p__/notes").unwrap(), "file:///notes");
        assert_eq!(strip("p", "ui://weather/card"), None);
        assert_eq!(strip("p", "ui://pq__weather/card"), None);
        assert_eq!(strip("", "ui://p__weather/card"), None);
    }

    #[test]
    fn a_uri_without_an_authority_keeps_its_spelling() {
        assert_eq!(namespaced("p", "urn:isbn:123"), "urn:isbn:123");
        assert_eq!(namespaced("p", "data:text/plain,hi"), "data:text/plain,hi");
        assert_eq!(namespaced("p", "1ab://x"), "1ab://x");
        assert!(!has_authority("urn:isbn:123"));
        assert!(has_authority("ui://x"));
    }

    #[test]
    fn a_template_matches_what_it_expands_to() {
        assert!(template_matches("file:///{path}", "file:///a/b.txt"));
        assert!(template_matches("ui://w/{city}/card", "ui://w/berlin/card"));
        assert!(!template_matches("ui://w/{city}/card", "ui://w/berlin/map"));
        assert!(template_matches("db://{a}/{b}", "db://x/y"));
        assert!(template_matches("plain://x", "plain://x"));
        assert!(!template_matches("plain://x", "plain://xy"));
        assert!(template_matches("odd://{x", "odd://{x"));
        assert!(template_matches("{a}{b}", ""));
        assert!(template_matches("x://{a}/y/{b}/y", "x://q/y/y/y"));
        assert!(!template_matches("x://{a}/y/{b}/y", "x://q/y"));
        assert!(!template_matches("ab{x}ba", "aba"));
        assert!(template_matches("ab{x}ba", "abba"));
        assert!(template_matches("ü://{x}é", "ü://ñé"));
    }

    /// A URI that a backtracking match would take forever on — many places
    /// for every expression, and no way to fit at the end — is answered at
    /// once.
    #[test]
    fn a_long_uri_that_nearly_fits_is_answered_at_once() {
        let template = "x://{a}/{b}/{c}/{d}/{e}/{f}/{g}/{h}/end";
        let uri = format!("x://{}nope", "/".repeat(200_000));
        let started = std::time::Instant::now();
        assert!(!template_matches(template, &uri));
        let fits = format!("x://{}end", "/".repeat(200_000));
        assert!(template_matches(template, &fits));
        assert!(!template_matches("x://{a}/{b}/{c}!{d}/{e}/end", &fits));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_prefixed_server_s_template_needs_a_literal_authority() {
        assert!(template_usable("p", "ui://w/{city}"));
        assert!(template_usable("p", "file:///{path}"));
        assert!(!template_usable("p", "{uri}"));
        assert!(!template_usable("p", "{scheme}://x"));
        assert!(!template_usable("p", "urn:isbn:{n}"));
        assert!(template_usable("", "{uri}"));
        assert!(template_usable(" ", "urn:isbn:{n}"));
    }
}
