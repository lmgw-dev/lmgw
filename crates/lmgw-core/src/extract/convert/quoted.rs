//! A caller's value quoted in a refusal or a note (security round B, M4):
//! at most [`MAX_QUOTED_BYTES`] of it, cut at a character boundary, and the
//! cut said with the constant, so a 5 MiB `pages` field is not echoed into
//! the answer, the usage row and the request record. Every message that
//! repeats what a caller sent quotes it through [`quoted`] (`options.rs`,
//! `pages.rs`, the docling option table).
//!
//! Whatever a message formats, the documents and docling routes bound it
//! once more where it becomes an answer (the review of round B): [`outlet`]
//! for a message, [`outlet_detail`] for docling's `detail`, at
//! `ERROR_MSG_MAX_BYTES`, the bound the usage row and a job's stored error
//! keep too. So no message path echoes megabytes, a value holding a
//! backtick or a quote, an unknown field's name or a job id included.

use serde_json::Value;

use crate::store::error_msg::{self, ERROR_MSG_MAX_BYTES};

/// The most bytes of a caller's value a message repeats.
pub const MAX_QUOTED_BYTES: usize = 64;

/// The most bytes of a run between two quotes that [`quotes_bounded`]
/// keeps: serde's own words between the values are short ("`, expected one
/// of `"); a longer run is a caller's value that held a quote mark itself
/// and closed its quotes early.
pub const MAX_UNQUOTED_RUN_BYTES: usize = 256;

/// `value` as a message repeats it: whole up to [`MAX_QUOTED_BYTES`], else
/// its start and `… (cut at 64 bytes, MAX_QUOTED_BYTES)`.
pub fn quoted(value: &str) -> String {
    match cut(value, MAX_QUOTED_BYTES) {
        (head, false) => head.to_string(),
        (head, true) => format!("{head}… (cut at {MAX_QUOTED_BYTES} bytes, MAX_QUOTED_BYTES)"),
    }
}

/// A message another library wrote about a caller's value (serde's
/// "unknown variant `…`, expected one of …"), each run of it between
/// backticks or double quotes bounded by [`quoted`], each run outside them
/// by [`MAX_UNQUOTED_RUN_BYTES`]: the list of what is expected stays whole,
/// and a value that holds a backtick or a quote itself is bounded too.
pub fn quotes_bounded(message: &str) -> String {
    let mut out = String::with_capacity(message.len().min(4096));
    let mut rest = message;
    while let Some(open) = rest.find(['`', '"']) {
        let mark = &rest[open..open + 1];
        out.push_str(&unquoted(&rest[..open]));
        out.push_str(mark);
        let after = &rest[open + 1..];
        let Some(close) = after.find(mark) else {
            // An open quote to the end: bounded as one value.
            out.push_str(&quoted(after));
            return out;
        };
        out.push_str(&quoted(&after[..close]));
        out.push_str(mark);
        rest = &after[close + 1..];
    }
    out.push_str(&unquoted(rest));
    out
}

/// A run outside quotes, within [`MAX_UNQUOTED_RUN_BYTES`].
fn unquoted(run: &str) -> std::borrow::Cow<'_, str> {
    match cut(run, MAX_UNQUOTED_RUN_BYTES) {
        (whole, false) => whole.into(),
        (head, true) => {
            format!("{head}… (cut at {MAX_UNQUOTED_RUN_BYTES} bytes, MAX_UNQUOTED_RUN_BYTES)")
                .into()
        }
    }
}

/// A refusal's message as the answer carries it (module doc): whole up to
/// `ERROR_MSG_MAX_BYTES`, else its start and the cut said.
pub fn outlet(message: &str) -> String {
    error_msg::bounded(message).into_owned()
}

/// docling's `detail` as the answer carries it (module doc): each string
/// in it bounded by [`outlet`], and a list kept while it stays within
/// `ERROR_MSG_MAX_BYTES` in all, the rest named by one last entry of
/// docling's shape.
pub fn outlet_detail(detail: Value) -> Value {
    match detail {
        Value::String(s) => Value::String(outlet(&s)),
        Value::Array(items) => {
            let total = items.len();
            let mut kept = Vec::new();
            let mut size = 0usize;
            for item in items {
                let item = outlet_detail(item);
                let len = item.to_string().len();
                if !kept.is_empty() && size + len > ERROR_MSG_MAX_BYTES {
                    break;
                }
                size += len;
                kept.push(item);
            }
            let left = total - kept.len();
            if left > 0 {
                kept.push(serde_json::json!({
                    "loc": ["body"],
                    "msg": format!(
                        "{left} more errors left out (cut at {ERROR_MSG_MAX_BYTES} bytes, \
                         ERROR_MSG_MAX_BYTES)"
                    ),
                    "type": "value_error",
                }));
            }
            Value::Array(kept)
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (outlet(&k), outlet_detail(v)))
                .collect(),
        ),
        other => other,
    }
}

