//! Model help, tier 1 (document-conversion design §3, §7.5, §7.7, D5, D6,
//! D14, D20, D23, D27): a vision model reading the pages the tools could
//! not read well, through an lmgw alias, as the calling principal.
//!
//! - **Off until picked** (D5, D15): the page-reading model is a setting
//!   (`documents.page_reader`, Settings › Documents). With none picked, a
//!   request's `model_help` other than `off` is answered `model_not_chosen`
//!   ([`plan`]) and nothing here ever opens a route, so the card is never
//!   touched.
//! - **Which pages** ([`select`]): a function over the page's own facts in
//!   the block model (was it read, does it hold a table), so a later reader
//!   (text recognition) changes the answer by what it leaves in the model.
//! - **One way to call a model** ([`call`]): the gate's `open` and the
//!   in-process `sample_once`, as `/v1/responses` and the knowledge bases
//!   call theirs: the key's expiry, scope, budget and tokens-per-minute
//!   window, the GPU hold and its fallback chain, candidate aliases, one
//!   usage row per call under the caller's key ([`PROTO`]), with no page
//!   content in it.
//! - **Who answers decides what is sent** ([`session`], [`call`]): a
//!   fallback that takes no pictures is never sent a page (the pages keep
//!   the tools' text, `model_cannot_see` names the fallback), and only the
//!   chosen model itself is asked for DocTags; any fallback gets lmgw's
//!   Markdown prompts.
//! - **The model-call pool** ([`session`], D20): per request, at most as
//!   many calls in flight as the answering model has slots, and a page's
//!   image is rendered only once a call can take it ([`page`]), within
//!   [`render::PAGE_MAX_PIXELS`].
//! - **Budgets from the model** ([`budget`]): `max_tokens` is the answering
//!   model's context less the prompt and the image; an answer that ends at
//!   that bound is a failed page, never half a page.
//! - **The answer is untrusted text** ([`answer`]): markdown read into
//!   blocks through [`super::doc::from_md`], or DocTags through
//!   [`super::doctags`]; it only ever becomes text blocks with the source
//!   `vision`.
//! - [`pdf`] is the stage `file/pdf.rs` calls, after text recognition and
//!   within the file's clock (`pool::clock`); [`settings`] what the
//!   Settings row shows and checks.
//!
//! **The picture-description model shares the machinery** (WP11,
//! `super::describe`): a [`Reader`] carries both choices, each a
//! [`Chosen`] with its [`Purpose`], and [`session`], [`turn`], [`call`] and
//! [`budget`] serve both. The two choices share one [`session`] (one route,
//! one model-call pool) when they name the model by the same name, so a
//! conversion then keeps no more calls in flight at it than it has slots
//! (two names for one model, an alias and the row it names, have a pool
//! each yet).
//!
//! Picture files are not read here yet: text recognition reads them
//! (`ocr::pages::image_file`). The seam is [`pdf::read_pages`]'s page loop
//! with a page whose image is the file itself; what it needs first is a
//! picture every model takes (PNG or JPEG within
//! [`render::PAGE_MAX_PIXELS`]), which for a TIFF, a WebP or a larger
//! picture means decoding and scaling it in lmgw's document reader, never
//! in the gateway.

use std::sync::Arc;

use lmgw_api_types::documents::convert::{ConvertOptions, ConvertWarning, ModelHelp, WarningCode};

use crate::proxy::RequestCtx;
use crate::state::SharedState;

pub mod answer;
pub(crate) mod budget;
pub(crate) mod call;
mod page;
pub mod pdf;
pub mod prompts;
pub mod render;
pub mod select;
pub(crate) mod session;
pub mod settings;
pub(crate) mod turn;

/// The `ingress_proto` of a page-reading call's usage row: real model
/// traffic with real tokens, attributed to the conversion's key, told
/// apart from the conversion's own `documents` row.
pub const PROTO: &str = "documents-page";

/// The endpoint named in a refusal of a model that cannot generate text.
pub(crate) const ENDPOINT: &str = "/v1/documents/convert";

/// What a request may ask models for: the page-reading model and the
/// picture-description model as the settings name them, and who asks.
/// Built once per request by its HTTP half; every file of the request
/// shares it, and with it the request's model calls in flight.
#[derive(Clone, Default)]
pub struct Reader {
    pub(crate) pages: Option<Arc<Chosen>>,
    pub(crate) pictures: Option<Arc<Chosen>>,
}

/// What a chosen model is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    /// Read a page (this module).
    Pages,
    /// Describe a picture (`super::describe`).
    Pictures,
}

impl Purpose {
    /// The `ingress_proto` of its calls' usage rows.
    pub(crate) fn proto(self) -> &'static str {
        match self {
            Self::Pages => PROTO,
            Self::Pictures => super::describe::PROTO,
        }
    }

    /// The model, as a warning names it.
    pub(crate) fn model(self) -> &'static str {
        match self {
            Self::Pages => "page-reading model",
            Self::Pictures => "picture-description model",
        }
    }

    /// What is sent, as a message names it.
    pub(crate) fn noun(self) -> &'static str {
        match self {
            Self::Pages => "page",
            Self::Pictures => "picture",
        }
    }

    /// The image sent, as a bound's words name it.
    pub(crate) fn image(self) -> &'static str {
        match self {
            Self::Pages => "page image",
            Self::Pictures => "picture",
        }
    }
}

