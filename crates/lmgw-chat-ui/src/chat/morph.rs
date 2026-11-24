//! A reply's rendered markdown written into its element by changing only
//! what differs, never by replacing the whole subtree.
//!
//! The markdown is rendered afresh on every streamed token. Writing it with
//! `innerHTML` threw the element's whole subtree away each time, and in
//! WebKitGTK (2.52) that moves the transcript pane: when a subtree holding a
//! scroller of its own (a code block, `overflow-x: auto`) is replaced inside
//! the pane, the pane drops back to an earlier scroll position — the end of
//! the transcript as it was when the reply started — without the page
//! asking, and without a scroll event in that frame (measured 2026-11-16 in
//! WebKitGTK 2.52 under broadway: five drops in five seconds of replacing a
//! reply holding a code block with its own HTML, none when only its last
//! paragraph was replaced or text was appended; Chrome never drops). A
//! reader following the stream saw the reply jump back to its start, again
//! and again, until the next token scrolled them down. Written as below, a
//! 300-token reply with a code block streamed with no jump at all; the one
//! move left is when the reply settles and `codeblocks.js` wraps a `<pre>`
//! in its toolbar's box, which the pane's resize puts back at the end
//! (`scripts/drive/chat-stream-scroll.json` checks it).
//!
//! [`write`] parses the new HTML off the document and brings the element to
//! it node by node. A top-level block whose HTML is the one it was made
//! from last time ([`Made`]) is left as it stands, decorations and all: a
//! finished paragraph, a finished code block with its highlighting. That
//! holds only while the element's children are provably the nodes the last
//! write left, each in its place: the node written, or the box a decoration
//! put it in (`codeblocks.js` wraps a `<pre>` in its toolbar's box, a
//! diagram's in its card, and adds no top-level node of its own; a failed
//! diagram's error goes inside the block's box). When anything else stands
//! among them, no block is left as it stands on that write: positions no
//! longer say which block is which, and a block kept by its position once
//! showed an old paragraph after its continued one. Below
//! that, a node of the same kind (a text node, or an element of the same tag
//! and the same `class`) is kept and brought to the new one — its text, its
//! attributes, its children —, any other is replaced, and what the new HTML
//! lacks goes. A block written so holds what `innerHTML` would have made,
//! so the decorations that run after it (`codeblocks.js`: highlighting,
//! math, the code toolbars) see the DOM they always did, and a block they
//! decorated before and that did not change is not decorated again.
//!
//! The class is part of an element's kind so that a decorated node is never
//! kept for a plain one: a highlighted `<code class="hljs …">`, a toolbar's
//! `div.code-wrap` around a `<pre>`, a rendered diagram are replaced by the
//! plain node, with whatever listeners they carried.

use wasm_bindgen::JsCast;

/// What the last [`write`] left: for each top-level node, the node it wrote
/// or kept and the HTML that node was made from.
#[derive(Default)]
pub(crate) struct Made(Vec<(web_sys::Node, String)>);

/// Make `el`'s children what `html` parses to (module doc). `made` is what
/// the last write left, and becomes this write's.
pub(crate) fn write(el: &web_sys::Element, html: &str, made: &mut Made) {
    let Some(fresh) = el
        .owner_document()
        .and_then(|doc| doc.create_element("div").ok())
    else {
        el.set_inner_html(html);
        made.0.clear();
        return;
    };
    // A detached `div` is what `innerHTML` on the element parses into too
    // (its images may start loading, as they always did).
    fresh.set_inner_html(html);
    let old: &web_sys::Node = el.as_ref();
    let (had, want) = (list(old), list(fresh.as_ref()));
    // Each child the node the last write left there (or its decoration's
    // box), and no other child: only then is a block left as it stands.
    let ours = had.len() == made.0.len()
        && made
            .0
            .iter()
            .zip(&had)
            .all(|((m, _), h)| holder(old, m).is_some_and(|t| t.is_same_node(Some(h))));
    let mut now = Vec::with_capacity(want.len());
    for (i, n) in want.into_iter().enumerate() {
        let src = source(&n);
        let node = match (had.get(i), made.0.get(i)) {
            // Made from the same HTML: as it stands, decorated or not.
            (Some(_), Some((m, s))) if ours && *s == src => m.clone(),
            (Some(o), _) if same_kind(o, &n) => {
                bring(o, &n);
                o.clone()
            }
            (Some(o), _) => {
                let _ = old.replace_child(&n, o);
                n
            }
            (None, _) => {
                let _ = old.append_child(&n);
                n
            }
        };
        now.push((node, src));
    }
    for o in had.iter().skip(now.len()) {
        let _ = old.remove_child(o);
    }
    made.0 = now;
}

