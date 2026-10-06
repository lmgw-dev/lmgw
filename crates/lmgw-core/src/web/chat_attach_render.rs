//! A stored attachment rendered into a model request (chat-complete design
//! §8): text as `<file>` blocks, images and PDF pages as image parts, audio
//! natively or as its transcript — for the model that will answer.

use base64::Engine as _;
use bytes::Bytes;
use serde_json::json;

use crate::extract::pdf;
use crate::ir::{ContentPart, ImageSource};
use crate::state::SharedState;
use crate::store::{ChatAttachmentFull, ChatThread};

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
    /// A PDF whose pages went as images: the parts it renders to for a model
    /// that cannot see — its extracted text, and a note for the pages
    /// without text — which no page is rasterized for. The send swaps them
    /// in when a fallback that cannot see answers (`chat_turn::blind`).
    /// `None` for every other attachment.
    pub text_form: Option<Vec<ContentPart>>,
    /// What the model it was rendered for lacked, and so got in another
    /// form ([`marker`]); `None` when it went as it is.
    pub lacked: Option<Lacked>,
}

/// What a model lacked for an attachment, and so got it in another form:
/// the turn's request row says so ([`marker`], `request_logs.degraded`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lacked {
    /// Vision: an image went as a note.
    ImageNote,
    /// Vision: a PDF's page images went as its text, or a note for the
    /// pages without text.
    PdfText,
    /// Audio input: an audio file went as its transcript.
    AudioTranscript,
}

/// The request row's marker for the attachments of a turn rendered for
/// `model` ([`crate::degraded`]): what it lacked, and what went instead.
/// `None` when every attachment went as it is.
pub fn marker<'a>(model: &str, rendered: impl IntoIterator<Item = &'a Rendered>) -> Option<String> {
    let lacked: Vec<Lacked> = rendered.into_iter().filter_map(|r| r.lacked).collect();
    let count = |l: Lacked| lacked.iter().filter(|x| **x == l).count();
    let images = count(Lacked::ImageNote);
    crate::degraded::join([
        (images > 0).then(|| {
            let sent = crate::degraded::images(images, "a note", "notes");
            crate::degraded::lacks(model, false, "vision", &sent)
        }),
        (count(Lacked::PdfText) > 0)
            .then(|| crate::degraded::lacks(model, false, "vision", "PDF pages sent as text")),
        (count(Lacked::AudioTranscript) > 0)
            .then(|| crate::degraded::lacks(model, false, "audio", "transcript sent")),
    ])
}

/// Who a PDF's text form names as the model that cannot see: whichever
/// answers, which the render does not know.
const ANSWERING: &str = "the answering model";

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

/// Render one attachment of `thread` for its model, which has `caps`. Never
/// refuses — a fresh send is gated before this (`chat_attach_gate::blockers`);
/// anything that still cannot go (history replayed on another model) becomes
/// a note. An audio transcript made now uses the thread's ASR alias
/// (chat-voice design §2.1).
pub async fn render(
    state: &SharedState,
    att: &ChatAttachmentFull,
    caps: Caps,
    thread: &ChatThread,
) -> Rendered {
    let model = thread.model_alias.as_str();
    let mut out = Rendered::default();
    match att.kind.as_str() {
        "image" if caps.vision == Some(false) => {
            out.note(format!(
                "image \"{}\" not sent: {model} does not accept images",
                escape_attr(&att.name)
            ));
            out.lacked = Some(Lacked::ImageNote);
        }
        "image" => out.parts.push(image_part(&att.mime, &att.data)),
        "pdf" => {
            render_pdf(state, att, caps.vision != Some(false), model, &mut out).await;
            let paged = out
                .parts
                .iter()
                .any(|p| matches!(p, ContentPart::Image { .. }));
            if paged {
                let mut text = Rendered::default();
                render_pdf(state, att, false, ANSWERING, &mut text).await;
                out.text_form = Some(text.parts);
            }
        }
        "office" => {
            let attrs = format!(" kind=\"office\"{}", format_attr(att));
            let text = att.extracted.as_deref().unwrap_or_default();
            out.parts.push(file_block(&att.name, &attrs, text));
        }
        "audio" => render_audio(state, att, caps, thread, &mut out).await,
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

/// Render a PDF for a model that `sees` images, or not; `model` is who the
/// notes of the second case name.
async fn render_pdf(
    state: &SharedState,
    att: &ChatAttachmentFull,
    sees: bool,
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

    // Every page as an image: a text-class PDF the user sent as Pages.
    if class == "text" && att.mode.as_deref() == Some("images") {
        if sees {
            let all: Vec<u32> = (1..=pages).collect();
            push_page_images(state, att, &all, out).await;
            return;
        }
        // History replayed on a model that cannot see: the pages cannot go, so
        // the extracted text does, and the note says so.
        out.lacked = Some(Lacked::PdfText);
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
        out.lacked = Some(Lacked::PdfText);
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
    thread: &ChatThread,
    out: &mut Rendered,
) {
    if let Some(mime) = native_audio(caps, &att.mime) {
        out.parts.push(ContentPart::Audio {
            mime: mime.to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(&att.data),
        });
        return;
    }
    let (transcript, alias) = match transcript_of(state, att, thread).await {
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
    // The model is not known to take audio: it lacked it. (One that takes
    // audio gets a container it cannot decode as its transcript too: a
    // format, not a capability.)
    if caps.audio != Some(true) {
        out.lacked = Some(Lacked::AudioTranscript);
    }
}

/// The transcript, stored one or made now (and stored) with the thread's
/// speech-to-text alias (its override, then `chat_stt_alias`, then
/// `realtime.asr_alias`). The alias that made it comes with it.
async fn transcript_of(
    state: &SharedState,
    att: &ChatAttachmentFull,
    thread: &ChatThread,
) -> Result<(String, String), String> {
    let stored_alias = att
        .meta
        .get("transcript_alias")
        .and_then(|a| a.as_str())
        .map(str::to_string);
    if let (Some(text), Some(alias)) = (&att.extracted, stored_alias) {
        return Ok((text.clone(), alias));
    }
    let Some(stt) = super::chat_voice::asr_alias(&state.snapshot(), thread) else {
        return Err("the model takes no audio and no speech-to-text model is set".into());
    };
    let text = crate::proxy::transcribe_for(
        state,
        super::chat_voice::speech_proto(thread),
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
