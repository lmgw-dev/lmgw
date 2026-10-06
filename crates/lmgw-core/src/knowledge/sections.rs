//! A file's bytes → the parts the chunker works on (chat-complete design
//! §9.2), through the shared extraction module (§8, [`crate::extract`]):
//!
//! - a text file is one part;
//! - a PDF is one part per page. A page with no text is read by the base's
//!   vision model with [`OCR_PROMPT`](crate::extract::vision_prompts::OCR_PROMPT),
//!   a page that looks like a table with
//!   [`STRUCTURE_PROMPT`](crate::extract::vision_prompts::STRUCTURE_PROMPT) —
//!   folder-chat's measured words and page rule. Without a vision model a
//!   text-less page is skipped and counted, and the file's notes say which;
//! - an office file is one part per document, slide or sheet.
//!
//! Images and audio are not knowledge-base material and are refused at upload
//! ([`accepts`]).

use std::time::Duration;

use bytes::Bytes;

use crate::config::Route;
use crate::extract::{self, vision_prompts, Kind, Sniffed};
use crate::ir::{ChatRequest, ContentPart, ImageSource, Message, Params, Role};
use crate::state::SharedState;

use super::chunk::Part;

/// Why a knowledge base will not take a file, or `None` when it will.
pub fn accepts(s: &Sniffed) -> Option<String> {
    match s.kind {
        Kind::Text | Kind::Pdf | Kind::Office => None,
        Kind::Image => Some(
            "images are not knowledge-base material — a knowledge base holds text: PDF, \
             Word/Excel/PowerPoint and OpenDocument files, and UTF-8 text files"
                .into(),
        ),
        Kind::Audio => Some(
            "audio is not knowledge-base material — transcribe it first; a knowledge base \
             holds PDF, Word/Excel/PowerPoint and OpenDocument files, and UTF-8 text files"
                .into(),
        ),
    }
}

/// A file's text, ready to assemble and chunk.
#[derive(Debug, Clone, Default)]
pub struct FileText {
    pub parts: Vec<Part>,
    /// PDF pages, slides or sheets; `None` for a text or word file.
    pub pages: Option<i64>,
    /// Text-less PDF pages nobody read.
    pub skipped_pages: i64,
    pub notes: Vec<String>,
}

/// Why reading a file stopped short of an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The GPU hold (or a benchmark's lease) refuses the local vision model:
    /// the file stays pending and the job stops, with this reason.
    Held(String),
    Canceled,
    /// The file itself cannot be read; it is marked failed with this reason.
    Failed(String),
}

/// Read `bytes` into parts. `vision_alias` is the base's (empty: none);
/// `canceled` is polled between pages.
pub async fn read(
    state: &SharedState,
    vision_alias: &str,
    bytes: Bytes,
    canceled: &(dyn Fn() -> bool + Send + Sync),
) -> Result<FileText, Stop> {
    let sniffed = extract::sniff_async(bytes.clone())
        .await
        .map_err(|e| Stop::Failed(e.to_string()))?;
    if let Some(why) = accepts(&sniffed) {
        return Err(Stop::Failed(why));
    }
    match sniffed.kind {
        Kind::Pdf => read_pdf(state, vision_alias, bytes, canceled).await,
        _ => {
            let extracted = extract::extract(&sniffed, bytes)
                .await
                .map_err(|e| Stop::Failed(e.to_string()))?;
            Ok(match extracted {
                extract::Extracted::Text(t) => FileText {
                    parts: vec![Part {
                        page: None,
                        heading: None,
                        body: t,
                    }],
                    ..Default::default()
                },
                extract::Extracted::Office(o) => {
                    let headed = o.parts.iter().filter(|p| p.heading.is_some()).count();
                    FileText {
                        pages: (headed > 0).then_some(o.parts.len() as i64),
                        parts: o
                            .parts
                            .into_iter()
                            .map(|p| Part {
                                page: None,
                                heading: p.heading,
                                body: p.body,
                            })
                            .collect(),
                        ..Default::default()
                    }
                }
                // Sniffed as a PDF above, so never here.
                extract::Extracted::Pdf(p) => FileText {
                    parts: vec![Part {
                        page: None,
                        heading: None,
                        body: p.marked(),
                    }],
                    ..Default::default()
                },
            })
        }
    }
}

