//! HTML to markdown (document-conversion design §4.2, §5.2): web pages, an
//! EPUB's chapters and an e-mail's HTML body. The page is parsed by html5ever
//! and written as markdown by `htmd` (headings, lists, links, emphasis,
//! code), with lmgw's own table writer ([`table`]) and with what a reader
//! does not see, or what would reach out, left out:
//!
//! - scripts, styles, `noscript`, templates and the document's head are
//!   skipped, as are embedded frames, objects, media, canvases and SVG;
//! - an element with the `hidden` attribute, or whose own `style` says
//!   `display:none` or `visibility:hidden`, is skipped with its content
//!   ([`is_hidden`]). Text hidden by a stylesheet, a class or `font-size:0`
//!   is not told apart yet: no CSS is read;
//! - nothing is fetched: every picture (`img`, `picture`) becomes docling's
//!   placeholder `<!-- image -->`, so a remote image's address and a `data:`
//!   URI's bytes stay out; links keep their address as text;
//! - TeX math in a `span.math` (pandoc's `--mathjax` HTML) is written as
//!   `$…$` and `$$…$$` ([`math`]).
//!
//! Hostile input is this process's problem, not the gateway's: html5ever's
//! time grows with the nesting depth and with what its tree builder
//! reconstructs, and a page can ask for any amount of tree. The run's
//! timeout and memory limit end such a page as its own named failure. The
//! one bound kept here is the one that gives a better answer than a crash:
//! htmd writes the tree recursively, so the parsed tree's depth is counted
//! (without recursion, on the tree itself) and a page nested deeper than
//! [`MAX_HTML_DEPTH`] is answered as too deep instead of overflowing the
//! stack. Before that, the parse itself is bounded ([`depth`], security
//! round A): a page nested past four times that bound is answered too deep
//! as soon as its parse gets there, so html5ever's time, which grows with
//! the square of the nesting, never runs to the reader's timeout.

use htmd::element_handler::{Element, HandlerResult, Handlers};
use htmd::HtmlToMarkdown;
use lmgw_doc_reader::wire::{MAX_HTML_DEPTH, PICTURE};

mod depth;
mod math;
mod table;

/// The elements left out with their content.
const SKIPPED: &[&str] = &[
    "head", "script", "style", "noscript", "template", "iframe", "frame", "frameset", "object",
    "embed", "video", "audio", "canvas", "svg", "math", "map",
];

/// The elements [`is_hidden`] is asked about: what holds text a reader
/// would see. `html` and `body` are not among them: a page that hides its
/// body until a script shows it is still that page.
const HIDEABLE: &[&str] = &[
    "a",
    "abbr",
    "address",
    "article",
    "aside",
    "b",
    "blockquote",
    "button",
    "center",
    "cite",
    "code",
    "dd",
    "del",
    "details",
    "dialog",
    "div",
    "dl",
    "dt",
    "em",
    "fieldset",
    "figcaption",
    "figure",
    "font",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "i",
    "ins",
    "label",
    "li",
    "main",
    "mark",
    "nav",
    "ol",
    "p",
    "pre",
    "q",
    "s",
    "section",
    "small",
    "span",
    "strong",
    "sub",
    "summary",
    "sup",
    "u",
    "ul",
];

/// A page's markdown, or how deep it nests when that is past the bound.
#[derive(Debug)]
pub struct Markdown {
    pub text: String,
    pub too_deep: Option<usize>,
}

/// Whether the element itself says it is not shown: the `hidden` attribute
/// (not `hidden="until-found"`, which a search reveals), or an inline style
/// of `display:none` or `visibility:hidden`.
fn is_hidden(node: &htmd::node::Node<'_>) -> bool {
    if node
        .attr("hidden")
        .is_some_and(|v| !v.eq_ignore_ascii_case("until-found"))
    {
        return true;
    }
    node.attr("style").is_some_and(|style| {
        let s: String = style
            .chars()
            .filter(|c| !c.is_whitespace())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        s.split(';')
            .any(|d| d == "display:none" || d == "visibility:hidden")
    })
}

fn converter() -> HtmlToMarkdown {
    HtmlToMarkdown::builder()
        .skip_tags(SKIPPED.to_vec())
        .add_handler(vec!["img", "picture"], |_: &dyn Handlers, _: &Element| {
            Some(format!("\n\n{PICTURE}\n\n").into())
        })
        .add_handler(
            HIDEABLE.to_vec(),
            |h: &dyn Handlers, e: &Element| -> Option<HandlerResult> {
                // An empty link (a line anchor in pandoc's highlighted code)
                // shows nothing, so it writes nothing (`[](#cb1-1)` else).
                if is_hidden(e.node) || (e.tag == "a" && e.node.children().is_empty()) {
                    None
                } else if let Some(m) = (e.tag == "span").then(|| math::write(e.node)).flatten() {
                    Some(m.into())
                } else {
                    h.fallback(e)
                }
            },
        )
        .add_handler(
            vec!["table"],
            |h: &dyn Handlers, e: &Element| -> Option<HandlerResult> {
                if is_hidden(e.node) {
                    None
                } else {
                    table::write(h, e)
                }
            },
        )
        .scripting_enabled(false)
        .build()
}

