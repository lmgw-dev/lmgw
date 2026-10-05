//! The writer's paced queue (realtime design §4.3, §7.3, §8.2): a speaking
//! response's output, held until it is due.
//!
//! **Why it is not the data lane.** The lane is the session's flow-control
//! window: the core's flush waits for room on it. Audio paced to real time
//! sits for seconds, so on the lane it would block the core — and with it a
//! barge-in — for the whole playback. The writer therefore moves a
//! generation's purgeable output (its audio and its transcript deltas) off
//! the lane into this queue as it arrives.
//!
//! **What bounds it** is the speaker, not this queue: synthesis waits while
//! more than `synthesis_ahead_s` of a response's audio is queued and not yet
//! released ([`Shared::released`] says how much has been), so the queue
//! holds at most that much of a looping model's answer (WP3 review M3).
//! Audio waits here as **PCM** — 48 KB a second rather than the ~64 KB of
//! its base64 — and is encoded by the writer as it leaves, off the core.
//!
//! **Order is kept per generation** (WP1c review H1). Everything of a
//! generation queued behind its first paced entry stays behind its audio:
//! transcript deltas wait here with the clause they belong to, and the
//! `Drained(g)` marker is queued behind the last audio, so the drained
//! acknowledgement — and with it the closing events and `response.done` —
//! never fires before the paced send has ended (and then waits for the
//! playing window's end, below). A transcript delta leaves when
//! the first audio chunk behind it is due, just before it. (Function-call
//! items are not purgeable and are not held: a tool may run while the
//! preamble before it plays — realtime §7.4's call needs no audio.)
//!
//! **When audio is due** is its generation's [`Pacer`]'s answer, asked of
//! the head only, and told when each chunk really left
//! ([`Shared::left`]): a chunk that left late, behind a stalled socket,
//! re-bases what follows instead of letting it burst (`pacing`, review m7).
//!
//! **A purge is synchronous** (H1, H4). The queue lives behind a lock shared
//! with the core: [`Shared::purge`] removes exactly one generation's queued
//! entries and answers, from the same critical section, how many of its
//! audio samples the writer took for sending, per item. An entry is counted
//! as sent the moment the writer pops it, under the lock, so every sample
//! is either still queued (and purged) or counted — the heard table's
//! "cancel keeps what was sent" (§7.3) needs nothing better than that.
//! Entries of the generation still on the lane are dropped as the writer
//! takes them (`Purged`, below).
//!
//! **The drained marker waits for the playing window's end** (§4.3, §6.4,
//! owner's decision Q1): the response's closing events and `response.done`
//! go out when the client has played its audio, not when the last chunk
//! left `output_lead_ms` earlier — so a barge-in in that tail still has a
//! response to cancel, and the stock client keeps its playback state to the
//! real end. The window is the [`Playback`] record, written under this lock
//! wherever a generation's schedule changes; it outlives the schedule
//! (package B review 11). A purged generation's window ended at its purge,
//! so its stale marker is due at once and never holds back the next
//! response's audio behind it.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{watch, Notify};
use tokio::time::Instant;

use super::super::pacing::Pacer;
use super::super::protocol::{PartRef, ServerEvent};
use super::playback::Playback;
use super::OUTPUT_RATE;

/// One paced entry.
pub(super) struct Entry {
    pub gen: u64,
    pub kind: Kind,
}

pub(super) enum Kind {
    /// 100 ms or so of item `at`'s audio, PCM16-LE, which reached the
    /// writer at `ready`.
    Audio {
        at: PartRef,
        pcm: Bytes,
        ready: Instant,
    },
    /// Any other purgeable event: leaves with the audio behind it.
    Other(ServerEvent),
    /// The drained marker of `gen` (§4.3).
    Drained,
}

impl Kind {
    pub fn samples(pcm: &Bytes) -> u64 {
        (pcm.len() / 2) as u64
    }
}

