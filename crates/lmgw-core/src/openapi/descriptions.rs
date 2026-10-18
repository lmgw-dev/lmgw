//! Rust intra-doc links rendered as plain text in the finished document.
//!
//! A doc comment becomes a `description` verbatim (schemars), and a doc
//! comment links Rust items for rustdoc: ``[`ThreadRow`]``,
//! ``[`X`](crate::path::X)``, `[Phase::Load]`. In the document they point at
//! nothing, so a reader (the API page, a client generator, the project site)
//! shows the brackets and the Rust paths. `plain_links` keeps the text and
//! drops the link:
//!
//! - ``[`X`]``, ``[`X`](item::path)`` and ``[`X`][ref]`` become `` `X` ``: the
//!   code span stays one;
//! - `[text](item::path)` becomes `text`;
//! - a bare `[A::B]` (one `::` at least) becomes `A::B`;
//! - in the text that is shown, a leading `crate::`, `self::`, `super::` or
//!   `Self::` (repeated) and a rustdoc disambiguator (`struct@`) are dropped,
//!   for rustdoc needs them and a reader does not.
//!
//! What is not a link to a Rust item stays as written: a link to a URL
//! (``[`x`](https://example.com)``, `[text](https://example.com)`), and a
//! bracketed word or list that is prose (`[laughing]`, a TTS tag the docs
//! quote; `[a, b]`; `[n - 1]`). An "item path" is `Ident(::Ident)*`, with an
//! optional `kind@` in front and a `()` or `!` at the end.

use std::sync::OnceLock;

use regex::{Captures, Regex};
use serde_json::Value;

use lmgw_api_types::openapi_ext;

/// The keys whose string is prose.
const PROSE: &[&str] = &["description", "summary"];

/// The keys whose value is data, not prose: an example payload may hold a
/// `description` of its own, which is not ours to rewrite.
const DATA: &[&str] = &["example", "examples", "default", "enum", "const"];

/// The keys whose object maps a *name* to a schema, a response or an event:
/// a property a Rust struct calls `default` or `examples` (the chat profile
/// has one), the `default` response, or an event with such a name, is a name
/// there, not a data keyword.
pub(super) const NAME_MAPS: &[&str] = &[
    "properties",
    "patternProperties",
    "responses",
    "$defs",
    "definitions",
    "schemas",
    openapi_ext::SSE_EVENTS,
];

/// The `kind@` prefixes rustdoc knows. Anything else before an `@` is not a
/// disambiguator (a code span can hold `user@host`).
const DISAMBIGUATORS: &[&str] = &[
    "struct",
    "enum",
    "union",
    "trait",
    "type",
    "tyalias",
    "typealias",
    "mod",
    "module",
    "const",
    "constant",
    "static",
    "fn",
    "function",
    "method",
    "field",
    "variant",
    "macro",
    "derive",
    "attr",
    "prim",
    "primitive",
    "value",
];

/// Rewrite every `description` and `summary` string in `doc` with `plain`,
/// wherever the document keeps one, except in example data (`DATA`).
pub(super) fn plain_links(doc: &mut Value) {
    walk(doc, false);
}

/// `names`: `value` is a map from names to schemas, so its keys are not
/// keywords.
fn walk(value: &mut Value, names: bool) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if names {
                    walk(child, false);
                } else if let Value::String(s) = child {
                    if PROSE.contains(&key.as_str()) {
                        *s = plain(s);
                    }
                } else if !DATA.contains(&key.as_str()) {
                    walk(child, NAME_MAPS.contains(&key.as_str()));
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| walk(item, false)),
        _ => {}
    }
}

/// One item path: `Ident(::Ident)*`, an optional `kind@` in front, a `()` or
/// `!` at the end.
const ITEM: &str = r"(?:[a-z]+@)?[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*(?:\(\)|!)?";

/// The same with at least one `::`.
const ITEM_PATH: &str =
    r"(?:[a-z]+@)?[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)+(?:\(\)|!)?";

