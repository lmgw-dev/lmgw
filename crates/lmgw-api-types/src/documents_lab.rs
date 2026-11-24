//! The Documents lab (document-conversion design §10, WP12): the dashboard's
//! playground for `POST /v1/documents/convert`, its async jobs and the
//! docling-compatible `POST /v1/convert/file`, on the lab store like the
//! Image and Audio labs (labs-store design §5).
//!
//! A click is a group of one cell: the form ([`DocumentsLabForm`]: the route,
//! the native options, the files in request order) with the uploads as the
//! group's input artifacts. Its run is the route's answer, each document's
//! text an output artifact named `lab-artifact:<id>` where the text was, so
//! the history reads small and a result's text is fetched when it is shown.
//!
//! The fields a request sends are built here ([`native_fields`],
//! [`docling_fields`]), once, for the gateway that dispatches them and the
//! page that shows them as curl, so the two cannot disagree.

use serde::{Deserialize, Serialize};

use crate::documents::convert::{ConvertOptions, ModelHelp, Ocr, Output, Pictures};
use crate::lab_store::{LabFileRef, LabGroupView};

/// A request's text fields in order, and the native options the route has
/// no field for.
pub type Fields = (Vec<(String, String)>, Vec<&'static str>);

/// The native route.
pub const CONVERT_ENDPOINT: &str = "/v1/documents/convert";
/// The native route's jobs: `/v1/documents/jobs/{id}`.
pub const JOBS_ENDPOINT: &str = "/v1/documents/jobs";
/// docling-serve's sync file route, as lmgw answers it.
pub const DOCLING_ENDPOINT: &str = "/v1/convert/file";

/// The plan's variant of a sync request on the native route.
pub const NATIVE_VARIANT: &str = "native";
/// The plan's variant of an `async: true` request on the native route: the
/// run is the job's result once it has ended.
pub const ASYNC_VARIANT: &str = "async";
/// The plan's variant of a request on the docling route.
pub const DOCLING_VARIANT: &str = "docling";

/// The model a cell names: the alias the usage log gives each route.
pub const NATIVE_MODEL: &str = "documents/convert";
pub const DOCLING_MODEL: &str = "docling/v1/convert/file";

/// Which route the lab sends the files to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DocumentsLabRoute {
    /// `POST /v1/documents/convert`.
    #[default]
    Native,
    /// `POST /v1/convert/file`: what a docling-serve client gets.
    Docling,
}

/// The route and the options: what a preset keeps (`document_options`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocumentsLabSettings {
    pub route: DocumentsLabRoute,
    /// The native route's options; on the docling route they are sent as
    /// docling's ([`docling_fields`]).
    pub options: ConvertOptions,
}

impl DocumentsLabSettings {
    /// What a preset must not hold: a picture area that is no number.
    pub fn check(&self) -> Result<(), String> {
        if !self.options.picture_min_area.is_finite() {
            return Err("picture_min_area is not a number".into());
        }
        if !self.options.images_scale.is_finite() {
            return Err("images_scale is not a number".into());
        }
        Ok(())
    }

    /// The plan's variant.
    pub fn variant(&self) -> &'static str {
        match (self.route, self.options.run_async) {
            (DocumentsLabRoute::Docling, _) => DOCLING_VARIANT,
            (DocumentsLabRoute::Native, true) => ASYNC_VARIANT,
            (DocumentsLabRoute::Native, false) => NATIVE_VARIANT,
        }
    }

    /// The route's path.
    pub fn endpoint(&self) -> &'static str {
        match self.route {
            DocumentsLabRoute::Native => CONVERT_ENDPOINT,
            DocumentsLabRoute::Docling => DOCLING_ENDPOINT,
        }
    }

    /// The model the cell names.
    pub fn model(&self) -> &'static str {
        match self.route {
            DocumentsLabRoute::Native => NATIVE_MODEL,
            DocumentsLabRoute::Docling => DOCLING_MODEL,
        }
    }

    /// The text fields the request sends, in order, or why the options
    /// cannot be sent on this route; with the options the route has no
    /// field for (docling), by name.
    pub fn fields(&self) -> Result<Fields, String> {
        match self.route {
            DocumentsLabRoute::Native => Ok((native_fields(&self.options), Vec::new())),
            DocumentsLabRoute::Docling => docling_fields(&self.options),
        }
    }
}

