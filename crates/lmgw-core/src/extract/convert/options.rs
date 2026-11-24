//! The request's options (document-conversion design §7.1): read from the
//! multipart form or the JSON body into [`ConvertOptions`], then judged into
//! the [`Plan`] the files are converted by.
//!
//! Nothing is dropped. An unknown field is a 400 that names it. An option
//! whose feature is not built yet is accepted at its default; another value
//! is a request-wide warning that says what was done without it (`not_yet`,
//! or `model_not_chosen` where the design names that code). `async: true`
//! is the HTTP half's (a job instead of the answer).

use lmgw_api_types::documents::convert::{
    ConvertOptions, ConvertWarning, Ocr, Output, Pictures, WarningCode,
};
use lmgw_api_types::documents::InstallPlatform;
use serde_json::{Map, Value};

use super::model_help::{self, Reader};
use super::ocr::langs;
use super::pages::PageSet;
use super::quoted::quoted;

/// A request refused before any file is read: a 400 with its code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    fn invalid(message: String) -> Self {
        Self {
            code: "invalid_option",
            message,
        }
    }
}

/// The form fields that are true or false.
const BOOLS: &[&str] = &["tables", "describe_pictures", "compact_tables", "async"];

/// The options of a multipart form: each text field by its name (a field
/// given twice is refused), the true/false and number fields read from
/// their text, `ocr_languages` as codes separated by commas, `+` or spaces.
pub fn from_form(fields: Vec<(String, String)>) -> Result<ConvertOptions, Refusal> {
    let mut map = Map::new();
    for (name, text) in fields {
        let value = if BOOLS.contains(&name.as_str()) {
            match text.trim() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                other => {
                    return Err(Refusal::invalid(format!(
                        "{name} is '{}'; it takes true or false",
                        quoted(other)
                    )))
                }
            }
        } else if [
            "picture_min_area",
            "describe_min_area",
            "images_scale",
            "document_timeout",
        ]
        .contains(&name.as_str())
        {
            let nan =
                || Refusal::invalid(format!("{name} is '{}'; it takes a number", quoted(&text)));
            let n: f64 = text.trim().parse().map_err(|_| nan())?;
            serde_json::Number::from_f64(n)
                .map(Value::Number)
                .ok_or_else(nan)?
        } else if name == "ocr_languages" {
            Value::Array(
                text.split([',', '+', ' '])
                    .filter(|s| !s.is_empty())
                    .map(|s| Value::String(s.to_string()))
                    .collect(),
            )
        } else {
            Value::String(text)
        };
        if map.insert(name.clone(), value).is_some() {
            return Err(Refusal::invalid(format!(
                "the field {} is given twice; give each option once",
                quoted(&name)
            )));
        }
    }
    from_map(map)
}

/// The options of a JSON body, `files` already taken out.
pub fn from_map(map: Map<String, Value>) -> Result<ConvertOptions, Refusal> {
    serde_json::from_value(Value::Object(map)).map_err(|e| {
        Refusal::invalid(format!(
            "the request's options: {}",
            super::quoted::quotes_bounded(&e.to_string())
        ))
    })
}

/// What the files are converted by: the options judged.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub output: Output,
    pub pages: Option<PageSet>,
    pub tables: bool,
    pub compact_tables: bool,
    pub page_break: Option<String>,
    /// What pictures become (`pictures`, `picture_min_area`,
    /// `images_scale`; design §5.3).
    pub pictures: super::pictures::Settings,
    /// Also write the `output: text` form beside the markdown
    /// ([`ConvertedDocument::text`]): the docling route asked for both.
    pub also_text: bool,
    /// Who the install hints of a missing tool are written for.
    pub platform: InstallPlatform,
    /// Text recognition: which pages (`ocr.rs`).
    pub ocr: Ocr,
    /// The languages asked for, judged ([`langs::validated`]: each a code by
    /// its shape, each once, at most `MAX_OCR_LANGUAGES`); `None`: the
    /// default (`ocr/langs.rs`).
    pub ocr_languages: Option<Vec<String>>,
    /// What they resolve to against the installed packs, found once per
    /// request by the first file that needs it.
    pub ocr_chosen: langs::Chosen,
    /// The most one file's conversion may take from its first slot on
    /// (`document_timeout`); `None`: no deadline per file.
    pub deadline: Option<std::time::Duration>,
    /// Where a file whose pages are read one by one is spooled
    /// (`ocr/spool.rs`): `<data dir>/convert-spool`. `None` (a test without
    /// a data dir): each page's tool is sent the file.
    pub spool_dir: Option<std::path::PathBuf>,
    /// The page-reading model and the pages that go to it; `None`: no page
    /// goes to a model.
    pub model_help: Option<model_help::Help>,
    /// The picture-description model and its prompt; `None`: no picture
    /// goes to a model.
    pub describe: Option<super::describe::Describe>,
    /// Who transcribes audio and video (`media.rs`): the calling key and
    /// the speech-to-text model chosen when the request was planned.
    /// `None` (Chat's and knowledge bases' reads, a test): nothing is
    /// transcribed.
    pub speech: Option<super::media::Speech>,
}

