//! The descriptions of both served documents hold no Rust intra-doc link.
//!
//! A doc comment becomes a `description` verbatim, and a doc comment links
//! Rust items for rustdoc (a code span in brackets, a path in brackets, a
//! link to ``crate::…``). In the document those point at nothing, so the
//! build rewrites them as plain text (`openapi/descriptions.rs`). This is the
//! check over the finished documents: it reads every `description` and
//! `summary` string, wherever the document keeps one, and names each leak by
//! its JSON pointer.
//!
//! The checker does not share code with the rewrite, so a gap in one is not a
//! gap in the other, and a planted leak shows that it can fail.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Value};

use lmgw_core::openapi::{admin_doc, v1_doc};

fn both() -> [(&'static str, &'static Value); 2] {
    [("admin", admin_doc()), ("v1", v1_doc())]
}

/// One Rust item path (`Ident(::Ident)*`, a `kind@` in front, `()` or `!` at
/// the end), as a rustdoc link target is written.
const ITEM: &str = r"(?:[a-z]+@)?[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*(?:\(\)|!)?";

struct Patterns {
    /// ``[`…`]``, with the `(target)` that follows, if one does.
    code: Regex,
    /// `](crate::`, `](self::`, `](super::`, `](Self::`.
    crate_target: Regex,
    /// `](Item)`: a link to an item whatever its prefix.
    item_target: Regex,
    /// A bare `[A::B]`, checked for what follows it separately.
    path: Regex,
    /// A whole link target, to tell an item from a URL.
    item: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        code: Regex::new(r"\[`[^`\]]+`\](?:\(([^)\s]*)\))?").unwrap(),
        crate_target: Regex::new(r"\]\((?:crate|self|super|Self)::").unwrap(),
        item_target: Regex::new(&format!(r"\]\({ITEM}\)")).unwrap(),
        path: Regex::new(r"\[(?:[A-Za-z_][A-Za-z0-9_]*::)+[A-Za-z_][A-Za-z0-9_]*(?:\(\))?\]")
            .unwrap(),
        item: Regex::new(&format!("^{ITEM}$")).unwrap(),
    })
}

/// What in `s` is intra-doc link syntax, as written. A link to a URL is not
/// one (``[`x`](https://…)`` stays a link), nor is a bracketed word that is
/// prose (`[laughing]`, `[DONE]`, `[a, b]`).
fn leaks(s: &str) -> Vec<String> {
    let p = patterns();
    let mut found = Vec::new();
    for c in p.code.captures_iter(s) {
        let url = c.get(1).is_some_and(|t| !p.item.is_match(t.as_str()));
        if !url {
            found.push(c[0].to_string());
        }
    }
    found.extend(p.crate_target.find_iter(s).map(|m| m.as_str().to_string()));
    found.extend(p.item_target.find_iter(s).map(|m| m.as_str().to_string()));
    for m in p.path.find_iter(s) {
        // `[A::B](https://…)` and `[A::B][label]` are markdown links of their
        // own, not a path that stands alone.
        if !s[m.end()..].starts_with(['(', '[']) {
            found.push(m.as_str().to_string());
        }
    }
    found
}

/// Every `description` and `summary` string below `value` with its JSON
/// pointer, under any key and at any depth, example data included: a payload
/// that quotes a Rust link would be a choice made by hand, not by accident.
fn prose<'a>(value: &'a Value, at: &str, out: &mut Vec<(String, &'a str)>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let here = format!("{at}/{}", key.replace('~', "~0").replace('/', "~1"));
                match child {
                    Value::String(s) if key == "description" || key == "summary" => {
                        out.push((here, s.as_str()));
                    }
                    _ => prose(child, &here, out),
                }
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                prose(child, &format!("{at}/{i}"), out);
            }
        }
        _ => {}
    }
}

/// The pointers of the strings in `doc` that leak, with what leaks.
fn leaking(doc: &Value) -> Vec<(String, Vec<String>)> {
    let mut strings = Vec::new();
    prose(doc, "", &mut strings);
    strings
        .into_iter()
        .filter_map(|(at, s)| {
            let found = leaks(s);
            (!found.is_empty()).then_some((at, found))
        })
        .collect()
}

