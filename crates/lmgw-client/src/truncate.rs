//! The truncate bookkeeping: each response's audio as a client plays it,
//! and the `conversation.item.truncate` that keeps in the thread what was
//! *heard* of a reply that is cut (chat-voice design §9.2, §11.1).
//!
//! A cut — a barge-in (`speech_started` while a reply plays), a stop, or
//! another playback taking the speakers — flushes the reply's audio in the
//! client's player. The player says how much of it was heard (played, less
//! the output latency): that is the [`PlaybackCursor`], and the truncate
//! carries exactly that as `audio_end_ms`. A barge-in's response the server
//! cancels itself; a stop sends the truncate first and a `response.cancel`
//! only where the truncate did not stop the response
//! ([`crate::voice::cancel_after_truncate`]). Audio of a cut response still
//! on its way is dropped ([`Incoming::Drop`]).
//!
//! The player itself is the client's: this keeps no handle of it.

use crate::realtime::ClientEvent;

/// How much of an item the listener heard: `heard_samples` at
/// `sample_rate` (the realtime audio is 24 kHz).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackCursor {
    pub heard_samples: u64,
    pub sample_rate: u32,
}

impl PlaybackCursor {
    /// The truncate's `audio_end_ms`: the whole milliseconds heard.
    pub fn audio_end_ms(&self) -> u64 {
        self.heard_samples * 1000 / u64::from(self.sample_rate.max(1))
    }
}

/// One response's audio.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reply {
    /// The assistant item its audio belongs to: what a truncate names.
    pub item_id: Option<String>,
    /// No more of its audio comes: its `response.output_audio.done`, or a
    /// `response.done` without one (a cancelled response).
    pub ended: bool,
    /// Its `response.output_audio.done` came: the gateway's item is
    /// complete, so a truncate no longer stops the response.
    pub audio_done: bool,
    /// Cut: later audio of it is dropped.
    pub cut: bool,
}

/// A reply whose audio started, as [`Replies::items`] lists it: its
/// assistant item and what the crate knows of its audio. How much of it was
/// heard is the player's ([`PlaybackCursor`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyItem {
    pub response_id: String,
    /// The assistant item its audio belongs to: what a truncate names.
    pub item_id: String,
    /// No more of its audio comes ([`Reply::ended`]): uncut, it plays out.
    pub ended: bool,
    /// The gateway's item is complete ([`Reply::audio_done`]): a truncate
    /// no longer stops the response.
    pub audio_done: bool,
    /// Cut: its audio was flushed, and what arrives later is dropped.
    pub cut: bool,
}

/// What a client does with an audio delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Incoming {
    /// The response was cut, or its audio ended: drop it.
    Drop,
    /// The response's first audio: open its item in the player (and tell
    /// the voice state, [`crate::voice::Machine::audio_started`]), then
    /// play it.
    Start,
    /// More of a playing response's audio: play it.
    Continue,
}

/// Every response's audio in a session, by response id, in the order the
/// responses came: a plain list, so it needs no hasher (no seed drawn from
/// the system) and lists the same way every time. A session has one
/// response per turn.
///
/// **It keeps every response it has seen** until it is told to forget one
/// ([`Self::forget`]) or the session ends ([`Self::clear`]): a few dozen
/// bytes per turn, searched in order on every audio delta, and all of it
/// listed by [`Self::items`]. A long session that wants it small forgets a
/// reply once its response is done and its audio has played out or was
/// cut: nothing of it can arrive or be truncated then.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Replies {
    by_response: Vec<(String, Reply)>,
}

impl Replies {
    fn find(&self, response_id: &str) -> Option<&Reply> {
        self.by_response
            .iter()
            .find(|(id, _)| id == response_id)
            .map(|(_, r)| r)
    }

    fn find_mut(&mut self, response_id: &str) -> Option<&mut Reply> {
        self.by_response
            .iter_mut()
            .find(|(id, _)| id == response_id)
            .map(|(_, r)| r)
    }

