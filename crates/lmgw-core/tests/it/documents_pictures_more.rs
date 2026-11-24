//! Picture descriptions (WP11) with a **local** picture-description model
//! on `support/gpu_world.rs` (the GPU hold without a fallback, with one that
//! sees, with one that takes no pictures; the row's own two slots as the
//! bound), the same model chosen for pages and pictures on one PDF, a
//! picture on a page the page-reading model read (the same model and
//! another), and the docling route's picture-description options (none of
//! the caller's connection settings reaches the model).

use std::time::Duration;

use base64::Engine as _;
use lmgw_core::config::LlamaParams;
use serde_json::{json, Value};

use crate::common::{self, Gw};
use crate::documents_model_help::{convert, settings, state_with, Model, ALIAS};
use crate::documents_pictures::{big, docx, request, sent};
use crate::support::gpu_world::{Gpu, GIB};

/// The local chat row chosen as the picture-description model.
const LOCAL: &str = "chat-model";

fn has_poppler() -> bool {
    ["pdftotext", "pdftoppm", "pdftohtml", "pdfimages"]
        .iter()
        .all(|t| std::process::Command::new(t).arg("-v").output().is_ok())
}

/// The local row as the picture-description model, the cloud alias
/// `cloud-chat` beside it, and `fallback` as the local row's fallback.
async fn world(fallback: Option<&str>) -> Gpu {
    let g = Gpu::new(8 * GIB, 2, 5).await;
    g.model(LOCAL, GIB).await;
    g.cloud("cloud-chat", None).await;
    crate::support::fallbacks::every_chat_model_falls_back_to(&g.state, fallback).await;
    let mut s = g.state.snapshot().settings.clone();
    s.documents.picture_alias = LOCAL.into();
    lmgw_core::store::save_settings(&g.state.db, &s)
        .await
        .unwrap();
    g.state.reload_snapshot().await.unwrap();
    {
        let mut w = g.world();
        w.projectors.insert(LOCAL.into());
        w.prompt_tokens = 900;
        w.props.insert(
            LOCAL.into(),
            json!({
                "default_generation_settings": {"n_ctx": 8192},
                "modalities": {"vision": true, "audio": false},
            }),
        );
    }
    g
}

async fn post(gw: &Gw, body: &Value) -> (u16, Value) {
    let r = gw
        .client()
        .post(format!("{gw}/v1/documents/convert"))
        .json(body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

fn warning<'a>(doc: &'a Value, code: &str) -> Option<&'a Value> {
    doc["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["code"] == code)
}

/// The owner's capability facts of a cloud chat model that takes no
/// pictures.
fn blind() -> Value {
    json!({"capabilities": {
        "task": "chat",
        "endpoints": ["/v1/chat/completions"],
        "input_modalities": ["text"],
        "vision": false,
        "source": "owner",
    }})
}

#[tokio::test]
async fn under_the_hold_pictures_stay_undescribed_go_to_a_seeing_fallback_or_to_none() {
    let file = docx(&[big(400, 300), big(300, 200)]);
    // No fallback: held, nothing starts, the text converts, the file is
    // partial and never fails for it.
    let g = world(None).await;
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();
    let gw = common::serve(g.state.clone()).await;
    let (status, a) = post(&gw, &request(&file, json!({}))).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "partial", "{d}");
    let held = warning(d, "model_held").expect("the held pictures are named");
    assert!(held["message"]
        .as_str()
        .unwrap()
        .starts_with("pictures p1, p2 were not described"));
    assert!(d["content"]
        .as_str()
        .unwrap()
        .contains("Text before picture 1."));
    assert!(g.world().runs.is_empty(), "the held model was not started");
    assert!(g.cloud_bodies().await.is_empty());

    // A fallback that sees: it answers, with the picture itself, and the
    // answer says which model did.
    let g = world(Some("cloud-chat")).await;
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();
    let gw = common::serve(g.state.clone()).await;
    let (status, a) = post(&gw, &request(&file, json!({"output": "json"}))).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    let by = &d["stats"]["models"][0];
    assert_eq!(
        (&by["alias"], &by["answered_for"], &by["pictures_described"]),
        (&json!("cloud-chat"), &json!(LOCAL), &json!(2))
    );
    assert_eq!(d["pictures"][0]["description"], "from the cloud");
    let bodies = g.cloud_bodies().await;
    assert_eq!(bodies.len(), 2);
    let (_, image) = sent(&bodies[0]);
    assert!(image.starts_with("data:image/png;base64,"), "{}", bodies[0]);
    assert!(g.world().runs.is_empty());

    // A fallback that takes no pictures is sent nothing.
    let g = world(None).await;
    g.cloud("blind-chat", Some(blind())).await;
    crate::support::fallbacks::every_chat_model_falls_back_to(&g.state, Some("blind-chat")).await;
    lmgw_core::ops::hold_set(&g.state, true).await.unwrap();
    let gw = common::serve(g.state.clone()).await;
    let (status, a) = post(&gw, &request(&file, json!({}))).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "partial", "{d}");
    let w = warning(d, "model_cannot_see").expect("the pictures are named");
    let said = w["message"].as_str().unwrap();
    assert!(said.contains("its fallback 'blind-chat'"), "{said}");
    assert!(said.contains("nothing was sent to it"), "{said}");
    assert!(g.cloud_bodies().await.is_empty(), "nothing was sent to it");
    // Settings › Documents says so beforehand.
    let caps = lmgw_core::extract::convert::doc_tools(&g.state)
        .await
        .unwrap();
    let f = caps
        .picture_describer
        .fallback
        .expect("the fallback is named");
    assert_eq!((f.alias.as_str(), f.sees), ("blind-chat", Some(false)));
    assert!(caps.picture_describer.local);
    let row = caps
        .formats
        .iter()
        .find(|f| f.id == "picture_descriptions")
        .expect("the capability row");
    assert!(row.said.contains(LOCAL), "{}", row.said);
}

