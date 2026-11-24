//! TeX math as pandoc writes it into HTML with `--mathjax` (WP7, the
//! gateway's `pandoc.rs`): `<span class="math inline">\(…\)</span>` and
//! `<span class="math display">\[…\]</span>`, written as `$…$` and
//! `$$…$$`, the TeX as it is (no markdown escaping), so a Word file's
//! equations read as LaTeX. A web page that marks its math the same way
//! (MathJax's own convention) gets the same.

/// The span's TeX as markdown math, or `None` when it is no such span (or
/// holds anything but text).
pub fn write(node: &htmd::node::Node<'_>) -> Option<String> {
    let class = node.attr("class")?;
    let classes: Vec<&str> = class.split_whitespace().collect();
    if !classes.contains(&"math") {
        return None;
    }
    let display = classes.contains(&"display");
    if !display && !classes.contains(&"inline") {
        return None;
    }
    let mut tex = String::new();
    for child in node.children().iter() {
        tex.push_str(&child.text()?);
    }
    let tex = tex.trim();
    let (open, close) = if display {
        ("\\[", "\\]")
    } else {
        ("\\(", "\\)")
    };
    let tex = tex
        .strip_prefix(open)
        .and_then(|t| t.strip_suffix(close))
        .unwrap_or(tex)
        .trim();
    if tex.is_empty() {
        return None;
    }
    // Formulas side by side arrive as one span (the converter joins
    // adjacent spans of one class): each its own, and two inline ones apart
    // by a space, since `$a$$b$` would read as display delimiters.
    Some(if display {
        format!("\n\n$${}$$\n\n", tex.replace("\\]\\[", "$$\n\n$$"))
    } else {
        format!("${}$", tex.replace("\\)\\(", "$ $"))
    })
}