/// A group's form. Also what `POST /documents-lab/api/groups` takes as its
/// `form` part: a file with `id` 0 is the next `file` part of the upload, any
/// other names a file the store keeps, so a re-run sends kept files without
/// uploading them again.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocumentsLabForm {
    #[serde(flatten)]
    pub settings: DocumentsLabSettings,
    /// The files, in request order.
    pub files: Vec<LabFileRef>,
}

impl DocumentsLabForm {
    /// A group's title: its first file's name, and how many more.
    pub fn title(&self) -> String {
        match self.files.as_slice() {
            [] => "no files".into(),
            [one] => one.name.clone(),
            [first, rest @ ..] => format!("{} and {} more", first.name, rest.len()),
        }
    }
}

/// What `POST /documents-lab/api/groups/{id}/run` answers (labs-store
/// Decision 20: 200 whatever the route said): the group with its run, or,
/// for an async request, the group in flight and the job's id, whose state
/// the page reads at `/v1/documents/jobs/{job}`; its run lands when the job
/// has ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocumentsLabAnswer {
    pub group: LabGroupView,
    /// Why the run was not kept; the answer is the route's all the same.
    pub store_error: Option<String>,
    /// The async job (`cj_…`) the run waits for.
    pub job: Option<String>,
}

fn wire<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

fn bool_text(b: bool) -> String {
    if b { "true" } else { "false" }.into()
}

/// `/v1/documents/convert`'s form fields for `o`: every option that differs
/// from the route's default, under the route's own names.
pub fn native_fields(o: &ConvertOptions) -> Vec<(String, String)> {
    let d = ConvertOptions::default();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut put = |k: &str, v: String| out.push((k.to_string(), v));
    if o.output != d.output {
        put("output", wire(&o.output));
    }
    if let Some(p) = o.pages.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        put("pages", p.to_string());
    }
    if o.tables != d.tables {
        put("tables", bool_text(o.tables));
    }
    if o.compact_tables != d.compact_tables {
        put("compact_tables", bool_text(o.compact_tables));
    }
    if let Some(b) = o.page_break.as_deref().filter(|b| !b.is_empty()) {
        put("page_break", b.to_string());
    }
    if o.ocr != d.ocr {
        put("ocr", wire(&o.ocr));
    }
    if let Some(l) = o.ocr_languages.as_ref().filter(|l| !l.is_empty()) {
        put("ocr_languages", l.join(","));
    }
    if let Some(t) = o.document_timeout {
        put("document_timeout", t.to_string());
    }
    if let Some(m) = o.model_help {
        put("model_help", wire(&m));
    }
    if o.pictures != d.pictures {
        put("pictures", wire(&o.pictures));
    }
    if o.picture_min_area != d.picture_min_area {
        put("picture_min_area", o.picture_min_area.to_string());
    }
    if o.images_scale != d.images_scale {
        put("images_scale", o.images_scale.to_string());
    }
    if o.describe_pictures != d.describe_pictures {
        put("describe_pictures", bool_text(o.describe_pictures));
    }
    if let Some(p) = o.picture_prompt.as_deref().filter(|p| !p.is_empty()) {
        put("picture_prompt", p.to_string());
    }
    if o.run_async {
        put("async", "true".into());
    }
    out
}

/// One span of pages as docling's `page_range` (`[first, last]`): `"5"`,
/// `"3-7"`. Anything else has no docling spelling.
fn page_range(spec: &str) -> Result<(u32, u32), String> {
    let num = |s: &str| s.trim().parse::<u32>().ok().filter(|n| *n > 0);
    let bad = || {
        format!(
            "the docling route takes one span of pages (\"3-7\" or \"5\"), not \"{spec}\"; \
             send several spans on the native route"
        )
    };
    match spec.split_once('-') {
        None => num(spec).map(|n| (n, n)).ok_or_else(bad),
        Some((a, b)) => match (num(a), num(b)) {
            (Some(a), Some(b)) if a <= b => Ok((a, b)),
            _ => Err(bad()),
        },
    }
}