#[test]
fn no_description_or_summary_holds_a_rust_link() {
    for (name, doc) in both() {
        let mut strings = Vec::new();
        prose(doc, "", &mut strings);
        // The walk saw the places prose lives, so an empty result means no
        // leak and not that nothing was read.
        for place in ["/info/", "/paths/", "/components/schemas/"] {
            assert!(
                strings.iter().any(|(at, _)| at.starts_with(place)),
                "{name}: no description or summary read under {place}"
            );
        }
        let leaked = leaking(doc);
        assert!(
            leaked.is_empty(),
            "{name}: Rust link syntax in a description or summary (write the doc \
             comment's link as it is; the build renders it as text):\n{}",
            leaked
                .iter()
                .map(|(at, found)| format!("  {at}: {found:?}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

/// What the rewrite leaves alone is still there: a bracketed word is prose.
#[test]
fn prose_brackets_stay_in_the_documents() {
    for (name, doc) in both() {
        let chunk = doc["components"]["schemas"]["ChatCompletionChunk"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(chunk.contains("`data: [DONE]`"), "{name}: {chunk:?}");
    }
    let realtime = admin_doc()["paths"]["/v1/realtime"]["get"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(realtime.contains("([laughing])"), "{realtime:?}");
}

/// Every form the rewrite takes away is a leak to the checker, and every
/// form it leaves is not.
#[test]
fn the_checker_names_each_form_of_link() {
    for leaky in [
        "see [`ThreadRow`].",
        "[`crate::status::ImageCapabilities`]",
        "[`Self::merge`]",
        "[`ImageCapabilities`](crate::status::ImageCapabilities)",
        "[`Display`](std::fmt::Display)",
        "[`Foo`][crate::Foo]",
        "[the stop](crate::Registry::stop)",
        "[the stop](Registry::stop)",
        "[the stop](self::stop)",
        "[the stop](super::stop)",
        "[the stop](Self::stop)",
        "from [Phase::Load] on",
        "[crate::a::B] and [Self::run()]",
    ] {
        assert!(!leaks(leaky).is_empty(), "not caught: {leaky:?}");
    }
    for clean in [
        "see `ThreadRow`.",
        "`status::ImageCapabilities`",
        "the stop",
        "from Phase::Load on",
        "[the spec](https://example.com/spec)",
        "[`spec`](https://example.com/spec)",
        "[`spec`](https://en.wikipedia.org/wiki/Foo_(bar))",
        "[A::B](https://example.com)",
        "say [laughing] or [whispers]",
        "ending with the literal `data: [DONE]`",
        "an array [a, b], [0], [n - 1], [] and […]",
        "`#[serde(default)]` and [tag]",
        "",
    ] {
        assert!(leaks(clean).is_empty(), "wrongly caught: {clean:?}");
    }
}

/// A leak planted in a copy of each real document is found, at its pointer
/// and nowhere else: in a schema, in an operation's summary, in the info.
#[test]
fn a_planted_leak_is_caught_in_both_documents() {
    for (name, doc) in both() {
        assert!(leaking(doc).is_empty(), "{name}: the real document leaks");

        let schema = doc["components"]["schemas"]
            .as_object()
            .and_then(|m| m.keys().next())
            .unwrap_or_else(|| panic!("{name}: no schema"))
            .clone();
        let (path, method) = doc["paths"]
            .as_object()
            .and_then(|m| {
                m.iter().find_map(|(path, ops)| {
                    let method = ops.as_object()?.keys().next()?;
                    Some((path.clone(), method.clone()))
                })
            })
            .unwrap_or_else(|| panic!("{name}: no operation"));

        let mut planted = doc.clone();
        planted["info"]["description"] = json!("uses [`Planted`](crate::planted::Planted).");
        planted["paths"][&path][&method]["summary"] = json!("see [Planted::Path]");
        planted["components"]["schemas"][&schema]["description"] = json!("as [`Self::planted`]");

        let found: Vec<String> = leaking(&planted).into_iter().map(|(at, _)| at).collect();
        let pointer = |parts: &[&str]| {
            parts
                .iter()
                .map(|p| format!("/{}", p.replace('~', "~0").replace('/', "~1")))
                .collect::<String>()
        };
        let mut expected = vec![
            pointer(&["info", "description"]),
            pointer(&["paths", &path, &method, "summary"]),
            pointer(&["components", "schemas", &schema, "description"]),
        ];
        expected.sort();
        let mut found_sorted = found.clone();
        found_sorted.sort();
        assert_eq!(
            found_sorted, expected,
            "{name}: planted leaks, found {found:?}"
        );
    }
}
