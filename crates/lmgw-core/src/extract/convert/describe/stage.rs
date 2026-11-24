//! A file's pictures described (design §5.3, §7.5, §7.7): each picture
//! with bytes a model takes ([`super::sendable`]) and at least
//! `describe_min_area` of its page asks for its turn in the request's
//! model-call pool at once, in reading order, as a lane of the file's
//! clock, and is sent through the page-reading model's machinery
//! ([`turn::take`]). A picture holds only its bytes (shared) and its bounded
//! context; its message is made when its call has its turn. The answer,
//! without a model's control tokens, is the picture's description; every
//! picture that is not described is named in a note with why, except one
//! below `describe_min_area`, which is counted in the stats.

use std::collections::BTreeMap;
use std::sync::Arc;

use lmgw_api_types::documents::convert::{ConvertWarning, WarningCode};
use lmgw_api_types::documents::model_help::ModelCalls;

use super::prompt::{context, message};
use super::sendable::{check, NotSent};
use super::{Describe, PICTURE_CONTEXT_MAX_BYTES};
use crate::erased::Erased;
use crate::extract::convert::model_help::answer::without_control;
use crate::extract::convert::model_help::call::{Asked, What};
use crate::extract::convert::model_help::session::{Keep, Open};
use crate::extract::convert::model_help::turn::{self, Answered, By, Stopped};
use crate::extract::convert::model_help::Chosen;
use crate::extract::convert::pictures::{self, Found};
use crate::extract::convert::pool::clock::{self, FileClock};

/// What describing a file's pictures did.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Described {
    pub warnings: Vec<ConvertWarning>,
    /// The models that answered, for the file's `stats.models`.
    pub models: Vec<ModelCalls>,
    /// Calls sent, the failed ones included.
    pub calls: u32,
    /// A picture asked for was not described for the model's reasons (held,
    /// refused, failed, late): the file is `partial`.
    pub partial: bool,
    /// Pictures not described for being smaller than `describe_min_area`.
    pub below_min_area: u32,
}

/// How one picture's call ended.
enum Outcome {
    Described(String),
    Held(String),
    Blind(String),
    NotAllowed(String),
    Failed(String),
    Late(String),
}

struct One {
    outcome: Outcome,
    /// The call that was sent for it, if one was.
    sent: Option<By>,
}

/// One picture to describe: its index, its bytes (shared with the
/// picture) and their type, its context (bounded, [`context`]).
struct Ask {
    index: usize,
    bytes: bytes::Bytes,
    mime: &'static str,
    context: String,
}

/// Describe `found`'s pictures with `d`'s model, in place.
///
/// Erased ([`crate::erased`]): a file's whole description stage behind one
/// future, as the page-reading model's stage is.
pub fn run<'a>(d: &'a Describe, found: &'a mut Found) -> Erased<'a, Described> {
    Box::pin(stage(d, found))
}

/// `pictures p1, p3` (with their parts), as a note names them.
fn named(found: &Found, indexes: &[usize]) -> (String, Vec<u32>) {
    let ids: Vec<String> = indexes
        .iter()
        .map(|i| pictures::id(u32::try_from(*i).unwrap_or(u32::MAX)))
        .collect();
    let mut parts: Vec<u32> = indexes
        .iter()
        .filter_map(|i| found.pictures.get(*i).map(|p| p.part))
        .collect();
    parts.sort_unstable();
    parts.dedup();
    let said = if ids.len() == 1 {
        format!("picture {}", ids[0])
    } else {
        format!("pictures {}", ids.join(", "))
    };
    (said, parts)
}

/// `picture p1 was not described` or `pictures p1, p3 were not described`,
/// with their parts.
fn not_described(found: &Found, indexes: &[usize]) -> (String, Vec<u32>) {
    let (said, parts) = named(found, indexes);
    let verb = if indexes.len() == 1 { "was" } else { "were" };
    (format!("{said} {verb} not described"), parts)
}

fn warning(code: WarningCode, message: String, parts: Vec<u32>) -> ConvertWarning {
    ConvertWarning {
        code,
        message,
        tool: None,
        parts,
        option: Some("describe_pictures".to_string()),
    }
}

/// What a picture that was not described keeps.
const KEPT: &str = "kept without a description";

/// The caller's document_timeout, in a note's words.
fn late(c: &FileClock) -> String {
    format!(
        "the caller's document_timeout of {} s passed before the model answered",
        c.limit().as_secs_f64()
    )
}

