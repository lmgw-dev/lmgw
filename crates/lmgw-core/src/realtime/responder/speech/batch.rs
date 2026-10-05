//! One TTS request for several sentences (realtime design §8.2, TTS batches
//! 2026-10-05).
//!
//! A TTS speaks each request on its own: a sentence sent alone starts and
//! ends as an utterance of its own, and two sentences in one request flow
//! as the engine would read them — an A/B listening test (2026-10-05)
//! preferred one request clearly. So the speaker joins clauses into a
//! [`Batch`] where it can wait for them:
//! - **the first clause of a response goes alone and at once**: nothing
//!   plays yet, and the first audio waits for nothing more;
//! - **a later clause joins while the listener still has audio to hear**
//!   ([`gather`]): one that is queued, or that comes while the speaker waits,
//!   joins when the batch with it is still predicted to be synthesized
//!   before the listener runs dry, with the prediction twice over ([`Pace`]);
//!   one that would not starts the next batch. The speaker waits for more
//!   until then, or until the text stops ([`Work::Break`]: a tool call, a
//!   flush, the stream's end) — so once the stream has ended nothing is
//!   waited for, and the TTS route is held no longer than before;
//! - **a batch is one paragraph's at most**: a clause that the model began
//!   on a new line — a list item, a heading, the next paragraph, the text
//!   after a skipped block — starts a batch of its own, and so do an
//!   announcement and a clause with a delivery cue (its cue is that
//!   sentence's alone); every other item ends the batch before it.
//!
//! **Where the listener runs dry** is counted from when the first audio
//! went out, as if it played from then without a break: playback cannot
//! have started earlier, and a break only moves the real end later, so the
//! speaker never waits past it. **The prediction** is this response's own
//! and errs long: the slowest request so far, and the slowest rate per
//! character — the short first request's fixed costs included — for a
//! larger batch. Nothing is capped: a batch is as long as the listener's
//! audio leaves time for, and an engine that chunks long input (audio.cpp)
//! does that itself — its joins are shortened like ours (`audio::pauses`).
//!
//! **Each clause keeps its own row** ([`Batch::split`]): the batch's audio
//! is shared out among its clauses by the characters each said, so the
//! core gets one `Msg::Clause` per clause as before — one transcript delta,
//! one heard-table row, one `speech` frame. A clause heard whole keeps what
//! the model wrote for it, and a cut is interpolated over one clause's
//! share, not the whole batch.

use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::time::Instant;

use super::super::super::heard::Written;
use super::Work;
use crate::error::GatewayError;
use crate::proxy::StopSignal;

/// One clause of a batch: what it said, and what the model wrote for it.
#[derive(Debug)]
pub(super) struct Part {
    pub said: String,
    pub written: Written,
}

/// The clauses of one TTS request, joined.
#[derive(Debug)]
pub(super) struct Batch {
    /// What the TTS gets, inline tags kept.
    pub tts: String,
    /// Its delivery cue — only ever a batch of one clause's.
    pub cue: Option<String>,
    /// Its clauses, in order.
    parts: Vec<Part>,
}

impl Batch {
    pub fn new(tts: String, said: String, written: Written, cue: Option<String>) -> Self {
        Self {
            tts,
            cue,
            parts: vec![Part { said, written }],
        }
    }

    /// Characters the TTS gets.
    pub fn chars(&self) -> usize {
        self.tts.chars().count()
    }

    fn last(&self) -> &Part {
        self.parts.last().expect("a batch has a clause")
    }

    /// More clauses may join it: no announcement, no cue (module doc).
    fn open(&self) -> bool {
        !self.last().written.announcement && self.cue.is_none()
    }

    /// Its last clause ended at a line end: a clause cut there owns the
    /// line end, and one cut at a sentence end leaves it to the next
    /// clause's `before` — a list item, a heading, a paragraph's last line.
    fn ends_line(&self) -> bool {
        self.last().written.own.ends_with('\n')
    }

    /// Join the clause `w` when it may (module doc) and the batch with it
    /// `fits` (given its characters), or give it back.
    fn join(&mut self, w: Work, fits: impl Fn(usize) -> bool) -> Option<Work> {
        match w {
            Work::Clause {
                tts,
                said,
                written,
                cue: None,
            } if !written.announcement
                && !self.ends_line()
                && !written.before.contains('\n')
                && fits(self.chars() + 1 + tts.chars().count()) =>
            {
                self.tts.push(' ');
                self.tts.push_str(&tts);
                self.parts.push(Part { said, written });
                None
            }
            other => Some(other),
        }
    }