/// D20 on a local row: its own two slots are the bound, one start serves
/// the file's thirty pictures, and each call's `max_tokens` is the slot's
/// context less the counted prompt.
#[tokio::test]
async fn a_local_row_s_two_slots_bound_thirty_pictures() {
    let g = world(None).await;
    let params = LlamaParams {
        parallel: Some(2),
        ..Default::default()
    };
    sqlx::query("UPDATE local_models SET params = ?1 WHERE model_id = ?2")
        .bind(serde_json::to_string(&params).unwrap())
        .bind(LOCAL)
        .execute(&g.state.db)
        .await
        .unwrap();
    g.state.reload_snapshot().await.unwrap();
    g.world().chat_delay = Duration::from_millis(50);
    let gw = common::serve(g.state.clone()).await;
    let pics: Vec<_> = (0..30).map(|i| big(400 + i, 300)).collect();
    let (status, a) = post(&gw, &request(&docx(&pics), json!({}))).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    let by = &d["stats"]["models"][0];
    assert_eq!(
        (
            &by["in_flight_bound"],
            &by["bound_from"],
            &by["pictures_described"]
        ),
        (&json!(2), &json!("row_slots"), &json!(30))
    );
    assert_eq!(g.world().runs.len(), 1, "one start for the file's pictures");
    let chats: Vec<Value> = g
        .posted()
        .await
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(chats.len(), 30);
    assert!(
        chats.iter().all(|c| c["max_tokens"] == 8192 - 900),
        "{}",
        chats[0]
    );
}

/// A PDF page: its lines of text and one RGB image drawn at `(x, y, w, h)`
/// points from the top-left, or none.
fn pdf(pages: &[(Option<&str>, bool)]) -> Vec<u8> {
    let (width, height) = (595.0f32, 842.0f32);
    let mut objs: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        Vec::new(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    let mut kids = Vec::new();
    for (text, image) in pages {
        let mut content = String::new();
        let mut xobjs = String::new();
        if let Some(t) = text {
            content.push_str(&format!(
                "BT /F1 14 Tf 72 {} Td ({t}) Tj ET\n",
                height - 100.0
            ));
        }
        if *image {
            let (pw, ph) = (60u32, 45u32);
            let pixels: Vec<u8> = (0..pw * ph).flat_map(|_| [200u8, 30, 30]).collect();
            let mut o = format!(
                "<< /Type /XObject /Subtype /Image /Width {pw} /Height {ph} /ColorSpace \
                 /DeviceRGB /BitsPerComponent 8 /Length {} >>\nstream\n",
                pixels.len()
            )
            .into_bytes();
            o.extend_from_slice(&pixels);
            o.extend_from_slice(b"\nendstream");
            objs.push(o);
            xobjs = format!("/Im0 {} 0 R", objs.len());
            content.push_str(&format!(
                "q 300 0 0 225 100 {} cm /Im0 Do Q\n",
                height - 200.0 - 225.0
            ));
        }
        objs.push(
            format!(
                "<< /Length {} >>\nstream\n{content}endstream",
                content.len()
            )
            .into_bytes(),
        );
        let contents = objs.len();
        objs.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] /Contents \
                 {contents} 0 R /Resources << /Font << /F1 3 0 R >> /XObject << {xobjs} >> >> >>"
            )
            .into_bytes(),
        );
        kids.push(format!("{} 0 R", objs.len()));
    }
    objs[1] = format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids.join(" "),
        kids.len()
    )
    .into_bytes();
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend(format!("{} 0 obj\n", i + 1).into_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).into_bytes());
    for o in offsets {
        out.extend(format!("{o:010} 00000 n \n").into_bytes());
    }
    out.extend(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .into_bytes(),
    );
    out
}

