//! The last-resort source kind (§8): HTML reduced to headings, prose and
//! **fenced code blocks**.
//!
//! Deliberately not a full HTML parser. This path exists for sites that publish
//! no `llms.txt`, no markdown and no rustdoc JSON, and the only thing it has to
//! get right is producing text a payload can be sliced out of verbatim — so it
//! keeps the structure that matters for retrieval (what is a heading, what is
//! code) and drops everything that is chrome. A DOM crate would buy correctness
//! on malformed markup that this path is explicitly a fallback for.
//!
//! The output — not the original HTML — is what the document's spans index and
//! what the corpus stores as payload. That is the honest framing: a chunk is
//! verbatim with respect to the text the model was shown.

/// Elements whose entire subtree is chrome, never documentation.
const SKIP: [&str; 8] = [
    "script", "style", "nav", "header", "footer", "aside", "svg", "noscript",
];

/// Convert an HTML document to the markdown-ish text ingestion works on.
pub fn to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut block = String::new();
    let mut prefix = String::new();
    let mut skip_depth = 0usize;
    let mut skip_tag = String::new();
    let bytes = html.as_bytes();
    let mut i = 0usize;

    // Close the block being accumulated, if it has any content.
    macro_rules! flush {
        () => {{
            let text = collapse(&block);
            if !text.is_empty() {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&prefix);
                out.push_str(&text);
            }
            block.clear();
            prefix.clear();
        }};
    }

    while i < bytes.len() {
        if bytes[i] != b'<' {
            let end = html[i..].find('<').map_or(html.len(), |o| i + o);
            if skip_depth == 0 {
                block.push_str(&html[i..end]);
            }
            i = end;
            continue;
        }
        let Some(close) = html[i..].find('>').map(|o| i + o) else {
            break;
        };
        let raw = &html[i + 1..close];
        i = close + 1;
        if raw.starts_with('!') {
            continue; // comment, doctype
        }
        let closing = raw.starts_with('/');
        let name = tag_name(raw.trim_start_matches('/'));

        if skip_depth > 0 {
            if name == skip_tag {
                if closing {
                    skip_depth -= 1;
                } else if !raw.ends_with('/') {
                    skip_depth += 1;
                }
            }
            continue;
        }
        if !closing && SKIP.contains(&name.as_str()) {
            if !raw.ends_with('/') {
                skip_depth = 1;
                skip_tag = name;
            }
            continue;
        }

        match (closing, name.as_str()) {
            (false, "pre") => {
                flush!();
                // Everything up to `</pre>` is code, tags and all: a fenced
                // block's whole point is that its content is not re-interpreted.
                let end = html[i..].find("</pre").map_or(html.len(), |o| i + o);
                // Entities stay encoded here — the single `decode` pass at the
                // end covers them, and decoding twice would turn `&amp;lt;`
                // into `<`.
                let code = strip_tags(&html[i..end]);
                let code = code.trim_matches('\n');
                if !code.trim().is_empty() {
                    if !out.is_empty() {
                        out.push_str("\n\n");
                    }
                    out.push_str("```\n");
                    out.push_str(code);
                    out.push_str("\n```");
                }
                i = end;
            }
            (false, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") => {
                flush!();
                let level: usize = name[1..].parse().unwrap_or(1);
                prefix = format!("{} ", "#".repeat(level));
            }
            (false, "li") => {
                flush!();
                prefix = "- ".into();
            }
            (false, "br") => block.push(' '),
            (
                true,
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "p" | "li" | "td" | "th" | "div"
                | "blockquote" | "tr" | "section" | "article",
            ) => flush!(),
            (false, "p" | "td" | "th" | "div" | "blockquote" | "tr" | "section" | "article") => {
                flush!()
            }
            (false, "code") => block.push('`'),
            (true, "code") => block.push('`'),
            _ => {}
        }
    }
    flush!();
    decode(&out)
}

/// Name of a tag, lowercased: everything up to the first whitespace or `/`.
fn tag_name(raw: &str) -> String {
    raw.split(|c: char| c.is_whitespace() || c == '/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for ch in s.chars() {
        match ch {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Runs of whitespace to one space, trimmed — HTML's own whitespace rule.
fn collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
            }
            space = false;
            out.push(ch);
        }
    }
    out
}

/// The named entities that actually appear in code samples, plus the numeric
/// forms. An unknown entity is left as written rather than mangled.
fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(semi) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let replacement = match entity {
            "amp" => Some("&".to_string()),
            "lt" => Some("<".to_string()),
            "gt" => Some(">".to_string()),
            "quot" => Some("\"".to_string()),
            "apos" | "#39" => Some("'".to_string()),
            "nbsp" => Some(" ".to_string()),
            e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16)
                .ok()
                .and_then(char::from_u32)
                .map(String::from),
            e if e.starts_with('#') => e[1..]
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .map(String::from),
            _ => None,
        };
        match replacement {
            Some(r) => {
                out.push_str(&r);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_headings_and_code_and_drops_chrome() {
        let html = "<html><head><style>b{}</style></head><body>\
            <nav><a href='/x'>menu</a></nav>\
            <h2>Routing</h2>\
            <p>Handlers are   async functions.</p>\
            <pre><code>let app = Router::new()\n    .route(&quot;/&quot;, get(root));</code></pre>\
            <script>alert(1)</script>\
            </body></html>";
        let text = to_text(html);
        assert!(text.contains("## Routing"), "{text}");
        assert!(text.contains("Handlers are async functions."), "{text}");
        assert!(
            text.contains("```\nlet app = Router::new()\n    .route(\"/\", get(root));\n```"),
            "{text}"
        );
        assert!(!text.contains("menu"), "nav is chrome: {text}");
        assert!(!text.contains("alert"), "script is chrome: {text}");
    }

    #[test]
    fn code_inside_a_fence_is_not_re_interpreted() {
        // `<T>` inside a code sample must survive; a tag-stripping pass that
        // treated it as markup would silently corrupt the payload.
        let text = to_text("<pre>fn f&lt;T&gt;(x: T) {}</pre>");
        assert!(text.contains("fn f<T>(x: T) {}"), "{text}");
    }
}
