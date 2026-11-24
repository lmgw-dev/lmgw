//! An e-mail's attachments and the message's clock (design §7.7,
//! `pool::clock`). An attachment's model stage (its pictures described, its
//! pages read by the page-reading model) takes the message's end over
//! (`FileClock::hand_over`), so the batch no longer ends the message at
//! `document_timeout` (`batch::within_deadline`). From then on the message
//! ends its attachments itself: one not started by the deadline is not
//! read, and one being read when it passes ends there; each is named in the
//! message's warnings, and what was read before is kept.

use std::future::Future;

use lmgw_api_types::documents::convert::{ConvertWarning, WarningCode};
use lmgw_api_types::documents::model_help::ModelCalls;

use super::super::super::pool::clock;
use super::super::super::sent;

/// Whether a stage took the message's end over and its clock has used its
/// limit: the attachments left are not read.
pub(super) fn expired() -> bool {
    clock::current().is_some_and(|c| c.handed_over() && c.expired())
}

/// `read` (one attachment's), ended at the deadline when a stage took the
/// message's end over before it started (`Err`: it ended there, with the
/// model calls it had sent, `convert::sent`, which the message's `stats`
/// keep: they were paid for). One that started before then ends as its
/// own stages do (they end their calls at the deadline and keep what was
/// answered).
pub(super) async fn read<F: Future>(read: F) -> Result<F::Output, (u32, Vec<ModelCalls>)> {
    let ledger = sent::Ledger::new();
    let read = sent::scope(ledger.clone(), read);
    let Some(c) = clock::current().filter(|c| c.handed_over()) else {
        return Ok(read.await);
    };
    tokio::pin!(read);
    tokio::select! {
        biased;
        () = c.expiry() => Err(ledger.take()),
        out = &mut read => Ok(out),
    }
}

/// The note naming the attachments the deadline ended or left unread.
pub(super) fn note(late: &[String]) -> Option<ConvertWarning> {
    if late.is_empty() {
        return None;
    }
    let limit = clock::current().map_or(0.0, |c| c.limit().as_secs_f64());
    Some(ConvertWarning {
        code: WarningCode::PartFailed,
        message: format!(
            "{} attachment(s) were not read, or not to their end, because the caller's \
             document_timeout of {limit} s passed: {}",
            late.len(),
            late.join(", ")
        ),
        tool: None,
        parts: Vec::new(),
        option: Some("document_timeout".to_string()),
    })
}