/// One model chosen for both: one route and one pool for the request, one
/// `stats.models` entry with its pages and its pictures, and a picture on
/// a page the model read described once.
#[tokio::test]
async fn the_same_model_reads_pages_and_describes_pictures_through_one_route() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    let m = Model::start(Duration::ZERO).await;
    let state = state_with(&m, true).await;
    // lmgw's reader cuts the crops: the one cargo built beside the tests.
    let mut dirs: Vec<std::path::PathBuf> =
        std::env::split_paths(&lmgw_core::extract::convert::reader::own_dirs()).collect();
    dirs.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    state
        .doc_tools
        .set_search_path(std::env::join_paths(dirs).unwrap());
    settings(&state, |s| s.documents.picture_alias = ALIAS.into()).await;
    let gw = common::serve(state.clone()).await;
    let file = pdf(&[(Some("Page one"), true), (None, false)]);
    let body = json!({
        "files": [{"name": "example.pdf",
                   "data_base64": base64::engine::general_purpose::STANDARD.encode(&file)}],
        "describe_pictures": true,
        "output": "json",
    });
    let (status, a) = convert(&gw, &body).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    assert_eq!(d["stats"]["parts_by_source"]["vision"], 1, "{d}");
    let models = d["stats"]["models"].as_array().unwrap();
    assert_eq!(models.len(), 1, "{d}");
    assert_eq!(
        (
            &models[0]["calls"],
            &models[0]["parts_read"],
            &models[0]["pictures_described"]
        ),
        (&json!(2), &json!(1), &json!(1)),
        "{d}"
    );
    assert_eq!(d["pictures"].as_array().unwrap().len(), 1, "{d}");
    assert!(d["pictures"][0]["description"].is_string(), "{d}");
    let prompts: Vec<String> = m.bodies().await.iter().map(|b| sent(b).0).collect();
    assert_eq!(prompts.len(), 2);
    assert_eq!(
        prompts
            .iter()
            .filter(|p| p.starts_with("Describe this image"))
            .count(),
        1,
        "the picture is described once: {prompts:?}"
    );
}

#[tokio::test]
async fn the_docling_route_s_picture_description_options_reach_the_chosen_model() {
    let m = Model::start(Duration::ZERO).await;
    let state = state_with(&m, false).await;
    let gw = common::serve(state.clone()).await;
    let send = |gw: &Gw| {
        let file = reqwest::multipart::Part::bytes(docx(&[big(400, 300)]))
            .file_name("example-pictures.docx");
        let form = reqwest::multipart::Form::new()
            .part("files", file)
            .text("do_picture_description", "true")
            .text(
                "picture_description_api",
                r#"{"url": "https://vision.example/v1", "prompt": "Describe the example.",
                    "params": {"model": "example-param-model"}, "timeout": 20,
                    "headers": {"x-example-secret": "example-secret-value"},
                    "picture_area_threshold": 0.3,
                    "classification_allow": ["bar_chart"]}"#,
            );
        gw.client()
            .post(format!("{gw}/v1/convert/file"))
            .multipart(form)
            .send()
    };
    // No model chosen: pictures without descriptions, said in the notes.
    let r = send(&gw).await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let notes = r.headers()["x-lmgw-option-notes"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        notes.contains("do_picture_description=mapped(model_not_chosen)"),
        "{notes}"
    );
    assert!(m.bodies().await.is_empty());

    // Chosen: the configured model answers with the caller's prompt; the
    // caller's url and settings are not used.
    settings(&state, |s| s.documents.picture_alias = ALIAS.into()).await;
    m.will_answer("An example chart.", "stop");
    let r = send(&gw).await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let notes = r.headers()["x-lmgw-option-notes"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        notes.contains("do_picture_description=mapped(picture-description model)"),
        "{notes}"
    );
    assert!(
        notes.contains(
            "picture_description_api=mapped(picture-description model in place of params and \
             timeout with its url and headers not followed with its picture_area_threshold \
             replaced by picture_description_area_threshold with classification_allow not yet)"
        ),
        "{notes}"
    );
    let a: Value = r.json().await.unwrap();
    assert_eq!(a["status"], "success", "{a}");
    let md = a["document"]["md_content"].as_str().unwrap();
    assert!(
        md.contains("<!-- image -->\n\nFigure 1: Example chart 1\n\nAn example chart."),
        "{md}"
    );
    let bodies = m.bodies().await;
    assert_eq!(bodies.len(), 1);
    assert!(sent(&bodies[0]).0.starts_with("Describe the example."));
    // Nothing of the caller's connection settings reaches the model: not
    // its url, not its headers, not its params.
    for r in m.mock.received_requests().await.unwrap_or_default() {
        assert!(
            r.headers.get("x-example-secret").is_none(),
            "{:?}",
            r.headers
        );
        let seen = format!("{:?} {}", r.headers, String::from_utf8_lossy(&r.body));
        for word in [
            "example-secret-value",
            "vision.example",
            "example-param-model",
        ] {
            assert!(!seen.contains(word), "{word} reached the model: {seen}");
        }
    }
}