impl Plan {
    /// The plan with its spool directory under `data_dir`.
    pub fn spooling_under(mut self, data_dir: &std::path::Path) -> Self {
        self.spool_dir = Some(data_dir.join(super::ocr::spool::SPOOL_DIR));
        self
    }

    /// The plan with the request's speech-to-text model and key (WP7).
    pub fn with_speech(
        mut self,
        state: &crate::state::SharedState,
        ctx: &crate::proxy::RequestCtx,
    ) -> Self {
        self.speech = Some(super::media::Speech::of(state, ctx));
        self
    }
}

fn note(code: WarningCode, option: &str, message: impl Into<String>) -> ConvertWarning {
    ConvertWarning {
        code,
        message: message.into(),
        tool: None,
        parts: Vec::new(),
        option: Some(option.to_string()),
    }
}

/// Judge `o` for a conversion without a page-reading model (none is chosen,
/// or the caller has no gateway to ask one through): [`plan_for`].
pub fn plan(
    o: &ConvertOptions,
    platform: InstallPlatform,
) -> Result<(Plan, Vec<ConvertWarning>), Refusal> {
    plan_for(o, platform, &Reader::none())
}

/// Judge `o`: the plan, and the request-wide notes for the options whose
/// feature is not built yet. `reader` is the page-reading model the
/// request may use.
pub fn plan_for(
    o: &ConvertOptions,
    platform: InstallPlatform,
    reader: &Reader,
) -> Result<(Plan, Vec<ConvertWarning>), Refusal> {
    let pages = o
        .pages
        .as_deref()
        .map(PageSet::parse)
        .transpose()
        .map_err(Refusal::invalid)?;
    if !(0.0..=1.0).contains(&o.picture_min_area) {
        return Err(Refusal::invalid(format!(
            "picture_min_area is {}; it is a share of the page, from 0 to 1",
            o.picture_min_area
        )));
    }
    if !(0.0..=1.0).contains(&o.describe_min_area) {
        return Err(Refusal::invalid(format!(
            "describe_min_area is {}; it is a share of the page, from 0 to 1",
            o.describe_min_area
        )));
    }
    // No upper bound: a scale whose page crops pass the raster bound is that
    // page's named warning (`pictures::render`).
    if !(o.images_scale.is_finite() && o.images_scale > 0.0) {
        return Err(Refusal::invalid(format!(
            "images_scale is {}; it is a scale above 0 (2.0 crops PDF pictures at 144 dpi)",
            o.images_scale
        )));
    }
    let ocr_languages = o
        .ocr_languages
        .as_deref()
        .map(langs::validated)
        .transpose()
        .map_err(Refusal::invalid)?;
    let deadline = match o.document_timeout {
        None => None,
        Some(s) => Some(
            Some(s)
                .filter(|s| s.is_finite() && *s > 0.0)
                .and_then(|s| std::time::Duration::try_from_secs_f64(s).ok())
                .ok_or_else(|| {
                    Refusal::invalid(format!(
                        "document_timeout is {s}; it is a number of seconds above 0 (leave it \
                         out for no deadline per file)"
                    ))
                })?,
        ),
    };
    let mut notes = Vec::new();
    let model_help = model_help::plan(o, reader, &mut notes);
    if o.pictures != Pictures::Placeholder && o.output == Output::Text {
        notes.push(note(
            WarningCode::OptionNotApplicable,
            "pictures",
            "pictures: output text writes no pictures, so none was embedded or referenced",
        ));
    }
    let describe = super::describe::plan(o, reader, &mut notes);
    if o.compact_tables && (o.output != Output::Markdown || !o.tables) {
        let why = match o.output {
            Output::Text => "output text writes tables as lines of cells",
            Output::Json => "output json gives tables as cells",
            Output::Markdown => "tables false writes tables as lines of cells",
        };
        notes.push(note(
            WarningCode::OptionNotApplicable,
            "compact_tables",
            format!("compact_tables: {why}, so no markdown table is written to compact"),
        ));
    }
    if o.output == Output::Json && o.page_break.as_deref().is_some_and(|b| !b.is_empty()) {
        notes.push(note(
            WarningCode::OptionNotApplicable,
            "page_break",
            "page_break: output json gives each page as a part of its own, so no marker is \
             written",
        ));
    }
    let plan = Plan {
        output: o.output,
        pages,
        tables: o.tables,
        compact_tables: o.compact_tables,
        page_break: o.page_break.clone().filter(|b| !b.is_empty()),
        pictures: super::pictures::Settings {
            mode: if o.output == Output::Text {
                Pictures::Placeholder
            } else {
                o.pictures
            },
            min_area: o.picture_min_area,
            scale: o.images_scale,
            describe: describe.is_some(),
        },
        also_text: false,
        platform,
        ocr: o.ocr,
        ocr_languages,
        ocr_chosen: langs::Chosen::default(),
        deadline,
        spool_dir: None,
        model_help,
        describe,
        speech: None,
    };
    Ok((plan, notes))
}