struct Rules {
    /// ``[`text`]``, then a `(target)` or a `[reference]` if one follows.
    code: Regex,
    /// `[text](target)`, the text without brackets.
    text: Regex,
    /// `[A::B]`.
    path: Regex,
    /// A target that is an item path, not a URL or a fragment.
    item: Regex,
}

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| {
        let compile = |pattern: String| Regex::new(&pattern).expect("a fixed pattern compiles");
        Rules {
            code: compile(format!(r"\[`([^`\]]+)`\](?:\(([^)\s]*)\)|\[({ITEM})?\])?")),
            text: compile(format!(r"\[([^\[\]]+)\]\(({ITEM})\)")),
            path: compile(format!(r"\[({ITEM_PATH})\]")),
            item: compile(format!("^{ITEM}$")),
        }
    })
}

/// `text` as a reader sees it: without what only rustdoc needs, a
/// `kind@` disambiguator and a leading `crate::`, `self::`, `super::` or
/// `Self::` (repeated).
fn shown(text: &str) -> &str {
    let mut t = text;
    if let Some((kind, rest)) = t.split_once('@') {
        if DISAMBIGUATORS.contains(&kind) {
            t = rest;
        }
    }
    while let Some(rest) = ["crate::", "self::", "super::", "Self::"]
        .iter()
        .find_map(|prefix| t.strip_prefix(prefix))
    {
        t = rest;
    }
    // A bare `crate::` leaves nothing to show: keep what was written.
    if t.is_empty() {
        text
    } else {
        t
    }
}

