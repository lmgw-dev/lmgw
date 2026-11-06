//! Where the feed resumes, when it tries again and when a quiet link is dead
//! (chat-plane design, "WP3a: the transport seam"). The browser's
//! `EventSource` did the first two on its own; a stream read off the page's
//! transport carries its cursor itself: the last `id:` it saw, or the
//! cursor `hello` and `caught_up` name, goes out as `Last-Event-ID` on the
//! next connection, so the catch-up names what changed while it was down.
//! A link that stalls without ending (a relay whose far side went away) is
//! the page's to time: `hello.keepalive_s` says how often the stream sends
//! at least a keep-alive, and [`DEAD_AFTER_KEEPALIVES`] of those intervals
//! without a byte end the connection, as Kai's link does.

use lmgw_ui_kit::http::sse::Record;

/// The first wait before a new connection, unless the server says one
/// (`retry:`).
pub(super) const FIRST_WAIT_MS: u64 = 1_000;
/// The longest wait between two tries: a gateway that is down for long is
/// still asked every half minute, so the page comes back soon after it does.
pub(super) const MAX_WAIT_MS: u64 = 30_000;

/// How many `hello.keepalive_s` intervals without a byte make a dead link:
/// two, so one keep-alive late or lost on the way is not one.
pub(super) const DEAD_AFTER_KEEPALIVES: u64 = 2;

#[derive(Debug, Default)]
pub(super) struct Resume {
    cursor: Option<String>,
    /// Tries since the last connection that delivered a record.
    failed: u32,
    /// The server's `retry:`.
    retry_ms: Option<u64>,
    /// This connection's `hello.keepalive_s`, once its `hello` came.
    keepalive_s: Option<u32>,
}

/// What a refused connection (a non-2xx answer) means for the feed.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Refused {
    /// A `400` to a connection that sent a cursor: the feed could not read
    /// it. Forget it and connect from now; `hello` makes the page read
    /// everything again, so nothing that changed meanwhile is missed.
    Fresh,
    /// Any other `4xx`: asking again gets the same answer. The feed stops
    /// and says why (a `401 session_required` locks the session instead,
    /// and the login opens a new feed).
    Stop,
    /// A `5xx`, a `408` or a `429`: lmgw is busy or restarting. Try again
    /// after the wait.
    Retry,
}

/// What `status`, the answer to a connection that did or did not send a
/// cursor, means ([`Refused`]).
pub(super) fn refused(status: u16, sent_cursor: bool) -> Refused {
    match status {
        400 if sent_cursor => Refused::Fresh,
        408 | 429 => Refused::Retry,
        400..=499 => Refused::Stop,
        _ => Refused::Retry,
    }
}

