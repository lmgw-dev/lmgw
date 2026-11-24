//! Picture description, tier 1 (document-conversion design §5.3, §7.5,
//! ruling 5, D5, D14, D20, D27): one vision call per exported picture,
//! through the picture-description model the owner picked under Settings ›
//! Documents ("Describe pictures with", `documents.picture_alias`), as the
//! calling principal.
//!
//! - **Off until picked** (D5): with no model picked, a request's
//!   `describe_pictures` is answered `model_not_chosen` ([`plan`]) and no
//!   picture is sent anywhere.
//! - **The pictures are the export's** (`super::pictures`): a PDF's
//!   pictures and crops, the layout model's figures, an office file's
//!   media, and an image file as one picture, each after `picture_min_area`.
//!   Of those, a picture smaller than `describe_min_area` of its page
//!   (docling's `picture_description_area_threshold`, which only skips the
//!   description) is kept without one and counted in the file's stats.
//!   Their bytes are made whatever the `pictures` mode; the mode decides
//!   only what the answer writes, and bytes made only for the call are
//!   reserved at their own size and given back after it.
//! - **One way to call a model**, the page-reading model's
//!   (`super::model_help`: [`session`](super::model_help::session),
//!   [`turn`](super::model_help::turn), [`call`](super::model_help::call),
//!   [`budget`](super::model_help::budget)): the key's expiry, scope, budget
//!   and tokens-per-minute window before each call, the route opened once
//!   while a file's stage holds it (the hold's swap along the fallback
//!   chain, a candidate alias's pick, GPU admission), the model-call pool sized by the answering model's slots
//!   (shared with page reads when both choices name the same model), a
//!   fallback that takes no pictures never sent one (`model_cannot_see`),
//!   `max_tokens` from the model's real context less the counted prompt,
//!   only a `stop` is a description, and one usage row per call under the
//!   caller's key ([`PROTO`]), with no document content in it.
//! - **The prompt** ([`prompt`]): the caller's `picture_prompt` or docling's
//!   own default ([`DEFAULT_PROMPT`]), held once for the request, then the
//!   picture's caption and the paragraph before it as context, each bounded
//!   by [`PICTURE_CONTEXT_MAX_BYTES`] (a cut is the `picture_context_cut`
//!   note). A picture's message is made only when its call has its turn.
//!   Where the chosen route's context is known, the caller's prompt is
//!   counted against it once ([`room`]): a prompt that leaves no room for an
//!   answer makes no call at all.
//! - **Where the answer goes**: the picture's `description` (its `figure`
//!   block in json, its entry in the `pictures` list); in markdown a
//!   paragraph after the picture and its caption ([`paragraph`],
//!   [`after_blocks`]), as docling's markdown serializer writes a picture's
//!   description after the picture item, which holds its caption
//!   (docling-core 2.104.0, `MarkdownDocSerializer`: the item's meta after
//!   the item, `\n\n` between); in text the description alone, after the
//!   caption the same way (docling's plain-text serializer, the same walk
//!   with an empty image placeholder).
//! - **A failed description never fails the file**: the picture is kept
//!   without one and a note names it and why; the file is `partial` when a
//!   picture asked for was not described for the model's reasons (held,
//!   refused, failed, late).
//! - **The file's clock** (`pool::clock`): each picture's call is a lane; a
//!   stage with pictures takes the file's end over, so `document_timeout`
//!   ends the calls still in flight and keeps the descriptions answered.

use std::sync::Arc;

use lmgw_api_types::documents::convert::{
    ConvertOptions, ConvertWarning, WarningCode, DESCRIBE_MIN_AREA_DEFAULT,
};

use super::doc::{BlockKind, Doc};
use super::model_help::{Chosen, Reader};
use super::pictures::Found;

mod place;
mod prompt;
mod room;
mod sendable;
pub mod settings;
mod stage;

pub(crate) use place::{after_blocks, is_caption_of};
pub(crate) use sendable::image_file;
pub use stage::{run, Described};

/// The `ingress_proto` of a picture-description call's usage row: real
/// model traffic with real tokens, attributed to the conversion's key, told
/// apart from the conversion's own `documents` row and a page's
/// `documents-page`.
pub const PROTO: &str = "documents-picture";

/// docling's default picture-description prompt (`PictureDescriptionBaseOptions`,
/// docling 2.142.1), so a caller that never set one gets the same kind of
/// answer as from docling.
pub const DEFAULT_PROMPT: &str = "Describe this image in a few sentences.";

/// The most bytes of document text each of a picture's two context pieces
/// (its caption, the paragraph before it) carries into the call: 2 KiB,
/// about 500 words, more than a caption or a paragraph needs to say what
/// the picture is about, and small beside an image's own tokens. A longer
/// piece is cut (the caption's start kept, the paragraph's end, the words
/// nearest the picture) and the file's `picture_context_cut` note names the
/// pictures and this bound.
pub const PICTURE_CONTEXT_MAX_BYTES: usize = 2048;

