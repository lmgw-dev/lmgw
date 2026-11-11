//! The drain before a declared oversize body's 413 on Windows.
//!
//! Windows resets a connection that is closed with bytes still unread (Linux
//! closes it), and the reset throws the 413 away before the client reads it:
//! the client sees "connection aborted" instead of the refusal. So there the
//! declared body is read and dropped before the answer goes out, bounded as
//! nginx's lingering close is: a time budget for the whole drain and for a
//! pause in it, and a byte budget derived from `max_body_mb`. A cut drain is
//! logged with the budget that cut it, since its client may then see the
//! reset instead of the refusal. On Linux nothing here runs: the body is
//! never read.
//!
//! Compiled for the tests on every system, called on Windows only.

use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, Version};
use http_body_util::BodyExt;

/// nginx's `lingering_time` default: the whole drain.
const TOTAL: Duration = Duration::from_secs(30);
/// nginx's `lingering_timeout` default: a pause in it.
const IDLE: Duration = Duration::from_secs(5);

/// What one drain may cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Budget {
    /// The largest declared body drained: twice `max_body_mb`, so the drain
    /// reads at most one more limit's worth past what the limit admits. A
    /// near miss (the case a client acts on by trimming) gets its 413; a
    /// bigger body is answered at once.
    pub bytes: u64,
    pub total: Duration,
    pub idle: Duration,
}

impl Budget {
    pub fn of(max_bytes: u64) -> Self {
        Self {
            bytes: max_bytes.saturating_mul(2),
            total: TOTAL,
            idle: IDLE,
        }
    }
}

/// Whether the refused body is read before the 413.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Plan {
    /// Read it to its end within the budget.
    Drain,
    /// `Expect: 100-continue`: the client waits for an answer before sending
    /// the body, and reading it would make hyper ask for it (`100
    /// Continue`). Answered at once, nothing is unread.
    Expecting,
    /// HTTP/2 refuses a stream without a connection reset; nothing to read.
    NotHttp1,
    /// Declared over [`Budget::bytes`].
    OverBudget,
}

pub(super) fn plan(req: &Request, declared: u64, budget: &Budget) -> Plan {
    if !matches!(req.version(), Version::HTTP_10 | Version::HTTP_11) {
        return Plan::NotHttp1;
    }
    let expecting = req
        .headers()
        .get(header::EXPECT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("100-continue"));
    if expecting {
        return Plan::Expecting;
    }
    if declared > budget.bytes {
        return Plan::OverBudget;
    }
    Plan::Drain
}

/// How a drain ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum End {
    /// The body's end, or the client's error (nothing more will come).
    Read,
    /// [`Budget::total`] ran out.
    Total,
    /// No bytes for [`Budget::idle`].
    Idle,
}

/// Read `body` and drop it, within `budget`'s times.
pub(super) async fn drain(mut body: Body, budget: &Budget) -> End {
    let deadline = tokio::time::Instant::now() + budget.total;
    loop {
        let next = tokio::time::timeout(budget.idle, body.frame());
        match tokio::time::timeout_at(deadline, next).await {
            Err(_) => return End::Total,
            Ok(Err(_)) => return End::Idle,
            Ok(Ok(Some(Ok(_)))) => {}
            Ok(Ok(None | Some(Err(_)))) => return End::Read,
        }
    }
}

/// The drain before a declared `declared`-byte body's refusal under
/// `max_mb`: [`plan`], then [`drain`], each cut logged with its budget.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) async fn before_refusal(req: Request, declared: u64, max_mb: u32) {
    let budget = Budget::of(u64::from(max_mb) * 1024 * 1024);
    match plan(&req, declared, &budget) {
        Plan::Drain => {}
        Plan::Expecting | Plan::NotHttp1 => return,
        Plan::OverBudget => {
            tracing::warn!(
                "a {declared}-byte body over max_body_mb ({max_mb} MiB) is refused without \
                 reading it: the drain before a refusal reads at most twice max_body_mb \
                 ({} bytes), so its client may see the connection reset instead of the 413",
                budget.bytes
            );
            return;
        }
    }
    let note = |what: &str| {
        tracing::warn!(
            "the drain of a {declared}-byte body over max_body_mb ({max_mb} MiB) stopped: {what}, \
             so its client may see the connection reset instead of the 413"
        );
    };
    match drain(req.into_body(), &budget).await {
        End::Read => {}
        End::Total => note(&format!(
            "it took longer than its time budget ({} s)",
            budget.total.as_secs()
        )),
        End::Idle => note(&format!(
            "no bytes came for its pause budget ({} s)",
            budget.idle.as_secs()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(version: Version, expect: Option<&str>) -> Request {
        let mut b = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .version(version);
        if let Some(e) = expect {
            b = b.header(header::EXPECT, e);
        }
        b.body(Body::empty()).unwrap()
    }

    #[test]
    fn the_budget_is_twice_the_limit_and_nginx_lingering_times() {
        let b = Budget::of(1 << 20);
        assert_eq!(b.bytes, 2 << 20);
        assert_eq!((b.total, b.idle), (TOTAL, IDLE));
        assert_eq!(Budget::of(u64::MAX).bytes, u64::MAX, "no overflow");
    }

    #[test]
    fn only_an_http1_body_without_expect_and_within_the_budget_is_drained() {
        let b = Budget::of(1 << 20);
        let at = |v, e, n| plan(&request(v, e), n, &b);
        assert_eq!(at(Version::HTTP_11, None, 2 << 20), Plan::Drain);
        assert_eq!(at(Version::HTTP_10, None, (1 << 20) + 1), Plan::Drain);
        assert_eq!(at(Version::HTTP_11, None, (2 << 20) + 1), Plan::OverBudget);
        assert_eq!(
            at(Version::HTTP_11, Some("100-Continue"), 2 << 20),
            Plan::Expecting
        );
        assert_eq!(at(Version::HTTP_2, None, 2 << 20), Plan::NotHttp1);
    }

    fn trickle(every: Duration, chunks: usize) -> Body {
        Body::from_stream(futures::stream::unfold(chunks, move |left| async move {
            if left == 0 {
                return None;
            }
            tokio::time::sleep(every).await;
            Some((
                Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"x")),
                left - 1,
            ))
        }))
    }

    #[tokio::test(start_paused = true)]
    async fn a_drain_reads_to_the_end_within_its_times() {
        let b = Budget::of(1 << 20);
        assert_eq!(
            drain(trickle(Duration::from_secs(4), 5), &b).await,
            End::Read
        );
        assert_eq!(drain(Body::empty(), &b).await, End::Read);
    }

    #[tokio::test(start_paused = true)]
    async fn a_trickling_body_is_cut_by_the_total_and_a_stalled_one_by_the_pause() {
        let b = Budget::of(1 << 20);
        let start = tokio::time::Instant::now();
        assert_eq!(
            drain(trickle(Duration::from_secs(1), usize::MAX), &b).await,
            End::Total
        );
        assert_eq!(start.elapsed(), TOTAL);

        let start = tokio::time::Instant::now();
        let stalled = Body::from_stream(futures::stream::pending::<
            Result<bytes::Bytes, std::io::Error>,
        >());
        assert_eq!(drain(stalled, &b).await, End::Idle);
        assert_eq!(start.elapsed(), IDLE);
    }
}
