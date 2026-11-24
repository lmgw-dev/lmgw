//! One call's way to a chosen model, shared by a page's read
//! ([`super::page`]) and a picture's description (`super::super::describe`):
//! the key's policy, the route, the call's turn in the model-call pool, the
//! image made, the call.
//!
//! **The order is the cost.** Nothing is opened for a key that may not use
//! the model, nothing is made (a page rendered, a picture read) until a
//! call can take it, and nothing is sent to a model that cannot see it:
//! 1. the key's policy ([`call::allowed`]): before a route is opened, so an
//!    over-budget or disabled key never cold-loads the model;
//! 2. the route ([`Session::current`](super::session::Session::current)),
//!    which says who answers, refuses a fallback that takes no pictures and
//!    judges the route's price for the key;
//! 3. the call's turn among the calls in flight;
//! 4. the key's policy again: a budget or a tokens-per-minute window may
//!    have filled while the call waited;
//! 5. the image, made by the caller's `make` (a page's render in a tool
//!    slot of its own; a picture's bytes, already made);
//! 6. the call ([`call::ask`]).
//!
//! **The file's clock** (`pool::clock`): opening the route and waiting for
//! the turn pause the caller's lane; making the image and the call in
//! flight count. The caller ends the call at the file's deadline by
//! dropping this future (a call in flight then leaves its `499` row).

use std::future::Future;
use std::sync::Arc;

use lmgw_api_types::documents::model_help::BoundFrom;

use super::call::{self, Asked, What};
use super::session::{Keep, Open, Stop};
use super::Chosen;
use crate::extract::convert::pool::clock;

/// Who answered a call, and under which bound.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct By {
    pub alias: String,
    pub answered_for: Option<String>,
    pub bound: u32,
    /// [`BoundFrom`], ordered.
    pub from: u8,
}

impl By {
    pub(crate) fn of(chosen: &Chosen, open: &Open, handed_to: Option<String>) -> Self {
        let fallback = open.fallback || handed_to.is_some();
        Self {
            alias: handed_to.unwrap_or_else(|| open.answered_by.clone()),
            answered_for: fallback.then(|| chosen.alias.clone()),
            bound: open.bound,
            from: match open.bound_from {
                BoundFrom::RowSlots => 0,
                // A page's or a picture's call is never the one call of a
                // transcription.
                BoundFrom::ToolPool | BoundFrom::OneCall => 1,
            },
        }
    }

    pub(crate) fn bound_from(&self) -> BoundFrom {
        match self.from {
            0 => BoundFrom::RowSlots,
            _ => BoundFrom::ToolPool,
        }
    }
}

/// Why a call was not made.
pub(crate) enum Stopped {
    /// The model is paused and nothing answers for it.
    Held(String),
    /// The fallback that answers for the model takes no pictures: its name.
    Blind(String),
    NotAllowed(String),
    /// The model cannot be opened.
    Failed(String),
    /// The image could not be made (the caller's words): a tool's failure,
    /// not the model's.
    Unmade(String),
}

/// A call that was made: how it ended, and on which route.
pub(crate) struct Answered {
    pub asked: Asked,
    pub open: Arc<Open>,
}

/// Take a turn at `chosen`'s model and ask it `what` about the image `make`
/// makes once the turn has come (an image and its type, or why there is
/// none). `sent` is told, with the route, when the call goes out.
pub(crate) async fn take<F, Fut>(
    chosen: &Chosen,
    keep: &Keep,
    make: F,
    what: What<'_>,
    sent: impl FnOnce(&Open),
) -> Result<Answered, Stopped>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<(Vec<u8>, &'static str), String>>,
{
    let (open, turn) = loop {
        if let Err(e) = call::allowed(chosen).await {
            return Err(Stopped::NotAllowed(e.to_string()));
        }
        // Opening the route (a local model's start) and the turn are
        // waits: the lane's clock stands while they last.
        let waiting = clock::paused();
        let open = match chosen.session.current(chosen, keep).await {
            Ok(o) => o,
            Err(Stop::Held(why)) => return Err(Stopped::Held(why)),
            Err(Stop::Blind(fallback)) => return Err(Stopped::Blind(fallback)),
            Err(Stop::NotAllowed(why)) => return Err(Stopped::NotAllowed(why)),
            Err(Stop::Failed(why)) => return Err(Stopped::Failed(why)),
        };
        // The call's turn: nothing of it exists until a call can take it.
        let Ok(turn) = open.calls.clone().acquire_owned().await else {
            return Err(Stopped::Failed(
                "the model-call pool was closed".to_string(),
            ));
        };
        if !chosen.session.is_current(&open, chosen).await {
            // The hold changed while it waited: take the path of now.
            continue;
        }
        drop(waiting);
        break (open, turn);
    };
    if let Err(e) = call::allowed(chosen).await {
        return Err(Stopped::NotAllowed(e.to_string()));
    }
    let (image, mime) = make().await.map_err(Stopped::Unmade)?;
    let asked = Box::pin(call::ask(chosen, &open, image, mime, what, || sent(&open))).await;
    drop(turn);
    Ok(Answered { asked, open })
}
