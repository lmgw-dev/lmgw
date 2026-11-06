//! One request's retries of a refused off (module doc of [`super`]): what
//! each refusal of the off becomes, the refused attempt's note, and the
//! lesson once a send answers. Shared by the chat sends ([`super::send_chat`])
//! and the native `/v1/responses` passthrough
//! (`responses/native/reasoning_off.rs`); each writes the form into what it
//! sends, and the refused attempt's row, in its own way.
//!
//! A lesson is kept for the route only when every refusal this request
//! retried was about the control alone
//! ([`refusal::not_about_the_control_alone`]): one that depends on what else
//! the body carried (a hosted tool, a key beside the control), or whose
//! words do not name the control while the request carried such a thing
//! (function tools, a tool choice and a response format do not count:
//! [`refusal::Carried`]), is still retried for this request, but
//! the form that answered is not what the route gets from now on. The
//! refused attempt's note and the INFO line say so, and why.

use crate::config::Route;
use crate::error::GatewayError;
use crate::state::SharedState;

use super::refusal::{self, Carried, Retry};
use super::{Fitted, Lesson, Off, Spelling};

/// The state of one request's off across its sends.
pub(crate) struct OffRetry {
    /// What a lesson learned here says was refused: the form this route's
    /// decision gives without one — the one sent first, or, when that came
    /// from an earlier lesson, what that lesson said was refused.
    first: Option<Off>,
    /// Whether the one retry with what a refusal names is still left.
    guided_left: bool,
    /// Whether a refusal of the off was retried on this request.
    retried: bool,
    /// Why what answered is not kept for the route: the first retried
    /// refusal that was not about the control alone.
    not_kept: Option<String>,
    spelling: Spelling,
}

/// One retry of a refused off: the form the next send carries `instead`,
/// and the refused attempt's row `note` (the provider's words, and what lmgw
/// did about them).
pub(crate) struct Retried {
    pub(crate) instead: Off,
    pub(crate) note: String,
}

impl OffRetry {
    /// The retries of a request whose off is not fitted yet: none.
    pub(crate) fn none() -> Self {
        Self {
            first: None,
            guided_left: false,
            retried: false,
            not_kept: None,
            spelling: Spelling::Responses,
        }
    }

    /// The retries of a request whose off went through [`super::fit_off`]
    /// as `fitted`, spelled for the body `spelling` names.
    pub(crate) fn open(
        state: &SharedState,
        route: &Route,
        fitted: &Fitted,
        spelling: Spelling,
    ) -> Self {
        let first = match fitted.learned() {
            true => state.reasoning_learned.get(route).map(|l| l.refused),
            false => fitted.retryable().cloned(),
        };
        Self {
            first,
            guided_left: true,
            retried: false,
            not_kept: None,
            spelling,
        }
    }

    /// A send answered (not an error): after a retry, the form that answered
    /// is what `route` gets from now on ([`super::Learned`]), an INFO line
    /// saying so — unless a refusal it retried was not about the control
    /// alone (module doc), which the INFO line says instead.
    pub(crate) fn answered(&self, state: &SharedState, route: &Route, fitted: &Fitted) {
        if !self.retried {
            return;
        }
        let (Some(refused), Some((instead, fallback))) = (self.first.clone(), fitted.sent()) else {
            return;
        };
        let now = instead.describe_in(route.upstream.protocol, self.spelling);
        if let Some(why) = &self.not_kept {
            tracing::info!(
                upstream = %route.upstream.name,
                model = %route.upstream_model,
                "reasoning off went out as {now} on this request; not kept for this model, \
                 since a refusal it retried was not about the control alone: {why}"
            );
            return;
        }
        state.reasoning_learned.remember(
            route,
            Lesson {
                refused,
                instead,
                fallback,
            },
        );
        tracing::info!(
            upstream = %route.upstream.name,
            model = %route.upstream_model,
            "reasoning off on this model goes out as {now} from now on"
        );
    }

    /// The upstream refused a send (a `400` or `422`) whose off went out as
    /// `fitted` says, read as `refused`: the form to send once more, and the
    /// refused attempt's note, or `None` when the refusal stands — no off
    /// went out to retry (none was asked, a local route's, or no control
    /// already), the refusal is about something else lmgw can tell (a
    /// classified error of its own, a prompt too long for the context), or
    /// nothing of the off is left to drop. `fitted` then says what the next
    /// send carries. `carried`: what the request carried besides the control
    /// that a refusal could be about (a hosted tool's type, a top-level
    /// key), for whether a lesson is kept (module doc).
    pub(crate) fn after(
        &mut self,
        route: &Route,
        fitted: &mut Fitted,
        refused: &GatewayError,
        carried: &Carried,
    ) -> Option<Retried> {
        let protocol = route.upstream.protocol;
        let off = fitted.retryable()?.clone();
        let message = refused.to_string();
        // A refusal lmgw can tell is about something else — one it maps to
        // an error of its own (llama-server's context refusal, a classified
        // structured-output refusal), or a prompt too long for the context —
        // stands as it came: without the off it would be refused again, the
        // prompt sent twice for nothing.
        let elsewhere = !matches!(
            refused,
            GatewayError::Upstream { .. } | GatewayError::UpstreamRelayed(_)
        ) || refusal::about_the_context(&message);
        if elsewhere {
            return None;
        }
        let (instead, how) =
            refusal::next_after_refusal(protocol, &off, &message, self.guided_left)?;
        let was = off.describe_in(protocol, self.spelling);
        let now = instead.describe_in(protocol, self.spelling);
        let alone = refusal::not_about_the_control_alone(protocol, refused, carried);
        if self.not_kept.is_none() {
            self.not_kept = alone;
        }
        let kept = match &self.not_kept {
            None => "keeps that for this model when it answers".to_string(),
            Some(why) => format!(
                "keeps nothing for this model, since a refusal it retried was not about the \
                 control alone: {why}"
            ),
        };
        let why = match how {
            Retry::Guided => "retrying once with what it says the model takes".to_string(),
            Retry::Fallback => "retrying with no reasoning control, so the model reasons as it \
                 does by default — the last resort"
                .to_string(),
        };
        tracing::info!(
            upstream = %route.upstream.name,
            model = %route.upstream_model,
            "reasoning off: the provider refused {was} ({message}); {why}: {now}"
        );
        let note = match how {
            Retry::Guided => format!(
                "{message} — reasoning off was sent as {was}; lmgw retried once with {now}, and \
                 {kept}"
            ),
            Retry::Fallback => format!(
                "{message} — reasoning off was sent as {was}; lmgw retried with {now} (the model \
                 then reasons as it does by default), and {kept}"
            ),
        };
        fitted.retried(instead.clone(), how);
        self.guided_left = false;
        self.retried = true;
        Some(Retried { instead, note })
    }
}
