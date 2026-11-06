//! How long the image server may take to answer a status read
//! (chat-media-tools design §5.4).
//!
//! A job's submit, its polls, its native cancel and an upscale's liveness
//! read ask sd-server about work it holds in memory; they answer at once
//! from a server that works. One that accepts the connection and then says
//! nothing has hung, and its pace cannot say so where it has none (a row
//! whose preview mode is off reports no steps, an upscale none at all). So
//! each such read is bounded by [`STATUS_ANSWER`] — lmgw's bound for a
//! control read of a container it runs (`vram::CONTROL_TIMEOUT`, the busy
//! probe's and the hold sweep's) — and one that does not answer within it
//! counts as a server that stopped working: the row's container is
//! restarted when the call is its only request, else the claim goes, with
//! a log line naming the bound. The work itself is never bounded by it: a
//! job's draw and an upscale's request run as long as they take.

use std::future::Future;
use std::time::Duration;

use crate::config::Route;
use crate::egress::apply_bearer_auth;
use crate::state::SharedState;

/// The bound on one status read (module doc).
pub(super) const STATUS_ANSWER: Duration = crate::vram::CONTROL_TIMEOUT;

/// A status read that did not answer within [`STATUS_ANSWER`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Unanswered;

impl std::fmt::Display for Unanswered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "did not answer a status read within {} s (the bound lmgw gives a control read of a \
             container it runs)",
            STATUS_ANSWER.as_secs()
        )
    }
}

/// `read`, given up after [`STATUS_ANSWER`].
pub(super) async fn within<F: Future>(read: F) -> Result<F::Output, Unanswered> {
    tokio::time::timeout(STATUS_ANSWER, read)
        .await
        .map_err(|_| Unanswered)
}

/// Whether the image server at `root` still answers while a synchronous
/// request runs on it (an upscale): its readiness route, the one the
/// registry judges a started container by. A refused connection or an
/// error status is an answer here — a container that died is the request's
/// own send to find and recover; only silence counts.
pub(super) async fn alive(
    state: &SharedState,
    route: &Route,
    root: &str,
) -> Result<(), Unanswered> {
    let url = format!("{root}{}", crate::runtime::descriptor::IMAGE_HEALTH_PATH);
    let rb = apply_bearer_auth(state.http.get(url), &route.upstream);
    within(async {
        if let Ok(resp) = rb.send().await {
            let _ = resp.bytes().await;
        }
    })
    .await
}