async fn stage(d: &Describe, found: &mut Found) -> Described {
    let chosen = &*d.chosen;
    let mut out = Described::default();
    let mut asks: Vec<Ask> = Vec::new();
    let mut not_sent: BTreeMap<NotSent, Vec<usize>> = BTreeMap::new();
    let mut no_bytes: Vec<usize> = Vec::new();
    let mut cut: Vec<usize> = Vec::new();
    for (index, p) in found.pictures.iter().enumerate() {
        // Below describe_min_area: kept, not described, counted (docling's
        // picture_description_area_threshold does the same).
        if p.share.is_some_and(|a| a < d.min_area) {
            out.below_min_area += 1;
            continue;
        }
        let Some(img) = &p.image else {
            no_bytes.push(index);
            continue;
        };
        match check(img) {
            Ok(mime) => {
                let (context, was_cut) = context(p.caption.as_deref(), p.before.as_deref());
                if was_cut {
                    cut.push(index);
                }
                asks.push(Ask {
                    index,
                    bytes: img.bytes.clone(),
                    mime,
                    context,
                });
            }
            Err(why) => not_sent.entry(why).or_default().push(index),
        }
    }
    for (why, indexes) in &not_sent {
        let (said, parts) = not_described(found, indexes);
        out.warnings.push(warning(
            WarningCode::NotYet,
            format!("{said}: {}; {KEPT}", why.said()),
            parts,
        ));
    }
    if !no_bytes.is_empty() {
        let (said, parts) = not_described(found, &no_bytes);
        out.warnings.push(warning(
            WarningCode::PicturesPartial,
            format!(
                "{said}: the picture was not exported (the pictures_partial note \
                 says why), so there was nothing to send; {KEPT}"
            ),
            parts,
        ));
    }
    if asks.is_empty() {
        return out;
    }
    let indexes: Vec<usize> = asks.iter().map(|a| a.index).collect();
    // The key's scope, before anything is opened for it (D6): a key that
    // may not use the model never loads it.
    let snap = chosen.state.snapshot();
    let key = chosen.ctx.current_key_name(&snap);
    if let Err(e) = crate::policy::check_scope(&snap, key.as_deref(), &chosen.alias) {
        let (said, parts) = not_described(found, &indexes);
        out.partial = true;
        out.warnings.push(warning(
            WarningCode::ModelNotAllowed,
            format!(
                "{said}: the picture-description model is one this key may not \
                 use ({e}); {KEPT}"
            ),
            parts,
        ));
        return out;
    }
    drop(snap);
    if !cut.is_empty() {
        let (said, parts) = named(found, &cut);
        out.warnings.push(warning(
            WarningCode::PictureContextCut,
            format!(
                "{said}: the caption or the paragraph before the picture was longer than \
                 PICTURE_CONTEXT_MAX_BYTES ({}) and was sent cut",
                crate::hf::fmt_bytes(PICTURE_CONTEXT_MAX_BYTES as u64)
            ),
            parts,
        ));
    }
    let clock = clock::current();
    // The file's deadline passed already (a page stage before this one
    // kept the file going past it): nothing is opened or sent.
    if let Some(c) = clock.as_ref().filter(|c| c.expired()) {
        let (said, parts) = named(found, &indexes);
        out.partial = true;
        out.warnings.push(warning(
            WarningCode::PartFailed,
            format!(
                "the picture-description model did not describe {said}: the caller's \
                 document_timeout of {} s had passed before their turn; {KEPT}",
                c.limit().as_secs_f64()
            ),
            parts,
        ));
        return out;
    }
    // From here on the stage ends the file's work at its deadline and keeps
    // what was answered.
    if let Some(c) = &clock {
        c.hand_over();
    }
    let keep = Keep::default();
    let all = clock::paused();
    // The caller's prompt against the answering model's context, once per
    // route the request opens: one that leaves no room sends nothing.
    if let Some(why) = d.room.refused(d, &keep).await {
        drop(all);
        let (said, parts) = named(found, &indexes);
        out.partial = true;
        out.warnings.push(ConvertWarning {
            option: Some("picture_prompt".to_string()),
            ..warning(
                WarningCode::PartFailed,
                format!("the picture-description model was not sent {said}: {why}; {KEPT}"),
                parts,
            )
        });
        return out;
    }
    let done = futures::future::join_all(
        asks.iter()
            .map(|a| Box::pin(clock::beside(one(d, &keep, a, clock.clone())))),
    )
    .await;
    drop(all);
    drop(keep);

    let mut by: BTreeMap<By, (u32, u32)> = BTreeMap::new();
    let mut held: (Vec<usize>, String) = Default::default();
    let mut refused: (Vec<usize>, String) = Default::default();
    let mut blind: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut failed: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (a, one) in asks.iter().zip(done) {
        let described = matches!(one.outcome, Outcome::Described(_));
        out.partial |= !described;
        if let Some(b) = one.sent {
            let e = by.entry(b).or_default();
            e.0 += 1;
            e.1 += u32::from(described);
            out.calls += 1;
        }
        match one.outcome {
            Outcome::Described(text) => found.pictures[a.index].description = Some(text),
            Outcome::Held(why) => {
                held.0.push(a.index);
                held.1 = why;
            }
            Outcome::NotAllowed(why) => {
                refused.0.push(a.index);
                refused.1 = why;
            }
            Outcome::Blind(fallback) => blind.entry(fallback).or_default().push(a.index),
            Outcome::Failed(why) | Outcome::Late(why) => {
                failed.entry(why).or_default().push(a.index)
            }
        }
    }
    if !held.0.is_empty() {
        let (said, parts) = not_described(found, &held.0);
        out.warnings.push(warning(
            WarningCode::ModelHeld,
            format!(
                "{said}: the picture-description model is paused ({}); {KEPT}",
                held.1
            ),
            parts,
        ));
    }
    for (fallback, indexes) in blind {
        let (said, parts) = not_described(found, &indexes);
        out.warnings.push(warning(
            WarningCode::ModelCannotSee,
            format!(
                "{said}: the picture-description model cannot run now; its \
                 fallback '{fallback}' answers in its place and takes no pictures, so nothing was \
                 sent to it; {KEPT}"
            ),
            parts,
        ));
    }
    if !refused.0.is_empty() {
        let (said, parts) = not_described(found, &refused.0);
        out.warnings.push(warning(
            WarningCode::ModelNotAllowed,
            format!(
                "{said}: the picture-description model is one this key may not \
                 use now ({}); {KEPT}",
                refused.1
            ),
            parts,
        ));
    }
    for (why, indexes) in failed {
        let (said, parts) = named(found, &indexes);
        out.warnings.push(warning(
            WarningCode::PartFailed,
            format!("the picture-description model did not describe {said}: {why}; {KEPT}"),
            parts,
        ));
    }
    out.models = by
        .into_iter()
        .map(|(b, (calls, described))| ModelCalls {
            bound_from: b.bound_from(),
            alias: b.alias,
            answered_for: b.answered_for,
            calls,
            parts_read: 0,
            pictures_described: described,
            files_transcribed: 0,
            in_flight_bound: b.bound,
        })
        .collect();
    out
}