/// How deep the tree under `root` nests, walked without recursion.
fn depth(root: htmd::node::Node<'_>) -> usize {
    let (mut deepest, mut d) = (0usize, 0usize);
    let mut node = root;
    loop {
        if let Some(c) = node.first_child() {
            node = c;
            d += 1;
            deepest = deepest.max(d);
            continue;
        }
        loop {
            if let Some(s) = node.next_sibling() {
                node = s;
                break;
            }
            match node.parent() {
                Some(p) if d > 0 => {
                    node = p;
                    d -= 1;
                }
                _ => return deepest,
            }
        }
    }
}

/// `html` as markdown (module doc). A page whose parse reaches
/// [`depth::STOP_DEPTH`] is answered too deep before htmd parses it.
pub fn markdown(html: &str) -> Result<Markdown, String> {
    if let Some(deep) = depth::past(html, false) {
        return Ok(Markdown {
            text: String::new(),
            too_deep: Some(deep),
        });
    }
    let conv = converter();
    let tree = conv
        .html_to_tree(html)
        .map_err(|e| format!("the page could not be parsed ({e})"))?;
    let deep = depth(tree.root());
    if deep > MAX_HTML_DEPTH {
        return Ok(Markdown {
            text: String::new(),
            too_deep: Some(deep),
        });
    }
    let text = conv
        .convert(&tree)
        .map_err(|e| format!("the page could not be written as markdown ({e})"))?;
    Ok(Markdown {
        text,
        too_deep: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pandocs_tex_math_is_written_as_dollar_math() {
        let page = r#"<p>a formula <span class="math inline">\(E = mc^{2}\)</span> here</p>
<p><span class="math display">\[\int_{0}^{1}x\, dx\]</span></p>
<p><span class="math">not math</span> <span class="inline">x_1</span></p>"#;
        let m = md(page);
        assert!(m.contains("a formula $E = mc^{2}$ here"), "{m}");
        assert!(m.contains(r"$$\int_{0}^{1}x\, dx$$"), "{m}");
        assert!(m.contains("not math"), "{m}");
        let side = r#"<p><span class="math inline">\(a\)</span><span class="math inline">\(b\)</span></p>"#;
        assert!(md(side).contains("$a$ $b$"), "{}", md(side));
        let code = r##"<pre><code><span id="cb1-1"><a href="#cb1-1" aria-hidden="true" tabindex="-1"></a>x = 1</span></code></pre>"##;
        let m = md(code);
        assert!(m.contains("x = 1") && !m.contains("](#cb1-1)"), "{m}");
    }

    fn md(page: &str) -> String {
        let m = markdown(page).unwrap();
        assert_eq!(m.too_deep, None);
        m.text
    }

    #[test]
    fn a_page_becomes_markdown_without_scripts_or_remote_pictures() {
        let page = r#"<!DOCTYPE html><html><head><title>Not shown</title>
            <script>alert(1)</script><style>p{}</style></head><body>
            <h1>Example</h1><p>Read <a href="https://docs.example/a">this</a>.</p>
            <img src="https://tracker.example/pixel.gif" alt="pixel">
            <img src="data:image/png;base64,AAAA">
            <iframe src="https://ads.example/"></iframe>
            <table><tr><th>Item</th><th>Cost</th></tr><tr><td>Pens</td><td>4</td></tr></table>
            <noscript>enable scripts</noscript></body></html>"#;
        let t = md(page);
        assert!(t.starts_with("# Example"), "{t}");
        assert!(t.contains("[this](https://docs.example/a)"), "{t}");
        assert!(
            t.contains("| Item | Cost |\n| --- | --- |\n| Pens | 4 |"),
            "{t}"
        );
        assert_eq!(t.matches(PICTURE).count(), 2, "{t}");
        for gone in [
            "alert",
            "Not shown",
            "p{}",
            "tracker",
            "AAAA",
            "ads.example",
            "enable scripts",
        ] {
            assert!(!t.contains(gone), "{gone} in {t}");
        }
    }

    /// Past the bound the page is answered as too deep (the gateway reads
    /// it flat); at the bound it is written, whichever handlers nest, on the
    /// stack the reader converts on.
    #[test]
    fn nesting_is_counted_on_the_tree_and_written_up_to_the_bound() {
        std::thread::Builder::new()
            .stack_size(lmgw_doc_reader::wire::READER_STACK_BYTES)
            .spawn(nesting)
            .unwrap()
            .join()
            .unwrap();
    }

    fn nesting() {
        let deep = format!(
            "<h1>Deep</h1>{}text{}",
            "<div>".repeat(2_000),
            "</div>".repeat(2_000)
        );
        let m = markdown(&deep).unwrap();
        // The parse stops past `depth::STOP_DEPTH`, before the page's end.
        assert!(
            m.too_deep.is_some_and(|d| d > depth::STOP_DEPTH),
            "{:?}",
            m.too_deep
        );
        assert!(m.text.is_empty());
        // The estimator's five blind spots are all real depth to the parser,
        // so the tree's own count sees them.
        for (open, n) in [
            ("<div/>", 400),
            ("<rt>", 400),
            ("<optgroup>", 400),
            ("<svg><title><div>", 1),
        ] {
            let page = if n == 1 {
                format!("<svg><title>{}x", "<div>".repeat(400))
            } else {
                format!("{}x", open.repeat(n))
            };
            let m = markdown(&page).unwrap();
            assert!(m.too_deep.is_some(), "{open}: {:?}", m.too_deep);
        }
        let m = markdown(&format!("<!----!>{}x", "<div>".repeat(400))).unwrap();
        assert!(m.too_deep.is_some());
        // html, body and the text are three of the levels; each chain with
        // the levels one repeat adds to the tree (a table brings its tbody).
        let n = MAX_HTML_DEPTH - 3;
        for (open, close, levels) in [
            ("<div>", "</div>", 1),
            ("<blockquote>", "</blockquote>", 1),
            ("<ul><li>", "</li></ul>", 2),
            ("<table><tr><td>", "</td></tr></table>", 4),
            ("<b><i>", "</i></b>", 2),
        ] {
            let k = n / levels;
            let page = format!("{}at the bound{}", open.repeat(k), close.repeat(k));
            let m = markdown(&page).unwrap();
            assert_eq!(m.too_deep, None, "{open}");
            assert!(m.text.contains("at the bound"), "{open}: {}", m.text);
            // One more repeat is past it.
            let page = format!("{}x{}", open.repeat(k + 1), close.repeat(k + 1));
            assert!(markdown(&page).unwrap().too_deep.is_some(), "{open}");
        }
    }

    /// What running as a child costs a file, measured: the same page
    /// converted in this process and by the built executable (stdin, parse,
    /// write, the frame on a pipe). Run by hand:
    /// `cargo nextest run --workspace --run-ignored ignored-only --no-capture -E 'test(the_childs_overhead)'`.
    #[test]
    #[ignore = "a measurement, run by hand (its doc comment has the command)"]
    fn the_childs_overhead() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};
        use std::time::Instant;
        let exe = std::env::current_exe().unwrap();
        let reader = exe
            .parent()
            .and_then(|deps| deps.parent())
            .unwrap()
            .join(format!("lmgw-doc-reader{}", std::env::consts::EXE_SUFFIX));
        let para = "<h2>Section</h2><p>Some <b>words</b> of an example page, with a \
                    <a href=\"https://docs.example/a\">link</a> and a list:</p>\
                    <ul><li>one</li><li>two</li></ul>\
                    <table><tr><th>Item</th><th>Cost</th></tr><tr><td>Pens</td><td>4</td></tr></table>";
        let child = |args: &[&str], input: &[u8]| {
            let mut c = Command::new(&reader)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = c.stdin.take().unwrap();
            let input = input.to_vec();
            let w = std::thread::spawn(move || {
                let _ = stdin.write_all(&input);
            });
            let out = c.wait_with_output().unwrap();
            w.join().unwrap();
            assert!(out.status.success());
            out.stdout.len()
        };
        let mean = |runs: u32, f: &mut dyn FnMut()| {
            let t = Instant::now();
            for _ in 0..runs {
                f();
            }
            t.elapsed().as_secs_f64() * 1000.0 / f64::from(runs)
        };
        let start = mean(100, &mut || {
            child(&["--version"], b"");
        });
        eprintln!("start and exit alone (--version): {start:.2} ms");
        for (name, bytes, runs) in [("2 KiB", 2usize << 10, 100u32), ("10 MiB", 10 << 20, 3)] {
            let page = format!(
                "<!DOCTYPE html><html><body>{}</body></html>",
                para.repeat(bytes / para.len())
            );
            let here = mean(runs, &mut || {
                markdown(&page).unwrap();
            });
            let there = mean(runs, &mut || {
                child(&["html"], page.as_bytes());
            });
            eprintln!(
                "{name} page ({} bytes): in this process {here:.2} ms, as a child {there:.2} ms, \
                 the child's overhead {:.2} ms",
                page.len(),
                there - here
            );
        }
    }

    #[test]
    fn text_an_element_hides_itself_is_left_out() {
        let t = md(r#"<p>Shown</p><p hidden>secret one</p>
            <div style="color:red; DISPLAY : none">secret two <b>bold</b></div>
            <span style="visibility:hidden">secret three</span>
            <table hidden><tr><td>secret four</td></tr></table>
            <p hidden="until-found">found by a search</p>
            <p style="display:none-such">odd style</p>"#);
        assert!(
            t.contains("Shown") && t.contains("found by a search"),
            "{t}"
        );
        assert!(t.contains("odd style"), "{t}");
        assert!(!t.contains("secret"), "{t}");
    }
}
