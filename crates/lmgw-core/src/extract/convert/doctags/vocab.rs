//! The DocTags vocabulary (docling-core 2.104.0, `types/doc/tokens.py` and
//! `transforms/serializer/doctags.py`, read 2026-11-19): every token a
//! docling model can write, as the reader's [`Tok`]. A `<…>` that is none of
//! them is text of the page.
//!
//! - **Elements** (`DocumentToken`, each with its closing tag): `text`,
//!   `paragraph`, `reference`, `handwritten_text`, `footnote`, `smiles`,
//!   `title`, `section_header_level_N`, `caption`, `formula`, `list_item`,
//!   `page_header`, `page_footer`, `code`, `checkbox_selected`,
//!   `checkbox_unselected`, `picture`, `chart` (a picture with its data),
//!   `otsl`, `table`, `document_index`.
//! - **Key-value cells** (the key-value serializer writes
//!   `<{label}_{id}>…</{label}_{id}>` for the labels `key`, `value`,
//!   `checkbox` and `unspecified`): elements too, each a paragraph; their
//!   `<link_N>` tokens say which cell answers which and are no text.
//! - **Groups**: `doctag` (the root), `ordered_list`, `unordered_list`,
//!   `inline`, `form`, `key_value_region`, `page_break`.
//! - **Tables** (`TableToken`): the OTSL cells `fcel`, `ecel`, `ched`,
//!   `rhed`, `srow`, `lcel`, `ucel`, `xcel`, the row end `nl`, and the cell
//!   labels `column_header`, `row_header`, `shed`, `data`.
//! - **Picture classes** ([`PICTURE_CLASSES`]) and **code languages**
//!   ([`CODE_LANGUAGES`], written `<_Python_>` or self-closing `<Python/>`).
//! - **Locations** `<loc_N>` (or `<loc_N/>`), N from 0 to the grid's size.
//! - A model's own **control tokens** (`<|end_of_text|>`), which a
//!   llama-server started with `--special` writes out beside the tags.

use super::GRID;

/// A leaf element: what its text becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Leaf {
    Text,
    Heading(u8),
    Caption,
    Formula,
    ListItem,
    PageHeader,
    PageFooter,
    Code,
    Checkbox(bool),
    Picture,
    Table,
}

impl Leaf {
    /// A picture or a table: an element other elements sit in.
    pub(super) fn floats(self) -> bool {
        matches!(self, Self::Picture | Self::Table)
    }
}

/// A group tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Group {
    Root,
    Inline,
    PageBreak,
    /// The lists, `form`, `key_value_region`.
    Other,
}