    fn entry(&mut self, response_id: &str) -> &mut Reply {
        let at = match self
            .by_response
            .iter()
            .position(|(id, _)| id == response_id)
        {
            Some(at) => at,
            None => {
                self.by_response
                    .push((response_id.to_string(), Reply::default()));
                self.by_response.len() - 1
            }
        };
        &mut self.by_response[at].1
    }

    /// `response.created`.
    pub fn created(&mut self, response_id: &str) {
        self.entry(response_id);
    }

    /// `response.output_audio.delta` of `item_id`: what to do with it.
    pub fn audio(&mut self, response_id: &str, item_id: &str) -> Incoming {
        let r = self.entry(response_id);
        if r.cut || r.ended {
            Incoming::Drop
        } else if r.item_id.is_none() {
            r.item_id = Some(item_id.to_string());
            Incoming::Start
        } else {
            Incoming::Continue
        }
    }

    /// `response.output_audio.done`: the gateway's item is complete.
    pub fn audio_done(&mut self, response_id: &str) {
        if let Some(r) = self.find_mut(response_id) {
            r.audio_done = true;
        }
    }

    /// No more audio comes for the response (its audio's `done`, or the
    /// response's): `true` when that is news — it was known, not ended and
    /// not cut — and the client ends its item in the player, so the reply
    /// plays out.
    pub fn end(&mut self, response_id: &str) -> bool {
        match self.find_mut(response_id) {
            Some(r) if !r.ended && !r.cut => {
                r.ended = true;
                true
            }
            _ => false,
        }
    }

    /// The response's audio is cut: `true` the first time.
    pub fn cut(&mut self, response_id: &str) -> bool {
        let r = self.entry(response_id);
        if r.cut {
            return false;
        }
        r.cut = true;
        true
    }

    /// The truncate keeping what was heard of the response's item: `None`
    /// when no audio of it was heard or its item is not known, and then
    /// nothing is truncated.
    pub fn truncate(
        &self,
        response_id: &str,
        heard: Option<PlaybackCursor>,
    ) -> Option<ClientEvent> {
        let item_id = self.find(response_id)?.item_id.clone()?;
        Some(ClientEvent::Truncate {
            item_id,
            content_index: 0,
            audio_end_ms: heard?.audio_end_ms(),
        })
    }

    /// After a stop's truncate (`truncated`: one was sent): whether the
    /// response, `open` in the voice state, still needs a `response.cancel`
    /// ([`crate::voice::cancel_after_truncate`]).
    pub fn cancel_after_truncate(&self, response_id: &str, open: bool, truncated: bool) -> bool {
        let audio_done = self.find(response_id).is_some_and(|r| r.audio_done);
        crate::voice::cancel_after_truncate(open, truncated, audio_done)
    }

    pub fn get(&self, response_id: &str) -> Option<&Reply> {
        self.find(response_id)
    }

    /// Every reply whose audio started, in the order the responses came,
    /// with its item and its audio's state: a client that truncates every
    /// item it flushes walks this rather than keeping the ids beside it.
    pub fn items(&self) -> Vec<ReplyItem> {
        self.by_response
            .iter()
            .filter_map(|(response_id, r)| {
                Some(ReplyItem {
                    response_id: response_id.clone(),
                    item_id: r.item_id.clone()?,
                    ended: r.ended,
                    audio_done: r.audio_done,
                    cut: r.cut,
                })
            })
            .collect()
    }

    /// The response an assistant item's audio belongs to.
    pub fn response_of(&self, item_id: &str) -> Option<&str> {
        self.by_response
            .iter()
            .find(|(_, r)| r.item_id.as_deref() == Some(item_id))
            .map(|(id, _)| id.as_str())
    }

    /// Forget one response, once nothing of it can come or be cut any more
    /// (its response is done, its audio played out or flushed). Audio of a
    /// forgotten response that still arrived would read as a new reply's.
    pub fn forget(&mut self, response_id: &str) {
        self.by_response.retain(|(id, _)| id != response_id);
    }