#[cfg(test)]
mod tests {
    use lmgw_api_types::documents::convert::ModelHelp;

    use super::*;

    fn form(fields: &[(&str, &str)]) -> Result<ConvertOptions, Refusal> {
        from_form(
            fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn form_fields_read_into_the_options_and_defaults_fill_the_rest() {
        let o = form(&[
            ("output", "text"),
            ("pages", "1-2"),
            ("tables", "false"),
            ("ocr_languages", "eng+deu"),
            ("picture_min_area", "0.1"),
        ])
        .unwrap();
        assert_eq!(o.output, Output::Text);
        assert_eq!(o.pages.as_deref(), Some("1-2"));
        assert!(!o.tables);
        assert_eq!(o.ocr_languages, Some(vec!["eng".into(), "deu".into()]));
        assert_eq!(o.picture_min_area, 0.1);
        assert_eq!(o.ocr, Ocr::Auto);
        assert_eq!(form(&[]).unwrap(), ConvertOptions::default());
    }

    /// WP4 review, finding 5: the languages are judged here, before any
    /// file is read: their count, each code's shape, a repeat once.
    #[test]
    fn ocr_languages_are_judged_when_the_request_is_planned() {
        let plan_with = |langs: Vec<String>| {
            plan(
                &ConvertOptions {
                    ocr_languages: Some(langs),
                    ..Default::default()
                },
                InstallPlatform::Fedora,
            )
        };
        let many: Vec<String> = (0..1_000_000).map(|i| format!("l{i:06}")).collect();
        let started = std::time::Instant::now();
        let e = plan_with(many).unwrap_err();
        assert_eq!(e.code, "invalid_option");
        assert!(
            e.message.contains("1000000 codes") && e.message.contains("MAX_OCR_LANGUAGES (32)"),
            "{e:?}"
        );
        assert!(
            started.elapsed().as_millis() < 500,
            "{:?}",
            started.elapsed()
        );
        let e = plan_with(vec!["eng".into(), "../../etc".into()]).unwrap_err();
        assert!(e.message.contains("1 of its 2 codes"), "{e:?}");
        let (p, _) = plan_with(vec!["deu".into(), "eng".into(), "deu".into()]).unwrap();
        assert_eq!(p.ocr_languages, Some(vec!["deu".into(), "eng".into()]));
    }

    /// `document_timeout` is seconds above zero; a file's deadline in the
    /// plan, none without it.
    #[test]
    fn document_timeout_is_a_deadline_per_file_or_none() {
        let with = |t: Option<f64>| {
            plan(
                &ConvertOptions {
                    document_timeout: t,
                    ..Default::default()
                },
                InstallPlatform::Fedora,
            )
        };
        assert_eq!(with(None).unwrap().0.deadline, None);
        assert_eq!(
            with(Some(1.5)).unwrap().0.deadline,
            Some(std::time::Duration::from_millis(1500))
        );
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, 1e300] {
            let e = with(Some(bad)).unwrap_err();
            assert!(e.message.contains("document_timeout"), "{bad}: {e:?}");
        }
        let o = form(&[("document_timeout", "2.5")]).unwrap();
        assert_eq!(o.document_timeout, Some(2.5));
        assert!(form(&[("document_timeout", "soon")]).is_err());
    }

    #[test]
    fn an_unknown_field_a_bad_value_and_a_repeat_are_refused_by_name() {
        let e = form(&[("colour", "red")]).unwrap_err();
        assert!(e.message.contains("colour"), "{e:?}");
        let e = form(&[("output", "html")]).unwrap_err();
        assert!(
            e.message.contains("html") && e.message.contains("markdown"),
            "{e:?}"
        );
        let e = form(&[("tables", "yes")]).unwrap_err();
        assert!(e.message.contains("true or false"), "{e:?}");
        let e = form(&[("pages", "1"), ("pages", "2")]).unwrap_err();
        assert!(e.message.contains("twice"), "{e:?}");
    }

    #[test]
    fn json_is_built_and_async_is_the_http_halfs() {
        let o = ConvertOptions {
            output: Output::Json,
            ..Default::default()
        };
        let (p, notes) = plan(&o, InstallPlatform::Fedora).unwrap();
        assert_eq!((p.output, notes.len()), (Output::Json, 0));
        let o = ConvertOptions {
            output: Output::Json,
            page_break: Some("<!-- page -->".into()),
            compact_tables: true,
            ..Default::default()
        };
        let notes = plan(&o, InstallPlatform::Fedora).unwrap().1;
        let named: Vec<_> = notes.iter().map(|n| n.option.as_deref().unwrap()).collect();
        assert_eq!(named, ["compact_tables", "page_break"]);
        let o = ConvertOptions {
            run_async: true,
            ..Default::default()
        };
        assert!(plan(&o, InstallPlatform::Fedora).unwrap().1.is_empty());
    }

    #[test]
    fn defaults_say_nothing_and_each_unbuilt_value_says_what_was_done() {
        let (_, notes) = plan(&ConvertOptions::default(), InstallPlatform::Fedora).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        let o = ConvertOptions {
            model_help: Some(ModelHelp::EveryPage),
            ocr: Ocr::Force,
            ocr_languages: Some(vec!["deu".into()]),
            pictures: Pictures::Embedded,
            picture_min_area: 0.2,
            describe_pictures: true,
            picture_prompt: Some("Describe the example.".into()),
            ..Default::default()
        };
        let (_, notes) = plan(&o, InstallPlatform::Fedora).unwrap();
        let got: Vec<(WarningCode, &str)> = notes
            .iter()
            .map(|n| (n.code, n.option.as_deref().unwrap()))
            .collect();
        assert_eq!(
            got,
            [
                (WarningCode::ModelNotChosen, "model_help"),
                (WarningCode::ModelNotChosen, "describe_pictures"),
            ]
        );
        assert!(notes[0].message.contains("no page-reading model is chosen"));
        assert!(
            notes[1]
                .message
                .contains("no picture-description model is chosen"),
            "{}",
            notes[1].message
        );
        assert!(notes[1].message.contains("Describe pictures with"));
        // A prompt without descriptions asked for changes nothing, said.
        let o = ConvertOptions {
            picture_prompt: Some("Describe the example.".into()),
            ..Default::default()
        };
        let (p, notes) = plan(&o, InstallPlatform::Fedora).unwrap();
        assert!(p.describe.is_none() && !p.pictures.describe);
        assert_eq!(notes.len(), 1);
        assert_eq!(
            (notes[0].code, notes[0].option.as_deref()),
            (WarningCode::OptionNotApplicable, Some("picture_prompt"))
        );
        // Pictures are built: only output text says it writes none.
        let o = ConvertOptions {
            output: Output::Text,
            pictures: Pictures::Referenced,
            ..Default::default()
        };
        let (p, notes) = plan(&o, InstallPlatform::Fedora).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].code, WarningCode::OptionNotApplicable);
        assert_eq!(p.pictures.mode, Pictures::Placeholder);
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let o = ConvertOptions {
                images_scale: bad,
                ..Default::default()
            };
            let e = plan(&o, InstallPlatform::Fedora).unwrap_err();
            assert!(e.message.contains("images_scale"), "{e:?}");
        }
        // `off` is what there is: nothing to say.
        let o = ConvertOptions {
            model_help: Some(ModelHelp::Off),
            ocr: Ocr::Off,
            ..Default::default()
        };
        assert!(plan(&o, InstallPlatform::Fedora).unwrap().1.is_empty());
    }

    #[test]
    fn a_bad_page_spec_is_refused_before_any_file_is_read() {
        let o = ConvertOptions {
            pages: Some("5-2".into()),
            ..Default::default()
        };
        let e = plan(&o, InstallPlatform::Fedora).unwrap_err();
        assert_eq!(e.code, "invalid_option");
        assert!(e.message.contains("backwards"), "{e:?}");
    }

    #[test]
    fn compact_tables_that_changes_nothing_is_said() {
        let plan = |o: ConvertOptions| plan(&o, InstallPlatform::Fedora).unwrap().1;
        let on = ConvertOptions {
            compact_tables: true,
            ..Default::default()
        };
        assert!(plan(on.clone()).is_empty());
        for o in [
            ConvertOptions {
                output: Output::Text,
                ..on.clone()
            },
            ConvertOptions {
                tables: false,
                ..on
            },
        ] {
            let notes = plan(o);
            assert_eq!(notes.len(), 1);
            assert_eq!(notes[0].code, WarningCode::OptionNotApplicable);
            assert_eq!(notes[0].option.as_deref(), Some("compact_tables"));
        }
    }
}
