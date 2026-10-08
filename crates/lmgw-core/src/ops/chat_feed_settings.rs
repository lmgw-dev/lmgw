//! The Chat change feed's four settings (client-apps design §2.1, §2.3):
//! retention, keep-alive, the catch-up page and the live buffer, applied and
//! checked here for both save paths — `settings_set` (the self-admin tool)
//! and the dashboard's `settings_set_full` — so the two cannot drift.
//!
//! Every limit the feed has is one of these, and each is named where it
//! shows: `hello` carries the keep-alive and the retention, a `resync` for
//! an old cursor names the retention, and a `state` after an overrun names
//! the live buffer.

use lmgw_api_types::chat_feed::{MAX_LIVE_BUFFER, MAX_PAGE_SIZE};

use crate::config::Settings;

/// The four keys as a sparse patch; `None` leaves a key alone.
#[derive(Debug, Default)]
pub struct ChatFeedPatch {
    pub chat_feed_retention_days: Option<i64>,
    pub chat_feed_keepalive_s: Option<i64>,
    pub chat_feed_page_size: Option<i64>,
    pub chat_feed_live_buffer: Option<i64>,
}

/// `chat_feed_retention_days`: `0` (keep every record) or more.
pub fn validate_chat_feed_retention_days(v: i64) -> Result<i64, String> {
    if v < 0 {
        return Err(format!(
            "chat_feed_retention_days {v} is negative; 0 keeps every record"
        ));
    }
    Ok(v)
}

/// A count the feed needs at least one of: the keep-alive interval, the
/// catch-up page, the live buffer.
fn at_least_one(key: &str, v: i64, why: &str) -> Result<u32, String> {
    if v < 1 {
        return Err(format!("{key} must be at least 1: {why}"));
    }
    u32::try_from(v).map_err(|_| format!("{key} {v} does not fit in 32 bits"))
}

/// A count with a stated maximum (review W4-1): refused past it, by name and
/// with the reason the maximum exists.
fn at_most(key: &str, v: u32, max: u32, why: &str) -> Result<u32, String> {
    if v > max {
        return Err(format!("{key} must be at most {max}: {why}"));
    }
    Ok(v)
}

/// The reasons the two maxima exist, as a refusal and a load warning say them.
const LIVE_BUFFER_WHY: &str =
    "the buffer is allocated whole, about 80 bytes a slot, at every change of its size";
const PAGE_SIZE_WHY: &str = "one catch-up holds a whole page, every record rendered, in memory";

/// Settings read from the database that a save would refuse now (written
/// before the maxima, or by hand) are clamped into range, with a warn line
/// naming the setting — never an allocation that aborts every start
/// (review W4-1).
pub fn clamp_loaded_chat_feed(s: &mut Settings) {
    let clamp = |key: &str, v: &mut u32, max: u32, why: &str| {
        let fixed = (*v).clamp(1, max);
        if fixed != *v {
            tracing::warn!(
                "stored setting {key} = {} is out of range (1 to {max}: {why}); using {fixed} \
                 until it is saved again",
                *v
            );
            *v = fixed;
        }
    };
    clamp(
        "chat_feed_live_buffer",
        &mut s.chat_feed_live_buffer,
        MAX_LIVE_BUFFER,
        LIVE_BUFFER_WHY,
    );
    clamp(
        "chat_feed_page_size",
        &mut s.chat_feed_page_size,
        MAX_PAGE_SIZE,
        PAGE_SIZE_WHY,
    );
}