/// One token of the answer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Tok<'a> {
    Text(&'a str),
    Open(Leaf, &'a str),
    Close(&'a str),
    Group {
        group: Group,
        close: bool,
    },
    Loc(f32),
    /// A location token whose number is none of the grid's (`<loc_abc>`,
    /// `<loc_99999>`).
    BadLoc,
    /// An OTSL cell token.
    Cell(CellTok),
    /// A cell's label: what kind the cell it follows is.
    Label(CellTok),
    Nl,
    /// A picture's class: no text of the page.
    Class,
    /// A code block's language: no text of the page.
    Language,
    /// A key-value cell's link to another, or a model's control token.
    Dropped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CellTok {
    Filled,
    Empty,
    ColumnHeader,
    RowHeader,
    SectionRow,
    Left,
    Up,
    Cross,
}

impl CellTok {
    /// A cell joined to another: it has no text of its own.
    pub(super) fn joined(self) -> bool {
        matches!(self, Self::Left | Self::Up | Self::Cross)
    }
}

/// docling-core's `_PictureClassificationToken`, the legacy names included.
pub(super) const PICTURE_CLASSES: &[&str] = &[
    "bar_chart",
    "box_plot",
    "flow_chart",
    "line_chart",
    "pie_chart",
    "scatter_plot",
    "table",
    "full_page_image",
    "page_thumbnail",
    "photograph",
    "chemistry_structure",
    "bar_code",
    "icon",
    "logo",
    "qr_code",
    "signature",
    "stamp",
    "engineering_drawing",
    "screenshot_from_computer",
    "screenshot_from_manual",
    "geographical_map",
    "topographical_map",
    "calendar",
    "crossword_puzzle",
    "music",
    "other",
    "cad_drawing",
    "chart",
    "decoration",
    "electrical_diagram",
    "map",
    "heatmap",
    "illustration",
    "infographic",
    "chemistry_markush_structure",
    "chemistry_molecular_structure",
    "natural_image",
    "person",
    "picture_group",
    "remote_sensing",
    "scatter_chart",
    "screenshot",
    "stacked_bar_chart",
    "stratigraphic_chart",
    "ui_element",
];

/// docling-core's `_CodeLanguageToken`, without the `<_` `_>` around each.
pub(super) const CODE_LANGUAGES: &[&str] = &[
    "Ada",
    "Awk",
    "Bash",
    "bc",
    "C",
    "C#",
    "C++",
    "CMake",
    "COBOL",
    "CSS",
    "Ceylon",
    "Clojure",
    "Crystal",
    "Cuda",
    "Cython",
    "D",
    "Dart",
    "dc",
    "Dockerfile",
    "Elixir",
    "Erlang",
    "FORTRAN",
    "Forth",
    "Go",
    "HTML",
    "Haskell",
    "Haxe",
    "Java",
    "JavaScript",
    "Julia",
    "Kotlin",
    "Lisp",
    "Lua",
    "Matlab",
    "MoonScript",
    "Nim",
    "OCaml",
    "ObjectiveC",
    "Octave",
    "PHP",
    "Pascal",
    "Perl",
    "Prolog",
    "Python",
    "Racket",
    "Ruby",
    "Rust",
    "SML",
    "SQL",
    "Scala",
    "Scheme",
    "Swift",
    "TypeScript",
    "unknown",
    "VisualBasic",
    "XML",
    "YAML",
];

/// The labels of a key-value cell's tag (docling-core's `GraphCellLabel`).
const KEY_VALUE_LABELS: &[&str] = &["key", "value", "checkbox", "unspecified"];

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `name` is a key-value cell's tag: a label, `_`, the cell's number.
fn key_value_cell(name: &str) -> bool {
    name.rsplit_once('_')
        .is_some_and(|(label, id)| KEY_VALUE_LABELS.contains(&label) && digits(id))
}

/// The element a tag name opens.
fn leaf(name: &str) -> Option<Leaf> {
    Some(match name {
        "text" | "paragraph" | "reference" | "handwritten_text" | "footnote" | "smiles" => {
            Leaf::Text
        }
        "title" => Leaf::Heading(1),
        "caption" => Leaf::Caption,
        "formula" => Leaf::Formula,
        "list_item" => Leaf::ListItem,
        "page_header" => Leaf::PageHeader,
        "page_footer" => Leaf::PageFooter,
        "code" => Leaf::Code,
        "checkbox_selected" => Leaf::Checkbox(true),
        "checkbox_unselected" => Leaf::Checkbox(false),
        "picture" | "chart" => Leaf::Picture,
        "otsl" | "document_index" | "table" => Leaf::Table,
        _ if key_value_cell(name) => Leaf::Text,
        _ => {
            // docling's level N is markdown's N + 1 (`<title>` is level 1).
            let level = name.strip_prefix("section_header_level_")?;
            let level: u8 = digits(level).then(|| level.parse().ok()).flatten()?;
            Leaf::Heading(level.saturating_add(1).clamp(1, 6))
        }
    })
}

fn group(name: &str) -> Option<Group> {
    Some(match name {
        "doctag" => Group::Root,
        "inline" => Group::Inline,
        "page_break" => Group::PageBreak,
        "ordered_list" | "unordered_list" | "form" | "key_value_region" => Group::Other,
        _ => return None,
    })
}

fn cell(name: &str) -> Option<CellTok> {
    Some(match name {
        "fcel" => CellTok::Filled,
        "ecel" => CellTok::Empty,
        "ched" => CellTok::ColumnHeader,
        "rhed" => CellTok::RowHeader,
        "srow" => CellTok::SectionRow,
        "lcel" => CellTok::Left,
        "ucel" => CellTok::Up,
        "xcel" => CellTok::Cross,
        _ => return None,
    })
}

/// A cell label, as the kind of cell it says.
fn label(name: &str) -> Option<CellTok> {
    Some(match name {
        "column_header" => CellTok::ColumnHeader,
        "row_header" => CellTok::RowHeader,
        "shed" => CellTok::SectionRow,
        "data" => CellTok::Filled,
        _ => return None,
    })
}

/// A code language token's name: `_Python_`, or self-closing `Python/`.
fn language(name: &str) -> bool {
    let inner = name
        .strip_prefix('_')
        .and_then(|n| n.strip_suffix('_'))
        .or_else(|| name.strip_suffix('/'));
    inner.is_some_and(|n| CODE_LANGUAGES.contains(&n))
}

/// A model's control token's name: `|word|`.
fn control(name: &str) -> bool {
    name.strip_prefix('|')
        .and_then(|n| n.strip_suffix('|'))
        .is_some_and(|word| {
            !word.is_empty() && word.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}

/// The tag `name` (between `<` and `>`) as a token; `None` for a name the
/// vocabulary does not have, which stays text. `in_picture`: the open
/// element is a picture, where `table` and `chart` are its class and not
/// an element.
fn tag(name: &str, in_picture: bool) -> Option<Tok<'_>> {
    if control(name) {
        return Some(Tok::Dropped);
    }
    if let Some(n) = name.strip_prefix("loc_") {
        let n = n.strip_suffix('/').unwrap_or(n);
        // Digits only (no sign, no space); a number past the grid, or one
        // too long to be a number, is no position.
        let on_grid = digits(n)
            .then(|| n.parse::<u32>().ok())
            .flatten()
            .filter(|v| *v <= GRID as u32);
        return Some(on_grid.map_or(Tok::BadLoc, |v| Tok::Loc(v as f32)));
    }
    if name.strip_prefix("link_").is_some_and(digits) {
        return Some(Tok::Dropped);
    }
    if let Some(close) = name.strip_prefix('/') {
        return match (group(close), leaf(close)) {
            (Some(group), _) => Some(Tok::Group { group, close: true }),
            (None, Some(_)) => Some(Tok::Close(close)),
            (None, None) => None,
        };
    }
    if let Some(group) = group(name) {
        return Some(Tok::Group {
            group,
            close: false,
        });
    }
    if name == "nl" {
        return Some(Tok::Nl);
    }
    if let Some(c) = cell(name) {
        return Some(Tok::Cell(c));
    }
    if let Some(l) = label(name) {
        return Some(Tok::Label(l));
    }
    if in_picture && matches!(name, "table" | "chart") {
        return Some(Tok::Class);
    }
    if let Some(l) = leaf(name) {
        return Some(Tok::Open(l, name));
    }
    if PICTURE_CLASSES.contains(&name) {
        return Some(Tok::Class);
    }
    language(name).then_some(Tok::Language)
}

/// The next token of `s` and what is left. A `<` that opens no tag of the
/// vocabulary is text.
pub(super) fn next(s: &str, in_picture: bool) -> (Tok<'_>, &str) {
    let mut from = 0;
    loop {
        let Some(at) = s[from..].find('<').map(|i| i + from) else {
            return (Tok::Text(s), "");
        };
        let window = &s.as_bytes()[at + 1..s.len().min(at + 1 + super::MAX_TAG_BYTES)];
        let end = window.iter().position(|b| *b == b'>' || *b == b'<');
        let found = match end {
            Some(e) if window[e] == b'>' => tag(&s[at + 1..at + 1 + e], in_picture),
            _ => None,
        };
        match found {
            Some(_) if at > 0 => return (Tok::Text(&s[..at]), &s[at..]),
            Some(t) => {
                let len = end.unwrap_or(0) + 2;
                return (t, &s[len..]);
            }
            None => from = at + 1,
        }
    }
}
