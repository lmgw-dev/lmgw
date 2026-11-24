//! One page of the model stage (design §3 tier 1, §7.5, §7.7): the key's
//! policy, the route, the page's turn in the model-call pool, its image,
//! its call, its answer as blocks.
//!
//! **The order is the cost** ([`turn::take`], shared with picture
//! descriptions): the key's policy, the route, the page's turn among the
//! calls in flight, the key's policy again (a refusal there leaves the
//! remaining pages `model_not_allowed`), the render, bounded ([`render`]),
//! in a tool slot of its own, the call, and the answer read in the form its
//! answering model was asked in. Nothing is rendered until a call can take
//! it.
//!
//! **The file's clock** (`pool::clock`, docling's `document_timeout`): a
//! page is a lane of its own. Opening the route and waiting for its turn
//! pause the lane, as waiting for the render's slot does; the render and
//! the call in flight count. The deadline ends a page wherever it stands:
//! a call in flight is dropped and leaves its `499` row.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::answer;
use super::call::{Asked, PageAsk, What};
use super::render;
use super::select::Ask;
use super::session::{Keep, Open};
use super::turn::{self, Answered, Stopped};
use super::Chosen;
use crate::extract::convert::doc::Block;
use crate::extract::convert::ocr::spool::Shared;
use crate::extract::convert::pool::clock::FileClock;

pub(super) use super::turn::By;

/// How one page ended.
pub(super) enum Outcome {
    Read {
        blocks: Vec<Block>,
        note: Option<String>,
    },
    /// The model is paused and nothing answers for it.
    Held(String),
    /// The fallback that answers for the model takes no pictures: its name.
    Blind(String),
    NotAllowed(String),
    /// The model's call failed, or its answer was no page.
    Failed(String),
    /// The page could not be rendered for the model: a tool's failure, not
    /// the model's.
    Unrendered(String),
    /// The file's deadline passed before the page had its answer.
    Late(String),
}

pub(super) struct PageDone {
    pub outcome: Outcome,
    /// The call that was sent for it, if one was (one dropped at the
    /// deadline included).
    pub sent: Option<By>,
    /// What the page's `page_scaled` note says, when its image was sent
    /// scaled down.
    pub scaled: Option<String>,
}

/// What a page has done so far, kept where the deadline cannot drop it.
#[derive(Default)]
struct Progress {
    sent: Option<By>,
    scaled: Option<String>,
}

/// One page to read.
pub(super) struct Page<'a> {
    pub chosen: &'a Chosen,
    /// The file stage's hold on the route (it lives as long as the stage).
    pub keep: &'a Keep,
    /// The file, spooled once for every page's render.
    pub spool: &'a Shared,
    /// `pdftoppm`, when the probe found it.
    pub bin: Option<&'a PathBuf>,
    pub number: u32,
    pub ask: Ask,
    /// The page's text as the tools read it, for [`Ask::ImageAndText`].
    pub text: String,
    /// Width and height in points.
    pub size: Option<(f32, f32)>,
}

/// Why a page the deadline ended has no answer.
pub(super) fn late(limit: std::time::Duration) -> String {
    format!(
        "the caller's document_timeout of {} s passed before the model answered",
        limit.as_secs_f64()
    )
}

/// Read `page` with the model, at most until the file's `clock` has used
/// its limit.
pub(super) async fn read(page: Page<'_>, clock: Option<Arc<FileClock>>) -> PageDone {
    let progress = Mutex::new(Progress::default());
    let outcome = match clock {
        None => one(&page, &progress).await,
        Some(c) => {
            tokio::select! {
                outcome = one(&page, &progress) => outcome,
                () = c.expiry() => Outcome::Late(late(c.limit())),
            }
        }
    };
    let Progress { sent, scaled } = progress.into_inner().unwrap_or_else(|e| e.into_inner());
    PageDone {
        outcome,
        // A page that was never sent was not sent scaled either.
        scaled: scaled.filter(|_| sent.is_some()),
        sent,
    }
}

fn note(progress: &Mutex<Progress>, f: impl FnOnce(&mut Progress)) {
    f(&mut progress.lock().unwrap_or_else(|e| e.into_inner()));
}

async fn one(p: &Page<'_>, progress: &Mutex<Progress>) -> Outcome {
    let chosen = p.chosen;
    // The page's image, made once its turn has come.
    let image = || async {
        let Some(bin) = p.bin else {
            return Err(
                "pdftoppm (part of poppler-utils) was not found, so the page could not \
                        be rendered for the model"
                    .to_string(),
            );
        };
        let scale = render::scale(p.size);
        let spool = p.spool.get().await;
        let png = render::page(bin, &spool, p.number, scale)
            .await
            .map_err(|e| format!("the page could not be rendered for the model: {e}"))?;
        note(progress, |g| g.scaled = scale.said());
        Ok((png, "image/png"))
    };
    let page = PageAsk {
        ask: p.ask,
        text: &p.text,
    };
    let sent = |open: &Open| note(progress, |g| g.sent = Some(By::of(chosen, open, None)));
    let Answered { asked, open } =
        match turn::take(chosen, p.keep, image, What::Page(page), sent).await {
            Ok(a) => a,
            Err(Stopped::Held(why)) => return Outcome::Held(why),
            Err(Stopped::Blind(fallback)) => return Outcome::Blind(fallback),
            Err(Stopped::NotAllowed(why)) => return Outcome::NotAllowed(why),
            Err(Stopped::Failed(why)) => return Outcome::Failed(why),
            Err(Stopped::Unmade(why)) => return Outcome::Unrendered(why),
        };
    match asked {
        Asked::Text {
            text,
            handed_to,
            doctags,
        } => {
            note(progress, |g| {
                g.sent = Some(By::of(chosen, &open, handed_to))
            });
            let read = answer::read(&text, doctags, p.size);
            if read.has_text() {
                Outcome::Read {
                    blocks: read.blocks,
                    note: read.note,
                }
            } else if read.blocks.is_empty() {
                Outcome::Failed("the model's answer held no text".to_string())
            } else {
                Outcome::Failed("the model's answer held no text, only picture regions".to_string())
            }
        }
        Asked::Held(why) => Outcome::Held(why),
        Asked::Blind(fallback) => Outcome::Blind(fallback),
        Asked::NotAllowed(why) => Outcome::NotAllowed(why),
        Asked::Failed(why) => Outcome::Failed(why),
    }
}