    /// Its clauses, each with its share of `pcm` (PCM16-LE, the batch's
    /// audio): contiguous, by the characters each said (module doc), the
    /// last to the end. The audio is the same; only the rows are the
    /// clauses'.
    pub fn split(self, pcm: Bytes) -> Vec<(Part, Bytes)> {
        let samples = pcm.len() / 2;
        let chars: Vec<usize> = self.parts.iter().map(|p| p.said.chars().count()).collect();
        let total: usize = chars.iter().sum();
        let n = self.parts.len();
        let mut from = 0usize;
        let mut said = 0usize;
        self.parts
            .into_iter()
            .zip(chars)
            .enumerate()
            .map(|(k, (part, c))| {
                said += c;
                let to = if k + 1 == n || total == 0 {
                    pcm.len()
                } else {
                    let at = (samples as u128 * said as u128 / total as u128) as usize;
                    (at * 2).max(from)
                };
                let share = pcm.slice(from..to);
                from = to;
                (part, share)
            })
            .collect()
    }
}

/// What this response's synthesis has taken, and when its audio began to
/// go out (module doc).
#[derive(Debug, Default)]
pub(super) struct Pace {
    /// When the first audio went out; `None` before — nothing is gathered
    /// then.
    first_out: Option<Instant>,
    /// The slowest request so far.
    slowest: Duration,
    /// The slowest rate so far, per character the TTS got.
    per_char: Duration,
}

impl Pace {
    /// Audio has gone out: the first sets the listener's clock.
    pub fn out(&mut self) {
        self.first_out.get_or_insert_with(Instant::now);
    }

    /// A request of `chars` characters took `took`, decoding included.
    pub fn took(&mut self, chars: usize, took: Duration) {
        self.slowest = self.slowest.max(took);
        if let Ok(n) = u32::try_from(chars.max(1)) {
            self.per_char = self.per_char.max(took / n);
        }
    }

    /// How long a batch of `chars` characters is predicted to take.
    fn predict(&self, chars: usize) -> Duration {
        let by_rate =
            u32::try_from(chars).map_or(Duration::MAX, |n| self.per_char.saturating_mul(n));
        self.slowest.max(by_rate)
    }

    /// Until when a batch of `chars` characters may wait for more, `audio`
    /// gone out so far: the listener runs dry then, less twice its
    /// prediction. `None` before the first audio: the first batch waits for
    /// nothing.
    pub fn deadline(&self, audio: Duration, chars: usize) -> Option<Instant> {
        let first = self.first_out?;
        Some(first + audio.saturating_sub(self.predict(chars).saturating_mul(2)))
    }
}