/// The child of `top` that holds `n` (`n` itself, or the box a decoration
/// put it in); `None` once `n` is out of `top`.
fn holder(top: &web_sys::Node, n: &web_sys::Node) -> Option<web_sys::Node> {
    let mut at = n.clone();
    loop {
        let up = at.parent_node()?;
        if up.is_same_node(Some(top)) {
            return Some(at);
        }
        at = up;
    }
}

/// A node's children, as they are now.
fn list(n: &web_sys::Node) -> Vec<web_sys::Node> {
    let kids = n.child_nodes();
    (0..kids.length()).filter_map(|i| kids.item(i)).collect()
}

/// The HTML a parsed top-level node stands for.
fn source(n: &web_sys::Node) -> String {
    match n.dyn_ref::<web_sys::Element>() {
        Some(e) => e.outer_html(),
        None => format!("#{}:{}", n.node_type(), n.node_value().unwrap_or_default()),
    }
}

/// Same kind (module doc): kept and brought to the new node.
fn same_kind(old: &web_sys::Node, new: &web_sys::Node) -> bool {
    if old.node_type() != new.node_type() {
        return false;
    }
    match (
        old.dyn_ref::<web_sys::Element>(),
        new.dyn_ref::<web_sys::Element>(),
    ) {
        (Some(o), Some(n)) => {
            o.tag_name() == n.tag_name() && o.get_attribute("class") == n.get_attribute("class")
        }
        (None, None) => true,
        _ => false,
    }
}

/// Bring `old`'s children to `new`'s, moving the new nodes over where a node
/// is replaced or added.
fn children(old: &web_sys::Node, new: &web_sys::Node) {
    let (had, want) = (list(old), list(new));
    for (i, n) in want.iter().enumerate() {
        match had.get(i) {
            Some(o) if same_kind(o, n) => bring(o, n),
            Some(o) => {
                let _ = old.replace_child(n, o);
            }
            None => {
                let _ = old.append_child(n);
            }
        }
    }
    for o in had.iter().skip(want.len()) {
        let _ = old.remove_child(o);
    }
}

/// Bring node `old` to `new`, which is of the same kind.
fn bring(old: &web_sys::Node, new: &web_sys::Node) {
    match (
        old.dyn_ref::<web_sys::Element>(),
        new.dyn_ref::<web_sys::Element>(),
    ) {
        (Some(o), Some(n)) => {
            attributes(o, n);
            children(old, new);
        }
        _ => {
            let text = new.node_value();
            if old.node_value() != text {
                old.set_node_value(text.as_deref());
            }
        }
    }
}

/// `old` takes `new`'s attributes, and loses those `new` has not.
fn attributes(old: &web_sys::Element, new: &web_sys::Element) {
    let names = |e: &web_sys::Element| -> Vec<String> {
        e.get_attribute_names()
            .iter()
            .filter_map(|v| v.as_string())
            .collect()
    };
    for name in names(old) {
        if !new.has_attribute(&name) {
            let _ = old.remove_attribute(&name);
        }
    }
    for name in names(new) {
        let value = new.get_attribute(&name).unwrap_or_default();
        if old.get_attribute(&name).as_deref() != Some(value.as_str()) {
            let _ = old.set_attribute(&name, &value);
        }
    }
}