/// `s` with its intra-doc links as plain text (module doc).
pub(super) fn plain(s: &str) -> String {
    if !s.contains('[') {
        return s.to_string();
    }
    let rules = rules();
    let s = rules.code.replace_all(s, |c: &Captures| match c.get(2) {
        // A link to a URL stays one.
        Some(target) if !rules.item.is_match(target.as_str()) => c[0].to_string(),
        _ => format!("`{}`", shown(&c[1])),
    });
    let s = rules.text.replace_all(&s, |c: &Captures| c[1].to_string());
    let out = rules.path.replace_all(&s, |c: &Captures| {
        // Followed by `(…)` or `[…]` it is a markdown link of its own: the
        // passes above took the ones to items, so this one goes elsewhere.
        let end = c.get(0).map_or(0, |m| m.end());
        if s[end..].starts_with(['(', '[']) {
            c[0].to_string()
        } else {
            shown(&c[1]).to_string()
        }
    });
    out.into_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{plain, plain_links};

    #[test]
    fn code_links_keep_their_code_span() {
        assert_eq!(plain("see [`ThreadRow`]."), "see `ThreadRow`.");
        assert_eq!(
            plain("[`crate::status::ImageCapabilities`] and [`Self::ALL`]"),
            "`status::ImageCapabilities` and `ALL`"
        );
        assert_eq!(
            plain("[`crate::chat_approvals::MOVED_ON`]"),
            "`chat_approvals::MOVED_ON`"
        );
        assert_eq!(plain("[`Hello::principal`]"), "`Hello::principal`");
        assert_eq!(plain("[`Self::not_run`]"), "`not_run`");
        assert_eq!(plain("[`Vec<T>`]"), "`Vec<T>`");
        assert_eq!(plain("[`f()`] and [`m!`]"), "`f()` and `m!`");
    }

    #[test]
    fn code_links_with_a_target_or_a_reference() {
        assert_eq!(
            plain("[`ImageCapabilities`](crate::status::ImageCapabilities)"),
            "`ImageCapabilities`"
        );
        assert_eq!(plain("[`Display`](std::fmt::Display)"), "`Display`");
        assert_eq!(plain("[`Foo`](struct@Foo)"), "`Foo`");
        assert_eq!(plain("[`Foo`](Foo)"), "`Foo`");
        assert_eq!(plain("[`Foo`][crate::Foo]"), "`Foo`");
        assert_eq!(plain("[`Foo`][]"), "`Foo`");
        // Not a reference to an item: the code link alone is rewritten, the
        // brackets after it are prose.
        assert_eq!(plain("[`a`][0]"), "`a`[0]");
    }

    #[test]
    fn disambiguators_and_repeated_prefixes_are_dropped() {
        assert_eq!(plain("[`struct@Foo`]"), "`Foo`");
        assert_eq!(plain("[`fn@crate::run`]"), "`run`");
        assert_eq!(plain("[`crate::self::super::Foo`]"), "`Foo`");
        assert_eq!(plain("[struct@Foo::Bar]"), "Foo::Bar");
        // Not a disambiguator: an address in a code span.
        assert_eq!(plain("[`user@host`]"), "`user@host`");
        // Nothing left to show: keep what was written.
        assert_eq!(plain("[`crate::`]"), "`crate::`");
        // A word that only starts like a prefix.
        assert_eq!(
            plain("[`crates::Foo`] [`selfish::Foo`]"),
            "`crates::Foo` `selfish::Foo`"
        );
    }

    #[test]
    fn paths_lose_their_brackets() {
        assert_eq!(plain("from [Phase::Load] on"), "from Phase::Load on");
        assert_eq!(
            plain("[Phase::Load] and [Phase::Decode]."),
            "Phase::Load and Phase::Decode."
        );
        assert_eq!(plain("[super::x::y]"), "x::y");
        assert_eq!(plain("[Self::run()]"), "run()");
        assert_eq!(plain("([crate::a::B])"), "(a::B)");
    }

    #[test]
    fn text_links_to_items_keep_their_text() {
        assert_eq!(plain("[the stop](Registry::stop)"), "the stop");
        assert_eq!(plain("[the stop](Registry::stop())"), "the stop");
        assert_eq!(plain("[a `Foo`](crate::Foo)"), "a `Foo`");
        assert_eq!(plain("[one\ntwo](crate::a)"), "one\ntwo");
        assert_eq!(plain("[A::B](crate::A::B)"), "A::B");
        assert_eq!(plain("[a](struct@A) and [b](B)"), "a and b");
    }

    #[test]
    fn real_links_and_prose_brackets_stay() {
        for s in [
            "[the spec](https://example.com/spec)",
            "[`spec`](https://example.com/spec)",
            "[`spec`](https://en.wikipedia.org/wiki/Foo_(bar))",
            "[`x`](#anchor)",
            "[the page](/api/docs)",
            "[A::B](https://example.com)",
            "[A::B][label]",
            "say [laughing] or [whispers]",
            "an array [a, b], [0], [n - 1], [] and […]",
            "[image/png image, N base64 bytes — omitted]",
            "[{id, type:function, function:{name}}]",
            "`#[serde(default)]` and [tag]",
            "data: [DONE]",
            "[x]",
            "[",
            "]",
            "[`unclosed",
            "no links here",
            "",
        ] {
            assert_eq!(plain(s), s, "{s:?}");
        }
    }

    #[test]
    fn a_string_with_several_kinds_is_rewritten_once_each() {
        assert_eq!(
            plain(
                "uses [`ImageCapabilities`](crate::status::ImageCapabilities), see \
                 [the spec](https://example.com), [Phase::Load], [laughing] and [`Self::merge`]."
            ),
            "uses `ImageCapabilities`, see [the spec](https://example.com), Phase::Load, \
             [laughing] and `merge`."
        );
    }

    #[test]
    fn plain_is_idempotent() {
        for s in [
            "[`crate::a::B`] [`B`](crate::a::B) [t](a::B) [a::B] [`x`](https://x.y) [laughing]",
            "[`Self::x`] and [`Self::y`]",
        ] {
            let once = plain(s);
            assert_eq!(plain(&once), once, "{s:?}");
        }
    }

    /// `description` and `summary` are rewritten wherever the document keeps
    /// one; nothing else is.
    #[test]
    fn plain_links_rewrites_descriptions_and_summaries_only() {
        let mut doc = json!({
            "info": { "description": "[`A`]", "title": "[`A`]" },
            "tags": [{ "name": "t", "description": "[`B`] and [C::D]" }],
            "paths": { "/x": { "get": {
                "summary": "[`E`](crate::E)",
                "x-note": "[`F`]",
                "responses": { "200": { "description": "[`G`]" } },
            } } },
            "components": { "schemas": { "S": {
                "description": "[`H`]",
                "properties": { "p": { "description": "[`I`]" } },
            } } },
        });
        plain_links(&mut doc);
        assert_eq!(doc["info"]["description"], "`A`");
        assert_eq!(doc["info"]["title"], "[`A`]");
        assert_eq!(doc["tags"][0]["description"], "`B` and C::D");
        assert_eq!(doc["paths"]["/x"]["get"]["summary"], "`E`");
        assert_eq!(doc["paths"]["/x"]["get"]["x-note"], "[`F`]");
        assert_eq!(
            doc["paths"]["/x"]["get"]["responses"]["200"]["description"],
            "`G`"
        );
        assert_eq!(doc["components"]["schemas"]["S"]["description"], "`H`");
        assert_eq!(
            doc["components"]["schemas"]["S"]["properties"]["p"]["description"],
            "`I`"
        );
    }

    /// Example data is data: a payload may hold a `description` that is not
    /// ours. Nothing under `example`, `examples`, `default`, `enum` or `const`
    /// changes.
    #[test]
    fn plain_links_leaves_example_data_alone() {
        let data = json!({ "description": "[`X`]", "summary": "[Y::Z]", "n": ["[`X`]"] });
        let schema = json!({
            "example": data.clone(),
            "default": data.clone(),
            "const": "[`X`]",
            "enum": ["[`X`]"],
            "examples": [data.clone()],
        });
        let mut doc = json!({ "description": "[`X`]", "schema": schema.clone() });
        plain_links(&mut doc);
        assert_eq!(doc["description"], "`X`");
        assert_eq!(doc["schema"], schema);
    }

    /// A property a Rust struct calls `default`, `examples` or `enum`, and
    /// the `default` response, are names in their map: their descriptions are
    /// prose like the others.
    #[test]
    fn plain_links_reads_a_name_that_is_a_data_keyword_as_a_name() {
        let mut doc = json!({
            "components": { "schemas": { "Profile": { "properties": {
                "examples": { "description": "see [`Example`]", "default": [] },
                "default": { "description": "see [`Other`]" },
                "enum": { "description": "see [`Third`]" },
            } } } },
            "paths": { "/x": { "get": { "responses": {
                "default": { "description": "see [`ApiError`]" },
                "200": { "x-lmgw-sse-events": {
                    "default": { "description": "see [`Frame`]" },
                } },
            } } } },
        });
        plain_links(&mut doc);
        let props = &doc["components"]["schemas"]["Profile"]["properties"];
        assert_eq!(props["examples"]["description"], "see `Example`");
        assert_eq!(props["examples"]["default"], json!([]));
        assert_eq!(props["default"]["description"], "see `Other`");
        assert_eq!(props["enum"]["description"], "see `Third`");
        assert_eq!(
            doc["paths"]["/x"]["get"]["responses"]["default"]["description"],
            "see `ApiError`"
        );
        assert_eq!(
            doc["paths"]["/x"]["get"]["responses"]["200"]["x-lmgw-sse-events"]["default"]
                ["description"],
            "see `Frame`"
        );
    }

    /// Under `properties` a property may be called `description`, and its
    /// value is a schema: it is walked, not mistaken for prose.
    #[test]
    fn plain_links_walks_a_property_called_description() {
        let mut doc = json!({ "properties": { "description": {
            "type": "string", "description": "the [`Text`]",
        } } });
        plain_links(&mut doc);
        assert_eq!(
            doc["properties"]["description"]["description"],
            "the `Text`"
        );
    }
}