/// How much audio of a generation the writer has released (taken for
/// sending): what a speaker's back-pressure waits on (`synthesis_ahead_s`).
/// One value for the session: generations only grow, and only the latest
/// one speaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Released {
    pub gen: u64,
    pub samples: u64,
}

/// What of a purged generation had reached the client (§4.3, §7.3): the
/// purge's answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Sent {
    /// Audio samples taken for sending, per item: what the heard table
    /// keeps.
    pub audio: HashMap<String, u64>,
    /// Anything of it left for the client — an audio chunk or a transcript
    /// delta: the client has heard some of the answer (B3 review 1).
    pub heard: bool,
}

/// The core's and the writer's shared view of the paced queue.
pub(super) struct Shared {
    inner: Mutex<Inner>,
    /// Wakes the writer when a purge changed what is due.
    pub wake: Notify,
    released: watch::Sender<Released>,
}

struct Inner {
    purged: Purged,
    queue: VecDeque<Entry>,
    /// Audio samples taken for sending, per generation and item: kept until
    /// the generation is purged or a later one paces.
    sent: HashMap<u64, HashMap<String, u64>>,
    /// The latest generation any purgeable output of which — audio or a
    /// transcript delta — has been released: the client has heard some of
    /// it. Generations only grow, and only the latest one speaks.
    heard: u64,
    /// The release schedule of each generation that paces audio.
    pacers: HashMap<u64, Pacer>,
    /// The pacers' clock starts here.
    epoch: Instant,
    /// The latest speaking response's playing window (module doc): one
    /// slot, since windows never overlap — the next response starts after
    /// this one's `response.done` (its window's end) or its cancel (a purge,
    /// which ends it).
    playback: Option<Playback>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                purged: Purged::default(),
                queue: VecDeque::new(),
                sent: HashMap::new(),
                heard: 0,
                pacers: HashMap::new(),
                epoch: Instant::now(),
                playback: None,
            }),
            wake: Notify::new(),
            released: watch::Sender::new(Released::default()),
        }
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while the lock was held cannot leave the queue half
        // edited (every edit is one push, pop or retain), so the data is
        // still sound.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Drop what of `generation`'s purgeable output has not left — queued
    /// here, or still on the lane — and answer how many of its audio
    /// samples did, per item, and whether anything of it did (module doc).
    /// Its drained marker stays: the generation's acknowledgement still
    /// comes, and is stale by then.
    pub fn purge(&self, generation: u64) -> Sent {
        let mut inner = self.lock();
        inner.purged.add(generation);
        inner
            .queue
            .retain(|e| e.gen != generation || matches!(e.kind, Kind::Drained));
        inner.pacers.remove(&generation);
        if let Some(p) = inner.playback.as_mut().filter(|p| p.gen == generation) {
            p.cut(Instant::now());
        }
        let audio = inner.sent.remove(&generation).unwrap_or_default();
        let heard = inner.heard == generation;
        drop(inner);
        self.wake.notify_one();
        Sent { audio, heard }
    }

    /// Whether anything of `generation`'s purgeable output has left for the
    /// client (module doc of [`Sent`]).
    pub fn heard(&self, generation: u64) -> bool {
        generation != 0 && self.lock().heard == generation
    }

    /// `generation`'s audio runs `lead` ahead of real time (§8.2), from its
    /// first chunk. Generations only grow: an older schedule is over, and so
    /// are its counts — only the active response is ever purged.
    pub fn pace(&self, generation: u64, lead: Duration) {
        let mut inner = self.lock();
        inner.pacers.retain(|g, _| *g > generation);
        inner.sent.retain(|g, _| *g >= generation);
        if let Ok(p) = Pacer::new(OUTPUT_RATE, lead) {
            inner.pacers.insert(generation, p);
        }
        inner.begin_playback(generation);
    }

    /// `generation`'s drained marker, taken off the lane: queued behind the
    /// paced output if there is any (H1), or — with nothing queued — until
    /// its playing window ends (module doc): `true`. `false`: due at once,
    /// its schedule over with it (review m8).
    pub fn marker(&self, generation: u64) -> bool {
        let mut inner = self.lock();
        inner.purged.drops(generation, false);
        let now = Instant::now();
        let playing = inner.window_end(generation).is_some_and(|end| end > now);
        if inner.queue.is_empty() && !playing {
            inner.pacers.remove(&generation);
            return false;
        }
        inner.queue.push_back(Entry {
            gen: generation,
            kind: Kind::Drained,
        });
        true
    }

    /// Queue `entry` behind everything paced — unless its generation was
    /// purged, which is decided under the same lock as the push, so a purge
    /// can never slip between the two.
    pub fn push(&self, entry: Entry) {
        let mut inner = self.lock();
        let event = !matches!(entry.kind, Kind::Drained);
        if inner.purged.drops(entry.gen, event) {
            return;
        }
        if let Kind::Audio { at, .. } = &entry.kind {
            // Registered at queueing, so a purge before the first send says
            // "nothing of it left" rather than nothing at all.
            inner
                .sent
                .entry(entry.gen)
                .or_default()
                .entry(at.item_id.clone())
                .or_insert(0);
            // Every speaking response names its lead first (`Outbox::pace`);
            // one that did not gets no lead rather than a guessed one.
            inner.pacers.entry(entry.gen).or_insert_with(|| {
                Pacer::new(OUTPUT_RATE, Duration::ZERO).expect("the output rate is not zero")
            });
        }
        inner.queue.push_back(entry);
    }

    /// When the head is due; `None`: nothing is, until more arrives.
    pub fn next_due(&self) -> Option<Instant> {
        self.lock().head_due(Instant::now())
    }

    /// The head, popped, if it is due at `now` — its audio counted as sent
    /// in the same breath. `None`: nothing is due yet ([`Self::next_due`]
    /// says when). An audio chunk's schedule learns when it left from
    /// [`Self::left`], once it has.
    pub fn next(&self, now: Instant) -> Option<Entry> {
        let mut inner = self.lock();
        if !inner.head_due(now).is_some_and(|d| d <= now) {
            return None;
        }
        let entry = inner.queue.pop_front()?;
        if !matches!(entry.kind, Kind::Drained) {
            inner.heard = inner.heard.max(entry.gen);
        }
        match &entry.kind {
            Kind::Audio { at, pcm, .. } => {
                let samples = Kind::samples(pcm);
                *inner
                    .sent
                    .entry(entry.gen)
                    .or_default()
                    .entry(at.item_id.clone())
                    .or_insert(0) += samples;
                self.released.send_modify(|r| {
                    if r.gen != entry.gen {
                        *r = Released {
                            gen: entry.gen,
                            samples: 0,
                        };
                    }
                    r.samples += samples;
                });
            }
            // The generation's schedule is over.
            Kind::Drained => {
                inner.pacers.remove(&entry.gen);
            }
            // Kept past the marker: a cancel can still land before the core
            // has taken the acknowledgement, and must then keep it all.
            Kind::Other(_) => {}
        }
        Some(entry)
    }

    /// An audio chunk of `generation`, `samples` long, has left at `at` —
    /// on time, or late behind a stalled socket (module doc).
    pub fn left(&self, generation: u64, samples: u64, at: Instant) {
        let mut inner = self.lock();
        let since = at.saturating_duration_since(inner.epoch);
        if let Some(p) = inner.pacers.get_mut(&generation) {
            p.release(samples, since);
        }
        // The window counts what really left, when it did: a chunk that
        // left late behind a stalled socket moves the end with it.
        inner.begin_playback(generation);
        if let Some(p) = inner.playback.as_mut().filter(|p| p.gen == generation) {
            p.released(at, samples, OUTPUT_RATE);
        }
    }

    /// The writer's releases, for a speaker to wait on.
    pub fn releases(&self) -> watch::Receiver<Released> {
        self.released.subscribe()
    }

    /// The latest speaking response's playing window (module doc), with
    /// whether its audio still waits here (`Playback::waiting`).
    pub fn playback(&self) -> Option<Playback> {
        let inner = self.lock();
        let mut p = inner.playback?;
        p.waiting = p.ended.is_none()
            && inner
                .queue
                .iter()
                .any(|e| e.gen == p.gen && matches!(e.kind, Kind::Audio { .. }));
        Some(p)
    }

    /// How many schedules are kept.
    #[cfg(test)]
    pub fn pacers(&self) -> usize {
        self.lock().pacers.len()
    }

    /// Drop everything queued — the session is closing, and its client is
    /// going away: minutes of paced audio pushed at once before the close
    /// would only delay it (review m8).
    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.queue.clear();
        inner.sent.clear();
        inner.pacers.clear();
    }
}

