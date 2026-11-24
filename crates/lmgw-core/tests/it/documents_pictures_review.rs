//! Picture descriptions (WP11), the review round's cases: a prompt that
//! fills the model's context sends nothing and makes no message per picture
//! (B1), a description cannot change the document's structure in markdown
//! and stays the model's words in json (S8), bytes made only for the calls
//! are not in the answer (S5), and an image file is its picture whether it
//! is described or not (S7).

use std::time::Duration;

use base64::Engine as _;
use lmgw_core::extract::convert::describe::messages_made;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::common::serve;
use crate::documents_model_help::{convert, Model};
use crate::documents_pictures::{big, describing, docx, request, Pic};

fn warnings_with<'a>(d: &'a Value, option: &str) -> Vec<&'a Value> {
    d["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w["option"] == option)
        .collect()
}

/// B1: the caller's prompt is held once and counted once against the
/// answering model's context (here from its catalog, 1000 tokens): a
/// prompt that fills it sends no picture, says so in one note, and no
/// picture's message is ever made.
#[tokio::test]
async fn a_prompt_that_fills_the_model_s_context_sends_no_picture() {
    let m = Model::start(Duration::ZERO).await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{"id": "vision-tgt", "object": "model", "context_length": 1000}],
        })))
        .mount(&m.mock)
        .await;
    let state = describing(&m).await;
    let gw = serve(state.clone()).await;
    let pics: Vec<Pic> = (0..40).map(|i| big(40 + i, 30)).collect();
    let prompt = "Describe the example picture in plain words, please. ".repeat(400);
    let made = messages_made();
    let (status, a) = convert(
        &gw,
        &request(
            &docx(&pics),
            json!({"picture_prompt": prompt, "output": "json"}),
        ),
    )
    .await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "partial", "{d}");
    assert!(m.bodies().await.is_empty(), "no call reached the model");
    assert_eq!(messages_made(), made, "no picture's message was made");
    let notes = warnings_with(d, "picture_prompt");
    assert_eq!(notes.len(), 1, "{d}");
    let said = notes[0]["message"].as_str().unwrap();
    assert_eq!(notes[0]["code"], "part_failed");
    assert!(
        said.contains("leaves no room") && said.contains("context of 1000 tokens"),
        "{said}"
    );
    assert!(said.contains("p1") && said.contains("p40"), "{said}");
    assert!(d["pictures"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["description"].is_null()));

    // docling's own prompt fits: the same model describes.
    let (_, a) = convert(&gw, &request(&docx(&pics[..2]), json!({}))).await;
    assert_eq!(a["documents"][0]["status"], "converted", "{a}");
    assert_eq!(m.bodies().await.len(), 2);
    assert_eq!(messages_made(), made + 2, "one message per call made");
}

/// S8: a description with markup and a model's control token. Markdown:
/// every line of it is paragraph text, and the document goes on as it was
/// after it. Json: the model's words but for the control token.
#[tokio::test]
async fn a_description_s_markup_stays_text_and_the_document_goes_on() {
    let m = Model::start(Duration::ZERO).await;
    let state = describing(&m).await;
    let gw = serve(state.clone()).await;
    let answer = "# Results\n<!-- hidden\n```\n$$ x^2 $$\n| a | b |\nTitle\n---\n\
                  [x]: https://example.invalid/\nPlain words.<|im_end|>";
    let file = docx(&[big(400, 300), big(300, 200)]);
    m.will_answer(answer, "stop");
    let (status, a) = convert(&gw, &request(&file, json!({}))).await;
    assert_eq!(status, 200, "{a}");
    let md = a["documents"][0]["content"].as_str().unwrap();
    let (_, from) = md
        .split_once("Figure 1: Example chart 1\n\n")
        .unwrap_or_else(|| panic!("{md}"));
    let (description, rest) = from.split_once("\n\n").unwrap();
    for l in description.lines() {
        for start in ["#", "<", "```", "$$", "|", "---", "[x]:"] {
            assert!(!l.starts_with(start), "{l:?} in {md}");
        }
    }
    assert!(!md.contains("<|im_end|>"), "{md}");
    assert!(
        rest.starts_with("Text before picture 2.\n\n<!-- image -->"),
        "{md}"
    );
    m.will_answer(answer, "stop");
    let (_, a) = convert(&gw, &request(&file, json!({"output": "json"}))).await;
    let d = &a["documents"][0];
    let want = answer.trim_end_matches("<|im_end|>");
    assert_eq!(d["pictures"][0]["description"], want, "{d}");
    let figure = d["structure"][0]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["kind"] == "figure")
        .unwrap();
    assert_eq!(figure["description"], want);
}