/// Describe one picture, at most until the file's `clock` has used its
/// limit (checked first: a lane on an expired clock opens nothing).
async fn one(d: &Describe, keep: &Keep, a: &Ask, clock: Option<Arc<FileClock>>) -> One {
    let sent = std::sync::Mutex::new(None);
    let outcome = match clock {
        None => ask(d, keep, a, &sent).await,
        Some(c) => {
            tokio::select! {
                biased;
                () = c.expiry() => Outcome::Late(late(&c)),
                outcome = ask(d, keep, a, &sent) => outcome,
            }
        }
    };
    let sent = sent.into_inner().unwrap_or_else(|e| e.into_inner());
    One { outcome, sent }
}

async fn ask(d: &Describe, keep: &Keep, a: &Ask, sent: &std::sync::Mutex<Option<By>>) -> Outcome {
    let chosen: &Chosen = &d.chosen;
    let set = |b: By| *sent.lock().unwrap_or_else(|e| e.into_inner()) = Some(b);
    let image = || async { Ok((a.bytes.to_vec(), a.mime)) };
    // The message is made when the call is (`call::ask`), not before.
    let make = || message(&d.prompt, &a.context);
    let what = What::Picture(&make);
    let on = |open: &Open| set(By::of(chosen, open, None));
    let Answered { asked, open } = match turn::take(chosen, keep, image, what, on).await {
        Ok(answered) => answered,
        Err(Stopped::Held(why)) => return Outcome::Held(why),
        Err(Stopped::Blind(fallback)) => return Outcome::Blind(fallback),
        Err(Stopped::NotAllowed(why)) => return Outcome::NotAllowed(why),
        Err(Stopped::Failed(why) | Stopped::Unmade(why)) => return Outcome::Failed(why),
    };
    match asked {
        Asked::Text {
            text, handed_to, ..
        } => {
            set(By::of(chosen, &open, handed_to));
            let text = without_control(&text).trim().to_string();
            if text.is_empty() {
                Outcome::Failed("the model answered with no text".to_string())
            } else {
                Outcome::Described(text)
            }
        }
        Asked::Held(why) => Outcome::Held(why),
        Asked::Blind(fallback) => Outcome::Blind(fallback),
        Asked::NotAllowed(why) => Outcome::NotAllowed(why),
        Asked::Failed(why) => Outcome::Failed(why),
    }
}