pub(crate) struct Chosen {
    pub(crate) state: SharedState,
    pub(crate) ctx: RequestCtx,
    /// The chosen model's lmgw name.
    pub(crate) alias: String,
    /// It answers in DocTags.
    pub(crate) doctags: bool,
    pub(crate) purpose: Purpose,
    /// The route and pool its calls go through, shared with the other
    /// purpose's when both name the same model (module doc).
    pub(crate) session: Arc<session::Session>,
}

impl Reader {
    /// No model: none is chosen, or the caller has no gateway to ask (a
    /// unit test's conversion).
    pub fn none() -> Self {
        Self::default()
    }

    /// Whether a page-reading model is chosen for the request.
    pub fn is_chosen(&self) -> bool {
        self.pages.is_some()
    }

    /// Whether a picture-description model is chosen for the request.
    pub fn describes(&self) -> bool {
        self.pictures.is_some()
    }

    /// The models the settings name, for `ctx`'s request.
    pub fn of(state: &SharedState, ctx: &RequestCtx) -> Self {
        let snap = state.snapshot();
        let d = &snap.settings.documents;
        let chosen = |alias: &str, doctags, purpose, session| {
            Arc::new(Chosen {
                state: state.clone(),
                ctx: ctx.clone(),
                alias: alias.to_string(),
                doctags,
                purpose,
                session,
            })
        };
        let page_alias = d.page_reader.trim();
        let pages = (!page_alias.is_empty()).then(|| {
            chosen(
                page_alias,
                d.page_reader_doctags,
                Purpose::Pages,
                Arc::default(),
            )
        });
        let picture_alias = d.picture_alias.trim();
        let pictures = (!picture_alias.is_empty()).then(|| {
            // The same model name: the same route and pool. Two names that
            // reach one model (an alias and the row it names) are two pools
            // yet, each bounded by the model's slots.
            let session = pages
                .as_ref()
                .filter(|p| p.alias == picture_alias)
                .map_or_else(Arc::default, |p| p.session.clone());
            chosen(picture_alias, false, Purpose::Pictures, session)
        });
        Self { pages, pictures }
    }
}

/// A conversion's model help, judged: the model and which pages go to it.
#[derive(Clone)]
pub struct Help {
    pub(crate) chosen: Arc<Chosen>,
    pub(crate) pages: select::Pages,
}

impl std::fmt::Debug for Help {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Help")
            .field("alias", &self.chosen.alias)
            .field("pages", &self.pages)
            .finish()
    }
}

impl PartialEq for Help {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.chosen, &other.chosen) && self.pages == other.pages
    }
}

impl Eq for Help {}

/// Judge the request's `model_help` against `reader`: who reads which
/// pages, or `None` for a conversion without a model, with the note that
/// says why when the request asked for one.
///
/// Absent, the option is the setting's choice: pages without text and
/// table pages once a model is chosen (§8's row), off without one. A
/// request can lower it (`off`, or `missing` for only the pages nothing
/// read) or widen it (`every_page`); whether the key may use the model is
/// checked where a page would go to it ([`pdf`]).
pub fn plan(o: &ConvertOptions, reader: &Reader, notes: &mut Vec<ConvertWarning>) -> Option<Help> {
    let Some(chosen) = &reader.pages else {
        if matches!(
            o.model_help,
            Some(ModelHelp::Missing | ModelHelp::MissingOrTables | ModelHelp::EveryPage)
        ) {
            notes.push(ConvertWarning {
                code: WarningCode::ModelNotChosen,
                message: "model help is asked for, but no page-reading model is chosen, so every \
                          page is read without a model; choose one under Settings → Documents \
                          (\"Read pages with a model\")"
                    .to_string(),
                tool: None,
                parts: Vec::new(),
                option: Some("model_help".to_string()),
            });
        }
        return None;
    };
    let pages = match o.model_help.unwrap_or(ModelHelp::MissingOrTables) {
        ModelHelp::Off => return None,
        ModelHelp::Missing => select::Pages::Missing,
        ModelHelp::MissingOrTables => select::Pages::MissingOrTables,
        ModelHelp::EveryPage => select::Pages::Every,
    };
    Some(Help {
        chosen: chosen.clone(),
        pages,
    })
}

/// What a picture file's "no reader" error adds when the request has model
/// help: the page-reading model reads a PDF's pages and not picture files
/// yet (not_yet), so the caller does not wait for it to.
pub fn picture_note(format: super::caps::Format, model_help: bool) -> &'static str {
    if model_help && format == super::caps::Format::Image {
        "; the page-reading model reads the pages of a PDF and does not read picture files yet \
         (not_yet)"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_chosen_model_asking_for_help_is_said_and_off_says_nothing() {
        let ask = |m| ConvertOptions {
            model_help: m,
            ..Default::default()
        };
        for m in [
            ModelHelp::Missing,
            ModelHelp::MissingOrTables,
            ModelHelp::EveryPage,
        ] {
            let mut notes = Vec::new();
            assert!(plan(&ask(Some(m)), &Reader::none(), &mut notes).is_none());
            assert_eq!(notes.len(), 1);
            assert_eq!(notes[0].code, WarningCode::ModelNotChosen);
            assert_eq!(notes[0].option.as_deref(), Some("model_help"));
            assert!(notes[0].message.contains("Settings → Documents"));
        }
        for m in [None, Some(ModelHelp::Off)] {
            let mut notes = Vec::new();
            assert!(plan(&ask(m), &Reader::none(), &mut notes).is_none());
            assert!(notes.is_empty());
        }
    }
}