/// S5: in placeholder mode the picture's bytes were made only for its
/// call: the answer carries none of them (no type, no size), as without a
/// description.
#[tokio::test]
async fn bytes_made_only_for_a_description_are_not_in_the_answer() {
    let m = Model::start(Duration::ZERO).await;
    let state = describing(&m).await;
    let gw = serve(state.clone()).await;
    m.will_answer("An example chart.", "stop");
    let (status, a) = convert(&gw, &request(&docx(&[big(400, 300)]), json!({}))).await;
    assert_eq!(status, 200, "{a}");
    let p = &a["documents"][0]["pictures"][0];
    assert_eq!(p["description"], "An example chart.", "{p}");
    assert!(
        p["mime"].is_null() && p["bytes"].is_null() && p["data_base64"].is_null(),
        "{p}"
    );
}

fn has_tesseract() -> bool {
    std::process::Command::new("tesseract")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A white PNG of `w` × `h`.
fn png_file(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgb);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(&vec![255u8; (w * h * 3) as usize])
            .unwrap();
        wr.finish().unwrap();
    }
    out
}

/// S7: an image file is its one picture whenever pictures are exported,
/// described or not; in placeholder mode it is a picture of the answer
/// only with a description made (a failed one leaves no placeholder).
#[tokio::test]
async fn an_image_file_is_its_picture_whether_described_or_not() {
    if !has_tesseract() {
        eprintln!("skipped: tesseract is not installed");
        return;
    }
    let m = Model::start(Duration::ZERO).await;
    let state = describing(&m).await;
    // lmgw's reader lists the image: the one cargo built beside the tests.
    let mut dirs: Vec<std::path::PathBuf> =
        std::env::split_paths(&lmgw_core::extract::convert::reader::own_dirs()).collect();
    dirs.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    state
        .doc_tools
        .set_search_path(std::env::join_paths(dirs).unwrap());
    let gw = serve(state.clone()).await;
    let file = png_file(120, 80);
    let body = |options: Value| {
        let mut b = json!({"files": [{"name": "example.png",
            "data_base64": base64::engine::general_purpose::STANDARD.encode(&file)}]});
        for (k, v) in options.as_object().unwrap() {
            b[k] = v.clone();
        }
        b
    };
    // Embedded, nothing described: the image itself, no not_yet note.
    let (status, a) = convert(&gw, &body(json!({"pictures": "embedded"}))).await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert!(
        d["content"]
            .as_str()
            .unwrap()
            .starts_with("![Image](data:image/png;base64,"),
        "{d}"
    );
    assert_eq!(d["pictures"].as_array().unwrap().len(), 1, "{d}");
    assert!(warnings_with(d, "pictures").is_empty(), "{d}");
    assert!(m.bodies().await.is_empty());

    // Placeholder, described, the call fails: no placeholder line of a
    // picture the answer does not have.
    m.will_fail(500);
    let (_, a) = convert(&gw, &body(json!({"describe_pictures": true}))).await;
    let d = &a["documents"][0];
    assert_eq!(m.bodies().await.len(), 1);
    assert!(
        !d["content"].as_str().unwrap().contains("<!-- image -->"),
        "{d}"
    );
    assert_eq!(d["pictures"].as_array().map_or(0, Vec::len), 0, "{d}");

    // Placeholder, described: the placeholder and its description.
    m.will_answer("An example picture.", "stop");
    let (_, a) = convert(&gw, &body(json!({"describe_pictures": true}))).await;
    let d = &a["documents"][0];
    assert!(
        d["content"]
            .as_str()
            .unwrap()
            .starts_with("<!-- image -->\n\nAn example picture."),
        "{d}"
    );
}