/// An OpenAI-shaped error body (`{"error": {"message": …}}`) with its
/// message through [`outlet`].
pub fn outlet_openai(mut body: Value) -> Value {
    if let Some(m) = body.pointer_mut("/error/message") {
        if let Some(s) = m.as_str() {
            *m = Value::String(outlet(s));
        }
    }
    body
}

/// `s` cut to at most `max` bytes at a character boundary, and whether it
/// was cut.
pub fn cut(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_value_is_cut_and_says_so() {
        assert_eq!(quoted("3-7,10"), "3-7,10");
        let long = "Z".repeat(5 << 20);
        let q = quoted(&long);
        assert!(q.len() < 200, "{}", q.len());
        assert!(q.ends_with("… (cut at 64 bytes, MAX_QUOTED_BYTES)"), "{q}");
        // A character across the bound is left out whole.
        let wide = format!("{}é", "a".repeat(63));
        assert_eq!(cut(&wide, 64), (&wide[..63], true));
        assert_eq!(quoted(&"a".repeat(64)), "a".repeat(64));
        // serde's words around a long value stay whole.
        let said = format!(
            "unknown variant `{}`, expected one of `markdown`, `text`, `json` at line 1",
            "Z".repeat(1 << 20)
        );
        let b = quotes_bounded(&said);
        assert!(b.len() < 300, "{}", b.len());
        assert!(
            b.ends_with("(cut at 64 bytes, MAX_QUOTED_BYTES)`, expected one of `markdown`, `text`, `json` at line 1"),
            "{b}"
        );
        assert_eq!(quotes_bounded("plain"), "plain");
        assert!(quotes_bounded(&format!("open `{}", "y".repeat(500))).len() < 200);
    }

    /// The review of round B, S1: a value that holds a backtick or a quote
    /// closes its quotes early; what follows is still bounded.
    #[test]
    fn a_value_with_its_own_quote_mark_is_bounded_too() {
        let z = "Z".repeat(5 << 20);
        for said in [
            format!("unknown variant `a`{z}`, expected one of `markdown`, `text`"),
            format!("invalid type: string \"x\\\"{z}\", expected a sequence at line 1"),
            format!("unknown field `q`{z}`, expected one of `files`, `output`"),
        ] {
            let b = quotes_bounded(&said);
            assert!(b.len() < 1024, "{}", b.len());
            assert!(b.contains("MAX_UNQUOTED_RUN_BYTES"), "{b}");
        }
        // serde's own words stay whole.
        let said = "invalid type: map, expected a string at line 1 column 5";
        assert_eq!(quotes_bounded(said), said);
    }

    #[test]
    fn the_outlet_bounds_a_message_and_a_detail_list() {
        let long = "x".repeat(5 << 20);
        let m = outlet(&long);
        assert!(m.len() <= ERROR_MSG_MAX_BYTES + 64, "{}", m.len());
        assert!(
            m.ends_with("(cut at 4096 bytes, ERROR_MSG_MAX_BYTES)"),
            "{m}"
        );
        assert_eq!(outlet("short"), "short");
        let many: Vec<Value> = (0..10_000)
            .map(|i| serde_json::json!({"loc": ["body", i], "msg": "bad", "type": "value_error"}))
            .collect();
        let d = outlet_detail(Value::Array(many));
        assert!(
            d.to_string().len() < 2 * ERROR_MSG_MAX_BYTES,
            "{}",
            d.to_string().len()
        );
        let last = d.as_array().unwrap().last().unwrap();
        assert!(
            last["msg"]
                .as_str()
                .unwrap()
                .ends_with("more errors left out (cut at 4096 bytes, ERROR_MSG_MAX_BYTES)"),
            "{last}"
        );
        let one = outlet_detail(serde_json::json!([{"loc": ["body"], "msg": long, "type": "x"}]));
        assert!(one.to_string().len() < 2 * ERROR_MSG_MAX_BYTES);
        let o = outlet_openai(serde_json::json!({"error": {"message": long, "code": "c"}}));
        assert!(o.to_string().len() < 2 * ERROR_MSG_MAX_BYTES);
        assert_eq!(o["error"]["code"], "c");
    }
}
