//! The delivery cue each clause speaks with (realtime design §8.1; WP9b C2,
//! C3), worked out in the stream without knowing the route: the route that
//! answers decides whether it takes one (`crate::audio::cues`).
//!
//! A cue covers the clause it opens and no other. Every clause ends a
//! sentence or a line (`clauses`, 2026-10-05), so the sentence a leading tag
//! opens is that one clause: the first-comma cut that made "[laughing] Oh
//! no," a clause of its own, and with it carrying a cue on to the sentence
//! end, is gone. A clause that opens with no tag has no cue, whatever the one
//! before it had. Each response has a splitter of its own, so nothing
//! outlives it.

use crate::audio::cues;

/// The cue of the clause `tts` (its TTS text, after the carry): the tags it
/// opens with, normalised, or `None`.
pub(super) fn of(tts: &str) -> Option<String> {
    cues::lead(tts).map(|(cue, _)| cue)
}
