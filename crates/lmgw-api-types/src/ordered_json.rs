//! A JSON pretty-printer that keeps every object's members in the order they
//! were written (labs-store design §1.3).
//!
//! `serde_json::to_string_pretty` over a `Value` would sort them: the map is
//! a `BTreeMap` in this build, and the order of a System One question's
//! options is part of the wire. So the text is parsed into a tree whose
//! objects are lists of members, and written back indented. Strings and
//! numbers are kept as the bytes they were written as (escapes, `2.50`), so
//! only the whitespace between tokens changes.
//!
//! The parser takes what RFC 8259 takes and refuses the rest (a raw control
//! character in a string, an unknown escape, a leading zero), so a text it
//! prints is one a JSON parser reads. Nesting stops at [`MAX_DEPTH`],
//! serde_json's own limit, with an `Err` rather than a stack overflow.

/// One JSON value, each token kept as written.
#[derive(Debug, Clone, PartialEq)]
enum Node<'a> {
    /// `null`, `true`, `false` or a number, as written.
    Bare(&'a str),
    /// A string with its quotes and escapes, as written.
    Str(&'a str),
    Array(Vec<Node<'a>>),
    /// The members in order, each key as written (quotes included).
    Object(Vec<(&'a str, Node<'a>)>),
}

/// The deepest nesting the printer reads: serde_json's own parse limit
/// (`remaining_depth` 128, so a 128th open bracket is refused). A text
/// deeper than that is one serde_json does not read into a value either.
pub const MAX_DEPTH: usize = 127;

/// `text` indented by two spaces per level, every member where it was.
/// `Err` says where the text stops being one JSON value, or that it nests
/// deeper than [`MAX_DEPTH`].
pub fn pretty(text: &str) -> Result<String, String> {
    let mut p = Parser {
        s: text,
        at: 0,
        depth: 0,
    };
    p.ws();
    let node = p.value()?;
    p.ws();
    if p.at != text.len() {
        return Err(format!("unexpected text after the value at byte {}", p.at));
    }
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    write(&node, 0, &mut out);
    Ok(out)
}

struct Parser<'a> {
    s: &'a str,
    at: usize,
    /// The arrays and objects open around the parser.
    depth: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.at).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.peek() == Some(b) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", b as char, self.at))
        }
    }

    fn value(&mut self) -> Result<Node<'a>, String> {
        match self.peek() {
            Some(b'{') => self.nested(Self::object),
            Some(b'[') => self.nested(Self::array),
            Some(b'"') => self.string().map(Node::Str),
            Some(b't') => self.literal("true"),
            Some(b'f') => self.literal("false"),
            Some(b'n') => self.literal("null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(format!("unexpected character at byte {}", self.at)),
            None => Err("the text ends where a value should be".into()),
        }
    }

    /// An array or object one level deeper, refused past [`MAX_DEPTH`].
    fn nested(
        &mut self,
        read: fn(&mut Self) -> Result<Node<'a>, String>,
    ) -> Result<Node<'a>, String> {
        if self.depth == MAX_DEPTH {
            return Err(format!(
                "the value nests deeper than {MAX_DEPTH} levels at byte {}: serde_json's own \
                 parse limit is 128, and the pretty view stops there too",
                self.at
            ));
        }
        self.depth += 1;
        let node = read(self);
        self.depth -= 1;
        node
    }

    fn literal(&mut self, word: &'static str) -> Result<Node<'a>, String> {
        if self.s[self.at..].starts_with(word) {
            let start = self.at;
            self.at += word.len();
            Ok(Node::Bare(&self.s[start..self.at]))
        } else {
            Err(format!("unexpected word at byte {}", self.at))
        }
    }

    fn number(&mut self) -> Result<Node<'a>, String> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        let digits = |p: &mut Self| {
            let from = p.at;
            while matches!(p.peek(), Some(b'0'..=b'9')) {
                p.at += 1;
            }
            p.at > from
        };
        let int = self.at;
        if !digits(self) {
            return Err(format!("a number without digits at byte {start}"));
        }
        if self.s.as_bytes()[int] == b'0' && self.at - int > 1 {
            return Err(format!("a number with a leading zero at byte {start}"));
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            if !digits(self) {
                return Err(format!("a number without decimals at byte {start}"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if !digits(self) {
                return Err(format!("a number without an exponent at byte {start}"));
            }
        }
        Ok(Node::Bare(&self.s[start..self.at]))
    }

    /// A string token, quotes and escapes kept. An escape is one RFC 8259
    /// names (`\"` `\\` `\/` `\b` `\f` `\n` `\r` `\t`, `\u` and four hex
    /// digits); a character below U+0020 must be escaped.
    fn string(&mut self) -> Result<&'a str, String> {
        let start = self.at;
        self.expect(b'"')?;
        loop {
            match self.peek() {
                Some(b'"') => {
                    self.at += 1;
                    return Ok(&self.s[start..self.at]);
                }
                Some(b'\\') => {
                    let at = self.at;
                    self.at += 1;
                    match self.peek() {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            self.at += 1
                        }
                        Some(b'u') => {
                            let hex = self.s.as_bytes().get(self.at + 1..self.at + 5);
                            if !hex.is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit)) {
                                return Err(format!(
                                    "a \\u escape without four hex digits at byte {at}"
                                ));
                            }
                            self.at += 5;
                        }
                        Some(_) => return Err(format!("an unknown escape at byte {at}")),
                        None => break,
                    }
                }
                Some(b) if b < 0x20 => {
                    return Err(format!(
                        "a raw control character (U+{b:04X}) in a string at byte {}: JSON wants \
                         it escaped",
                        self.at
                    ));
                }
                Some(_) => {
                    // A whole UTF-8 character: a multi-byte one never holds
                    // a quote, a backslash or a control byte.
                    self.at += 1;
                }
                None => break,
            }
        }
        Err(format!("a string from byte {start} is not closed"))
    }

    fn array(&mut self) -> Result<Node<'a>, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Ok(Node::Array(items));
        }
        loop {
            self.ws();
            items.push(self.value()?);
            self.ws();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(Node::Array(items));
                }
                _ => return Err(format!("expected ',' or ']' at byte {}", self.at)),
            }
        }
    }

    fn object(&mut self) -> Result<Node<'a>, String> {
        self.expect(b'{')?;
        let mut members = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(Node::Object(members));
        }
        loop {
            self.ws();
            let key = self.string()?;
            self.ws();
            self.expect(b':')?;
            self.ws();
            members.push((key, self.value()?));
            self.ws();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Node::Object(members));
                }
                _ => return Err(format!("expected ',' or '}}' at byte {}", self.at)),
            }
        }
    }
}

