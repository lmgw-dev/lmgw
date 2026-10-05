//! An upload becoming an attachment (chat-complete design §8): sniffed from
//! its bytes, extracted at upload time, classified — and for audio a model
//! without audio input gets its transcript right away so the chip can show it.

use axum::http::StatusCode;
use bytes::Bytes;
use serde_json::{json, Value};

use crate::extract::{self, ExtractError, Extracted, Kind, PdfClass, PdfError};
use crate::state::SharedState;
use crate::store::{ChatThread, NewAttachment};

use super::chat_turn;

/// A refusal: status, code, message — the flat `ApiError` of the route.
pub type Refusal = (StatusCode, &'static str, String);

fn class_str(c: PdfClass) -> &'static str {
    match c {
        PdfClass::Text => "text",
        PdfClass::Scanned => "scanned",
        PdfClass::Hybrid => "hybrid",
    }
}

fn extract_refusal(e: ExtractError) -> Refusal {
    match e {
        ExtractError::Pdf(PdfError::ToolMissing) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "tool_missing",
            PdfError::ToolMissing.to_string(),
        ),
        other => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "extract_failed",
            format!("could not read the file: {other}"),
        ),
    }
}

/// Sniff and extract `body`, named `name`, for `thread` — answered by its
/// model, and transcribed (audio its model cannot hear) with its
/// speech-to-text alias (chat-voice design §2.1).
pub(super) async fn ingest(
    state: &SharedState,
    thread: &ChatThread,
    name: &str,
    body: Bytes,
) -> Result<NewAttachment, Refusal> {
    let model_alias = thread.model_alias.as_str();
    let sniffed = extract::sniff_async(body.clone()).await.map_err(|e| {
        (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_attachment",
            e.to_string(),
        )
    })?;
    let mut new = NewAttachment {
        kind: sniffed.kind.as_str().to_string(),
        name: super::chat_attach::clean_name(name),
        mime: sniffed.mime.to_string(),
        data: body.to_vec(),
        extracted: None,
        meta: json!({}),
        mode: None,
    };
    match sniffed.kind {
        Kind::Image => {}
        Kind::Text => {
            // The bytes are the text: only the estimate is stored.
            let text = String::from_utf8_lossy(&body);
            new.meta = json!({ "tokens": extract::approx_tokens(&text) });
        }
        Kind::Pdf | Kind::Office => {
            let out = extract::extract(&sniffed, body)
                .await
                .map_err(extract_refusal)?;
            let text = out.text();
            let mut meta = json!({ "tokens": extract::approx_tokens(&text) });
            match &out {
                Extracted::Pdf(p) => {
                    let class = p.classify();
                    meta["pages"] = json!(p.pages.len());
                    meta["textless"] = json!(p.textless);
                    meta["class"] = json!(class_str(class));
                    if class == PdfClass::Text {
                        new.mode = match state.snapshot().settings.chat_pdf_mode.as_str() {
                            "images" => Some("images".to_string()),
                            "ask" => None,
                            _ => Some("text".to_string()),
                        };
                    }
                }
                Extracted::Office(o) => {
                    meta["format"] = json!(sniffed.sub);
                    meta["parts"] = json!(o.parts.len());
                    let sheets = o.sheet_names();
                    if !sheets.is_empty() {
                        meta["sheets"] = json!(sheets);
                    }
                }
                Extracted::Text(_) => {}
            }
            new.extracted = Some(text);
            new.meta = meta;
        }
        Kind::Audio => {
            new.meta = json!({ "format": sniffed.sub });
            let stt = super::chat_voice::asr_alias(&state.snapshot(), thread);
            let caps = chat_turn::model_caps(state, model_alias).await;
            let native = super::chat_attach_gate::native_audio(caps, sniffed.mime).is_some();
            if let Some(stt) = stt.filter(|_| !native) {
                let proto = super::chat_voice::speech_proto(thread);
                let text = crate::proxy::transcribe_for(
                    state,
                    proto,
                    &stt,
                    body.clone(),
                    &new.name,
                    sniffed.mime,
                );
                match text.await {
                    Ok(text) => apply_transcript(&mut new.meta, &stt, &text, &mut new.extracted),
                    Err(e) => new.meta["transcript_error"] = Value::String(e.to_string()),
                }
            }
        }
    }
    Ok(new)
}

/// Record a transcript: the text in `extracted`, its alias and token estimate
/// in `meta` (a stale error goes away).
pub(super) fn apply_transcript(
    meta: &mut Value,
    alias: &str,
    text: &str,
    extracted: &mut Option<String>,
) {
    *extracted = Some(text.to_string());
    meta["transcript_alias"] = json!(alias);
    meta["tokens"] = json!(extract::approx_tokens(text));
    if let Some(o) = meta.as_object_mut() {
        o.remove("transcript_error");
    }
}