/// Apply `p` to `s`; the keys it changed, or the first refusal.
pub fn apply_chat_feed(s: &mut Settings, p: ChatFeedPatch) -> Result<Vec<&'static str>, String> {
    let mut changed = Vec::new();
    if let Some(v) = p.chat_feed_retention_days {
        s.chat_feed_retention_days = validate_chat_feed_retention_days(v)?;
        changed.push("chat_feed_retention_days");
    }
    if let Some(v) = p.chat_feed_keepalive_s {
        s.chat_feed_keepalive_s = at_least_one(
            "chat_feed_keepalive_s",
            v,
            "a feed without keep-alives gives a client no way to tell a quiet link from a \
             dead one",
        )?;
        changed.push("chat_feed_keepalive_s");
    }
    if let Some(v) = p.chat_feed_page_size {
        s.chat_feed_page_size = at_most(
            "chat_feed_page_size",
            at_least_one(
                "chat_feed_page_size",
                v,
                "a catch-up reads at least one record per query",
            )?,
            MAX_PAGE_SIZE,
            PAGE_SIZE_WHY,
        )?;
        changed.push("chat_feed_page_size");
    }
    if let Some(v) = p.chat_feed_live_buffer {
        s.chat_feed_live_buffer = at_most(
            "chat_feed_live_buffer",
            at_least_one(
                "chat_feed_live_buffer",
                v,
                "the live events need room for at least one",
            )?,
            MAX_LIVE_BUFFER,
            LIVE_BUFFER_WHY,
        )?;
        changed.push("chat_feed_live_buffer");
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_key_is_checked_and_named() {
        let mut s = Settings::default();
        let changed = apply_chat_feed(
            &mut s,
            ChatFeedPatch {
                chat_feed_retention_days: Some(0),
                chat_feed_keepalive_s: Some(30),
                chat_feed_page_size: Some(2),
                chat_feed_live_buffer: Some(8),
            },
        )
        .unwrap();
        assert_eq!(changed.len(), 4);
        assert_eq!(
            (
                s.chat_feed_retention_days,
                s.chat_feed_keepalive_s,
                s.chat_feed_page_size,
                s.chat_feed_live_buffer
            ),
            (0, 30, 2, 8)
        );
        for (patch, key) in [
            (
                ChatFeedPatch {
                    chat_feed_keepalive_s: Some(0),
                    ..Default::default()
                },
                "chat_feed_keepalive_s",
            ),
            (
                ChatFeedPatch {
                    chat_feed_page_size: Some(0),
                    ..Default::default()
                },
                "chat_feed_page_size",
            ),
            (
                ChatFeedPatch {
                    chat_feed_live_buffer: Some(-3),
                    ..Default::default()
                },
                "chat_feed_live_buffer",
            ),
            (
                ChatFeedPatch {
                    chat_feed_retention_days: Some(-1),
                    ..Default::default()
                },
                "chat_feed_retention_days",
            ),
        ] {
            let err = apply_chat_feed(&mut s, patch).unwrap_err();
            assert!(err.starts_with(key), "{err}");
        }
    }

    /// Review W4-1: a size past the stated maximum is refused by name, and
    /// one already stored is clamped on load instead of aborting the start.
    #[test]
    fn the_buffer_and_the_page_have_a_stated_maximum() {
        let mut s = Settings::default();
        for (patch, key, max) in [
            (
                ChatFeedPatch {
                    chat_feed_live_buffer: Some(4_000_000_000),
                    ..Default::default()
                },
                "chat_feed_live_buffer",
                MAX_LIVE_BUFFER,
            ),
            (
                ChatFeedPatch {
                    chat_feed_page_size: Some(i64::from(MAX_PAGE_SIZE) + 1),
                    ..Default::default()
                },
                "chat_feed_page_size",
                MAX_PAGE_SIZE,
            ),
        ] {
            let err = apply_chat_feed(&mut s, patch).unwrap_err();
            assert!(
                err.starts_with(&format!("{key} must be at most {max}")),
                "{err}"
            );
        }
        let at_max = ChatFeedPatch {
            chat_feed_live_buffer: Some(i64::from(MAX_LIVE_BUFFER)),
            chat_feed_page_size: Some(i64::from(MAX_PAGE_SIZE)),
            ..Default::default()
        };
        assert_eq!(apply_chat_feed(&mut s, at_max).unwrap().len(), 2);

        let mut stored = Settings {
            chat_feed_live_buffer: u32::MAX,
            chat_feed_page_size: 0,
            ..Settings::default()
        };
        clamp_loaded_chat_feed(&mut stored);
        assert_eq!(
            (stored.chat_feed_live_buffer, stored.chat_feed_page_size),
            (MAX_LIVE_BUFFER, 1)
        );
    }

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let s = Settings::default();
        assert_eq!(s.chat_feed_retention_days, 7);
        assert_eq!(s.chat_feed_keepalive_s, 15);
        assert!(s.chat_feed_page_size >= 1 && s.chat_feed_live_buffer >= 1);
    }
}
