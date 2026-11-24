//! Picture descriptions on `POST /v1/documents/convert` (document-conversion
//! design §5.3, §7.5, ruling 5, WP11) against `documents_model_help.rs`'
//! mock model, on Word files with pictures (no tool needed): what a call
//! carries (the picture, the prompt, the caption and the paragraph before
//! it), where the description goes in every output and picture mode, the
//! usage row, `picture_min_area`, no model chosen, a key that may not use
//! the model, a refusal and a cut answer, pictures a model cannot take, the
//! bound on calls in flight with thirty pictures, a key's tokens-per-minute
//! window. The file's clock and a client that goes away are
//! `documents_pictures_clock.rs`; the GPU hold, the same model for pages
//! and pictures, and the docling route `documents_pictures_more.rs`; the
//! review round's cases `documents_pictures_review.rs`.

use std::io::Write as _;
use std::time::Duration;

use base64::Engine as _;
use lmgw_core::config::{hash_api_key, KeyPolicy};
use lmgw_core::extract::convert::describe::DEFAULT_PROMPT;
use lmgw_core::state::SharedState;
use lmgw_core::store;
use serde_json::{json, Value};

use crate::common::{serve, Gw};
use crate::documents_model_help::{convert, settings, state_with, Model, ALIAS, PATH};

const W: &str = r#"xmlns:w="w" xmlns:wp="wp" xmlns:a="a" xmlns:r="r" xmlns:pic="pic""#;

/// A PNG's signature and header of `w` × `h` (what the sniffer reads).
pub(crate) fn png(w: u32, h: u32) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    b.extend_from_slice(&w.to_be_bytes());
    b.extend_from_slice(&h.to_be_bytes());
    b.extend_from_slice(&[8, 2, 0, 0, 0]);
    b
}

fn drawing(rel: &str, cx: u64, cy: u64) -> String {
    format!(
        r#"<w:r><w:drawing><wp:inline><wp:extent cx="{cx}" cy="{cy}"/><a:graphic><a:graphicData><pic:pic><pic:blipFill><a:blip r:embed="{rel}"/></pic:blipFill></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>"#
    )
}

fn para(text: &str) -> String {
    format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>")
}

/// One picture of a Word file: its media entry's bytes and its drawn size
/// (EMU) on an A4 page.
pub(crate) struct Pic {
    pub bytes: Vec<u8>,
    pub ext: &'static str,
    pub cx: u64,
    pub cy: u64,
}

/// A picture that is a fifth of the page: always exported.
pub(crate) fn big(w: u32, h: u32) -> Pic {
    Pic {
        bytes: png(w, h),
        ext: "png",
        cx: 5_000_000,
        cy: 3_000_000,
    }
}

