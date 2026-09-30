//! Markdown to HTML for chat replies (and the docs playground).
//!
//! Three rules on top of plain CommonMark:
//! * `$…$` / `$$…$$` become `.math-inline` / `.math-display` spans, which
//!   `assets/codeblocks.js` renders with the vendored KaTeX. Models also write
//!   `\(…\)` and `\[…\]`; a pre-pass turns those into the dollar forms first
//!   (outside fenced code and inline code spans, where they are literal text).
//! * Raw HTML in the source is emitted as escaped text. The output goes into
//!   `innerHTML` with the owner's rights, and model output (or a knowledge-base
//!   document it quotes) is untrusted. Seeing HTML rendered is what the
//!   sandboxed Preview of a fenced ```` ```html ```` block is for.
//! * Link targets are kept only for http(s), mailto and relative URLs; any
//!   other scheme (`javascript:`, `data:`, …) becomes `#`. Images are never
//!   loaded from the network: a remote image renders as a link to it, because
//!   an image URL the model was talked into writing (by a document it read)
//!   would otherwise send whatever it encodes to that host the moment the reply
//!   is shown. Only inline `data:image/…` images render as images.

use pulldown_cmark::{html, CowStr, Event, Options, Parser, Tag, TagEnd};

pub(super) fn md_to_html(src: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_MATH);
    let src = rewrite_latex_delimiters(src);
    // One entry per open image: whether it was turned into a link.
    let mut images: Vec<bool> = Vec::new();
    let events = Parser::new_ext(&src, opts).map(|ev| match ev {
        Event::Html(s) | Event::InlineHtml(s) => Event::Text(s),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: safe_href(dest_url),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            let inline = scheme_of(&dest_url).is_some_and(|s| s == "data")
                && dest_url
                    .trim_start()
                    .to_ascii_lowercase()
                    .starts_with("data:image/");
            images.push(!inline);
            if inline {
                Event::Start(Tag::Image {
                    link_type,
                    dest_url,
                    title,
                    id,
                })
            } else {
                Event::Start(Tag::Link {
                    link_type,
                    dest_url: safe_href(dest_url),
                    title,
                    id,
                })
            }
        }
        Event::End(TagEnd::Image) => {
            if images.pop().unwrap_or(false) {
                Event::End(TagEnd::Link)
            } else {
                Event::End(TagEnd::Image)
            }
        }
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, events);
    out
}

