//! The upstream's request timeout past a stream's headers: the longest the
//! upstream may say nothing.
//!
//! `Upstream::request_timeout` bounds the wait for an answer's headers
//! (`vram::local_hold::send_built`), and, on every streamed relay, each
//! silence after them: the chat relay and its in-process and Chat-page
//! callers (`drive_upstream_until`), the native `/v1/responses` passthrough
//! and the legacy completions relay. The stream's total length stays
//! unbounded — a model that keeps producing may run all day — and a timeout
//! of 0 (`None`) bounds no silence either.
//!
//! Silence is counted in SSE events, not bytes. A keep-alive is no sign of
//! the answer going on: llama-server writes a `:` comment every 30 s while a
//! result is still to come (`--sse-ping-interval`), an aggregator such as
//! OpenRouter writes `: OPENROUTER PROCESSING` while its provider is quiet,
//! and Anthropic's `ping` event carries nothing. A byte-level clock was reset
//! by each of them, so a provider that stalled behind its aggregator's
//! keep-alives held the client's stream open until the client gave up (a live
//! check saw it: minutes after one delta, the row a `canceled`).
//! Every other event, a chunk with an empty delta included, counts as the
//! upstream speaking.
//!
//! Keep-alives still count while the model may be working out of sight
//! ([`sight`]): before the stream's first visible output, inside a
//! hidden-thinking span (an Anthropic thinking block, a Responses reasoning
//! item), where a model that thinks for minutes legitimately sends nothing
//! else, and, since 2026-11-05, inside an Anthropic compaction block and
//! while a tool its provider runs is running (an Anthropic server tool
//! until its result block or the model's next block of another kind, a
//! Responses hosted call such as `web_search_call` until its item is done
//! or the next item that is no span). There every byte read starts the
//! clock again. The gap after a hosted call's done counts for one more
//! timeout at most (*changed 2026-11-05*). So a stream that stalls before
//! any output while its keep-alives go on is not yet bounded by the
//! timeout, only by its client; one that stalls behind keep-alives once its
//! answer shows is cut. Once the answer is finished (its finish reason,
//! Anthropic's `message_delta` with a `stop_reason`, a Responses terminal
//! event) keep-alives count no more.
//!
//! A relay that hears nothing in time stops reading the upstream (dropping
//! it closes the connection, which ends a local model's generation). Before
//! the answer is finished it ends its client's stream with its protocol's
//! mid-stream error, and writes its row with the kind [`KIND`] and
//! [`Silence::message`]. After it the answer is whole: the relay ends its
//! client's stream as the upstream's own end would have, and the row is no
//! error (the chat relay since 2026-11-05; the legacy relay and the native
//! passthrough had it before).

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

use crate::sse::SseEvent;

mod sight;

/// The row's error kind for a stream the upstream fell silent on: the one a
/// header wait that ran out has (`GatewayError::Timeout`).
pub(crate) const KIND: &str = "timeout";

/// The clock of one stream's silence.
pub(crate) struct Silence {
    limit: Option<Duration>,
    /// When the current silence runs out; `None` for no bound.
    deadline: Option<Instant>,
    /// What the stream showed so far: whether a keep-alive counts.
    sight: sight::Sight,
}

impl Silence {
    /// A clock that starts now, as the stream's headers arrived; `limit`
    /// `None` bounds nothing.
    pub(crate) fn new(limit: Option<Duration>) -> Self {
        let mut s = Self {
            limit,
            deadline: None,
            sight: sight::Sight::default(),
        };
        s.restart();
        s
    }

    fn restart(&mut self) {
        self.deadline = self.limit.and_then(|l| Instant::now().checked_add(l));
    }

    /// A read: whether it brought any bytes, and the events it completed.
    /// The clock starts again when one of them is more than a keep-alive
    /// ([`keeps_alive`]), or, while the model may be working out of sight
    /// ([`sight`]), when anything came at all: a keep-alive, a comment, half
    /// an event.
    pub(crate) fn hears(&mut self, read: bool, events: &[SseEvent]) {
        let Some(limit) = self.limit else {
            return;
        };
        let now = Instant::now();
        let mut spoke = false;
        for ev in events.iter().filter(|ev| !keeps_alive(ev)) {
            spoke = true;
            self.sight.observe(ev, now);
        }
        if spoke || (read && self.sight.may_be_thinking(now, limit)) {
            self.restart();
        }
    }

    /// `next`, or `None` when the silence runs out first.
    pub(crate) async fn bound<F: Future>(&self, next: F) -> Option<F::Output> {
        match self.deadline {
            Some(d) => tokio::time::timeout_at(d, next).await.ok(),
            None => Some(next.await),
        }
    }

