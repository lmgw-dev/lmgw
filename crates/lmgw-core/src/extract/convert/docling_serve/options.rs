//! docling's options mapped onto the native ones (document-conversion
//! design §7.4.2): every option a request sets gets one outcome —
//! `honoured`, `mapped`, `not_yet` or `refused` — and the native options it
//! becomes.
//!
//! **Map by option.** An option whose feature has a native option sets that
//! native option and takes the native route's own answer: the native plan's
//! note for that option ([`Mapped::settle`]) turns `honoured` into `not_yet`
//! (or `mapped` where the native note is `model_not_chosen`). Those options
//! are `image_export_mode` and `include_images` (native `pictures`, built:
//! WP10), `images_scale` (native `images_scale`),
//! `do_picture_description` and its settings, `table_mode` (`accurate` and
//! `fast` onto the native `model_help`, with `pipeline: vlm`),
//! `do_ocr`/`force_ocr`,
//! `page_range`, `md_*`, `pipeline: asr` (WP7: audio and video are
//! transcribed by the chosen speech-to-text model on every pipeline, so
//! `asr` is `mapped` onto it). When the native route builds the feature behind one
//! of them (picture export, WP10; picture descriptions, WP11; tier 1, WP6)
//! it answers for them here without a change to this table.
//!
//! Formula and code enrichment (`do_formula_enrichment`,
//! `do_code_enrichment`, `code_formula_preset`, `code_formula_custom_config`)
//! are `mapped` (D23): the page-reading model's prompt asks for formulas as
//! LaTeX and code as fenced blocks on every page it reads, and no option
//! picks pages for them; which pages it reads is `model_help`'s.
//!
//! Options with **no native option yet** are `not_yet` here by name, and the
//! work package that adds the native option has to change this table too:
//! the picture classification, chart extraction and chunking options and
//! their `*_custom_config` (§12). The build notes in the design doc's §14
//! list them again.
//!
//! Types are checked as docling checks them: a wrong type for an option
//! docling knows is a 422 in docling's shape ([`Invalid`]); enum values are
//! read leniently (an unknown one is mapped or a note, never a refusal), as
//! docling's own lists keep growing.

use std::time::Duration;

use lmgw_api_types::documents::adapter_usage::OptionOutcome;
use lmgw_api_types::documents::convert::{
    ConvertOptions, ConvertWarning, ModelHelp, Ocr, Output, Pictures, WarningCode,
};
use serde_json::{Map, Value};

use super::lang;

mod kinds;
mod picture;
pub mod unknown;

use crate::extract::convert::calls::{Noted, Value as Rec};
use crate::extract::convert::quoted::quoted;
pub use kinds::form_value;
use kinds::{check, kind_of};

/// The note of docling's formula and code enrichment options (D23).
const FORMULAS_AND_CODE: &str = "page-reading model on the pages it reads";

/// The note of docling's picture-description options: lmgw's own model.
const PICTURE_MODEL: &str = "picture-description model";

/// The outputs lmgw writes: docling's `md` and `text`.
pub const OUTPUTS: &[&str] = &["md", "text"];

/// A docling validation error: `loc`, `msg`, `type` as FastAPI lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid(pub Vec<(String, String, &'static str)>);

impl Invalid {
    /// docling's 422 `detail` list; `at` is where the options sit (`body`
    /// and `options` for JSON, `body` for a form).
    pub fn detail(&self, at: &[&str]) -> Value {
        Value::Array(
            self.0
                .iter()
                .map(|(name, msg, ty)| {
                    let mut loc: Vec<Value> = at.iter().map(|s| Value::from(*s)).collect();
                    loc.push(Value::from(name.as_str()));
                    serde_json::json!({ "loc": loc, "msg": msg, "type": ty })
                })
                .collect(),
        )
    }
}