/// A Word file: per picture a paragraph before it ("Text before picture
/// N."), the picture, and its caption ("Figure N: Example chart N").
pub(crate) fn docx(pics: &[Pic]) -> Vec<u8> {
    let mut body = String::new();
    let mut rels = String::new();
    for (i, p) in pics.iter().enumerate() {
        let n = i + 1;
        body.push_str(&para(&format!("Text before picture {n}.")));
        body.push_str(&format!(
            "<w:p>{}</w:p>",
            drawing(&format!("rId{n}"), p.cx, p.cy)
        ));
        body.push_str(&para(&format!("Figure {n}: Example chart {n}")));
        rels.push_str(&format!(
            r#"<Relationship Id="rId{n}" Target="media/image{n}.{}"/>"#,
            p.ext
        ));
    }
    body.push_str(r#"<w:sectPr><w:pgSz w:w="11906" w:h="16838"/></w:sectPr>"#);
    let doc = format!(r#"<w:document {W}><w:body>{body}</w:body></w:document>"#);
    let types = r#"<Types><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    let mut put = |name: &str, data: &[u8]| {
        w.start_file(name, opts).unwrap();
        w.write_all(data).unwrap();
    };
    put("[Content_Types].xml", types.as_bytes());
    put("word/document.xml", doc.as_bytes());
    put(
        "word/_rels/document.xml.rels",
        format!("<Relationships>{rels}</Relationships>").as_bytes(),
    );
    for (i, p) in pics.iter().enumerate() {
        put(&format!("word/media/image{}.{}", i + 1, p.ext), &p.bytes);
    }
    w.finish().unwrap().into_inner()
}

pub(crate) fn request(file: &[u8], options: Value) -> Value {
    let mut body = json!({
        "files": [{
            "name": "example-pictures.docx",
            "data_base64": base64::engine::general_purpose::STANDARD.encode(file),
        }],
        "describe_pictures": true,
    });
    for (k, v) in options.as_object().unwrap() {
        body[k] = v.clone();
    }
    body
}

/// [`state_with`] with the mock's alias chosen as the picture-description
/// model (and no page-reading model).
pub(crate) async fn describing(m: &Model) -> SharedState {
    let state = state_with(m, false).await;
    settings(&state, |s| s.documents.picture_alias = ALIAS.into()).await;
    state
}

/// The `documents-picture` rows of `alias`, once there are `at_least`.
pub(crate) async fn picture_rows(
    state: &SharedState,
    at_least: usize,
) -> Vec<store::RequestLogRow> {
    let mut rows = Vec::new();
    for _ in 0..300 {
        rows = store::query_logs(
            &state.db,
            &store::LogFilter {
                alias: Some(ALIAS.into()),
                limit: 100,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        rows.retain(|r| r.ingress_proto == "documents-picture");
        if rows.len() >= at_least {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    rows
}

/// The text and the image of a call's one message.
pub(crate) fn sent(body: &Value) -> (String, String) {
    crate::documents_model_help::sent(body)
}

fn warning<'a>(doc: &'a Value, code: &str) -> Option<&'a Value> {
    doc["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["code"] == code)
}

#[tokio::test]
async fn a_picture_goes_with_its_prompt_and_context_and_its_description_lands_everywhere() {
    let m = Model::start(Duration::ZERO).await;
    let state = describing(&m).await;
    let gw = serve(state.clone()).await;
    let file = docx(&[big(400, 300)]);
    m.will_answer("A bar chart of example sales.", "stop");
    let (status, a) = convert(&gw, &request(&file, json!({"output": "json"}))).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    assert_eq!(d["warnings"], json!([]), "{d}");
    // The call: the picture as stored, docling's default prompt, the
    // caption and the paragraph before it as context; nothing else.
    let bodies = m.bodies().await;
    assert_eq!(bodies.len(), 1);
    let (text, image) = sent(&bodies[0]);
    assert!(image.starts_with("data:image/png;base64,"), "{image}");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(image.trim_start_matches("data:image/png;base64,"))
        .unwrap();
    assert_eq!(decoded, png(400, 300));
    assert!(text.starts_with(DEFAULT_PROMPT), "{text}");
    assert!(
        text.contains("Caption: Figure 1: Example chart 1"),
        "{text}"
    );
    assert!(
        text.contains("Paragraph before the picture: Text before picture 1."),
        "{text}"
    );
    assert!(!text.contains("example-pictures"), "no file name: {text}");
    assert_eq!(bodies[0]["temperature"], 0.0);
    // JSON: the figure block's description, and the pictures list's.
    let blocks = d["structure"][0]["blocks"].as_array().unwrap();
    let figure = blocks.iter().find(|b| b["kind"] == "figure").unwrap();
    assert_eq!(figure["description"], "A bar chart of example sales.");
    assert_eq!(
        d["pictures"][0]["description"],
        "A bar chart of example sales."
    );
    let by = &d["stats"]["models"][0];
    assert_eq!(
        (
            &by["alias"],
            &by["calls"],
            &by["pictures_described"],
            &by["parts_read"]
        ),
        (&json!(ALIAS), &json!(1), &json!(1), &json!(0))
    );
    assert_eq!(d["stats"]["model_calls"], 1);
    // One usage row under the caller, with no document content.
    let rows = picture_rows(&state, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].status, rows[0].prompt_tokens), (200, Some(1200)));

    // Markdown, every picture mode: the description is a paragraph after
    // the picture and its caption, as docling writes a picture item's
    // meta after the item.
    for (mode, line) in [
        ("placeholder", "<!-- image -->"),
        ("embedded", "![Image](data:image/png;base64,"),
        ("referenced", "![Image](artifacts/image_000000_"),
    ] {
        m.will_answer("A bar chart of example sales.", "stop");
        let (status, a) = convert(&gw, &request(&file, json!({"pictures": mode}))).await;
        assert_eq!(status, 200, "{a}");
        let md = a["documents"][0]["content"].as_str().unwrap();
        let at = md.find(line).unwrap_or_else(|| panic!("{mode}: {md}"));
        let after = &md[at..];
        let end = after.find('\n').unwrap();
        assert!(
            after[end..]
                .starts_with("\n\nFigure 1: Example chart 1\n\nA bar chart of example sales."),
            "{mode}: {md}"
        );
    }
    // A caller's prompt is the call's, and output text writes the
    // description after the picture's caption.
    m.will_answer("A bar chart of example sales.", "stop");
    let (_, a) = convert(
        &gw,
        &request(
            &file,
            json!({"output": "text", "picture_prompt": "Name the chart type."}),
        ),
    )
    .await;
    let text = a["documents"][0]["content"].as_str().unwrap();
    assert_eq!(
        text,
        "Text before picture 1.\n\nFigure 1: Example chart 1\n\nA bar chart of example sales."
    );
    let (prompt, _) = sent(m.bodies().await.last().unwrap());
    assert!(prompt.starts_with("Name the chart type.\n\n"), "{prompt}");
}

#[tokio::test]
async fn small_pictures_are_not_sent_and_without_a_model_nothing_is() {
    let m = Model::start(Duration::ZERO).await;
    let state = describing(&m).await;
    let gw = serve(state.clone()).await;
    let icon = Pic {
        bytes: png(8, 8),
        ext: "png",
        cx: 100_000,
        cy: 100_000,
    };
    let file = docx(&[big(400, 300), icon]);
    let (_, a) = convert(&gw, &request(&file, json!({}))).await;
    let d = &a["documents"][0];
    assert_eq!(
        m.bodies().await.len(),
        1,
        "the icon is under picture_min_area"
    );
    assert!(warning(d, "pictures_left_out").is_some(), "{d}");
    // A smaller picture_min_area exports the icon too; describe_min_area
    // (docling's picture_description_area_threshold) keeps it undescribed,
    // counted and without a warning.
    let (_, a) = convert(&gw, &request(&file, json!({"picture_min_area": 0.0}))).await;
    let d = &a["documents"][0];
    assert_eq!(m.bodies().await.len(), 2);
    assert_eq!(d["pictures"].as_array().unwrap().len(), 2, "{d}");
    assert_eq!(d["stats"]["pictures_below_describe_min_area"], 1, "{d}");
    assert_eq!(d["status"], "converted", "{d}");
    assert_eq!(d["warnings"], json!([]), "{d}");
    // And a smaller describe_min_area describes it.
    let (_, a) = convert(
        &gw,
        &request(
            &file,
            json!({"picture_min_area": 0.0, "describe_min_area": 0.0}),
        ),
    )
    .await;
    assert_eq!(m.bodies().await.len(), 4);
    assert_eq!(
        a["documents"][0]["stats"]["pictures_below_describe_min_area"],
        0
    );

    // No model chosen: the picture is kept, a request-wide note says why,
    // and no call is made.
    settings(&state, |s| s.documents.picture_alias.clear()).await;
    let (status, a) = convert(&gw, &request(&file, json!({}))).await;
    assert_eq!(status, 200, "{a}");
    assert_eq!(a["documents"][0]["status"], "converted");
    let note = &a["warnings"][0];
    assert_eq!(
        (&note["code"], &note["option"]),
        (&json!("model_not_chosen"), &json!("describe_pictures"))
    );
    assert!(a["documents"][0]["content"]
        .as_str()
        .unwrap()
        .contains("<!-- image -->"));
    assert_eq!(m.bodies().await.len(), 4, "no call without a chosen model");
}

const KEY: &str = "lmgw-example-key";

async fn with_key(state: &SharedState, policy: KeyPolicy) {
    settings(state, |s| s.auth_enabled = true).await;
    sqlx::query(
        "INSERT INTO api_keys (name, key_hash, enabled, scope_mode, scope_patterns,
             budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit, expires_at)
         VALUES ('example-agent', ?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(hash_api_key(KEY))
    .bind(policy.scope_mode.as_str())
    .bind(&policy.scope_patterns)
    .bind(policy.budget_micro)
    .bind(policy.budget_period.as_str())
    .bind(policy.rpm_limit)
    .bind(policy.tpm_limit)
    .bind(policy.concurrency_limit)
    .bind(policy.expires_at.clone())
    .execute(&state.db)
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
}

async fn convert_as_key(gw: &Gw, body: &Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(format!("{gw}{PATH}"))
        .bearer_auth(KEY)
        .json(body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_key_that_may_not_use_the_model_keeps_its_pictures_undescribed() {
    let m = Model::start(Duration::ZERO).await;
    let state = describing(&m).await;
    with_key(
        &state,
        KeyPolicy {
            scope_mode: lmgw_core::config::ScopeMode::Allow,
            scope_patterns: "example-other*".into(),
            ..Default::default()
        },
    )
    .await;
    let gw = serve(state.clone()).await;
    let (status, a) = convert_as_key(&gw, &request(&docx(&[big(400, 300)]), json!({}))).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "partial", "{d}");
    let w = warning(d, "model_not_allowed").expect("the picture is named");
    assert!(w["message"]
        .as_str()
        .unwrap()
        .starts_with("picture p1 was not described"));
    assert!(d["content"]
        .as_str()
        .unwrap()
        .contains("Text before picture 1."));
    assert!(
        m.bodies().await.is_empty(),
        "the key never reached the model"
    );
}

/// S2's rule for pictures: the first call's tokens fill the key's window;
/// the pictures left are not sent.
#[tokio::test]
async fn a_key_s_tokens_per_minute_window_stops_the_pictures_left() {
    let m = Model::start(Duration::ZERO).await;
    let state = describing(&m).await;
    settings(&state, |s| s.documents.tool_slots = 1).await;
    with_key(
        &state,
        KeyPolicy {
            tpm_limit: 1000,
            rpm_limit: 1,
            ..Default::default()
        },
    )
    .await;
    let gw = serve(state.clone()).await;
    let file = docx(&[big(400, 300), big(300, 200), big(200, 100)]);
    let (status, a) = convert_as_key(&gw, &request(&file, json!({"output": "json"}))).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "partial", "{d}");
    assert_eq!(m.bodies().await.len(), 1);
    let w = warning(d, "model_not_allowed").expect("the pictures left are named");
    let said = w["message"].as_str().unwrap();
    assert!(
        said.starts_with("pictures p2, p3 were not described"),
        "{said}"
    );
    assert!(said.contains("tokens/minute limit of 1000"), "{said}");
    assert!(d["pictures"][0]["description"].is_string());
    assert!(d["pictures"][1]["description"].is_null());
}

#[tokio::test]
async fn a_refusal_and_a_cut_answer_keep_their_pictures_without_a_description() {
    let m = Model::start(Duration::ZERO).await;
    let state = describing(&m).await;
    settings(&state, |s| s.documents.tool_slots = 1).await;
    let gw = serve(state.clone()).await;
    m.will_refuse("I can't help with that.");
    m.will_answer("A chart that goes on and on", "length");
    m.will_answer("A small example chart.", "stop");
    let file = docx(&[big(400, 300), big(300, 200), big(200, 100)]);
    let (status, a) = convert(&gw, &request(&file, json!({"output": "json"}))).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "partial", "{d}");
    let failed: Vec<(String, String)> = d["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w["code"] == "part_failed")
        .map(|w| {
            (
                w["message"].as_str().unwrap().to_string(),
                w["option"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(failed.len(), 2, "{d}");
    let all: String = failed.iter().map(|f| f.0.as_str()).collect();
    assert!(all.contains("refused to describe the picture"), "{all}");
    assert!(all.contains("not a whole description"), "{all}");
    assert!(
        !all.contains("I can't help") && !all.contains("on and on"),
        "{all}"
    );
    let descriptions: Vec<&Value> = d["pictures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| &p["description"])
        .collect();
    assert_eq!(
        descriptions,
        [&Value::Null, &Value::Null, &json!("A small example chart.")]
    );
    // Both calls were made and paid; only one described.
    let by = &d["stats"]["models"][0];
    assert_eq!(
        (&by["calls"], &by["pictures_described"]),
        (&json!(3), &json!(1))
    );
}

#[tokio::test]
async fn pictures_a_model_cannot_take_are_named_not_yet_and_not_sent() {
    let m = Model::start(Duration::ZERO).await;
    let state = describing(&m).await;
    let gw = serve(state.clone()).await;
    let gif = Pic {
        bytes: b"GIF89a\x10\x00\x10\x00\x00\x00\x00".to_vec(),
        ext: "gif",
        cx: 5_000_000,
        cy: 3_000_000,
    };
    let file = docx(&[gif, big(4000, 3000)]);
    let (status, a) = convert(&gw, &request(&file, json!({}))).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    let said: Vec<String> = d["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w["code"] == "not_yet")
        .map(|w| w["message"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(said.len(), 2, "{d}");
    assert!(said[0].contains("picture p1 was not described: a picture stored as image/gif"));
    assert!(said[1].contains("PAGE_MAX_PIXELS"), "{said:?}");
    assert!(m.bodies().await.is_empty());
}

/// D20: thirty pictures on a bound of two never have three calls in
/// flight, and the answer says which bound was in use.
#[tokio::test]
async fn thirty_pictures_keep_at_most_the_bound_of_calls_in_flight() {
    let delay = Duration::from_millis(150);
    let m = Model::start(delay).await;
    let state = describing(&m).await;
    settings(&state, |s| s.documents.tool_slots = 2).await;
    let gw = serve(state.clone()).await;
    let pics: Vec<Pic> = (0..30).map(|i| big(400 + i, 300)).collect();
    let (status, a) = convert(&gw, &request(&docx(&pics), json!({}))).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    let by = &d["stats"]["models"][0];
    assert_eq!(
        (
            &by["in_flight_bound"],
            &by["bound_from"],
            &by["pictures_described"]
        ),
        (&json!(2), &json!("tool_pool"), &json!(30))
    );
    let arrivals = m.arrivals.lock().unwrap().clone();
    assert_eq!(arrivals.len(), 30);
    let window = delay - Duration::from_millis(50);
    for (i, t) in arrivals.iter().enumerate() {
        let before = arrivals[..i]
            .iter()
            .filter(|e| t.duration_since(**e) < window)
            .count();
        assert!(
            before < 2,
            "call {i} arrived with {before} others in flight"
        );
    }
    // In reading order: the first call is the first picture's.
    let (text, _) = sent(&m.bodies().await[0]);
    assert!(
        text.contains("Caption: Figure 1: Example chart 1\n"),
        "{text}"
    );
}