    /// Forget every response (the session ended).
    pub fn clear(&mut self) {
        self.by_response.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 24_000;

    fn heard(ms: u64) -> Option<PlaybackCursor> {
        Some(PlaybackCursor {
            heard_samples: ms * u64::from(RATE) / 1000,
            sample_rate: RATE,
        })
    }

    #[test]
    fn a_cursor_is_whole_milliseconds_heard() {
        let c = PlaybackCursor {
            heard_samples: 35_999,
            sample_rate: RATE,
        };
        assert_eq!(c.audio_end_ms(), 1499);
        let c = PlaybackCursor {
            heard_samples: 10,
            sample_rate: 0,
        };
        assert_eq!(c.audio_end_ms(), 10_000, "a zero rate is taken as one");
    }

    #[test]
    fn audio_starts_its_item_once_and_stops_at_a_cut() {
        let mut r = Replies::default();
        r.created("r1");
        assert_eq!(r.audio("r1", "item_1"), Incoming::Start);
        assert_eq!(r.audio("r1", "item_1"), Incoming::Continue);
        assert_eq!(r.response_of("item_1"), Some("r1"));
        assert!(r.cut("r1"));
        assert!(!r.cut("r1"), "once");
        assert_eq!(r.audio("r1", "item_1"), Incoming::Drop);
        // A response the client never saw created still plays.
        assert_eq!(r.audio("r2", "item_2"), Incoming::Start);
    }

    #[test]
    fn an_ended_response_plays_out_and_drops_what_comes_after() {
        let mut r = Replies::default();
        assert!(!r.end("r0"), "unknown");
        r.created("r1");
        r.audio("r1", "item_1");
        r.audio_done("r1");
        assert!(r.end("r1"));
        assert!(!r.end("r1"), "once");
        assert_eq!(r.audio("r1", "item_1"), Incoming::Drop);
        r.cut("r2");
        assert!(!r.end("r2"), "a cut response has nothing to play out");
    }

    #[test]
    fn the_truncate_keeps_what_was_heard() {
        let mut r = Replies::default();
        r.created("r1");
        assert_eq!(r.truncate("r1", heard(500)), None, "no audio yet");
        r.audio("r1", "item_1");
        assert_eq!(r.truncate("r1", None), None, "nothing heard");
        assert_eq!(
            r.truncate("r1", heard(1500)),
            Some(ClientEvent::Truncate {
                item_id: "item_1".into(),
                content_index: 0,
                audio_end_ms: 1500
            })
        );
        assert_eq!(r.truncate("r9", heard(1)), None, "unknown");
    }

    #[test]
    fn the_items_are_listed_in_order_with_their_state() {
        let mut r = Replies::default();
        r.created("r2");
        r.audio("r2", "item_b");
        r.created("r1");
        r.created("r3");
        r.audio("r3", "item_c");
        r.audio_done("r3");
        r.end("r3");
        r.cut("r2");
        let item = |rid: &str, item: &str, ended, audio_done, cut| ReplyItem {
            response_id: rid.into(),
            item_id: item.into(),
            ended,
            audio_done,
            cut,
        };
        assert_eq!(
            r.items(),
            [
                item("r2", "item_b", false, false, true),
                item("r3", "item_c", true, true, false),
            ],
            "r1 has no audio yet, so no item"
        );
        // A reply done with is forgotten on request (review F-17).
        r.forget("r2");
        assert_eq!(r.items(), [item("r3", "item_c", true, true, false)]);
        assert!(r.get("r2").is_none() && r.get("r1").is_some());
        r.clear();
        assert!(r.items().is_empty());
    }

    #[test]
    fn a_stop_cancels_what_its_truncate_did_not_stop() {
        let mut r = Replies::default();
        r.created("r1");
        r.audio("r1", "item_1");
        // Producing, truncated: stopped by the truncate.
        assert!(!r.cancel_after_truncate("r1", true, true));
        // Not truncated (nothing heard): the cancel.
        assert!(r.cancel_after_truncate("r1", true, false));
        // Its item complete: the truncate stops nothing.
        r.audio_done("r1");
        assert!(r.cancel_after_truncate("r1", true, true));
        assert!(!r.cancel_after_truncate("r1", false, true), "done already");
    }
}