/// The options mapped.
#[derive(Debug, Clone, PartialEq)]
pub struct Mapped {
    pub native: ConvertOptions,
    /// The outputs asked for (docling's default: `md`).
    pub md: bool,
    pub text: bool,
    /// `to_formats` lmgw does not write yet (`json`, `html`, …).
    pub missing_outputs: Vec<String>,
    /// `from_formats`, when the request set it.
    pub from_formats: Option<Vec<String>>,
    pub abort_on_error: bool,
    pub deadline: Option<Duration>,
    /// Every option the request set, with its outcome.
    pub noted: Vec<Noted>,
    /// `table_mode: fast`: tables stay with lmgw's own reader, so table
    /// pages do not go to the page-reading model ([`Self::lower_for`]).
    pub tables_fast: bool,
    /// Which native option each docling option set (for [`Self::settle`]).
    setters: Vec<(&'static str, usize)>,
}

fn strs(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

/// A caller's value as recorded: bounded (`quoted`, security round B, M4),
/// as the request record and `x-lmgw-option-notes` keep it.
fn enumv(v: &Value) -> Rec {
    Rec::Enum(quoted(&match v {
        Value::String(s) => s.clone(),
        Value::Array(_) => strs(v).join(","),
        Value::Null => "null".into(),
        other => other.to_string(),
    }))
}

impl Mapped {
    /// `table_mode: fast` against the request's page-reading model: with
    /// one chosen (`chosen`), and nothing else of the request having asked
    /// for the model (`pipeline: vlm`), the native `model_help` is
    /// `missing`, so the model reads only pages nothing else read. Without
    /// a chosen model nothing changes: no model is asked either way, and
    /// the request did not ask for one.
    pub fn lower_for(&mut self, chosen: bool) {
        if self.tables_fast && chosen && self.native.model_help.is_none() {
            self.native.model_help = Some(ModelHelp::Missing);
        }
    }

    fn note(&mut self, n: Noted) -> usize {
        self.noted.push(n);
        self.noted.len() - 1
    }

    fn sets(&mut self, native: &'static str, n: Noted) {
        let i = self.note(n);
        self.setters.push((native, i));
    }

    /// The native plan's notes: each docling option that set a native
    /// option with a note takes its outcome.
    pub fn settle(&mut self, native_notes: &[ConvertWarning]) {
        for w in native_notes {
            let Some(opt) = w.option.as_deref() else {
                continue;
            };
            let (outcome, detail) = match w.code {
                WarningCode::NotYet => (OptionOutcome::NotYet, None),
                WarningCode::OptionNotApplicable => continue,
                WarningCode::ModelNotChosen => {
                    (OptionOutcome::Mapped, Some("model_not_chosen".to_string()))
                }
                other => (
                    OptionOutcome::Mapped,
                    serde_json::to_value(other)
                        .ok()
                        .and_then(|v| v.as_str().map(String::from)),
                ),
            };
            for (native, i) in &self.setters {
                if *native == opt && self.noted[*i].outcome == OptionOutcome::Honoured {
                    self.noted[*i].outcome = outcome;
                    if detail.is_some() {
                        self.noted[*i].detail = detail.clone();
                    }
                }
            }
        }
        // Pictures the native route describes are described by the model
        // the owner chose, never the caller's (§7.4.2): lmgw's own, so the
        // callers' summary shows who asks for descriptions.
        for (native, i) in &self.setters {
            let n = &mut self.noted[*i];
            if *native == "describe_pictures" && n.outcome == OptionOutcome::Honoured {
                n.outcome = OptionOutcome::Mapped;
                n.detail = Some(PICTURE_MODEL.to_string());
            }
        }
    }
}

/// Map `opts` (docling's options by name).
pub fn map(opts: &Map<String, Value>) -> Result<Mapped, Invalid> {
    let mut bad = Vec::new();
    for (name, v) in opts {
        if let Some(kind) = kind_of(name) {
            if let Err((msg, ty)) = check(name, kind, v) {
                bad.push((name.clone(), msg, ty));
            }
        }
    }
    if !bad.is_empty() {
        return Err(Invalid(bad));
    }
    let mut too_long: Vec<(String, String, &'static str)> = Vec::new();
    let mut unknown = unknown::Names::default();
    let mut m = Mapped {
        // docling exports every picture its layout finds, whatever its size
        // (its area threshold only skips descriptions): no picture_min_area.
        native: ConvertOptions {
            picture_min_area: 0.0,
            ..ConvertOptions::default()
        },
        md: true,
        text: false,
        missing_outputs: Vec::new(),
        from_formats: None,
        abort_on_error: false,
        deadline: None,
        noted: Vec::new(),
        tables_fast: false,
        setters: Vec::new(),
    };
    let honoured = |name: &str, v: &Value| Noted::new(name, enumv(v), OptionOutcome::Honoured);
    let set_only = |name: &str, o: OptionOutcome| Noted::new(name, Rec::Set, o);
    let do_ocr = opts.get("do_ocr").and_then(Value::as_bool).unwrap_or(true);
    for (name, v) in opts {
        let b = v.as_bool().unwrap_or(false);
        match name.as_str() {
            "from_formats" => {
                m.from_formats = (!v.is_null()).then(|| strs(v));
                m.note(honoured(name, v));
            }
            "to_formats" => {
                let asked = strs(v);
                m.md = asked.iter().any(|f| f == "md");
                m.text = asked.iter().any(|f| f == "text");
                m.missing_outputs = asked
                    .iter()
                    .filter(|f| !OUTPUTS.contains(&f.as_str()))
                    .cloned()
                    .collect();
                let mut n = honoured(name, v);
                if !m.missing_outputs.is_empty() {
                    n.outcome = OptionOutcome::NotYet;
                    n.detail = Some(quoted(&m.missing_outputs.join(" ")));
                }
                m.note(n);
            }
            "image_export_mode" => {
                let mode = v.as_str().unwrap_or_default();
                m.native.pictures = match mode {
                    "embedded" => Pictures::Embedded,
                    "referenced" => Pictures::Referenced,
                    _ => Pictures::Placeholder,
                };
                let mut n = honoured(name, v);
                if !matches!(mode, "placeholder" | "embedded" | "referenced") {
                    n.outcome = OptionOutcome::Mapped;
                    n.detail = Some(format!("{}->placeholder", quoted(mode)));
                }
                m.sets("pictures", n);
            }
            // Each takes the native note only when it changed the native
            // option: `do_ocr: true` is the default and says nothing.
            "do_ocr" => {
                if b {
                    m.note(honoured(name, v));
                } else {
                    m.native.ocr = Ocr::Off;
                    m.sets("ocr", honoured(name, v));
                }
            }
            "force_ocr" => {
                if b && do_ocr {
                    m.native.ocr = Ocr::Force;
                    m.sets("ocr", honoured(name, v));
                } else {
                    m.note(honoured(name, v));
                }
            }
            "ocr_engine" | "ocr_preset" => {
                let engine = v.as_str().unwrap_or_default();
                let mut n = honoured(name, v);
                if engine != "auto" {
                    n.outcome = OptionOutcome::Mapped;
                    n.detail = Some(format!("{}->lmgw-ocr", quoted(engine)));
                }
                m.note(n);
            }
            "ocr_lang" => {
                let tags = strs(v);
                // Judged before anything is mapped: a list past the bound is
                // docling's 422 for a list that is too long.
                let most = crate::extract::convert::ocr::langs::MAX_OCR_LANGUAGES;
                if tags.len() > most {
                    too_long.push((
                        name.clone(),
                        format!(
                            "{name}: List should have at most {most} items \
                             (MAX_OCR_LANGUAGES), not {}",
                            tags.len()
                        ),
                        "too_long",
                    ));
                    continue;
                }
                let (codes, unknown) = lang::to_tesseract(&tags);
                m.native.ocr_languages = codes;
                let mut n = honoured(name, v);
                if tags.iter().any(|t| t == "mul") {
                    n.outcome = OptionOutcome::Mapped;
                    n.detail = Some("mul->installed packs".into());
                } else if !unknown.is_empty() {
                    n.outcome = OptionOutcome::Mapped;
                    n.detail = Some(format!(
                        "no tesseract code known for {}",
                        quoted(&unknown.join(" "))
                    ));
                }
                m.sets("ocr_languages", n);
            }
            "pdf_backend" => {
                m.note(honoured(name, v).with(OptionOutcome::Mapped, "poppler"));
            }
            "table_mode" => {
                // `accurate` sends table-like pages to the page-reading model
                // (native `model_help`, never lowering `pipeline: vlm`'s every
                // page). `fast` leaves tables to lmgw's own reader: once a
                // page-reading model is chosen it lowers the request to the
                // native `missing` (only pages nothing read go to the
                // model; [`Mapped::tables_fast`], applied where the request's
                // reader is known), and the note says so.
                if v.as_str() == Some("accurate") {
                    if m.native.model_help != Some(ModelHelp::EveryPage) {
                        m.native.model_help = Some(ModelHelp::MissingOrTables);
                    }
                    m.sets("model_help", honoured(name, v));
                } else {
                    m.tables_fast = true;
                    m.note(honoured(name, v).with(
                        OptionOutcome::Mapped,
                        "lmgw tables and a chosen page-reading model on unread pages only",
                    ));
                }
            }
            "do_table_structure" => {
                m.native.tables = b;
                m.sets("tables", honoured(name, v));
            }
            "table_cell_matching" => {
                m.note(honoured(name, v).with(OptionOutcome::Mapped, "lmgw tables"));
            }
            "table_structure_preset" | "layout_preset" => {
                m.note(honoured(name, v).with(OptionOutcome::Mapped, "lmgw layout"));
            }
            "table_structure_custom_config" | "layout_custom_config" => {
                m.note(set_only(name, OptionOutcome::Mapped).detail("lmgw layout"));
            }
            "pipeline" => {
                let p = v.as_str().unwrap_or_default();
                let n = honoured(name, v);
                match p {
                    "standard" => {
                        m.note(n);
                    }
                    "vlm" => {
                        m.native.model_help = Some(ModelHelp::EveryPage);
                        m.sets(
                            "model_help",
                            n.with(OptionOutcome::Mapped, "page-reading model on every page"),
                        );
                    }
                    // Audio and video are transcribed by the speech-to-text
                    // model chosen under Settings › Documents whatever the
                    // pipeline (WP7); other files take the native one.
                    "asr" => {
                        m.note(n.with(OptionOutcome::Mapped, "speech-to-text model"));
                    }
                    other => {
                        m.note(n.with(
                            OptionOutcome::Mapped,
                            &format!("{}->standard", quoted(other)),
                        ));
                    }
                }
            }
            "vlm_pipeline_preset" | "vlm_pipeline_model" => {
                m.note(honoured(name, v).with(OptionOutcome::Mapped, "page-reading model"));
            }
            "vlm_pipeline_model_local"
            | "vlm_pipeline_model_api"
            | "vlm_pipeline_custom_config" => {
                let url = v.get("url").is_some() || v.pointer("/engine_options/url").is_some();
                let d = if url {
                    "page-reading model with its url not followed"
                } else {
                    "page-reading model"
                };
                m.note(set_only(name, OptionOutcome::Mapped).detail(d));
            }
            "do_pdf_heading_hierarchy" => {
                m.note(honoured(name, v).with(OptionOutcome::Mapped, "lmgw heading levels"));
            }
            "pdf_heading_hierarchy_options" => {
                m.note(set_only(name, OptionOutcome::Mapped).detail("lmgw heading levels"));
            }
            "page_range" => {
                let r: Vec<u64> = v
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_u64).collect())
                    .unwrap_or_default();
                if let [a, b] = r[..] {
                    let last = u32::try_from(b).unwrap_or(u32::MAX);
                    let first = u32::try_from(a).unwrap_or(u32::MAX);
                    // docling's default, `[1, sys.maxsize]`, is every page.
                    if !(first == 1 && last == u32::MAX) {
                        m.native.pages = Some(format!("{first}-{last}"));
                    }
                }
                m.sets("pages", set_only(name, OptionOutcome::Honoured));
            }
            "document_timeout" => {
                // A value no duration can hold (past 5.8e11 years) is a 422
                // as docling's validation answers a number it cannot take.
                let secs = v.as_f64().filter(|s| s.is_finite() && *s > 0.0);
                if let Some(s) = secs {
                    match Duration::try_from_secs_f64(s) {
                        Ok(d) => m.deadline = Some(d),
                        Err(_) => too_long.push((
                            name.clone(),
                            format!(
                                "{name}: Input should be a number of seconds a timeout can hold"
                            ),
                            "less_than_equal",
                        )),
                    }
                }
                m.note(set_only(name, OptionOutcome::Honoured));
            }
            "abort_on_error" => {
                m.abort_on_error = b;
                m.note(honoured(name, v));
            }
            "include_images" => {
                // Native `pictures`: judged after the loop, beside
                // `image_export_mode`, which names the form.
                m.sets("pictures", honoured(name, v));
            }
            "images_scale" => {
                // Native `images_scale`: the crops' resolution (§5.3); a
                // scale the native plan refuses is docling's 422.
                if let Some(x) = v.as_f64() {
                    m.native.images_scale = x;
                }
                m.sets("images_scale", honoured(name, v));
            }
            "include_page_images" => {
                let n = honoured(name, v);
                m.note(if b {
                    n.with(OptionOutcome::NotYet, "page images")
                } else {
                    n
                });
            }
            "md_page_break_placeholder" => {
                let p = v.as_str().unwrap_or_default();
                m.native.page_break = (!p.is_empty()).then(|| p.to_string());
                m.sets("page_break", set_only(name, OptionOutcome::Honoured));
            }
            "md_compact_tables" => {
                m.native.compact_tables = b;
                m.sets("compact_tables", honoured(name, v));
            }
            "do_picture_description" => {
                m.native.describe_pictures = b;
                if b {
                    m.sets("describe_pictures", honoured(name, v));
                } else {
                    m.note(honoured(name, v));
                }
            }
            // docling skips the description of a smaller picture and keeps
            // the picture (probed in docling 2.142.1): the native
            // describe_min_area, which leaves the export alone.
            "picture_description_area_threshold" => {
                if let Some(t) = v.as_f64() {
                    m.native.describe_min_area = t;
                }
                m.sets("describe_min_area", honoured(name, v));
            }
            "picture_description_preset" => {
                m.note(honoured(name, v).with(OptionOutcome::Mapped, PICTURE_MODEL));
            }
            "picture_description_local"
            | "picture_description_api"
            | "picture_description_custom_config" => {
                if let Some(p) = v.get("prompt").and_then(Value::as_str) {
                    m.native.picture_prompt = Some(p.to_string());
                }
                m.sets(
                    "picture_prompt",
                    set_only(name, OptionOutcome::Mapped).detail(picture::detail(PICTURE_MODEL, v)),
                );
            }
            "do_picture_classification" | "do_chart_extraction" => {
                let n = honoured(name, v);
                m.note(if b {
                    n.with(OptionOutcome::NotYet, "")
                } else {
                    n
                });
            }
            // D23: formulas and code are what the page-reading model writes
            // on the pages it reads (its prompt asks for LaTeX and fenced
            // code); no option picks pages for them.
            "do_formula_enrichment" | "do_code_enrichment" => {
                let n = honoured(name, v);
                m.note(if b {
                    n.with(OptionOutcome::Mapped, FORMULAS_AND_CODE)
                } else {
                    n
                });
            }
            "code_formula_preset" => {
                m.note(honoured(name, v).with(OptionOutcome::Mapped, FORMULAS_AND_CODE));
            }
            "code_formula_custom_config" => {
                m.note(set_only(name, OptionOutcome::Mapped).detail(FORMULAS_AND_CODE));
            }
            "picture_classification_preset" | "chart_extraction_preset" | "chunking_preset" => {
                m.note(honoured(name, v).with(OptionOutcome::NotYet, ""));
            }
            "picture_classification_custom_config"
            | "chart_extraction_custom_config"
            | "chunking_options"
            | "ocr_custom_config" => {
                m.note(set_only(name, OptionOutcome::NotYet));
            }
            // An option this table does not know: a note with its name,
            // never a 422.
            _ => {
                if let Some(n) = unknown.note(name) {
                    m.note(n);
                }
            }
        }
    }
    // What lmgw writes when text alone is asked for: the native text form.
    if m.text && !m.md {
        m.native.output = Output::Text;
    }
    // `include_images` against the form `image_export_mode` names: off keeps
    // placeholders whatever the mode (docling makes no picture images to
    // embed or reference); on leaves the mode's form, and with no mode asked
    // that is docling's default, placeholder (`image_export_mode` defaults
    // to `placeholder` in docling's `ConvertDocumentsOptions`, main and
    // v2.142.1, read 2026-11-19).
    if opts.get("include_images").and_then(Value::as_bool) == Some(false) {
        m.native.pictures = Pictures::Placeholder;
    }
    if let Some(n) = unknown.rest() {
        m.note(n);
    }
    if !too_long.is_empty() {
        return Err(Invalid(too_long));
    }
    Ok(m)
}

trait With {
    fn with(self, o: OptionOutcome, detail: &str) -> Self;
}

impl With for Noted {
    fn with(mut self, o: OptionOutcome, detail: &str) -> Self {
        self.outcome = o;
        self.detail = (!detail.is_empty()).then(|| detail.to_string());
        self
    }
}

#[cfg(test)]
mod tests;