async fn read_pdf(
    state: &SharedState,
    vision_alias: &str,
    bytes: Bytes,
    canceled: &(dyn Fn() -> bool + Send + Sync),
) -> Result<FileText, Stop> {
    let text = extract::pdf::text(bytes.clone())
        .await
        .map_err(|e| Stop::Failed(e.to_string()))?;
    let mut pages: Vec<String> = text.pages.clone();
    let mut notes = Vec::new();
    let mut skipped: Vec<u32> = Vec::new();

    let wanted = vision_prompts::select(&text, false);
    let alias = vision_alias.trim();
    if alias.is_empty() {
        skipped = text.textless.clone();
    } else if !wanted.is_empty() {
        // The pages read before, for these very bytes with this model and
        // these prompts, are not read again: a re-chunk of a file that did
        // not change must not pay vision for it twice. The model is only
        // admitted (and a GPU hold only consulted) when a page needs it.
        let sha = super::originals::sha256_hex(&bytes);
        let pool = &state.knowledge.pool;
        let version = vision_prompts::VISION_PROMPT_VERSION;
        let mut reader: Option<VisionReader> = None;
        let mut reused: Vec<u32> = Vec::new();
        let mut failed: Vec<(u32, String)> = Vec::new();
        for (page, mode) in &wanted {
            if canceled() {
                return Err(Stop::Canceled);
            }
            let idx = (*page - 1) as usize;
            match super::store::get_page_read(
                pool,
                &sha,
                i64::from(*page),
                alias,
                mode.as_str(),
                version,
            )
            .await
            {
                Ok(Some(t)) => {
                    pages[idx] = t;
                    reused.push(*page);
                    continue;
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("reading the page-read cache: {e}"),
            }
            if reader.is_none() {
                reader = Some(VisionReader::open(state, alias).await?);
            }
            let Some(reader) = reader.as_ref() else {
                continue;
            };
            let png = match extract::pdf::render_page(bytes.clone(), *page).await {
                Ok(png) => png,
                Err(e) => {
                    failed.push((*page, e.to_string()));
                    continue;
                }
            };
            let prompt = vision_prompts::prompt(*mode, &pages[idx]);
            match reader.read_page(state, png, prompt).await {
                Ok(t) if !t.trim().is_empty() => {
                    if let Err(e) = super::store::put_page_read(
                        pool,
                        &sha,
                        i64::from(*page),
                        alias,
                        mode.as_str(),
                        version,
                        &t,
                    )
                    .await
                    {
                        tracing::warn!("keeping a page reading: {e}");
                    }
                    pages[idx] = t;
                }
                Ok(_) => failed.push((*page, "the vision model answered with no text".into())),
                Err(e) => failed.push((*page, e)),
            }
        }
        for (page, why) in failed {
            // A table page keeps its extracted text; a text-less one has none.
            if text.textless.contains(&page) {
                skipped.push(page);
            }
            notes.push(format!("page {page}: {alias} could not read it — {why}"));
        }
        let read: Vec<String> = wanted
            .iter()
            .filter(|(p, _)| !notes.iter().any(|n| n.starts_with(&format!("page {p}:"))))
            .map(|(p, m)| format!("{p} ({})", m.as_str()))
            .collect();
        if !read.is_empty() {
            notes.insert(
                0,
                format!(
                    "read by {alias} (vision prompt v{}): page {}",
                    vision_prompts::VISION_PROMPT_VERSION,
                    read.join(", ")
                ),
            );
        }
        if !reused.is_empty() {
            notes.push(format!(
                "{} of those page(s) reused the reading kept from an earlier ingest of these                  same bytes (per file, page, model and prompt version) — {alias} was not asked                  again: page {}",
                reused.len(),
                reused
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    skipped.sort_unstable();
    skipped.dedup();
    if !skipped.is_empty() {
        let list = skipped
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        notes.push(if alias.is_empty() {
            format!(
                "page {list}: no text, and this knowledge base has no vision model to read \
                 it — skipped. Set a vision model under the base's settings and re-ingest."
            )
        } else {
            format!("page {list}: no text, and the vision model could not read it — skipped")
        });
    }
    Ok(FileText {
        pages: Some(pages.len() as i64),
        skipped_pages: skipped.len() as i64,
        parts: pages
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !skipped.contains(&(*i as u32 + 1)))
            .map(|(i, body)| Part {
                page: Some(i as u32 + 1),
                heading: None,
                body,
            })
            .collect(),
        notes,
    })
}

/// The base's vision model, admitted once for a file's pages.
struct VisionReader {
    alias: String,
    route: Route,
    /// GPU admission for a local vision model, held across the file's pages
    /// and released before its chunks are embedded — the embedding batches
    /// admit the aux model on their own.
    hold: Option<crate::vram::LocalHold>,
}

impl VisionReader {
    /// Plain `resolve`, and the hold checked here: unattended batch work is
    /// refused under a GPU hold, never re-routed (gpu-hold design §2) — the
    /// file stays pending and Resume picks it up.
    async fn open(state: &SharedState, alias: &str) -> Result<Self, Stop> {
        let snap = state.snapshot();
        let mut route = snap.resolve(alias).map_err(|e| {
            Stop::Failed(format!("the vision model '{alias}' does not resolve: {e}"))
        })?;
        if let Some(target) = crate::vram::classify(&route) {
            if let Some(block) = snap.gpu_block() {
                return Err(Stop::Held(block.refusal(target.model_id, "").to_string()));
            }
        }
        let hold = crate::vram::admit(state, &route, alias)
            .await
            .map_err(|e| Stop::Failed(format!("admitting the vision model '{alias}': {e}")))?;
        if let Some(h) = &hold {
            h.point(&mut route);
        }
        Ok(Self {
            alias: alias.to_string(),
            route,
            hold,
        })
    }

    async fn read_page(
        &self,
        state: &SharedState,
        png: Vec<u8>,
        prompt: String,
    ) -> Result<String, String> {
        use base64::Engine;
        let ir = ChatRequest {
            model_alias: self.alias.clone(),
            messages: vec![Message {
                role: Role::User,
                content: vec![
                    ContentPart::Image {
                        mime: "image/png".into(),
                        source: ImageSource::Base64 {
                            data: base64::engine::general_purpose::STANDARD.encode(png),
                        },
                    },
                    ContentPart::text(prompt),
                ],
            }],
            params: Params::default(),
            tools: Vec::new(),
            tool_choice: None,
            stream: false,
            passthrough: Default::default(),
            llama_kwargs_enabled: None,
            anthropic_beta: Vec::new(),
        };
        let deadline =
            Duration::from_secs(state.snapshot().settings.responses_timeout_seconds.max(1));
        let done = crate::proxy::sample_once(
            state,
            self.hold.as_ref(),
            &self.route,
            None,
            &ir,
            crate::telemetry::INGEST_PROTO,
            None,
            deadline,
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(done
            .content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""))
    }
}
