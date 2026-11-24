//! A page's nesting bounded *while* it is parsed (security round A, M1),
//! before htmd builds its tree: html5ever's tree builder looks through its
//! stack of open elements for every start tag, so its time grows with the
//! square of the nesting (a 2.2 MiB page of 200,000 nested `div`s ran the
//! reader's whole `READER_TIMEOUT` of 300 s at 100 % of a core).
//!
//! The page is fed to html5ever in pieces of [`PIECE`] bytes with a sink
//! that keeps nothing but each node's parent, and the depth of every node
//! appended is counted from its parents. Once a node lies deeper than
//! [`STOP_DEPTH`] the parse is stopped and the page is answered as too deep
//! ([`super::Markdown::too_deep`], which the gateway reads flat and names);
//! otherwise htmd parses the page as before and its own count on the tree
//! ([`MAX_HTML_DEPTH`]) decides. The stop sits at four times that bound, so
//! a page the tree's count would take is never stopped here (a node counted
//! at its append can be moved up later by the tree builder; never by
//! hundreds of levels), and a parse stopped here has done at most one piece
//! past it.

use std::borrow::Cow;
use std::cell::{Cell, Ref, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeBuilderOpts, TreeSink};
use html5ever::{parse_document, Attribute, ParseOpts, QualName};
use lmgw_doc_reader::wire::MAX_HTML_DEPTH;

/// Where the parse stops: four times [`MAX_HTML_DEPTH`], 1,024 levels.
pub const STOP_DEPTH: usize = 4 * MAX_HTML_DEPTH;

/// The page is parsed this much at a time: 16 KiB.
const PIECE: usize = 16 << 10;

const NONE: u32 = u32::MAX;

/// A node's links, as a tree keeps them (ego_tree's, which htmd uses), so
/// moving a node or its children costs what it costs the real tree.
#[derive(Clone, Copy)]
struct Links {
    parent: u32,
    first: u32,
    last: u32,
    prev: u32,
    next: u32,
}

const UNLINKED: Links = Links {
    parent: NONE,
    first: NONE,
    last: NONE,
    prev: NONE,
    next: NONE,
};

struct Sink {
    links: RefCell<Vec<Links>>,
    names: RefCell<Vec<Option<QualName>>>,
    templates: RefCell<HashMap<u32, u32>>,
    mathml: RefCell<HashSet<u32>>,
    /// The deepest node so far, shared with [`past`].
    deepest: Rc<Cell<usize>>,
}

fn at(l: &mut [Links], n: u32) -> &mut Links {
    &mut l[n as usize]
}

impl Sink {
    fn node(&self, name: Option<QualName>) -> u32 {
        let mut l = self.links.borrow_mut();
        let id = u32::try_from(l.len()).unwrap_or(NONE - 1);
        l.push(UNLINKED);
        self.names.borrow_mut().push(name);
        id
    }

    /// How deep `n` lies, counted up its parents (at most past the stop).
    fn depth_of(&self, mut n: u32) -> usize {
        let l = self.links.borrow();
        let mut d = 0;
        while let Some(up) = l.get(n as usize).map(|x| x.parent) {
            if up == NONE || d > STOP_DEPTH {
                break;
            }
            d += 1;
            n = up;
        }
        d
    }

    fn count(&self, parent: u32) {
        let d = self.depth_of(parent) + 1;
        if d > self.deepest.get() {
            self.deepest.set(d);
        }
    }

    fn detach(l: &mut [Links], n: u32) {
        let me = l[n as usize];
        if me.parent == NONE {
            return;
        }
        if me.prev != NONE {
            at(l, me.prev).next = me.next;
        } else {
            at(l, me.parent).first = me.next;
        }
        if me.next != NONE {
            at(l, me.next).prev = me.prev;
        } else {
            at(l, me.parent).last = me.prev;
        }
        *at(l, n) = Links {
            parent: NONE,
            prev: NONE,
            next: NONE,
            ..me
        };
    }

    /// `n` as `parent`'s last child, or before `before`.
    fn link(l: &mut [Links], parent: u32, n: u32, before: u32) {
        Self::detach(l, n);
        let prev = if before == NONE {
            l[parent as usize].last
        } else {
            l[before as usize].prev
        };
        {
            let me = at(l, n);
            me.parent = parent;
            me.prev = prev;
            me.next = before;
        }
        if prev == NONE {
            at(l, parent).first = n;
        } else {
            at(l, prev).next = n;
        }
        if before == NONE {
            at(l, parent).last = n;
        } else {
            at(l, before).prev = n;
        }
    }

    fn placed(&self, parent: u32, child: &NodeOrText<u32>, before: u32) {
        self.count(parent);
        if let NodeOrText::AppendNode(id) = child {
            Self::link(&mut self.links.borrow_mut(), parent, *id, before);
        }
    }
}