/// A request's picture description, judged: the model, the prompt, the
/// least area described.
#[derive(Clone)]
pub struct Describe {
    pub(crate) chosen: Arc<Chosen>,
    /// The caller's prompt, or [`DEFAULT_PROMPT`]: held once for the
    /// request, never copied per picture.
    pub(crate) prompt: Arc<str>,
    /// `describe_min_area`.
    pub(crate) min_area: f64,
    /// Whether the prompt leaves the answering model room ([`room`]),
    /// judged once per route the request opens.
    pub(crate) room: Arc<room::Room>,
}

impl std::fmt::Debug for Describe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Describe")
            .field("alias", &self.chosen.alias)
            .field("min_area", &self.min_area)
            .finish()
    }
}

impl PartialEq for Describe {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.chosen, &other.chosen)
            && self.prompt == other.prompt
            && self.min_area == other.min_area
    }
}

/// The picture messages made in this process so far (a test's view of
/// [`prompt`]'s memory bound: a picture's message is made only when its
/// call is).
#[doc(hidden)]
pub fn messages_made() -> u64 {
    prompt::made()
}

fn note(code: WarningCode, option: &str, message: String) -> ConvertWarning {
    ConvertWarning {
        code,
        message,
        tool: None,
        parts: Vec::new(),
        option: Some(option.to_string()),
    }
}

/// Judge the request's `describe_pictures` and `picture_prompt` against
/// `reader`: the description to make, or `None`, with the note that says
/// why when the request asked for one.
///
/// `output: text` and `pictures: placeholder` are described too, as docling
/// describes them: the description is written where the picture was.
pub fn plan(
    o: &ConvertOptions,
    reader: &Reader,
    notes: &mut Vec<ConvertWarning>,
) -> Option<Describe> {
    let asked = o.picture_prompt.as_deref().map(str::trim);
    if !o.describe_pictures {
        if asked.is_some_and(|p| !p.is_empty()) {
            notes.push(note(
                WarningCode::OptionNotApplicable,
                "picture_prompt",
                "picture_prompt: describe_pictures is not on, so no picture is described and \
                 the prompt is not used"
                    .to_string(),
            ));
        }
        if o.describe_min_area != DESCRIBE_MIN_AREA_DEFAULT {
            notes.push(note(
                WarningCode::OptionNotApplicable,
                "describe_min_area",
                "describe_min_area: describe_pictures is not on, so no picture is described"
                    .to_string(),
            ));
        }
        return None;
    }
    let Some(chosen) = &reader.pictures else {
        notes.push(note(
            WarningCode::ModelNotChosen,
            "describe_pictures",
            "picture descriptions are asked for, but no picture-description model is chosen, so \
             pictures are kept without a description; choose one under Settings → Documents \
             (\"Describe pictures with\")"
                .to_string(),
        ));
        return None;
    };
    let prompt: Arc<str> = asked
        .filter(|p| !p.is_empty())
        .unwrap_or(DEFAULT_PROMPT)
        .into();
    Some(Describe {
        chosen: chosen.clone(),
        prompt,
        min_area: o.describe_min_area,
        room: Arc::default(),
    })
}

/// A description as markdown: its lines as paragraph text (a line that
/// would start a heading, a list, a table, a fence or HTML escaped, as a
/// paragraph's are, and one that would start a display formula or a link
/// definition too), blank lines kept between its paragraphs, once.
pub fn paragraph(description: &str) -> String {
    let mut out = String::new();
    let mut blank = false;
    for l in description.trim().lines() {
        if l.trim().is_empty() {
            blank = true;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank { "\n\n" } else { "\n" });
        }
        blank = false;
        let line = super::doc::render::escaped(l);
        if line.starts_with("$$") || link_definition(&line) {
            out.push('\\');
        }
        out.push_str(&line);
    }
    out
}

/// Whether `line` reads as a markdown link reference definition
/// (`[label]: target`), which a renderer would take out of the text.
fn link_definition(line: &str) -> bool {
    line.strip_prefix('[')
        .and_then(|r| r.split_once("]:"))
        .is_some_and(|(label, _)| !label.is_empty() && !label.contains(']'))
}

/// The descriptions of `found`'s pictures onto `doc`'s `figure` blocks,
/// which json and text write them from.
pub fn stamp(doc: &mut Doc, found: &Found) {
    for b in doc.parts.iter_mut().flat_map(|p| p.blocks.iter_mut()) {
        if b.kind != BlockKind::Figure {
            continue;
        }
        if let Some(f) = b.figure.as_mut() {
            if let Some(p) = found.pictures.get(f.index as usize) {
                f.description.clone_from(&p.description);
            }
        }
    }
}

#[cfg(test)]
mod tests;