/// `/v1/convert/file`'s form fields for the native options `o`, each under
/// the docling option lmgw maps onto it (design §7.4.2), and the options
/// docling's form has no field for. Async on this route is a docling task
/// the lab does not follow yet: an error, like a page set of several spans.
pub fn docling_fields(o: &ConvertOptions) -> Result<Fields, String> {
    if o.run_async {
        return Err(
            "background jobs in docling's shape (/v1/convert/file/async) are not in this lab \
             yet; switch Background job off, or send the files on the native route"
                .into(),
        );
    }
    let d = ConvertOptions::default();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut unsent = Vec::new();
    let mut put = |k: &str, v: String| out.push((k.to_string(), v));
    put(
        "to_formats",
        match o.output {
            Output::Markdown => "md",
            Output::Text => "text",
            Output::Json => "json",
        }
        .into(),
    );
    if let Some(p) = o.pages.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        let (a, b) = page_range(p)?;
        put("page_range", a.to_string());
        put("page_range", b.to_string());
    }
    if !o.tables {
        put("do_table_structure", "false".into());
    }
    if o.compact_tables {
        put("md_compact_tables", "true".into());
    }
    if let Some(b) = o.page_break.as_deref().filter(|b| !b.is_empty()) {
        put("md_page_break_placeholder", b.to_string());
    }
    match o.ocr {
        Ocr::Off => put("do_ocr", "false".into()),
        Ocr::Force => put("force_ocr", "true".into()),
        Ocr::Auto => {}
    }
    if let Some(l) = o.ocr_languages.as_ref().filter(|l| !l.is_empty()) {
        for code in l {
            put("ocr_lang", code.clone());
        }
    }
    if let Some(t) = o.document_timeout {
        put("document_timeout", t.to_string());
    }
    match o.model_help {
        Some(ModelHelp::EveryPage) => put("pipeline", "vlm".into()),
        Some(ModelHelp::MissingOrTables) => put("table_mode", "accurate".into()),
        // `fast` keeps table pages from a chosen page-reading model, which
        // then reads only the pages nothing else read: the native `missing`.
        Some(ModelHelp::Missing) => put("table_mode", "fast".into()),
        // docling has no option that keeps every page from a chosen model:
        // `fast` is the least it can ask for, and `off` itself is not sent.
        Some(ModelHelp::Off) => {
            put("table_mode", "fast".into());
            unsent.push("model_help");
        }
        None => {}
    }
    if o.pictures != d.pictures {
        put(
            "image_export_mode",
            match o.pictures {
                Pictures::Placeholder => "placeholder",
                Pictures::Embedded => "embedded",
                Pictures::Referenced => "referenced",
            }
            .into(),
        );
    }
    if o.describe_pictures {
        put("do_picture_description", "true".into());
    }
    if o.picture_min_area != d.picture_min_area {
        put(
            "picture_description_area_threshold",
            o.picture_min_area.to_string(),
        );
    }
    if o.images_scale != d.images_scale {
        put("images_scale", o.images_scale.to_string());
    }
    // docling's picture-description settings carry the prompt; lmgw takes
    // it from any of them and its own chosen model answers.
    if let Some(p) = o.picture_prompt.as_deref().filter(|p| !p.is_empty()) {
        put(
            "picture_description_local",
            serde_json::json!({ "prompt": p }).to_string(),
        );
    }
    Ok((out, unsent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_native_fields_are_the_options_that_differ_from_the_default() {
        assert!(native_fields(&ConvertOptions::default()).is_empty());
        let o = ConvertOptions {
            output: Output::Json,
            pages: Some(" 3-7,10 ".into()),
            tables: false,
            ocr: Ocr::Force,
            ocr_languages: Some(vec!["eng".into(), "deu".into()]),
            model_help: Some(ModelHelp::MissingOrTables),
            pictures: Pictures::Embedded,
            images_scale: 1.5,
            run_async: true,
            ..Default::default()
        };
        let f = native_fields(&o);
        let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("output"), Some("json"));
        assert_eq!(get("pages"), Some("3-7,10"));
        assert_eq!(get("tables"), Some("false"));
        assert_eq!(get("ocr"), Some("force"));
        assert_eq!(get("ocr_languages"), Some("eng,deu"));
        assert_eq!(get("model_help"), Some("missing_or_tables"));
        assert_eq!(get("pictures"), Some("embedded"));
        assert_eq!(get("images_scale"), Some("1.5"));
        assert_eq!(get("async"), Some("true"));
        assert_eq!(get("compact_tables"), None);
    }

    #[test]
    fn the_docling_fields_name_docling_options_and_refuse_what_it_cannot_say() {
        let o = ConvertOptions {
            output: Output::Text,
            pages: Some("2-5".into()),
            ocr: Ocr::Off,
            compact_tables: true,
            picture_prompt: Some("describe".into()),
            images_scale: 3.0,
            ..Default::default()
        };
        let (f, unsent) = docling_fields(&o).unwrap();
        assert!(f.contains(&("images_scale".into(), "3".into())));
        assert!(f.contains(&("to_formats".into(), "text".into())));
        let range: Vec<&str> = f
            .iter()
            .filter(|(k, _)| k == "page_range")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(range, ["2", "5"]);
        assert!(f.contains(&("do_ocr".into(), "false".into())));
        assert!(f.contains(&("md_compact_tables".into(), "true".into())));
        assert!(f.contains(&(
            "picture_description_local".into(),
            r#"{"prompt":"describe"}"#.into()
        )));
        assert!(unsent.is_empty(), "{unsent:?}");
        // Model help: docling's options say every page, table pages, or
        // (fast) the unread pages only; "off" has no docling option, so the
        // least is sent and the lab says `model_help` was not.
        let help = |m| {
            docling_fields(&ConvertOptions {
                model_help: Some(m),
                ..Default::default()
            })
            .unwrap()
        };
        let (f, unsent) = help(ModelHelp::Missing);
        assert!(f.contains(&("table_mode".into(), "fast".into())));
        assert!(unsent.is_empty());
        let (f, unsent) = help(ModelHelp::Off);
        assert!(f.contains(&("table_mode".into(), "fast".into())));
        assert_eq!(unsent, ["model_help"]);
        let (f, _) = help(ModelHelp::MissingOrTables);
        assert!(f.contains(&("table_mode".into(), "accurate".into())));
        let (f, _) = help(ModelHelp::EveryPage);
        assert!(f.contains(&("pipeline".into(), "vlm".into())));

        let several = ConvertOptions {
            pages: Some("1-2,9".into()),
            ..Default::default()
        };
        assert!(docling_fields(&several).unwrap_err().contains("one span"));
        let job = ConvertOptions {
            run_async: true,
            ..Default::default()
        };
        assert!(docling_fields(&job)
            .unwrap_err()
            .contains("not in this lab yet"));
        assert_eq!(page_range("4").unwrap(), (4, 4));
        assert!(page_range("7-3").is_err());
    }

    #[test]
    fn a_form_reads_back_with_its_files_and_names_its_variant() {
        let f = DocumentsLabForm {
            settings: DocumentsLabSettings {
                route: DocumentsLabRoute::Native,
                options: ConvertOptions {
                    run_async: true,
                    ..Default::default()
                },
            },
            files: vec![
                LabFileRef {
                    id: 0,
                    name: "example-a.pdf".into(),
                },
                LabFileRef {
                    id: 7,
                    name: "example-b.docx".into(),
                },
            ],
        };
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["route"], "native");
        assert_eq!(v["options"]["async"], true);
        assert_eq!(serde_json::from_value::<DocumentsLabForm>(v).unwrap(), f);
        assert_eq!(f.settings.variant(), ASYNC_VARIANT);
        assert_eq!(f.title(), "example-a.pdf and 1 more");
        assert_eq!(DocumentsLabForm::default().title(), "no files");
    }
}
