//! An upload becoming an attachment (chat-complete design §8): sniffed from
//! its bytes, extracted at upload time, classified — and for audio a model
//! without audio input gets its transcript right away so the chip can show it.

use axum::http::StatusCode;
use bytes::Bytes;
use serde_json::{json, Value};

use crate::extract::{self, ExtractError, Extracted, Kind, PdfClass, PdfError};
use crate::state::SharedState;
use crate::store::{ChatThread, NewAttachment};

use super::chat_caller::Caller;
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
        ExtractError::Pdf(e @ (PdfError::ToolMissing | PdfError::NoReader)) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "tool_missing",
            e.to_string(),
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
    caller: &Caller,
    thread: &ChatThread,
    name: &str,
    body: Bytes,
) -> Result<NewAttachment, Refusal> {
    let model_alias = thread.model_alias.as_str();
    let sniffed = match extract::sniff_async(body.clone()).await {
        Ok(s) => s,
        // What the Chat's sniffer refuses, read through the conversion's
        // table where it has a reader (document-conversion WP7).
        Err(refused) => return converted(state, name, body, refused).await,
    };
    let mut new = NewAttachment {
        kind: sniffed.kind.as_str().to_string(),
        name: super::chat_attach::clean_name(name),
        mime: sniffed.mime.to_string(),
        data: body.to_vec(),
        extracted: None,
        meta: json!({}),
        mode: None,
        source: lmgw_api_types::chat_media::SOURCE_UPLOAD.to_string(),
        call_id: None,
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
                    // What its text run said of its memory (`pdf::wide`),
                    // as a converted attachment's notes.
                    if !p.notes.is_empty() {
                        let notes: Vec<String> =
                            p.notes.iter().map(|n| n.said(&n.where_())).collect();
                        meta["notes"] = Value::from(notes);
                    }
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
                let upload = (body.clone(), new.name.as_str(), sniffed.mime);
                match caller.transcribe(state, proto, &stt, upload).await {
                    Ok(text) => apply_transcript(&mut new.meta, &stt, &text, &mut new.extracted),
                    Err(e) => new.meta["transcript_error"] = Value::String(e.to_string()),
                }
            }
        }
    }
    Ok(new)
}

/// An upload the Chat's sniffer refused, read through the conversion's
/// capability table (`extract::convert::own`): an old Word, PowerPoint or
/// Excel file, an EPUB book. It is an `office` attachment with its markdown
/// extracted, its own format in `meta.format`; what the table cannot read
/// here is the 415 it was, worded by the table where it knows more.
async fn converted(
    state: &SharedState,
    name: &str,
    body: Bytes,
    refused: extract::SniffError,
) -> Result<NewAttachment, Refusal> {
    let c = crate::extract::convert::own::read(state, name, body.clone(), refused)
        .await
        .map_err(|why| {
            (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_attachment",
                why,
            )
        })?;
    // The keys an office attachment's meta has (`tokens`, `format`,
    // `parts`), and the conversion's notes.
    let mut m = serde_json::Map::new();
    m.insert(
        "tokens".into(),
        Value::from(extract::approx_tokens(&c.markdown)),
    );
    m.insert("format".into(), Value::from(c.sniffed.sub));
    m.insert("parts".into(), Value::from(c.parts));
    if !c.notes.is_empty() {
        m.insert("notes".into(), Value::from(c.notes));
    }
    let meta = Value::Object(m);
    Ok(NewAttachment {
        kind: c.sniffed.kind.as_str().to_string(),
        name: super::chat_attach::clean_name(name),
        mime: c.sniffed.mime.to_string(),
        data: body.to_vec(),
        extracted: Some(c.markdown),
        meta,
        mode: None,
        source: lmgw_api_types::chat_media::SOURCE_UPLOAD.to_string(),
        call_id: None,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// poppler-utils is a Recommends, so it can be missing (document-conversion
    /// design D1): a PDF upload is then refused by name, with the package to
    /// install, never as a generic extraction failure.
    #[test]
    fn a_pdf_without_poppler_is_refused_naming_the_package() {
        let (status, code, message) = extract_refusal(ExtractError::Pdf(PdfError::ToolMissing));
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(code, "tool_missing");
        assert!(message.contains("install poppler-utils"), "{message}");
    }
}