/// Join the clauses after `batch`'s first that may join it (module doc):
/// each while the batch with it is still before its `deadline` (given the
/// batch's characters), and the speaker waits for more until the batch's
/// own. A `deadline` of `None` — the response's first batch — takes
/// nothing. Returns the item that ended the batch, for the speaker to take
/// next; `None` when there is none — the queue empty, closed, or a
/// [`Work::Break`] taken.
pub(super) async fn gather(
    batch: &mut Batch,
    queue: &mut mpsc::UnboundedReceiver<Work>,
    deadline: impl Fn(usize) -> Option<Instant>,
    stop: &StopSignal,
) -> Result<Option<Work>, GatewayError> {
    if deadline(batch.chars()).is_none() {
        return Ok(None);
    }
    let fits = |chars| deadline(chars).is_some_and(|at| at > Instant::now());
    while batch.open() {
        let next = match queue.try_recv() {
            Ok(w) => w,
            Err(TryRecvError::Disconnected) => return Ok(None),
            Err(TryRecvError::Empty) => {
                let Some(at) = deadline(batch.chars()).filter(|at| *at > Instant::now()) else {
                    return Ok(None);
                };
                tokio::select! {
                    biased;
                    () = stop.raised() => return Err(crate::proxy::canceled("stopped by the caller")),
                    w = queue.recv() => match w {
                        Some(w) => w,
                        None => return Ok(None),
                    },
                    () = tokio::time::sleep_until(at) => return Ok(None),
                }
            }
        };
        if matches!(next, Work::Break) {
            return Ok(None);
        }
        if let Some(other) = batch.join(next, fits) {
            return Ok(Some(other));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clause(before: &str, text: &str) -> Work {
        Work::Clause {
            tts: text.into(),
            said: text.into(),
            written: Written::said(before, text),
            cue: None,
        }
    }

    fn batch(text: &str) -> Batch {
        Batch::new(text.into(), text.into(), Written::said("", text), None)
    }

    fn said(b: &Batch) -> Vec<&str> {
        b.parts.iter().map(|p| p.said.as_str()).collect()
    }

    const ANY: fn(usize) -> bool = |_| true;

    #[test]
    fn sentences_of_one_paragraph_join_and_a_new_line_does_not() {
        let mut b = batch("Erst das.");
        assert!(b.join(clause(" ", "Dann *das*."), ANY).is_none());
        assert_eq!(b.tts, "Erst das. Dann *das*.");
        assert_eq!(said(&b), ["Erst das.", "Dann *das*."]);
        assert_eq!(b.parts[1].written.before, " ", "each keeps what it wrote");
        assert!(b.join(clause("\n\n", "Neuer Absatz."), ANY).is_some());
        // A clause cut at its line end owns it: the next one starts anew.
        let mut item = batch("Wasser holen\n");
        assert!(item.join(clause("- ", "Brot kaufen\n"), ANY).is_some());
        assert!(b.join(clause("\n- ", "Ein Punkt."), ANY).is_some());
        let announced = Work::Clause {
            tts: "Table.".into(),
            said: "Table.".into(),
            written: Written::announcing(" "),
            cue: None,
        };
        assert!(b.join(announced, ANY).is_some());
        let cued = Work::Clause {
            tts: "[laughing] Oh.".into(),
            said: "Oh.".into(),
            written: Written::said(" ", "[laughing] Oh."),
            cue: Some("laughing".into()),
        };
        assert!(b.join(cued, ANY).is_some());
        assert!(b.join(Work::Unspoken("x".into()), ANY).is_some());
        // One that would not be ready in time starts the next batch.
        let mut b = batch("Eins.");
        assert!(b.join(clause(" ", "Zwei."), |c| c <= 11).is_none());
        assert!(b.join(clause(" ", "Drei."), |c| c <= 11).is_some());
    }

    #[test]
    fn each_clause_gets_its_share_of_the_audio() {
        let mut b = batch("Eins.");
        b.join(clause(" ", "Zwei und drei."), ANY);
        b.join(clause(" ", "Vier."), ANY);
        // 24 characters said over 2400 samples: 500, 1400 and 500.
        let pcm = Bytes::from(vec![0u8; 4800]);
        let parts = b.split(pcm);
        let shares: Vec<(&str, usize)> = parts
            .iter()
            .map(|(p, pcm)| (p.said.as_str(), pcm.len() / 2))
            .collect();
        assert_eq!(
            shares,
            [("Eins.", 500), ("Zwei und drei.", 1400), ("Vier.", 500)]
        );
        // No audio at all: every clause gets none.
        let mut b = batch("Eins.");
        b.join(clause(" ", "Zwei."), ANY);
        assert!(b.split(Bytes::new()).iter().all(|(_, pcm)| pcm.is_empty()));
    }

    #[test]
    fn the_deadline_leaves_twice_the_prediction() {
        let mut p = Pace::default();
        assert_eq!(p.deadline(Duration::from_secs(3), 10), None, "before audio");
        p.out();
        let first = p.first_out.unwrap();
        // 50 chars in 100 ms: 2 ms a character, and 100 ms at least.
        p.took(50, Duration::from_millis(100));
        assert_eq!(
            p.deadline(Duration::from_secs(3), 20),
            Some(first + Duration::from_millis(2800))
        );
        assert_eq!(
            p.deadline(Duration::from_secs(3), 300),
            Some(first + Duration::from_millis(1800))
        );
        // A prediction past the audio waits for nothing.
        assert_eq!(p.deadline(Duration::from_millis(100), 300), Some(first));
    }

    #[tokio::test(start_paused = true)]
    async fn gathers_what_is_queued_then_waits_until_the_deadline() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_stop, stop) = crate::proxy::stop_pair();
        tx.send(clause(" ", "Zwei.")).unwrap();
        let mut b = batch("Eins.");
        let at = Instant::now() + Duration::from_secs(1);
        let late = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            late.send(clause(" ", "Drei.")).unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
            late.send(clause(" ", "Vier.")).unwrap();
        });
        let next = gather(&mut b, &mut rx, |_| Some(at), &stop).await.unwrap();
        assert!(next.is_none());
        assert_eq!(said(&b), ["Eins.", "Zwei.", "Drei."], "Vier came too late");
        // Past its deadline nothing joins, not even what is queued, and
        // what ended the batch comes back.
        tx.send(clause(" ", "Sechs.")).unwrap();
        let mut b = batch("Fünf.");
        let now = Some(Instant::now());
        let next = gather(&mut b, &mut rx, |_| now, &stop).await.unwrap();
        assert!(matches!(next, Some(Work::Clause { said, .. }) if said == "Sechs."));
        assert_eq!(said(&b), ["Fünf."]);
        // A break ends the batch and is taken.
        let later = Some(Instant::now() + Duration::from_secs(5));
        tx.send(clause(" ", "Acht.")).unwrap();
        tx.send(Work::Break).unwrap();
        tx.send(clause(" ", "Neun.")).unwrap();
        let mut b = batch("Sieben.");
        let next = gather(&mut b, &mut rx, |_| later, &stop).await.unwrap();
        assert!(next.is_none());
        assert_eq!(said(&b), ["Sieben.", "Acht."]);
        // The first batch takes nothing.
        let mut b = batch("Zehn.");
        assert!(gather(&mut b, &mut rx, |_| None, &stop)
            .await
            .unwrap()
            .is_none());
        assert_eq!(said(&b), ["Zehn."]);
    }
}