impl Inner {
    /// `generation` paces from here: its window takes the one slot, unless
    /// a newer one holds it. A generation purged before its lead reached
    /// the writer starts ended.
    fn begin_playback(&mut self, generation: u64) {
        if self.playback.is_some_and(|p| p.gen >= generation) {
            return;
        }
        let mut p = Playback::new(generation);
        if self.purged.contains(generation) {
            p.cut(Instant::now());
        }
        self.playback = Some(p);
    }

    /// Where `generation`'s playing window ends, if it has one and it is
    /// still the one recorded.
    fn window_end(&self, generation: u64) -> Option<Instant> {
        self.playback
            .filter(|p| p.gen == generation)
            .and_then(|p| p.end())
    }

    fn head_due(&self, now: Instant) -> Option<Instant> {
        match self.queue.front()? {
            Entry {
                gen,
                kind: Kind::Audio { pcm, ready, .. },
            } => Some(self.audio_due(*gen, pcm, *ready)),
            // At the window's end (module doc); a purged generation's ended
            // at its purge, and one with no window is due now.
            Entry {
                gen,
                kind: Kind::Drained,
            } => Some(self.window_end(*gen).unwrap_or(now)),
            // A transcript delta leaves with the first audio behind it — or
            // at once when the marker comes first (a clause with no audio):
            // with nothing behind it yet, it waits for what is on the lane.
            Entry {
                kind: Kind::Other(_),
                ..
            } => self.queue.iter().skip(1).find_map(|e| match &e.kind {
                Kind::Audio { pcm, ready, .. } => Some(self.audio_due(e.gen, pcm, *ready)),
                Kind::Drained => Some(now),
                Kind::Other(_) => None,
            }),
        }
    }

    /// When the next audio chunk of `gen` may leave: its pacer's answer,
    /// given everything before it has left.
    fn audio_due(&self, gen: u64, pcm: &Bytes, ready: Instant) -> Instant {
        let since = ready.saturating_duration_since(self.epoch);
        match self.pacers.get(&gen) {
            Some(p) => self.epoch + p.due(Kind::samples(pcm), since),
            None => ready,
        }
    }
}

/// The purged generations whose output may still be on the lane. Exact: a
/// purge of one says nothing about another. Generations on the lane only
/// grow, so a purged one is forgotten once a later one is taken; the paced
/// queue is purged directly ([`Shared::purge`]).
#[derive(Debug, Default)]
pub(super) struct Purged(BTreeSet<u64>);

impl Purged {
    pub fn add(&mut self, generation: u64) {
        self.0.insert(generation);
    }

    /// Whether an entry of `generation` is dropped rather than sent — a
    /// marker never is (`event` false).
    pub fn drops(&mut self, generation: u64, event: bool) -> bool {
        self.0 = self.0.split_off(&generation);
        event && self.0.contains(&generation)
    }

    /// Whether `generation` was purged (and not forgotten yet).
    pub fn contains(&self, generation: u64) -> bool {
        self.0.contains(&generation)
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
