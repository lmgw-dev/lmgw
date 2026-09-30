//! A stored attachment rendered into a model request (chat-complete design
//! §8): text as `<file>` blocks, images and PDF pages as image parts, audio
//! natively or as its transcript — for the model that will answer.

use base64::Engine as _;
use bytes::Bytes;
use serde_json::json;

use crate::extract::pdf;
use crate::ir::{ContentPart, ImageSource};
use crate::state::SharedState;
use crate::store::ChatAttachmentFull;

use super::chat_attach::{escape_attr, file_block};
use super::chat_attach_gate::{native_audio, Caps};
use super::chat_attach_ingest::apply_transcript;
use super::chat_repo::ChatRepo;

/// What one attachment renders to: the parts for the message, and the notes
/// among them that say something could not be sent (each note is also a text
/// part, so the model and the caller both see it).
#[derive(Debug, Default)]
pub struct Rendered {
    pub parts: Vec<ContentPart>,
    pub notes: Vec<String>,
}

impl Rendered {
    fn note(&mut self, text: String) {
        self.parts.push(ContentPart::text(format!("[{text}]")));
        self.notes.push(text);
    }
}

fn image_part(mime: &str, bytes: &[u8]) -> ContentPart {
    ContentPart::Image {
        mime: mime.to_string(),
        source: ImageSource::Base64 {
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        },
    }
}