impl TreeSink for Sink {
    type Handle = u32;
    type Output = ();
    type ElemName<'a> = Ref<'a, QualName>;

    fn finish(self) {}

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn get_document(&self) -> u32 {
        0
    }

    fn elem_name<'a>(&'a self, target: &'a u32) -> Ref<'a, QualName> {
        Ref::map(self.names.borrow(), |n| {
            n.get(*target as usize)
                .and_then(Option::as_ref)
                .expect("not an element")
        })
    }

    fn create_element(&self, name: QualName, _: Vec<Attribute>, flags: ElementFlags) -> u32 {
        let id = self.node(Some(name));
        if flags.template {
            let contents = self.node(None);
            self.templates.borrow_mut().insert(id, contents);
        }
        if flags.mathml_annotation_xml_integration_point {
            self.mathml.borrow_mut().insert(id);
        }
        id
    }

    fn create_comment(&self, _: StrTendril) -> u32 {
        self.node(None)
    }

    fn create_pi(&self, _: StrTendril, _: StrTendril) -> u32 {
        self.node(None)
    }

    fn append(&self, parent: &u32, child: NodeOrText<u32>) {
        self.placed(*parent, &child, NONE);
    }

    fn append_based_on_parent_node(&self, element: &u32, prev: &u32, child: NodeOrText<u32>) {
        let has_parent = self
            .links
            .borrow()
            .get(*element as usize)
            .is_some_and(|x| x.parent != NONE);
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev, child);
        }
    }

    fn append_doctype_to_document(&self, _: StrTendril, _: StrTendril, _: StrTendril) {}

    fn get_template_contents(&self, target: &u32) -> u32 {
        *self
            .templates
            .borrow()
            .get(target)
            .expect("not a template element")
    }

    fn same_node(&self, x: &u32, y: &u32) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, _: QuirksMode) {}

    fn append_before_sibling(&self, sibling: &u32, new_node: NodeOrText<u32>) {
        let parent = self
            .links
            .borrow()
            .get(*sibling as usize)
            .map_or(NONE, |x| x.parent);
        if parent != NONE {
            self.placed(parent, &new_node, *sibling);
        }
    }

    fn add_attrs_if_missing(&self, _: &u32, _: Vec<Attribute>) {}

    fn remove_from_parent(&self, target: &u32) {
        Self::detach(&mut self.links.borrow_mut(), *target);
    }

    fn reparent_children(&self, node: &u32, new_parent: &u32) {
        let mut l = self.links.borrow_mut();
        let mut c = l[*node as usize].first;
        while c != NONE {
            let next = l[c as usize].next;
            Self::link(&mut l, *new_parent, c, NONE);
            c = next;
        }
        drop(l);
        self.count(*new_parent);
    }

    fn is_mathml_annotation_xml_integration_point(&self, handle: &u32) -> bool {
        self.mathml.borrow().contains(handle)
    }
}

/// How deep `html` was found to nest when its parse was stopped past
/// [`STOP_DEPTH`]; `None` when it was parsed to its end within it.
/// `scripting` is the converter's own `scripting_enabled`.
pub fn past(html: &str, scripting: bool) -> Option<usize> {
    let deepest = Rc::new(Cell::new(0));
    let sink = Sink {
        links: RefCell::new(vec![UNLINKED]),
        names: RefCell::new(vec![None]),
        templates: RefCell::default(),
        mathml: RefCell::default(),
        deepest: deepest.clone(),
    };
    let opts = ParseOpts {
        tree_builder: TreeBuilderOpts {
            scripting_enabled: scripting,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut parser = parse_document(sink, opts);
    let mut rest = html;
    while !rest.is_empty() {
        let mut cut = rest.len().min(PIECE);
        while !rest.is_char_boundary(cut) {
            cut += 1;
        }
        let (piece, after) = rest.split_at(cut);
        parser.process(StrTendril::from_slice(piece));
        if deepest.get() > STOP_DEPTH {
            return Some(deepest.get());
        }
        rest = after;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deep_page_stops_its_parse_and_a_normal_one_does_not() {
        let deep = format!("{}x", "<div>".repeat(200_000));
        let started = std::time::Instant::now();
        let d = past(&deep, false).unwrap();
        assert!(d > STOP_DEPTH, "{d}");
        // Stopped within a piece past the bound, not after the whole page.
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        let normal = format!(
            "<h1>T</h1>{}text{}",
            "<div><p>".repeat(100),
            "</p></div>".repeat(100)
        );
        assert_eq!(past(&normal, false), None);
        // Between the markdown writer's bound and the stop: left to the
        // tree's own count.
        let between = format!("{}x", "<div>".repeat(MAX_HTML_DEPTH + 100));
        assert_eq!(past(&between, false), None);
        // Unclosed paragraphs and list items close themselves: not deep.
        let flat = "<p>a".repeat(20_000) + &"<li>b".repeat(20_000);
        assert_eq!(past(&flat, false), None);
    }
}