    /// What the row and the client are told when the silence ran out.
    pub(crate) fn message(&self) -> String {
        let limit = self.limit.unwrap_or_default();
        let span = match limit.subsec_millis() {
            0 => format!("{} s", limit.as_secs()),
            _ => format!("{} ms", limit.as_millis()),
        };
        format!("the upstream sent nothing for {span} mid-stream")
    }
}

/// An event that only keeps the connection alive: Anthropic's `ping`. (An
/// SSE comment is no event at all: the decoder hands none out for it.)
fn keeps_alive(ev: &SseEvent) -> bool {
    ev.event.as_deref() == Some("ping")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event: Option<&str>, data: &str) -> SseEvent {
        SseEvent {
            event: event.map(str::to_string),
            data: data.to_string(),
        }
    }

    fn ping() -> SseEvent {
        ev(Some("ping"), "{\"type\":\"ping\"}")
    }

    /// A chat chunk with answer text: the stream shows.
    fn shown() -> SseEvent {
        ev(
            None,
            "{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"}}]}",
        )
    }

    /// Whether `s` is still open `ms` from now. (A read that is ready at
    /// once would pass even a deadline gone by: the timeout polls it first.)
    async fn open_after(s: &Silence, ms: u64) -> bool {
        tokio::time::advance(Duration::from_millis(ms)).await;
        let read = tokio::time::sleep(Duration::from_millis(1));
        s.bound(read).await.is_some()
    }

    #[tokio::test(start_paused = true)]
    async fn once_shown_a_keep_alive_does_not_restart_the_clock() {
        let mut s = Silence::new(Some(Duration::from_millis(300)));
        s.hears(true, &[shown()]);
        tokio::time::advance(Duration::from_millis(200)).await;
        s.hears(true, &[ping()]);
        s.hears(
            true,
            &crate::sse::SseDecoder::new().feed(b": PROCESSING\n\n"),
        );
        tokio::time::advance(Duration::from_millis(150)).await;
        assert!(s.bound(std::future::pending::<()>()).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn before_any_output_a_keep_alive_restarts_the_clock() {
        let mut s = Silence::new(Some(Duration::from_millis(300)));
        tokio::time::advance(Duration::from_millis(200)).await;
        s.hears(
            true,
            &crate::sse::SseDecoder::new().feed(b": PROCESSING\n\n"),
        );
        assert!(open_after(&s, 200).await);
        s.hears(true, &[ping()]);
        assert!(open_after(&s, 200).await);
    }

    #[tokio::test(start_paused = true)]
    async fn inside_a_thinking_block_a_ping_restarts_the_clock() {
        let mut s = Silence::new(Some(Duration::from_millis(300)));
        s.hears(true, &[shown()]);
        let start = "{\"type\":\"content_block_start\",\"index\":1,\
                     \"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}";
        s.hears(true, &[ev(Some("content_block_start"), start)]);
        for _ in 0..4 {
            tokio::time::advance(Duration::from_millis(200)).await;
            s.hears(true, &[ping()]);
        }
        assert!(open_after(&s, 200).await);
        let stop = "{\"type\":\"content_block_stop\",\"index\":1}";
        s.hears(true, &[ev(Some("content_block_stop"), stop)]);
        tokio::time::advance(Duration::from_millis(200)).await;
        s.hears(true, &[ping()]);
        assert!(!open_after(&s, 200).await);
    }

    #[tokio::test(start_paused = true)]
    async fn while_a_server_tool_runs_a_ping_restarts_the_clock() {
        let mut s = Silence::new(Some(Duration::from_millis(300)));
        s.hears(true, &[shown()]);
        let tool = "{\"type\":\"content_block_start\",\"index\":1,\"content_block\":\
                    {\"type\":\"server_tool_use\",\"id\":\"srvtoolu_1\",\"name\":\"web_search\"}}";
        s.hears(true, &[ev(Some("content_block_start"), tool)]);
        let stop = |i: u64| {
            let data = format!("{{\"type\":\"content_block_stop\",\"index\":{i}}}");
            ev(Some("content_block_stop"), &data)
        };
        s.hears(true, &[stop(1)]);
        for _ in 0..4 {
            tokio::time::advance(Duration::from_millis(200)).await;
            s.hears(true, &[ping()]);
        }
        assert!(open_after(&s, 200).await);
        let result = "{\"type\":\"content_block_start\",\"index\":2,\"content_block\":\
                      {\"type\":\"web_search_tool_result\",\"tool_use_id\":\"srvtoolu_1\"}}";
        s.hears(true, &[ev(Some("content_block_start"), result), stop(2)]);
        tokio::time::advance(Duration::from_millis(200)).await;
        s.hears(true, &[ping()]);
        assert!(!open_after(&s, 200).await);
    }

    /// A compaction block is written out of sight: Anthropic sends only
    /// pings until its one `compaction_delta`, which shows nothing either.
    #[tokio::test(start_paused = true)]
    async fn inside_a_compaction_block_a_ping_restarts_the_clock() {
        let mut s = Silence::new(Some(Duration::from_millis(300)));
        s.hears(true, &[shown()]);
        let start = "{\"type\":\"content_block_start\",\"index\":1,\
                     \"content_block\":{\"type\":\"compaction\",\"content\":null}}";
        s.hears(true, &[ev(Some("content_block_start"), start)]);
        for _ in 0..4 {
            tokio::time::advance(Duration::from_millis(200)).await;
            s.hears(true, &[ping()]);
        }
        assert!(open_after(&s, 200).await);
        let delta = "{\"type\":\"content_block_delta\",\"index\":1,\
                     \"delta\":{\"type\":\"compaction_delta\",\"content\":\"Summary.\"}}";
        s.hears(true, &[ev(Some("content_block_delta"), delta)]);
        tokio::time::advance(Duration::from_millis(200)).await;
        s.hears(true, &[ping()]);
        assert!(open_after(&s, 200).await, "the block is still open");
        let stop = "{\"type\":\"content_block_stop\",\"index\":1}";
        s.hears(true, &[ev(Some("content_block_stop"), stop)]);
        tokio::time::advance(Duration::from_millis(200)).await;
        s.hears(true, &[ping()]);
        assert!(!open_after(&s, 200).await);
    }

    /// After a hosted call's done keep-alives count for one more timeout,
    /// then the gap is silence.
    #[tokio::test(start_paused = true)]
    async fn after_a_hosted_call_keep_alives_count_for_one_timeout() {
        let mut s = Silence::new(Some(Duration::from_millis(300)));
        let item = |name: &str, i: u64, ty: &str| {
            let data = format!(
                "{{\"type\":\"{name}\",\"output_index\":{i},\"item\":{{\"type\":\"{ty}\"}}}}"
            );
            ev(Some(name), &data)
        };
        s.hears(
            true,
            &[
                item("response.output_item.added", 0, "message"),
                item("response.output_item.done", 0, "message"),
                item("response.output_item.added", 1, "web_search_call"),
                item("response.output_item.done", 1, "web_search_call"),
            ],
        );
        tokio::time::advance(Duration::from_millis(200)).await;
        let comment = || crate::sse::SseDecoder::new().feed(b": PROCESSING\n\n");
        s.hears(true, &comment());
        tokio::time::advance(Duration::from_millis(200)).await;
        // 400 ms after the done: past its grace, the comment counts no more.
        s.hears(true, &comment());
        assert!(open_after(&s, 50).await);
        assert!(!open_after(&s, 100).await);
    }

    #[tokio::test(start_paused = true)]
    async fn after_the_finish_a_keep_alive_does_not_restart_the_clock() {
        let mut s = Silence::new(Some(Duration::from_millis(300)));
        let finish = "{\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}";
        s.hears(true, &[ev(None, finish)]);
        tokio::time::advance(Duration::from_millis(200)).await;
        s.hears(
            true,
            &crate::sse::SseDecoder::new().feed(b": PROCESSING\n\n"),
        );
        assert!(!open_after(&s, 200).await);
    }

    #[tokio::test(start_paused = true)]
    async fn half_an_event_restarts_the_clock_only_out_of_sight() {
        let mut s = Silence::new(Some(Duration::from_millis(300)));
        tokio::time::advance(Duration::from_millis(200)).await;
        s.hears(true, &[]);
        assert!(open_after(&s, 200).await);
        s.hears(true, &[shown()]);
        tokio::time::advance(Duration::from_millis(200)).await;
        s.hears(true, &[]);
        assert!(!open_after(&s, 200).await);
    }

    #[tokio::test(start_paused = true)]
    async fn an_event_restarts_the_clock() {
        let mut s = Silence::new(Some(Duration::from_millis(300)));
        s.hears(true, &[shown()]);
        tokio::time::advance(Duration::from_millis(200)).await;
        s.hears(true, &[ev(None, "{\"choices\":[]}")]);
        assert!(open_after(&s, 200).await);
    }

    #[tokio::test(start_paused = true)]
    async fn no_limit_bounds_nothing() {
        let s = Silence::new(None);
        let slow = async {
            tokio::time::sleep(Duration::from_secs(86_400)).await;
            1
        };
        assert_eq!(s.bound(slow).await, Some(1));
    }

    #[test]
    fn the_message_names_the_limit() {
        let s = Silence::new(Some(Duration::from_secs(120)));
        assert!(s.message().contains("nothing for 120 s"), "{}", s.message());
        let s = Silence::new(Some(Duration::from_millis(300)));
        assert!(
            s.message().contains("nothing for 300 ms"),
            "{}",
            s.message()
        );
    }
}
