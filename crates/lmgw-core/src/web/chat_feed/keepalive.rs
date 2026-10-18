//! A stream's own keep-alive (device link design §7.1, D16):
//! `GET /chat/api/feed?keepalive_s=<n>`, n from 1 to
//! `chat_feed_keepalive_max_s`; without it the gateway's
//! `chat_feed_keepalive_s`, as before. `hello.keepalive_s` says the value
//! the stream keeps for its life.
//!
//! **Only the comment follows it** (the split tick, `stream`): the table's
//! backstop read and the snapshot-behind wait keep the gateway's
//! `chat_feed_keepalive_s`, so a stream at 900 s still reads the table
//! every 15 s and delays no event, only its comments.
//!
//! **Why the device asks, not the owner per device:** the client knows its
//! situation (a visible app, a background service, a desktop on loopback);
//! a per-row value would be one the owner has to know and set for each
//! device. **Why a maximum:** between events a stream sends only its
//! comment, so the maximum bounds how long a stream whose device vanished
//! silently stays open.
//!
//! **Not built here:** a device's feed setting `TCP_USER_TIMEOUT` on its
//! connection from this value, so a vanished phone is noticed within about
//! one more interval. It needs the device listener's connection and its
//! socket handle (device link WP2, `Conn`); `super::open` marks the place.

use crate::config::Settings;

/// The keep-alive a stream keeps, in seconds: `asked` when the request gave
/// one in range, the gateway's `chat_feed_keepalive_s` when it gave none.
/// One out of range is the refusal's message (a 400 `bad_request`), naming
/// the bound and the setting that sets it.
pub(super) fn of_stream(settings: &Settings, asked: Option<i64>) -> Result<u32, String> {
    let max = settings.chat_feed_keepalive_max_s.max(1);
    let Some(n) = asked else {
        return Ok(settings.chat_feed_keepalive_s.max(1));
    };
    match u32::try_from(n) {
        Ok(n) if (1..=max).contains(&n) => Ok(n),
        _ => Err(format!(
            "keepalive_s {n} is out of range: a feed may ask for a keep-alive from 1 to {max} \
             seconds (chat_feed_keepalive_max_s, Settings → Chat → Change feed → Longest \
             keep-alive)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_keeps_what_it_asked_within_the_maximum() {
        let s = Settings {
            chat_feed_keepalive_s: 15,
            chat_feed_keepalive_max_s: 900,
            ..Settings::default()
        };
        assert_eq!(of_stream(&s, None), Ok(15), "none asked: the gateway's");
        for n in [1, 120, 900] {
            assert_eq!(of_stream(&s, Some(n)), Ok(n as u32), "{n}");
        }
        for n in [0, -5, 901, i64::from(u32::MAX) + 1] {
            let e = of_stream(&s, Some(n)).unwrap_err();
            assert!(
                e.starts_with(&format!("keepalive_s {n} is out of range"))
                    && e.contains("from 1 to 900 seconds")
                    && e.contains("chat_feed_keepalive_max_s"),
                "{e}"
            );
        }
    }
}
