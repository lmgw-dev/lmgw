//! Security round B (document-conversion design §14, 2026-11-19): what a
//! caller sends is echoed only within a bound, into the answer and into the
//! usage row (M4); a pptx slide with a title and a picture writes a
//! `figure` block in `output: json` (a carried bug).

use std::io::Write;

use lmgw_core::store;
use serde_json::{json, Value};

use crate::common::Gw;
use crate::docling_serve::{b64, gateway};

const PATH: &str = "/v1/documents/convert";

async fn convert(gw: &Gw, body: &Value) -> (u16, Value) {
    let r = gw
        .client()
        .post(format!("{gw}{PATH}"))
        .json(body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

/// M4: a 5 MiB `pages` field is refused with a message of its start, and
/// the usage row keeps no more than its bound.
#[tokio::test]
async fn a_refused_value_is_echoed_within_its_bound() {
    let (state, gw) = gateway().await;
    let pages = "Z".repeat(5 << 20);
    let form = reqwest::multipart::Form::new()
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"one".to_vec()).file_name("a.txt"),
        )
        .text("pages", pages);
    let r = gw
        .client()
        .post(format!("{gw}{PATH}"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
    let body = r.text().await.unwrap();
    assert!(body.len() < 2048, "{} bytes", body.len());
    assert!(
        body.contains("(cut at 64 bytes, MAX_QUOTED_BYTES)"),
        "{body}"
    );
    // The same in JSON, an unknown enum value through serde's message.
    let (status, a) = convert(
        &gw,
        &json!({
            "files": [{"name": "a.txt", "data_base64": b64(b"one")}],
            "output": "Y".repeat(1 << 20),
        }),
    )
    .await;
    assert_eq!(status, 400, "{a}");
    let said = a.to_string();
    assert!(said.len() < 2048, "{} bytes", said.len());
    assert!(said.contains("expected one of"), "{said}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let rows = loop {
        let rows = store::query_logs(
            &state.db,
            &store::LogFilter {
                alias: Some("documents/convert".into()),
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if rows.len() >= 2 || std::time::Instant::now() > deadline {
            break rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(rows.len(), 2, "{rows:?}");
    for row in rows {
        let m = row.error_msg.unwrap_or_default();
        assert!(m.len() < 2048, "{} bytes", m.len());
    }
}

/// The usage row's own bound, for any route's message.
#[tokio::test]
async fn a_usage_row_keeps_at_most_its_bound_of_a_message() {
    let (state, _gw) = gateway().await;
    let long = "x".repeat(1 << 20);
    let row = store::NewRequestLog {
        ingress_proto: "documents".into(),
        requested_alias: "example-bound".into(),
        status: 400,
        error_kind: Some("invalid_option".into()),
        error_msg: Some(long),
        ..Default::default()
    };
    store::insert_request_log(&state.db, &row).await.unwrap();
    let rows = store::query_logs(
        &state.db,
        &store::LogFilter {
            alias: Some("example-bound".into()),
            limit: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let m = rows[0].error_msg.as_deref().unwrap();
    let max = store::error_msg::ERROR_MSG_MAX_BYTES;
    assert!(m.len() <= max + 64, "{}", m.len());
    assert!(
        m.ends_with("(cut at 4096 bytes, ERROR_MSG_MAX_BYTES)"),
        "{m}"
    );
}

/// A deck of one slide: a title, then a picture beside it (shapes one line
/// apart in the reader's markdown).
fn titled_picture_pptx() -> Vec<u8> {
    let slide = r#"<p:sld xmlns:a="a" xmlns:p="p" xmlns:r="r"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>Example title</a:t></a:r></a:p></p:txBody></p:sp><p:pic><p:blipFill><a:blip r:embed="rId2"/></p:blipFill><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="4000000" cy="3000000"/></a:xfrm></p:spPr></p:pic></p:spTree></p:cSld></p:sld>"#;
    let pres = r#"<p:presentation xmlns:p="p" xmlns:r="r"><p:sldIdLst><p:sldId r:id="rId1"/></p:sldIdLst><p:sldSz cx="9144000" cy="6858000"/></p:presentation>"#;
    let types = r#"<Types><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/></Types>"#;
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    png.extend_from_slice(&400u32.to_be_bytes());
    png.extend_from_slice(&300u32.to_be_bytes());
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    let mut put = |name: &str, data: &[u8]| {
        w.start_file(name, opts).unwrap();
        w.write_all(data).unwrap();
    };
    put("[Content_Types].xml", types.as_bytes());
    put("ppt/presentation.xml", pres.as_bytes());
    put(
        "ppt/_rels/presentation.xml.rels",
        br#"<Relationships><Relationship Id="rId1" Target="slides/slide1.xml"/></Relationships>"#,
    );
    put("ppt/slides/slide1.xml", slide.as_bytes());
    put(
        "ppt/slides/_rels/slide1.xml.rels",
        br#"<Relationships><Relationship Id="rId2" Target="../media/image1.png"/></Relationships>"#,
    );
    put("ppt/media/image1.png", &png);
    w.finish().unwrap().into_inner()
}

/// A slide's title and its picture: the title paragraph keeps no marker,
/// and the picture is a `figure` block.
#[tokio::test]
async fn a_slide_s_title_and_picture_are_a_paragraph_and_a_figure() {
    let (_state, gw) = gateway().await;
    let deck = titled_picture_pptx();
    let (status, a) = convert(
        &gw,
        &json!({
            "files": [{"name": "example.pptx", "data_base64": b64(&deck)}],
            "output": "json",
        }),
    )
    .await;
    assert_eq!(status, 200, "{a}");
    let d = &a["documents"][0];
    assert_eq!(d["status"], "converted", "{d}");
    let blocks: Vec<&Value> = d["structure"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["blocks"].as_array().unwrap())
        .collect();
    assert!(
        blocks
            .iter()
            .all(|b| !b["text"].as_str().unwrap_or("").contains('\u{FFFC}')),
        "{d}"
    );
    assert!(
        blocks
            .iter()
            .any(|b| b["kind"] == "paragraph" && b["text"] == "Example title"),
        "{d}"
    );
    assert!(blocks.iter().any(|b| b["kind"] == "figure"), "{d}");
    // Markdown as before: the title, then the picture's line.
    let (_, a) = convert(
        &gw,
        &json!({"files": [{"name": "example.pptx", "data_base64": b64(&deck)}]}),
    )
    .await;
    let md = a["documents"][0]["content"].as_str().unwrap();
    assert!(md.contains("Example title\n<!-- image -->"), "{md:?}");
    assert!(!md.contains('\u{FFFC}'), "{md:?}");
}

/// A message's transcription and its one clock (carried bugs): the
/// speech-to-text call takes the message's end over, so a slow
/// transcription ends at the deadline and the message keeps what it read
/// before; and an attachment the deadline cuts while its call is in flight
/// still counts that call in `stats.models`.
mod mail_clock {
    use std::time::Duration;

    use base64::Engine as _;
    use serde_json::{json, Value};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::chat_attach_kinds::{chat_mock, setup, wav};
    use crate::common::Gw;
    use crate::documents_pictures_clock::mail;

    /// A speech-to-text upstream: with `fast_first` the first call answers
    /// at once, every other one after `slow`.
    async fn stt(fast_first: bool, slow: Duration) -> MockServer {
        let mock = MockServer::start().await;
        if fast_first {
            Mock::given(method("POST"))
                .and(path("/v1/audio/transcriptions"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"text": "fast example"})),
                )
                .up_to_n_times(1)
                .with_priority(1)
                .mount(&mock)
                .await;
        }
        Mock::given(method("POST"))
            .and(path("/v1/audio/transcriptions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"text": "slow example"}))
                    .set_delay(slow),
            )
            .with_priority(2)
            .mount(&mock)
            .await;
        mock
    }

    /// The gateway, and the chat upstream it was set up with (kept alive
    /// by the caller).
    async fn ready(stt: &MockServer) -> (Gw, MockServer) {
        let chat = chat_mock().await;
        let (state, gw) = setup(&chat, stt).await;
        // lmgw's reader reads the message: the one built beside the tests.
        state
            .doc_tools
            .set_search_path(crate::documents_media::path_with_reader());
        let r = gw
            .client()
            .post(format!("{gw}/api/op/settings_set"))
            .json(&json!({ "documents_asr_alias": "my-asr" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        (gw, chat)
    }

    async fn convert(gw: &Gw, message: &[u8]) -> (u16, Value, Duration) {
        let started = std::time::Instant::now();
        let r = gw
            .client()
            .post(format!("{gw}/v1/documents/convert"))
            .json(&json!({
                "files": [{"name": "example.eml",
                           "data_base64": base64::engine::general_purpose::STANDARD.encode(message)}],
                "document_timeout": 1.5,
            }))
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap(), started.elapsed())
    }

    fn asr_calls(d: &Value) -> u64 {
        d["stats"]["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["alias"] == "my-asr")
            .map(|m| m["calls"].as_u64().unwrap())
            .sum()
    }

    #[tokio::test]
    async fn a_slow_transcription_ends_at_the_message_s_deadline_and_the_rest_stays() {
        let stt = stt(false, Duration::from_secs(6)).await;
        let (gw, _chat) = ready(&stt).await;
        let message = mail(&[
            (
                "intro.txt",
                "text/plain",
                b"Example first attachment.".to_vec(),
            ),
            ("example.wav", "audio/wav", wav()),
            (
                "after.txt",
                "text/plain",
                b"Example last attachment.".to_vec(),
            ),
        ]);
        let (status, a, took) = convert(&gw, &message).await;
        assert_eq!(status, 200, "{a}");
        assert!(took < Duration::from_millis(4500), "{took:?}");
        let d = &a["documents"][0];
        assert_eq!(d["status"], "partial", "{d}");
        let content = d["content"].as_str().unwrap();
        assert!(content.contains("Example first attachment."), "{content}");
        assert!(!content.contains("Example last attachment."), "{content}");
        let named = d["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|w| w["message"].as_str())
            .any(|w| w.contains("example.wav") && w.contains("document_timeout of 1.5 s"));
        assert!(named, "{d}");
        assert_eq!(asr_calls(d), 1, "{d}");
    }

    #[tokio::test]
    async fn an_attachment_cut_mid_call_keeps_its_call_in_the_stats() {
        let stt = stt(true, Duration::from_secs(6)).await;
        let (gw, _chat) = ready(&stt).await;
        let message = mail(&[
            ("first.wav", "audio/wav", wav()),
            ("second.wav", "audio/wav", wav()),
        ]);
        let (status, a, took) = convert(&gw, &message).await;
        assert_eq!(status, 200, "{a}");
        assert!(took < Duration::from_millis(4500), "{took:?}");
        let d = &a["documents"][0];
        assert!(
            d["content"].as_str().unwrap().contains("fast example"),
            "{d}"
        );
        assert_eq!(asr_calls(d), 2, "both calls were sent: {d}");
    }
}