/// The URL's scheme, lowercased, if it has one. Browsers drop ASCII tabs and
/// newlines inside a URL and trim control characters and spaces around it, so
/// `java\tscript:` is `javascript:` to them and must be to this check too.
fn scheme_of(url: &str) -> Option<String> {
    let cleaned: String = url
        .trim_matches(|c: char| c.is_ascii_control() || c == ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    let end = cleaned.find([':', '/', '?', '#'])?;
    (cleaned.as_bytes()[end] == b':').then(|| cleaned[..end].to_ascii_lowercase())
}

/// A `//host/…` reference, which the browser resolves against the page's own
/// scheme and so leaves the origin. WHATWG URL parsing treats `\` as `/` in
/// http(s) URLs and drops tabs/newlines, so `\\`, `/\` and `\/` count too.
fn scheme_relative(url: &str) -> bool {
    let mut it = url
        .trim_matches(|c: char| c.is_ascii_control() || c == ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'));
    let slashy = |c: Option<char>| matches!(c, Some('/' | '\\'));
    slashy(it.next()) && slashy(it.next())
}

/// `url` if it is safe to follow from a reply, else `#`.
fn safe_href(url: CowStr<'_>) -> CowStr<'_> {
    if scheme_relative(&url) {
        return CowStr::Borrowed("#");
    }
    match scheme_of(&url).as_deref() {
        None | Some("http" | "https" | "mailto") => url,
        Some(_) => CowStr::Borrowed("#"),
    }
}

/// The opening fence of a fenced code block: (marker char, run length).
fn fence_open(line: &str) -> Option<(char, usize)> {
    let t = line.trim_start_matches(' ');
    if line.len() - t.len() > 3 {
        return None;
    }
    let c = t.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = t.chars().take_while(|x| *x == c).count();
    // A backtick fence's info string may not contain a backtick (else it is an
    // inline code span opening a line).
    (n >= 3 && !(c == '`' && t[n..].contains('`'))).then_some((c, n))
}

fn fence_closes(line: &str, (c, n): (char, usize)) -> bool {
    let t = line.trim_start_matches(' ');
    if line.len() - t.len() > 3 {
        return false;
    }
    let run = t.chars().take_while(|x| *x == c).count();
    run >= n && t[run..].trim().is_empty()
}

/// `\(x\)` → `$x$` and `\[x\]` → `$$x$$`, leaving fenced blocks and inline code
/// spans untouched.
pub(super) fn rewrite_latex_delimiters(src: &str) -> String {
    if !src.contains("\\(") && !src.contains("\\[") {
        return src.to_string();
    }
    let mut out = String::with_capacity(src.len());
    let mut prose = String::new();
    let mut fence: Option<(char, usize)> = None;
    for line in src.split_inclusive('\n') {
        match fence {
            Some(f) => {
                out.push_str(line);
                if fence_closes(line, f) {
                    fence = None;
                }
            }
            None => match fence_open(line) {
                Some(f) => {
                    out.push_str(&rewrite_prose(&prose));
                    prose.clear();
                    out.push_str(line);
                    fence = Some(f);
                }
                None => prose.push_str(line),
            },
        }
    }
    out.push_str(&rewrite_prose(&prose));
    out
}

/// Rewrite a run of non-fence lines (so a `\[ … \]` spanning lines is seen whole).
fn rewrite_prose(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        let c = rest.chars().next().unwrap();
        if c == '`' {
            let n = rest.chars().take_while(|x| *x == '`').count();
            // A code span closes on a backtick run of exactly the same length.
            match find_run(&rest[n..], n) {
                Some(end) => {
                    out.push_str(&rest[..n + end + n]);
                    i += n + end + n;
                }
                None => {
                    out.push_str(&rest[..n]);
                    i += n;
                }
            }
        } else if c == '\\' {
            let next = rest[1..].chars().next();
            let close = match next {
                Some('(') => Some(("\\)", "$")),
                Some('[') => Some(("\\]", "$$")),
                _ => None,
            };
            if let Some((close, mark)) = close {
                if let Some(end) = rest[2..].find(close) {
                    let inner = &rest[2..2 + end];
                    out.push_str(mark);
                    // `$ x $` is not math in CommonMark-math; `$$` does not care.
                    out.push_str(if mark == "$" { inner.trim() } else { inner });
                    out.push_str(mark);
                    i += 2 + end + 2;
                    continue;
                }
            }
            // An escape pair (`\\`, `\$`, an unmatched `\(`) is copied whole so
            // the second character is not read as a delimiter start.
            let take = 1 + next.map_or(0, char::len_utf8);
            out.push_str(&rest[..take]);
            i += take;
        } else {
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// Byte offset of the first run of exactly `n` backticks in `s`.
fn find_run(s: &str, n: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'`' {
            let start = i;
            while i < b.len() && b[i] == b'`' {
                i += 1;
            }
            if i - start == n {
                return Some(start);
            }
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dollar_math_becomes_math_spans() {
        let h = md_to_html("Euler: $e^{i\\pi}+1=0$ and\n\n$$\\int_0^1 x\\,dx$$\n");
        assert!(
            h.contains(r#"<span class="math math-inline">e^{i\pi}+1=0</span>"#),
            "{h}"
        );
        assert!(h.contains(r#"class="math math-display""#), "{h}");
    }

    #[test]
    fn currency_is_not_math() {
        let h = md_to_html("It costs $5 and then $10 more.");
        assert!(!h.contains("math"), "{h}");
        assert!(h.contains("$5 and then $10"), "{h}");
    }

    #[test]
    fn latex_delimiters_are_rewritten() {
        assert_eq!(rewrite_latex_delimiters(r"a \( x^2 \) b"), "a $x^2$ b");
        assert_eq!(rewrite_latex_delimiters(r"\[ x^2 \]"), "$$ x^2 $$");
        assert_eq!(
            rewrite_latex_delimiters("\\[\na = b\n\\]\n"),
            "$$\na = b\n$$\n"
        );
        let h = md_to_html(r"so \(a+b\) holds");
        assert!(
            h.contains(r#"<span class="math math-inline">a+b</span>"#),
            "{h}"
        );
    }

    #[test]
    fn rewrite_skips_code_spans_and_fences() {
        let src = "`\\(x\\)` and ``a \\[ b \\] `c` ``\n\n```tex\n\\(x\\)\n\\[y\\]\n```\n\n~~~\n\\(z\\)\n~~~\n\\(w\\)\n";
        let out = rewrite_latex_delimiters(src);
        assert!(out.contains("`\\(x\\)`"), "{out}");
        assert!(out.contains("``a \\[ b \\] `c` ``"), "{out}");
        assert!(out.contains("```tex\n\\(x\\)\n\\[y\\]\n```"), "{out}");
        assert!(out.contains("~~~\n\\(z\\)\n~~~"), "{out}");
        assert!(out.ends_with("$w$\n"), "{out}");
    }

    #[test]
    fn escaped_backslash_and_unmatched_stay_literal() {
        assert_eq!(
            rewrite_latex_delimiters(r"\\(not math\\)"),
            r"\\(not math\\)"
        );
        assert_eq!(rewrite_latex_delimiters(r"open \( only"), r"open \( only");
    }

    #[test]
    fn raw_html_is_escaped() {
        let h = md_to_html("hi <img src=x onerror=alert(1)> there\n\n<script>alert(1)</script>\n\n<div onclick=x>b</div>\n");
        assert!(!h.contains("<img"), "{h}");
        assert!(!h.contains("<script"), "{h}");
        assert!(!h.contains("<div"), "{h}");
        assert!(h.contains("&lt;img src=x onerror=alert(1)&gt;"), "{h}");
        assert!(h.contains("&lt;script&gt;"), "{h}");
    }

    #[test]
    fn scheme_relative_links_become_hash() {
        for u in [
            "//evil.example:8001/p",
            "\\\\evil/p",
            "/\\evil/p",
            "\\/evil/p",
        ] {
            assert_eq!(&*safe_href(CowStr::Borrowed(u)), "#", "{u}");
            assert_eq!(&*safe_href(CowStr::Borrowed(&format!("  {u}"))), "#", "{u}");
        }
        assert_eq!(&*safe_href(CowStr::Borrowed("/\t/evil")), "#");
        for u in [
            "/chat",
            "/a/b",
            "#frag",
            "a//b",
            "https://ok.example/",
            "./x",
        ] {
            assert_eq!(&*safe_href(CowStr::Borrowed(u)), u, "{u}");
        }
        let h = md_to_html("[x](//evil.example:8001/p)\n");
        assert!(h.contains(r##"href="#""##) && !h.contains("evil"), "{h}");
    }

    #[test]
    fn unsafe_link_schemes_become_hash() {
        let h = md_to_html(
            "[a](javascript:alert(1)) [b](<JaVa\tScript:x>) [c](data:text/html,x) [d](vbscript:x)",
        );
        assert!(!h.to_ascii_lowercase().contains("script:"), "{h}");
        assert!(!h.contains("data:"), "{h}");
        assert_eq!(h.matches(r##"href="#""##).count(), 4, "{h}");
        assert_eq!(
            scheme_of(" \u{1}java\tscr\nipt:x").as_deref(),
            Some("javascript")
        );
    }

    #[test]
    fn safe_links_are_kept() {
        let h = md_to_html(
            "[a](https://x.org/p?q=1) [b](mailto:me@x.org) [c](/chat?t=3) [d](#top) [e](notes/a:b)",
        );
        assert!(h.contains(r#"href="https://x.org/p?q=1""#), "{h}");
        assert!(h.contains(r#"href="mailto:me@x.org""#), "{h}");
        assert!(h.contains(r#"href="/chat?t=3""#), "{h}");
        assert!(h.contains(r##"href="#top""##), "{h}");
        assert!(h.contains(r#"href="notes/a:b""#), "{h}");
    }

    #[test]
    fn remote_images_become_links_and_data_images_stay() {
        let h = md_to_html("![leak](https://evil.example/p.png?d=secret) ![dot](data:image/png;base64,iVBORw0KGgo=)");
        assert!(!h.contains(r#"src="https://evil"#), "{h}");
        assert!(
            h.contains(r#"<a href="https://evil.example/p.png?d=secret">leak</a>"#),
            "{h}"
        );
        assert!(
            h.contains(r#"<img src="data:image/png;base64,iVBORw0KGgo=""#),
            "{h}"
        );
        let h = md_to_html("![x](javascript:alert(1))");
        assert!(h.contains(r##"<a href="#">x</a>"##), "{h}");
    }

    #[test]
    fn fenced_html_stays_a_code_block() {
        let h = md_to_html("```html\n<b>x</b>\n```\n");
        assert!(
            h.contains(r#"<pre><code class="language-html">&lt;b&gt;x&lt;/b&gt;"#),
            "{h}"
        );
    }
}