/// A second upstream and alias, `example-describer`, on `m`.
async fn describer_on(state: &lmgw_core::state::SharedState, m: &Model) {
    use lmgw_core::config::{Protocol, UpstreamKind};
    use lmgw_core::store::{self, NewAlias, NewUpstream};
    let up = store::insert_upstream(
        &state.db,
        &NewUpstream {
            name: "example-describer-up".into(),
            protocol: Protocol::OpenaiCompatible,
            kind: UpstreamKind::Generic,
            base_url: format!("{}/v1", m.mock.uri()),
            api_key: None,
            extra_headers: vec![],
            timeout_ms: 30_000,
            enabled: true,
            expose_all: false,
            expose_prefix: String::new(),
            supports_responses: false,
            catalog_all_modalities: true,
        },
    )
    .await
    .unwrap();
    store::insert_alias(
        &state.db,
        &NewAlias {
            alias: "example-describer".into(),
            upstream_id: up,
            upstream_model_id: "describer-tgt".into(),
            param_overrides: Default::default(),
            enabled: true,
            capabilities_override: None,
            fallback: None,
        },
    )
    .await
    .unwrap();
    state.reload_snapshot().await.unwrap();
    settings(state, |s| {
        s.documents.picture_alias = "example-describer".into()
    })
    .await;
}

/// S8: a picture on a page the page-reading model read (`every_page`) is
/// described once, by the picture-description model, when that is the same
/// model and when it is another; the page call describes nothing.
#[tokio::test]
async fn a_picture_on_a_page_the_model_read_is_described_once() {
    if !has_poppler() {
        eprintln!("skipped: poppler-utils is not installed");
        return;
    }
    for same in [true, false] {
        let m = Model::start(Duration::ZERO).await;
        let other = Model::start(Duration::ZERO).await;
        let state = state_with(&m, true).await;
        let mut dirs: Vec<std::path::PathBuf> =
            std::env::split_paths(&lmgw_core::extract::convert::reader::own_dirs()).collect();
        dirs.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        state
            .doc_tools
            .set_search_path(std::env::join_paths(dirs).unwrap());
        if same {
            settings(&state, |s| s.documents.picture_alias = ALIAS.into()).await;
        } else {
            describer_on(&state, &other).await;
        }
        let gw = common::serve(state.clone()).await;
        let file = pdf(&[(Some("Page one"), true)]);
        let body = json!({
            "files": [{"name": "example.pdf",
                       "data_base64": base64::engine::general_purpose::STANDARD.encode(&file)}],
            "describe_pictures": true,
            "model_help": "every_page",
            "output": "json",
        });
        let (status, a) = convert(&gw, &body).await;
        assert_eq!(status, 200, "{a}");
        let d = &a["documents"][0];
        assert_eq!(d["stats"]["parts_by_source"]["vision"], 1, "{d}");
        assert_eq!(d["pictures"].as_array().unwrap().len(), 1, "{d}");
        assert_eq!(d["pictures"][0]["part"], 1, "{d}");
        assert!(d["pictures"][0]["description"].is_string(), "{d}");
        let pages: Vec<String> = m.bodies().await.iter().map(|b| sent(b).0).collect();
        let pictures: Vec<String> = other.bodies().await.iter().map(|b| sent(b).0).collect();
        let all: Vec<&String> = pages.iter().chain(&pictures).collect();
        assert_eq!(all.len(), 2, "one page call, one picture call: {all:?}");
        assert_eq!(
            all.iter()
                .filter(|p| p.starts_with("Describe this image"))
                .count(),
            1,
            "{all:?}"
        );
        let models = d["stats"]["models"].as_array().unwrap();
        if same {
            assert_eq!(models.len(), 1, "{d}");
            assert_eq!(
                (&models[0]["parts_read"], &models[0]["pictures_described"]),
                (&json!(1), &json!(1))
            );
        } else {
            assert_eq!((pages.len(), pictures.len()), (1, 1));
            assert_eq!(models.len(), 2, "{d}");
            let of = |alias: &str| models.iter().find(|e| e["alias"] == alias).unwrap();
            assert_eq!(of(ALIAS)["parts_read"], 1);
            assert_eq!(of("example-describer")["pictures_described"], 1);
        }
    }
}