impl Resume {
    /// The cursor for the next connection's `Last-Event-ID`.
    pub(super) fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref()
    }

    /// A new connection starts: its `hello` says its keep-alive again.
    pub(super) fn connecting(&mut self) {
        self.keepalive_s = None;
    }

    /// `hello` or `caught_up` named where the stream stands: a cursor to
    /// resume from, as an event's `id:` is (an empty one changes nothing).
    pub(super) fn moved(&mut self, cursor: &str) {
        if !cursor.is_empty() {
            self.cursor = Some(cursor.to_string());
        }
    }

    /// The feed could not read the cursor ([`Refused::Fresh`]).
    pub(super) fn drop_cursor(&mut self) {
        self.cursor = None;
    }

    /// `hello.keepalive_s`: the stream sends something at least this often.
    pub(super) fn heard_keepalive(&mut self, keepalive_s: u32) {
        self.keepalive_s = (keepalive_s > 0).then_some(keepalive_s);
    }

    /// How long this connection may stay silent before it counts as dead:
    /// [`DEAD_AFTER_KEEPALIVES`] times the keep-alive `hello` said. `None`
    /// before `hello` (the server sends it first, so a connection that
    /// answered sends it at once).
    pub(super) fn dead_after_ms(&self) -> Option<u64> {
        self.keepalive_s
            .map(|k| u64::from(k) * 1_000 * DEAD_AFTER_KEEPALIVES)
    }

    /// A record arrived: the connection works, and its id (an empty one
    /// clears the cursor, as the spec says) and `retry:` are kept.
    pub(super) fn saw(&mut self, r: &Record) {
        if let Some(id) = &r.id {
            self.cursor = (!id.is_empty()).then(|| id.clone());
        }
        if let Some(ms) = r.retry {
            self.retry_ms = Some(ms);
        }
        self.failed = 0;
    }

    /// The connection ended: how long to wait before the next. Doubles per
    /// try that delivered nothing, up to [`MAX_WAIT_MS`] (or the server's
    /// `retry:` when that is longer).
    pub(super) fn next_wait_ms(&mut self) -> u64 {
        let first = self.retry_ms.unwrap_or(FIRST_WAIT_MS);
        let wait = first
            .saturating_mul(1u64 << self.failed.min(16))
            .min(MAX_WAIT_MS.max(first));
        self.failed = self.failed.saturating_add(1);
        wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: Option<&str>, retry: Option<u64>) -> Record {
        Record {
            event: "thread.updated".into(),
            data: "{}".into(),
            id: id.map(str::to_string),
            retry,
        }
    }

    #[test]
    fn the_cursor_is_the_last_id_seen() {
        let mut r = Resume::default();
        assert_eq!(r.cursor(), None, "a first connection resumes from nothing");
        r.saw(&rec(Some("7"), None));
        r.saw(&rec(None, None));
        assert_eq!(r.cursor(), Some("7"), "a record without an id keeps it");
        r.saw(&rec(Some("12"), None));
        assert_eq!(r.cursor(), Some("12"));
        r.saw(&rec(Some(""), None));
        assert_eq!(r.cursor(), None, "an empty id clears it");
    }

    #[test]
    fn waits_double_up_to_the_cap_and_start_over_once_a_record_came() {
        let mut r = Resume::default();
        let waits: Vec<u64> = (0..7).map(|_| r.next_wait_ms()).collect();
        assert_eq!(waits, [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000]);
        r.saw(&rec(Some("3"), None));
        assert_eq!(
            r.next_wait_ms(),
            1_000,
            "a working connection resets the wait"
        );
        assert_eq!(r.cursor(), Some("3"));
    }

    #[test]
    fn the_server_s_retry_is_the_first_wait() {
        let mut r = Resume::default();
        r.saw(&rec(None, Some(5_000)));
        assert_eq!(r.next_wait_ms(), 5_000);
        assert_eq!(r.next_wait_ms(), 10_000);
        // A retry longer than the cap is kept as asked.
        r.saw(&rec(None, Some(60_000)));
        assert_eq!(r.next_wait_ms(), 60_000);
        assert_eq!(r.next_wait_ms(), 60_000);
        // Many failures do not overflow.
        for _ in 0..100 {
            r.next_wait_ms();
        }
        assert_eq!(r.next_wait_ms(), 60_000);
    }

    #[test]
    fn hello_and_caught_up_move_the_cursor_too() {
        let mut r = Resume::default();
        r.moved("e:4:aa");
        assert_eq!(r.cursor(), Some("e:4:aa"));
        r.moved("");
        assert_eq!(r.cursor(), Some("e:4:aa"), "an empty one changes nothing");
        r.saw(&rec(Some("e:5:bb"), None));
        assert_eq!(r.cursor(), Some("e:5:bb"));
        r.drop_cursor();
        assert_eq!(r.cursor(), None);
    }

    #[test]
    fn a_link_is_dead_after_two_keepalives_of_silence_once_hello_said_them() {
        let mut r = Resume::default();
        assert_eq!(r.dead_after_ms(), None, "no hello yet: no clock");
        r.heard_keepalive(15);
        assert_eq!(r.dead_after_ms(), Some(30_000));
        r.heard_keepalive(0);
        assert_eq!(r.dead_after_ms(), None, "no keep-alive promised: no clock");
        r.heard_keepalive(900);
        r.connecting();
        assert_eq!(
            r.dead_after_ms(),
            None,
            "each connection's hello says it again"
        );
    }

    #[test]
    fn a_refusal_of_the_cursor_starts_fresh_and_other_4xx_stop() {
        assert_eq!(refused(400, true), Refused::Fresh);
        assert_eq!(refused(400, false), Refused::Stop);
        assert_eq!(refused(401, true), Refused::Stop);
        assert_eq!(refused(403, false), Refused::Stop);
        assert_eq!(refused(404, true), Refused::Stop);
        assert_eq!(refused(408, true), Refused::Retry);
        assert_eq!(refused(429, false), Refused::Retry);
        assert_eq!(refused(502, true), Refused::Retry);
        assert_eq!(refused(503, false), Refused::Retry);
    }
}
