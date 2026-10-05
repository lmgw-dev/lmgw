//! The turns a bound session's chat model hears (voice-audio-input design
//! §3.1): `Bound.hearing`, the one fact the core reads about them.
//!
//! An **audio turn** is one committed while the verdict said `audio`. It is
//! here, with its WAV (built once at the commit, shared with its ASR call),
//! until its transcript is in; once its response launched, with that
//! response's generation. For as long as it is here:
//! - it **has words** (`input::has_words`): a response may launch for it,
//!   and nothing decides it is noise before its transcript says so;
//! - a response does **not wait** for its transcript
//!   (`Transcriber::busy_for_launch`): it goes as audio.
//!
//! A launched response that heard turns is remembered ([`Launched`]) until
//! its last audio turn is transcribed: what it answers, so the hold can be
//! settled (`lifecycle::held`) and the journal told what was heard — and
//! whether its attempt really carried the audio to the model, as the
//! responder says it (`Msg::Carried`, WP3 review #3). Handed to a response
//! is not heard: a skipped or refused attempt, or one cut before the model
//! said anything, heard nothing.

use std::collections::HashMap;

use super::super::transcribe::Wav;

/// `Bound.hearing` (module doc).
#[derive(Default)]
pub(crate) struct Hearing {
    turns: HashMap<String, Entry>,
    launched: HashMap<u64, Launched>,
}

/// One audio turn: its WAV, and the response it went to once launched.
struct Entry {
    wav: Wav,
    gen: Option<u64>,
}

/// A launched response that heard turns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Launched {
    /// Its new turns, in order — the audio turns and those already
    /// transcribed at its launch: one user row, once heard.
    pub new: Vec<String>,
    /// Every turn it answers: its new turns, and those owed again after a
    /// cut, whose rows are an earlier response's.
    pub answers: Vec<String>,
    /// Its new turns it went with as audio.
    pub audio: Vec<String>,
    /// Whether its attempt carried them to the model: `None` until the
    /// responder said (module doc).
    pub carried: Option<bool>,
}

/// What a transcript that came in was, for the hearing.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Was {
    /// Not an audio turn.
    NotHeard,
    /// An audio turn no response launched for yet: it goes as its
    /// transcript, and its WAV is let go.
    Unlaunched,
    /// An audio turn response `gen` heard; `last` when it was that
    /// response's last one still being transcribed.
    Launched { gen: u64, last: Option<Launched> },
}

impl Hearing {
    /// Whether `item_id` is an audio turn still being transcribed.
    pub fn hears(&self, item_id: &str) -> bool {
        self.turns.contains_key(item_id)
    }

    /// `item_id` committed as an audio turn, its WAV being built.
    pub fn insert(&mut self, item_id: String, wav: Wav) {
        self.turns.insert(item_id, Entry { wav, gen: None });
    }

    /// The WAV of audio turn `item_id`, when it is one.
    pub fn wav(&self, item_id: &str) -> Option<Wav> {
        self.turns.get(item_id).map(|e| e.wav.clone())
    }

    /// Response `gen` launched with the audio turns among `launched.new`
    /// (those still here): they are its now.
    pub fn launch(&mut self, gen: u64, launched: Launched) {
        for id in &launched.new {
            if let Some(e) = self.turns.get_mut(id) {
                e.gen = Some(gen);
            }
        }
        self.launched.insert(gen, launched);
    }

    /// Response `gen`'s attempt carried its audio to the model, or did not
    /// (module doc): kept while it is remembered.
    pub fn carry(&mut self, gen: u64, carried: bool) {
        if let Some(l) = self.launched.get_mut(&gen) {
            l.carried = Some(carried);
        }
    }

    /// Whether response `gen`'s attempt carried its audio, as far as known.
    pub fn carried(&self, gen: u64) -> Option<bool> {
        self.launched.get(&gen).and_then(|l| l.carried)
    }

    /// `item_id`'s transcript is in: it is no audio turn any more (module
    /// doc), and what it was.
    pub fn transcribed(&mut self, item_id: &str) -> Was {
        let Some(e) = self.turns.remove(item_id) else {
            return Was::NotHeard;
        };
        let Some(gen) = e.gen else {
            return Was::Unlaunched;
        };
        let pending = self.turns.values().any(|t| t.gen == Some(gen));
        let last = (!pending).then(|| self.launched.remove(&gen)).flatten();
        Was::Launched { gen, last }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav() -> Wav {
        Wav::build(vec![0; 240])
    }

    #[tokio::test]
    async fn a_response_is_settled_by_its_last_audio_turn() {
        let mut h = Hearing::default();
        h.insert("a".into(), wav());
        h.insert("b".into(), wav());
        h.insert("c".into(), wav());
        assert!(h.hears("a") && h.wav("b").is_some());
        let mut launched = Launched {
            new: vec!["t".into(), "a".into(), "b".into()],
            answers: vec!["o".into(), "t".into(), "a".into(), "b".into()],
            audio: vec!["a".into(), "b".into()],
            carried: None,
        };
        h.launch(7, launched.clone());
        assert_eq!(h.transcribed("t"), Was::NotHeard);
        assert_eq!(h.transcribed("a"), Was::Launched { gen: 7, last: None });
        assert!(!h.hears("a"));
        // The responder says its attempt carried the audio; unknown before.
        assert_eq!(h.carried(7), None);
        h.carry(7, true);
        h.carry(8, false);
        assert_eq!(h.carried(7), Some(true));
        launched.carried = Some(true);
        assert_eq!(
            h.transcribed("b"),
            Was::Launched {
                gen: 7,
                last: Some(launched)
            }
        );
        assert_eq!(h.carried(7), None, "forgotten once settled");
        // Not launched: it goes as its transcript.
        assert_eq!(h.transcribed("c"), Was::Unlaunched);
        assert!(h.turns.is_empty() && h.launched.is_empty());
    }
}
