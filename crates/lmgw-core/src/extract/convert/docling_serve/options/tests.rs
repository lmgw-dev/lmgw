use serde_json::json;

use super::*;
use crate::extract::convert::options::plan;

fn opts(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

fn outcome(m: &Mapped, name: &str) -> (OptionOutcome, Option<String>) {
    let n = m.noted.iter().find(|n| n.name == name).unwrap();
    (n.outcome, n.detail.clone())
}

#[test]
fn defaults_map_to_the_native_defaults_with_markdown_out() {
    let m = map(&Map::new()).unwrap();
    // docling exports pictures whatever their size (its area threshold only
    // skips descriptions): the one native default the adapter changes.
    assert_eq!(
        m.native,
        ConvertOptions {
            picture_min_area: 0.0,
            ..ConvertOptions::default()
        }
    );
    assert!(m.md && !m.text && m.noted.is_empty());
}

#[test]
fn each_option_maps_onto_its_native_option() {
    let m = map(&opts(json!({
        "to_formats": ["text"],
        "do_ocr": false,
        "do_table_structure": false,
        "page_range": [2, 5],
        "md_page_break_placeholder": "<!-- page-break -->",
        "md_compact_tables": true,
        "document_timeout": 30.0,
        "abort_on_error": true,
        "from_formats": ["pdf", "docx"],
    })))
    .unwrap();
    assert_eq!(m.native.output, Output::Text);
    assert_eq!(m.native.ocr, Ocr::Off);
    assert!(!m.native.tables && m.native.compact_tables);
    assert_eq!(m.native.pages.as_deref(), Some("2-5"));
    assert_eq!(m.native.page_break.as_deref(), Some("<!-- page-break -->"));
    assert_eq!(m.deadline, Some(Duration::from_secs(30)));
    assert!(m.abort_on_error);
    assert_eq!(m.from_formats.as_deref().unwrap(), ["pdf", "docx"]);
    assert!(m.noted.iter().all(|n| n.outcome == OptionOutcome::Honoured));
    // docling's default range is every page.
    let m = map(&opts(json!({"page_range": [1, 9223372036854775807u64]}))).unwrap();
    assert_eq!(m.native.pages, None);
}

#[test]
fn any_ocr_engine_is_lmgws_ocr_with_its_name_in_the_note() {
    let m = map(&opts(
        json!({"ocr_engine": "some-engine", "ocr_preset": "auto"}),
    ))
    .unwrap();
    assert_eq!(
        outcome(&m, "ocr_engine"),
        (OptionOutcome::Mapped, Some("some-engine->lmgw-ocr".into()))
    );
    assert_eq!(outcome(&m, "ocr_preset").0, OptionOutcome::Honoured);
}

#[test]
fn options_on_native_features_take_the_native_routes_answer() {
    // force_ocr, ocr_lang, image_export_mode and picture descriptions set the
    // native options; the native plan's notes say what is not built yet.
    let mut m = map(&opts(json!({
        "force_ocr": true,
        "ocr_lang": ["en", "de-DE"],
        "image_export_mode": "embedded",
        "do_picture_description": true,
        "picture_description_api": {"url": "https://vision.example/v1", "prompt": "Describe."},
    })))
    .unwrap();
    assert_eq!(m.native.ocr, Ocr::Force);
    assert_eq!(
        m.native.ocr_languages.as_deref().unwrap(),
        ["eng".to_string(), "deu".to_string()]
    );
    assert_eq!(m.native.picture_prompt.as_deref(), Some("Describe."));
    let (_, notes) = plan(
        &m.native,
        lmgw_api_types::documents::InstallPlatform::Fedora,
    )
    .unwrap();
    m.settle(&notes);
    let native_said = |o: &str| {
        notes
            .iter()
            .find(|n| n.option.as_deref() == Some(o))
            .map(|n| n.code)
    };
    // Whatever the native route says today, the docling option says the same.
    for (docling, native) in [
        ("force_ocr", "ocr"),
        ("ocr_lang", "ocr_languages"),
        ("image_export_mode", "pictures"),
        ("do_picture_description", "describe_pictures"),
    ] {
        let expected = match native_said(native) {
            None => OptionOutcome::Honoured,
            Some(WarningCode::NotYet) => OptionOutcome::NotYet,
            Some(_) => OptionOutcome::Mapped,
        };
        assert_eq!(outcome(&m, docling).0, expected, "{docling}");
    }
    assert!(outcome(&m, "picture_description_api")
        .1
        .unwrap()
        .contains("url not followed"));
}

#[test]
fn an_option_at_its_default_takes_no_native_note() {
    let mut m = map(&opts(json!({"do_ocr": true, "force_ocr": true}))).unwrap();
    let (_, notes) = plan(
        &m.native,
        lmgw_api_types::documents::InstallPlatform::Fedora,
    )
    .unwrap();
    m.settle(&notes);
    assert_eq!(outcome(&m, "do_ocr").0, OptionOutcome::Honoured);
}

#[test]
fn outputs_and_features_not_built_yet_are_named_notes() {
    let m = map(&opts(json!({
        "to_formats": ["md", "json", "html"],
        "do_picture_classification": true,
        "do_chart_extraction": false,
        "chunking_options": {"chunker": "hybrid"},
        "some_new_option": 3,
    })))
    .unwrap();
    assert_eq!(m.missing_outputs, ["json", "html"]);
    assert_eq!(outcome(&m, "to_formats").0, OptionOutcome::NotYet);
    assert_eq!(
        outcome(&m, "do_picture_classification").0,
        OptionOutcome::NotYet
    );
    assert_eq!(
        outcome(&m, "do_chart_extraction").0,
        OptionOutcome::Honoured
    );
    assert_eq!(outcome(&m, "chunking_options").0, OptionOutcome::NotYet);
    assert_eq!(
        outcome(&m, "some_new_option"),
        (OptionOutcome::NotYet, Some("unknown option".into()))
    );
}

#[test]
fn a_wrong_type_for_a_known_option_is_doclings_422() {
    let e = map(&opts(json!({"do_ocr": "maybe", "page_range": [0, 3]}))).unwrap_err();
    assert_eq!(e.0.len(), 2);
    let d = e.detail(&["body", "options"]);
    assert_eq!(d[0]["loc"], json!(["body", "options", "do_ocr"]));
    assert_eq!(d[0]["type"], "bool_parsing");
}

#[test]
fn form_fields_read_as_docling_reads_them() {
    assert_eq!(
        form_value("do_ocr", vec!["false".into()]).unwrap(),
        json!(false)
    );
    assert_eq!(
        form_value("to_formats", vec!["md".into(), "text".into()]).unwrap(),
        json!(["md", "text"])
    );
    assert_eq!(
        form_value("page_range", vec!["2".into(), "4".into()]).unwrap(),
        json!([2, 4])
    );
    assert_eq!(
        form_value("picture_description_api", vec![r#"{"prompt":"x"}"#.into()]).unwrap(),
        json!({"prompt": "x"})
    );
    assert_eq!(
        form_value("images_scale", vec!["2".into()]).unwrap(),
        json!(2.0)
    );
    assert!(form_value("force_ocr", vec!["perhaps".into()]).is_err());
    assert_eq!(
        form_value("unknown_field", vec!["x".into()]).unwrap(),
        json!("x")
    );
}

#[test]
fn a_timeout_no_duration_can_hold_is_doclings_422_not_a_panic() {
    for v in [json!(1e300), json!(1.0e20)] {
        let e = map(&opts(json!({ "document_timeout": v }))).unwrap_err();
        assert_eq!(e.0[0].0, "document_timeout");
    }
    let m = map(&opts(json!({"document_timeout": 1e9}))).unwrap();
    assert_eq!(m.deadline, Some(Duration::from_secs(1_000_000_000)));
}

/// WP4 review, finding 5: a million language tags are docling's 422 for a
/// list that is too long, judged before one of them is mapped.
#[test]
fn an_ocr_lang_list_past_the_bound_is_doclings_422() {
    let many: Vec<String> = (0..1_000_000).map(|i| format!("l{i:06}")).collect();
    let started = std::time::Instant::now();
    let e = map(&opts(json!({ "ocr_lang": many }))).unwrap_err();
    assert_eq!(e.0[0].0, "ocr_lang");
    assert!(
        e.0[0]
            .1
            .contains("at most 32 items (MAX_OCR_LANGUAGES), not 1000000"),
        "{}",
        e.0[0].1
    );
    assert_eq!(e.0[0].2, "too_long");
    assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
    // Within the bound the tags map as before.
    let m = map(&opts(json!({ "ocr_lang": ["en", "de"] }))).unwrap();
    assert_eq!(
        m.native.ocr_languages,
        Some(vec!["eng".to_string(), "deu".to_string()])
    );
}

#[test]
fn picture_export_options_set_the_native_pictures() {
    // Picture export is built: the docling options are honoured, and
    // `include_images` alone keeps docling's default form, placeholder.
    let (m, notes) = plan_of(json!({"include_images": true}));
    assert_eq!(notes.0, Pictures::Placeholder);
    assert_eq!(notes.1, (OptionOutcome::Honoured, None));
    assert_eq!(m.native.pictures, Pictures::Placeholder);
    let (m, _) = plan_of(json!({"include_images": false, "image_export_mode": "embedded"}));
    assert_eq!(m.native.pictures, Pictures::Placeholder);
    let (m, notes) = plan_of(json!({"include_images": true, "image_export_mode": "referenced"}));
    assert_eq!(m.native.pictures, Pictures::Referenced);
    assert_eq!(notes.1, (OptionOutcome::Honoured, None));
    // The scale is the native one.
    let (m, _) = plan_of(json!({"images_scale": 3.0}));
    assert_eq!(m.native.images_scale, 3.0);
    assert_eq!(outcome(&m, "images_scale").0, OptionOutcome::Honoured);
    // One the native plan refuses is a refusal.
    let m = map(&opts(json!({"images_scale": 0.0}))).unwrap();
    assert!(plan(
        &m.native,
        lmgw_api_types::documents::InstallPlatform::Fedora
    )
    .is_err());
}

/// The options mapped and judged by the native plan, as the route does.
fn plan_of(v: Value) -> (Mapped, (Pictures, (OptionOutcome, Option<String>))) {
    let mut m = map(&opts(v)).unwrap();
    let (_, notes) = plan(
        &m.native,
        lmgw_api_types::documents::InstallPlatform::Fedora,
    )
    .unwrap();
    m.settle(&notes);
    let first = m
        .noted
        .first()
        .map_or((OptionOutcome::Honoured, None), |n| {
            (n.outcome, n.detail.clone())
        });
    let pictures = m.native.pictures;
    (m, (pictures, first))
}

#[test]
fn table_mode_accurate_asks_for_the_page_reading_model_on_table_pages() {
    let (m, _) = plan_of(json!({"table_mode": "accurate"}));
    assert_eq!(m.native.model_help, Some(ModelHelp::MissingOrTables));
    assert_eq!(
        outcome(&m, "table_mode"),
        (OptionOutcome::Mapped, Some("model_not_chosen".into()))
    );
    let (m, _) = plan_of(json!({"table_mode": "accurate", "pipeline": "vlm"}));
    assert_eq!(m.native.model_help, Some(ModelHelp::EveryPage));
    let (m, _) = plan_of(json!({"table_mode": "fast"}));
    assert_eq!(m.native.model_help, None);
}

/// S5: `fast` keeps table pages from the page-reading model once one is
/// chosen, never lowers `pipeline: vlm`, and asks for nothing without one.
#[test]
fn table_mode_fast_lowers_a_chosen_model_to_unread_pages_only() {
    let fast = || map(json!({"table_mode": "fast"}).as_object().unwrap()).unwrap();
    let mut m = fast();
    assert!(m.tables_fast);
    m.lower_for(true);
    assert_eq!(m.native.model_help, Some(ModelHelp::Missing));
    let n = m.noted.iter().find(|n| n.name == "table_mode").unwrap();
    assert_eq!(n.outcome, OptionOutcome::Mapped);
    assert_eq!(
        n.detail.as_deref(),
        Some("lmgw tables and a chosen page-reading model on unread pages only")
    );
    // No model chosen: nothing is asked for, so nothing is noted as missing.
    let mut m = fast();
    m.lower_for(false);
    assert_eq!(m.native.model_help, None);
    // `pipeline: vlm` asked for every page: `fast` does not lower it.
    for body in [
        json!({"table_mode": "fast", "pipeline": "vlm"}),
        json!({"pipeline": "vlm", "table_mode": "fast"}),
    ] {
        let mut m = map(body.as_object().unwrap()).unwrap();
        m.lower_for(true);
        assert_eq!(m.native.model_help, Some(ModelHelp::EveryPage));
    }
    // `accurate` is not lowered either.
    let mut m = map(json!({"table_mode": "accurate"}).as_object().unwrap()).unwrap();
    m.lower_for(true);
    assert_eq!(m.native.model_help, Some(ModelHelp::MissingOrTables));
}

#[test]
fn unknown_options_are_bounded_in_name_and_number() {
    let mut o = Map::new();
    for i in 0..500 {
        o.insert(format!("example_{i}_{}", "x".repeat(2_000)), json!(1));
    }
    let m = map(&o).unwrap();
    assert!(
        m.noted.len() <= unknown::MAX_LISTED + 1,
        "{}",
        m.noted.len()
    );
    assert!(m.noted.iter().all(|n| n.name.len() < 200));
    let last = m.noted.last().unwrap();
    assert_eq!(last.name, "unknown_options");
    assert!(last.detail.as_deref().unwrap().starts_with("468 more"));
    let h = crate::extract::convert::calls::header(&m.noted).unwrap();
    assert!(
        h.len() <= crate::extract::convert::calls::NOTES_HEADER_MAX_BYTES,
        "{}",
        h.len()
    );
}

/// `x-lmgw-option-notes` writes each note's text through a sanitiser that
/// turns `;` `,` `=` `(` `)` and anything non-printable into `_`, so a
/// built-in note that holds one reads as `page-reading model_ every page`.
/// Every note the table builds, over every option it knows, set both ways,
/// plus the overflow note for unknown names, comes through unchanged.
#[test]
fn no_built_in_note_text_has_a_character_the_header_would_replace() {
    use super::kinds::{Kind, OPTIONS};
    use crate::extract::convert::calls::safe;
    let mut seen = 0;
    for flag in [true, false] {
        let mut set = Map::new();
        for (name, kind) in OPTIONS {
            let v = match kind {
                Kind::Bool => json!(flag),
                Kind::Float => json!(0.5),
                Kind::OptFloat => json!(5.0),
                Kind::Str => json!("x"),
                Kind::OptStr => json!("x"),
                Kind::StrList => json!(["x"]),
                Kind::PageRange => json!([1, 2]),
                Kind::Object => json!({"url": "http://example.invalid/", "prompt": "p"}),
            };
            set.insert((*name).to_string(), v);
        }
        // The values the built-in texts hang on.
        set.insert("pipeline".into(), json!("vlm"));
        set.insert("table_mode".into(), json!("accurate"));
        set.insert("image_export_mode".into(), json!("embedded"));
        set.insert("to_formats".into(), json!(["md", "json"]));
        set.insert("ocr_lang".into(), json!(["mul"]));
        for i in 0..40 {
            set.insert(format!("not_an_option_{i}"), json!(1));
        }
        let mut m = map(&set).unwrap();
        let (_, notes) = plan(
            &m.native,
            lmgw_api_types::documents::InstallPlatform::Fedora,
        )
        .unwrap();
        m.settle(&notes);
        for n in &m.noted {
            for text in n.detail.iter().chain(std::iter::once(&n.name)) {
                assert_eq!(&safe(text), text, "{}: {text}", n.name);
                seen += 1;
            }
        }
    }
    assert!(seen > 80, "the options were noted: {seen}");
    // The text each of the sources' and the form's shape notes carries.
    for text in [
        "not called so poll the task or use its WebSocket",
        "not sent because lmgw sets them: x-a x-b",
    ] {
        assert_eq!(safe(text), text);
    }
}