/// `1, 3, 7` for a page list.
fn page_list(pages: &[u32]) -> String {
    pages
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Render one attachment for the model `model`, which has `caps`. Never refuses — a fresh
/// send is gated before this (`chat_attach_gate::blockers`); anything that
/// still cannot go (history replayed on another model) becomes a note.
pub async fn render(
    state: &SharedState,
    att: &ChatAttachmentFull,
    caps: Caps,
    model: &str,
) -> Rendered {
    let mut out = Rendered::default();
    match att.kind.as_str() {
        "image" if caps.vision == Some(false) => out.note(format!(
            "image \"{}\" not sent: {model} does not accept images",
            escape_attr(&att.name)
        )),
        "image" => out.parts.push(image_part(&att.mime, &att.data)),
        "pdf" => render_pdf(state, att, caps, model, &mut out).await,
        "office" => {
            let attrs = format!(" kind=\"office\"{}", format_attr(att));
            let text = att.extracted.as_deref().unwrap_or_default();
            out.parts.push(file_block(&att.name, &attrs, text));
        }
        "audio" => render_audio(state, att, caps, &mut out).await,
        // "text", and any future kind with no arm yet: a labeled text block
        // is what every model already understands.
        _ => out.parts.push(file_block(
            &att.name,
            "",
            &String::from_utf8_lossy(&att.data),
        )),
    }
    out
}

fn format_attr(att: &ChatAttachmentFull) -> String {
    att.meta
        .get("format")
        .and_then(|f| f.as_str())
        .map(|f| format!(" format=\"{}\"", escape_attr(f)))
        .unwrap_or_default()
}

// -- PDF ----------------------------------------------------------------------

async fn render_pdf(
    state: &SharedState,
    att: &ChatAttachmentFull,
    caps: Caps,
    model: &str,
    out: &mut Rendered,
) {
    let pages = att.meta.get("pages").and_then(|p| p.as_u64()).unwrap_or(0) as u32;
    let textless: Vec<u32> = att
        .meta
        .get("textless")
        .and_then(|t| serde_json::from_value(t.clone()).ok())
        .unwrap_or_default();
    let class = att
        .meta
        .get("class")
        .and_then(|c| c.as_str())
        .unwrap_or("text");
    let sees = caps.vision != Some(false);

    // Every page as an image: a text-class PDF the user sent as Pages.
    if class == "text" && att.mode.as_deref() == Some("images") {
        if sees {
            let all: Vec<u32> = (1..=pages).collect();
            push_page_images(state, att, &all, out).await;
            return;
        }
        // History replayed on a model that cannot see: the pages cannot go, so
        // the extracted text does, and the note says so.
        out.note(format!(
            "PDF \"{}\" was sent as page images and {model} does not accept images — its \
             extracted text is sent instead",
            escape_attr(&att.name)
        ));
    }

    let text = att.extracted.as_deref().unwrap_or_default();
    if !text.trim().is_empty() {
        let attrs = format!(" kind=\"pdf\" pages=\"{pages}\"");
        out.parts.push(file_block(&att.name, &attrs, text));
    }
    if textless.is_empty() {
        return;
    }
    // Scanned or hybrid: the pages without text go as images when the model
    // can see them, and as a visible note when it cannot.
    if sees {
        push_page_images(state, att, &textless, out).await;
    } else {
        out.note(format!(
            "\"{}\" pages {}: no text, and this model does not see images",
            escape_attr(&att.name),
            page_list(&textless)
        ));
    }
}

async fn push_page_images(
    state: &SharedState,
    att: &ChatAttachmentFull,
    pages: &[u32],
    out: &mut Rendered,
) {
    let repo = ChatRepo::of(att.id);
    let bytes = Bytes::from(att.data.clone());
    for &page in pages {
        let cached = repo.page_png(state, att.id, page).await.ok().flatten();
        let png = match cached {
            Some(png) => png,
            None => match pdf::render_page(bytes.clone(), page).await {
                Ok(png) => {
                    if let Err(e) = repo.put_page_png(state, att.id, page, &png).await {
                        tracing::warn!(attachment = att.id, page, "chat: page not cached: {e}");
                    }
                    png
                }
                Err(e) => {
                    out.note(format!(
                        "\"{}\" page {page} not sent: {e}",
                        escape_attr(&att.name)
                    ));
                    continue;
                }
            },
        };
        out.parts.push(ContentPart::text(format!(
            "[{} page {page}]",
            escape_attr(&att.name)
        )));
        out.parts.push(image_part("image/png", &png));
    }
}

// -- audio --------------------------------------------------------------------

async fn render_audio(
    state: &SharedState,
    att: &ChatAttachmentFull,
    caps: Caps,
    out: &mut Rendered,
) {
    if let Some(mime) = native_audio(caps, &att.mime) {
        out.parts.push(ContentPart::Audio {
            mime: mime.to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(&att.data),
        });
        return;
    }
    let (transcript, alias) = match transcript_of(state, att).await {
        Ok(t) => t,
        Err(why) => {
            out.note(format!(
                "audio \"{}\" not sent: {why}",
                escape_attr(&att.name)
            ));
            return;
        }
    };
    let attrs = format!(" kind=\"audio-transcript\" by=\"{}\"", escape_attr(&alias));
    out.parts.push(file_block(&att.name, &attrs, &transcript));
}

/// The transcript, stored one or made now (and stored) with the configured
/// speech-to-text alias. The alias that made it comes with it.
async fn transcript_of(
    state: &SharedState,
    att: &ChatAttachmentFull,
) -> Result<(String, String), String> {
    let stored_alias = att
        .meta
        .get("transcript_alias")
        .and_then(|a| a.as_str())
        .map(str::to_string);
    if let (Some(text), Some(alias)) = (&att.extracted, stored_alias) {
        return Ok((text.clone(), alias));
    }
    let stt = state.snapshot().settings.chat_stt_alias.clone();
    if stt.is_empty() {
        return Err("the model takes no audio and no speech-to-text model is set".into());
    }
    let text = crate::proxy::transcribe(
        state,
        &stt,
        Bytes::from(att.data.clone()),
        &att.name,
        &att.mime,
    )
    .await
    .map_err(|e| format!("transcription with '{stt}' failed: {e}"))?;
    let mut meta = if att.meta.is_object() {
        att.meta.clone()
    } else {
        json!({})
    };
    let mut extracted = None;
    apply_transcript(&mut meta, &stt, &text, &mut extracted);
    if let Err(e) = ChatRepo::of(att.id)
        .set_extracted(state, att.id, &text, &meta)
        .await
    {
        tracing::warn!(attachment = att.id, "chat: transcript not stored: {e}");
    }
    Ok((text, stt))
}