fn indent(level: usize, out: &mut String) {
    out.push('\n');
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn write(node: &Node<'_>, level: usize, out: &mut String) {
    match node {
        Node::Bare(t) | Node::Str(t) => out.push_str(t),
        Node::Array(items) if items.is_empty() => out.push_str("[]"),
        Node::Object(members) if members.is_empty() => out.push_str("{}"),
        Node::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                indent(level + 1, out);
                write(item, level + 1, out);
            }
            indent(level, out);
            out.push(']');
        }
        Node::Object(members) => {
            out.push('{');
            for (i, (key, value)) in members.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                indent(level + 1, out);
                out.push_str(key);
                out.push_str(": ");
                write(value, level + 1, out);
            }
            indent(level, out);
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn members_keep_their_order_at_every_depth() {
        let text = r#"{"z":1,"a":{"y":[{"q":true,"b":null}],"c":"x"},"m":[]}"#;
        assert_eq!(
            pretty(text).unwrap(),
            "{\n  \"z\": 1,\n  \"a\": {\n    \"y\": [\n      {\n        \"q\": true,\n        \
             \"b\": null\n      }\n    ],\n    \"c\": \"x\"\n  },\n  \"m\": []\n}"
        );
    }

    #[test]
    fn strings_and_numbers_stay_as_written() {
        let text = "{\"a\\\"b\": \"line\\nbreak \\u00e4 ü \\\\\", \"n\": 2.50, \"e\": -1E+3}";
        let out = pretty(text).unwrap();
        assert_eq!(
            out,
            "{\n  \"a\\\"b\": \"line\\nbreak \\u00e4 ü \\\\\",\n  \"n\": 2.50,\n  \"e\": -1E+3\n}"
        );
        // Pretty twice is pretty once.
        assert_eq!(pretty(&out).unwrap(), out);
    }

    #[test]
    fn a_scalar_or_an_empty_container_is_written_as_it_is() {
        assert_eq!(pretty(" \"x\" ").unwrap(), "\"x\"");
        assert_eq!(pretty("[]").unwrap(), "[]");
        assert_eq!(pretty("{ }").unwrap(), "{}");
        assert_eq!(pretty("null").unwrap(), "null");
    }

    #[test]
    fn what_rfc_8259_refuses_is_refused_as_serde_json_refuses_it() {
        let refused = [
            "\"a\u{1}b\"",
            "\"tab\there\"",
            "{\"k\": \"line\nbreak\"}",
            "\"\\x\"",
            "\"\\u12\"",
            "\"\\u12G4\"",
            "\"\\",
            "01",
            "-01",
            "[00]",
            "{\"n\": 012.5}",
            "+1",
            ".5",
        ];
        for bad in refused {
            assert!(pretty(bad).is_err(), "{bad:?}");
            assert!(
                serde_json::from_str::<serde::de::IgnoredAny>(bad).is_err(),
                "serde_json agrees: {bad:?}"
            );
        }
        for good in [
            "0",
            "-0",
            "0.5",
            "0e5",
            "10",
            "\"\\u00e4\\/\\b\"",
            "\"ü\u{7f}\"",
        ] {
            assert!(pretty(good).is_ok(), "{good:?}");
            assert!(serde_json::from_str::<serde::de::IgnoredAny>(good).is_ok());
        }
    }

    #[test]
    fn nesting_stops_where_serde_json_stops_with_an_error_not_an_overflow() {
        let nest = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
        // serde_json reading a value (a `Value`, a typed answer) stops at
        // 128; `IgnoredAny` skips without a limit and is no measure.
        for n in [MAX_DEPTH, MAX_DEPTH + 1] {
            assert_eq!(
                pretty(&nest(n)).is_ok(),
                serde_json::from_str::<serde_json::Value>(&nest(n)).is_ok(),
                "{n} levels"
            );
        }
        assert!(pretty(&nest(MAX_DEPTH)).is_ok());
        let e = pretty(&nest(MAX_DEPTH + 1)).unwrap_err();
        assert!(e.contains("deeper than 127") && e.contains("128"), "{e}");
        // Far deeper, objects and arrays mixed: still an Err.
        let deep = format!("{}1{}", "{\"a\":[".repeat(200_000), "]}".repeat(200_000));
        assert!(pretty(&deep).unwrap_err().contains("deeper than"));
    }

    #[test]
    fn what_is_no_json_value_is_refused() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\" 1}",
            "\"open",
            "1 2",
            "tru",
            "-",
            "1.",
            "{a:1}",
        ] {
            assert!(pretty(bad).is_err(), "{bad:?}");
        }
    }
}
