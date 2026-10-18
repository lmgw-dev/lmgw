//! The paced queue as a call's frame clock pulls it (calls design §5.1,
//! §5.3, §5.4).
//!
//! **Pulled, not timed.** On a call no pacer releases the audio: each tick
//! of the frame clock (`calls::audio::out`) takes the next 20 ms itself,
//! under this queue's lock ([`Pulled::tick`]): a chunk is taken whole and
//! cut by the frame (`calls::audio::frames`), and the transcript deltas
//! queued ahead of it leave with the tick that takes it. The writer still
//! takes everything off the lane, sends the unpaced events, and reports the
//! drained marker when it is due; only the audio's release is the clock's.
//!
//! **What the tick keeps** (§5.3), in the same critical section: the
//! record of every frame (`calls::audio::record`), the playback since the
//! last playout report (`calls::audio::playout`), and the playing window
//! ([`Playback`]): it opens with the response's first frame, ends at its
//! last frame's send end plus the playout allowance — the end the barge-in
//! gate judges by — and the drained marker is due half a round trip later
//! (or at the last frame's send end plus the measured playout delay, while
//! reports hold). The allowance moves with every receiver report and
//! playout report, so each tick sets both again from the last frame sent.
//!
//! **The session's end stops the clock first** ([`Shared::stop_pulled`]):
//! the deltas and `starved` a tick releases are handed to the link under
//! this lock, and a stopped queue gives no tick, so nothing the clock sends
//! can follow the session's last events. The writer stops it where those
//! are produced: before it sends a bound call's `lmgw.session.closed`
//! (§8.2), which the session queues and flushes ahead of the close, and on
//! every way its own sending ends (the close, every handle gone, the client
//! gone).
//!
//! **A purge** ([`Cut`], §5.4) drops what the frame clock still holds of
//! the response, and answers what the listener heard, per item, in the
//! shape a WebSocket's purge has: what was sent, exact to the frame; on a
//! bound call's barge-in what left by the allowance before the cut; at a
//! bound client's stop position, every item cut at the sample playing
//! there.
//!
//! **The call's output buffer** (§6.2, OpenAI's `output_audio_buffer.*`):
//! the tick that sends a response's first frame sends its `started`, ahead
//! of the deltas it releases, on any call; from then until its
//! `response.done` the response is in the buffer ([`Shared::speaking`],
//! what a `clear` cuts). The writer asks, as it sends that `response.done`,
//! what follows it ([`Shared::done`]): `cleared` for a response a purge cut,
//! `stopped` for one that played out, once; nothing for one that never sent
//! a frame.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use lmgw_api_types::realtime::{OutputAudioBuffer, OutputAudioStarved};
use tokio::sync::watch;
use tokio::time::Instant;

use super::super::super::ids::Ids;
use super::super::super::liveness::RoundTrip;
use super::super::super::pacing::Pacer;
use super::super::super::protocol::{ServerEvent, ServerFrame};
use super::super::playback::Playback;
use super::{Entry, Kind, Released, Shared};
use crate::realtime::calls::audio::clock::Slot;
use crate::realtime::calls::audio::frames::{self, Chunk, Frames, OutFrame};
use crate::realtime::calls::audio::heard;
use crate::realtime::calls::audio::line::SentLine;
use crate::realtime::calls::audio::playout::{Allowance, Note, Playout, Source};
use crate::realtime::calls::audio::record::Record;
use crate::realtime::calls::CallPlayout;

/// How a purge on a pulled queue counts what the listener heard (§5.4). A
/// paced queue's purge counts what was sent, whatever the cut.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Cut {
    /// What was sent, exact to the frame: a cancel, a `clear`, a stop
    /// without a position, an unbound call's barge-in.
    Sent,
    /// A bound call's barge-in: the phone mutes when `cleared` reaches it,
    /// so it played what left by the playout allowance before the cut.
    BargeIn,
    /// A bound client's stop at its playout position `playout_ntp_ms`
    /// (`lmgw.response.stop`), mapped through lmgw's sender reports.
    At(f64),
}

/// A pulled queue's own state (module doc), under the paced queue's lock.
pub(super) struct Pull {
    frames: Frames,
    record: Record,
    playout: Playout,
    /// The line lmgw's sender reports lie on, once one was read.
    line: Option<SentLine>,
    /// The latest generation whose audio was queued, and its response.
    latest: Option<(u64, String)>,
    /// The allowance last said at DEBUG: its value, source and round trip,
    /// in whole milliseconds.
    said: Option<(u128, Source, Option<u128>)>,
    /// The playout last told the status block (`CallPlayout`).
    told: Option<CallPlayout>,
    /// The latest response whose first frame was sent
    /// (`output_audio_buffer.started`), until its `response.done` went out,
    /// and whether it was cut meanwhile (module doc).
    spoke: Option<Spoke>,
    /// The session is closing: no more ticks (module doc).
    stopped: bool,
}

/// A response whose audio started on the call (`Pull::spoke`).
struct Spoke {
    response_id: String,
    /// A purge cut it: its `response.done` is followed by `cleared`, not
    /// `stopped`.
    cut: bool,
}

impl Pull {
    fn new(start: Instant, rtp0: u64, interval: Option<Duration>) -> Self {
        Self {
            frames: Frames::default(),
            record: Record::new(start, rtp0),
            playout: Playout::new(interval),
            line: None,
            latest: None,
            said: None,
            told: None,
            spoke: None,
            stopped: false,
        }
    }

    /// Audio of `generation`, of the response `response_id`, was queued.
    pub fn queued(&mut self, generation: u64, response_id: &str) {
        if self.latest.as_ref().is_none_or(|(g, _)| *g < generation) {
            self.latest = Some((generation, response_id.to_string()));
        }
    }

    /// `generation`'s response, while it is the latest that queued audio.
    fn response(&self, generation: u64) -> Option<&str> {
        self.latest
            .as_ref()
            .filter(|(g, _)| *g == generation)
            .map(|(_, r)| r.as_str())
    }

    /// Whether the frame clock still holds audio of `generation`, cut
    /// partway: not all of it has left.
    pub fn holds(&self, generation: u64) -> bool {
        self.response(generation)
            .is_some_and(|r| self.frames.holds(r))
    }

    /// `generation` was cut (module doc): what the frame clock holds of it
    /// goes, and what the listener heard of each item is answered.
    pub fn cut(
        &mut self,
        generation: u64,
        cut: Cut,
        rtt: Option<Duration>,
        now: Instant,
    ) -> HashMap<String, u64> {
        let Some(response) = self.response(generation).map(str::to_string) else {
            return HashMap::new();
        };
        if let Some(s) = self.spoke.as_mut().filter(|s| s.response_id == response) {
            s.cut = true;
        }
        let sent = self.frames.purge(&response);
        if self.record.response() != Some(response.as_str()) {
            return sent;
        }
        let (heard, how) = match cut {
            Cut::Sent => (sent.clone(), "what was sent".to_string()),
            Cut::BargeIn => {
                let a = self.playout.allowance(rtt).value;
                let less = heard::allowance_samples(a);
                (
                    heard::on_barge_in(&self.record, a, now),
                    format!("what left by the playout allowance's {less} samples before the cut"),
                )
            }
            Cut::At(ntp_ms) => match heard::at_position(&self.record, self.line.as_ref(), ntp_ms) {
                Ok((heard, clamped)) => {
                    if let Some(c) = clamped {
                        tracing::debug!("realtime response {response}: {c}");
                    }
                    (heard, format!("the stop's playout position {ntp_ms:.0}"))
                }
                Err(why) => (
                    sent.clone(),
                    format!("what was sent, the stop's playout_ntp_ms landing nowhere ({why})"),
                ),
            },
        };
        tracing::debug!(
            "realtime response {response}: cut; heard is {how}: {} of {} samples sent ({})",
            heard.values().sum::<u64>(),
            sent.values().sum::<u64>(),
            per_item(&heard, &sent)
        );
        heard
    }

    /// The session is closing: what the frame clock holds goes.
    pub fn clear(&mut self) {
        self.frames.clear();
    }

    /// When the head of `queue` is due for the writer (module doc): audio
    /// never, the frame clock takes it; a transcript delta with the audio
    /// behind it, so never too, or at once with the drained marker behind
    /// it; the drained marker at `drained` (`Playback::drained_at`), once
    /// the frame clock no longer holds its audio.
    pub fn head_due(
        &self,
        queue: &VecDeque<Entry>,
        drained: Option<Instant>,
        now: Instant,
    ) -> Option<Instant> {
        match queue.front()? {
            Entry {
                kind: Kind::Audio { .. },
                ..
            } => None,
            Entry {
                gen,
                kind: Kind::Drained,
            } => (!self.holds(*gen)).then(|| drained.unwrap_or(now)),
            Entry {
                kind: Kind::Other(_),
                ..
            } => queue.iter().skip(1).find_map(|e| match &e.kind {
                Kind::Audio { .. } => Some(None),
                Kind::Drained => Some(Some(now)),
                Kind::Other(_) => None,
            })?,
        }
    }

    /// The playing window after a frame (module doc). A response whose
    /// drained marker the writer released (its schedule is gone from
    /// `pacers`) keeps the window it had then: its answer is over, and an
    /// allowance that moves afterwards must not reopen it for the barge-in
    /// gate.
    fn window(
        &self,
        playback: Option<&mut Playback>,
        pacers: &HashMap<u64, Pacer>,
        frame: &OutFrame,
        now: Instant,
        rtt: Option<Duration>,
    ) {
        let Some(pb) = playback.filter(|p| p.ended.is_none() && pacers.contains_key(&p.gen)) else {
            return;
        };
        let Some(response) = self.response(pb.gen) else {
            return;
        };
        if self.record.response() != Some(response) {
            return;
        }
        if frame.response_id.as_deref() == Some(response) {
            pb.first.get_or_insert(now);
        }
        let Some(last) = self.record.frames().last() else {
            return;
        };
        let end = last.send_end();
        pb.play_end = Some(self.playout.window_end(end, rtt));
        pb.drained = Some(self.playout.drained(end, rtt));
    }

    /// The allowance now, when it changed since it was last said (§5.3's
    /// DEBUG line).
    fn allowance_changed(&mut self, rtt: Option<Duration>) -> Option<Allowance> {
        let a = self.playout.allowance(rtt);
        let key = (a.value.as_millis(), a.source, a.rtt.map(|r| r.as_millis()));
        (self.said.replace(key) != Some(key)).then_some(a)
    }

    /// The playout for the status block (calls design §9), when it changed
    /// since it was last told: the allowance and the round trip it was
    /// taken with in whole milliseconds, its source, the phone's jitter
    /// buffer, the latest report's arrival.
    fn playout_changed(&mut self, rtt: Option<Duration>) -> Option<CallPlayout> {
        let a = self.playout.allowance(rtt);
        let whole = |d: Duration| Duration::from_millis(d.as_millis() as u64);
        let now = CallPlayout {
            allowance: whole(a.value),
            source: a.source.wire(),
            round_trip: a.rtt.map(whole),
            jitter_buffer_ms: a.jitter_buffer_ms,
            last_report: self.playout.last_report(),
        };
        (self.told.replace(now) != Some(now)).then_some(now)
    }
}

/// Each item's samples heard and sent, in item order, for a log line.
fn per_item(heard: &HashMap<String, u64>, sent: &HashMap<String, u64>) -> String {
    let mut items: Vec<&String> = heard.keys().chain(sent.keys()).collect();
    items.sort();
    items.dedup();
    items
        .iter()
        .map(|item| {
            let n = |m: &HashMap<String, u64>| m.get(*item).copied().unwrap_or(0);
            format!("{item} {}/{}", n(heard), n(sent))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The queue as the frame clock pulls it (`frames::Pull`): a chunk and the
/// deltas ahead of it, taken under the lock.
struct Queue<'a> {
    queue: &'a mut VecDeque<Entry>,
    pacers: &'a HashMap<u64, Pacer>,
    latest: Option<&'a (u64, String)>,
    heard: &'a mut u64,
    released: &'a watch::Sender<Released>,
    /// The deltas taken, in order.
    events: Vec<ServerEvent>,
}

impl frames::Pull for Queue<'_> {
    fn chunk(&mut self) -> Option<Chunk> {
        // The deltas ahead of a chunk leave with it; ahead of the drained
        // marker, or of nothing yet, they are not the clock's to take.
        let at = self
            .queue
            .iter()
            .position(|e| !matches!(e.kind, Kind::Other(_)))?;
        if !matches!(self.queue[at].kind, Kind::Audio { .. }) {
            return None;
        }
        for e in self.queue.drain(..=at).collect::<Vec<_>>() {
            *self.heard = (*self.heard).max(e.gen);
            match e.kind {
                Kind::Other(event) => self.events.push(event),
                Kind::Audio { at, pcm, .. } => {
                    let samples = Kind::samples(&pcm);
                    self.released.send_modify(|r| {
                        if r.gen != e.gen {
                            *r = Released {
                                gen: e.gen,
                                samples: 0,
                            };
                        }
                        r.samples += samples;
                    });
                    return Some(Chunk {
                        response_id: at.response_id,
                        item_id: at.item_id,
                        pcm: pcm
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|&b| i16::from_le_bytes(b))
                            .collect(),
                    });
                }
                Kind::Drained => {}
            }
        }
        None
    }

    fn speaking(&self) -> Option<String> {
        let (generation, response) = self.latest?;
        // Its drained marker is queued: its last chunk is taken, whatever
        // waits ahead of the marker (the deltas of a clause with no audio,
        // which the writer releases at once).
        let reached = self
            .queue
            .iter()
            .any(|e| e.gen == *generation && matches!(e.kind, Kind::Drained));
        (self.pacers.contains_key(generation) && !reached).then(|| response.clone())
    }
}

/// What one tick took ([`Pulled::tick`]).
pub(crate) struct Ticked {
    /// The frame's 20 ms, for the encoder.
    pub pcm: Vec<i16>,
    /// `lmgw.output_audio.starved`, on the tick where the response ran dry
    /// or came again (C22); handed to the link on a bound call.
    pub starved: Option<OutputAudioStarved>,
    /// What the playout made of the passing time (`Playout::played`).
    pub notes: Vec<Note>,
    /// The allowance, when it changed (§5.3's DEBUG line).
    pub allowance: Option<Allowance>,
    /// The playout for the status block, when it changed (calls design §9).
    pub playout: Option<CallPlayout>,
}

/// The writer's paced queue as a link's frame clock holds it (module doc):
/// cheap to clone.
#[derive(Clone)]
pub(crate) struct Pulled {
    paced: Arc<Shared>,
    ids: Arc<Ids>,
    round_trip: Arc<RoundTrip>,
}

impl Pulled {
    pub(in crate::realtime::writer) fn new(
        paced: Arc<Shared>,
        ids: Arc<Ids>,
        round_trip: Arc<RoundTrip>,
    ) -> Self {
        Self {
            paced,
            ids,
            round_trip,
        }
    }

    /// The frame clock starts at `start` with RTP time `rtp0`; reports stop
    /// counting after `interval` of playback without one
    /// (`realtime.ping_interval_s`, `None` at 0). From here the queue is
    /// pulled.
    pub fn begin(&self, start: Instant, rtp0: u64, interval: Option<Duration>) {
        self.paced.lock().pulled = Some(Pull::new(start, rtp0, interval));
    }

    /// The tick of `slot` at `now` (module doc): the transcript deltas it
    /// released, and on a `bound` call `lmgw.output_audio.starved`, each go
    /// to `emit` as their text with an `event_id` minted as they leave, under
    /// the queue's lock. `None` before [`Self::begin`] and once the session
    /// closes ([`Shared::stop_pulled`]): the clock's end.
    pub fn tick(
        &self,
        slot: Slot,
        now: Instant,
        bound: bool,
        mut emit: impl FnMut(String),
    ) -> Option<Ticked> {
        let rtt = self.round_trip.get();
        let mut guard = self.paced.lock();
        let inner = &mut *guard;
        if inner.pulled.as_ref().is_none_or(|p| p.stopped) {
            return None;
        }
        // When the writer has something due (`Pull::head_due`): the tick
        // wakes it only when that comes earlier than it did (below).
        let due_before = inner.head_due(now);
        let pull = inner.pulled.as_mut()?;
        let mut q = Queue {
            queue: &mut inner.queue,
            pacers: &inner.pacers,
            latest: pull.latest.as_ref(),
            heard: &mut inner.heard,
            released: &self.paced.released,
            events: Vec::new(),
        };
        let frame = pull.frames.frame(&mut q);
        let events = q.events;
        pull.record.sent(slot, &frame, now);
        let notes = match frame.response_id {
            Some(_) => pull.playout.played(now).into_iter().collect(),
            None => Vec::new(),
        };
        pull.window(inner.playback.as_mut(), &inner.pacers, &frame, now, rtt);
        let allowance = pull.allowance_changed(rtt);
        let playout = pull.playout_changed(rtt);
        let starved = frame
            .starved
            .clone()
            .filter(|_| bound)
            .map(ServerEvent::LmgwOutputAudioStarved);
        // A response's first frame: `started` on any call, ahead of the
        // deltas it releases (§6.2).
        let started = frame.started.clone().map(|response_id| {
            pull.spoke = Some(Spoke {
                response_id: response_id.clone(),
                cut: false,
            });
            ServerEvent::OutputAudioBufferStarted(OutputAudioBuffer { response_id })
        });
        for e in started.into_iter().chain(events).chain(starved) {
            if let Some(text) = self.text(e) {
                emit(text);
            }
        }
        // Only a drained marker, or a delta ahead of one, can come due for
        // the writer (`Pull::head_due`). The writer sleeps until what was due
        // when it last looked, or without a timer when nothing was: it is
        // woken only when this tick made something due earlier — a delta
        // ahead of the marker reached the head, the marker no longer waits
        // for the clock to cut its last chunk, or its drained moved earlier
        // — so once per marker, not every tick while one waits.
        let due_after = inner.head_due(now);
        drop(guard);
        if due_after.is_some_and(|after| due_before.is_none_or(|before| after < before)) {
            self.paced.wake.notify_one();
        }
        Some(Ticked {
            pcm: frame.pcm,
            starved: frame.starved,
            notes,
            allowance,
            playout,
        })
    }

    /// A late tick's due `slots`, in order, at `now` (calls design §5.5):
    /// how many at the front are skipped — never sent, the record marking
    /// them so — and the allowance they were judged by. A slot due longer
    /// ago than the playout allowance would reach the phone after its
    /// playout time, and NetEQ discards such a frame, so it must not count
    /// as played (heard, the window, the playout's count); what was due in
    /// it leaves in the frames after. The last slot is never skipped, so a
    /// late clock still sends. `None` once the session closes, as for
    /// [`Self::tick`].
    pub fn skip_stale(&self, slots: &[Slot], now: Instant) -> Option<(usize, Duration)> {
        let rtt = self.round_trip.get();
        let mut inner = self.paced.lock();
        let pull = inner.pulled.as_mut().filter(|p| !p.stopped)?;
        let allowance = pull.playout.allowance(rtt).value;
        let stale = slots
            .iter()
            .take(slots.len().saturating_sub(1))
            .take_while(|s| now.saturating_duration_since(s.due) > allowance)
            .count();
        for s in &slots[..stale] {
            pull.record.skipped(*s);
        }
        Some((stale, allowance))
    }

    /// A round trip from a receiver report (`RoundTrip::reported`).
    pub fn reported(&self, rtt: Duration) {
        self.round_trip.reported(rtt);
    }

    /// The phone's jitter J, in RTP ticks, from a receiver report: what the
    /// playout made of it, for the call's log (`Playout::jitter`).
    pub fn jitter(&self, j: u32) -> Option<Note> {
        self.paced
            .lock()
            .pulled
            .as_mut()
            .and_then(|p| p.playout.jitter(j))
    }

    /// The line of lmgw's sender reports, from one it sent.
    pub fn line(&self, line: SentLine) {
        if let Some(p) = self.paced.lock().pulled.as_mut() {
            p.line = Some(line);
        }
    }

    /// The session's ids, which every event it sends is minted from: a
    /// call's link mints its parts' and errors' (`calls::sizes`).
    pub fn ids(&self) -> Arc<Ids> {
        self.ids.clone()
    }

    /// `event` as it leaves: its text, with an `event_id` minted now.
    pub fn text(&self, event: ServerEvent) -> Option<String> {
        let frame = ServerFrame {
            event_id: self.ids.event(),
            event,
        };
        match serde_json::to_string(&frame) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::error!("realtime: a server event did not serialize: {e}");
                None
            }
        }
    }
}

impl Shared {
    /// Whether `response_id`'s audio is in the call's output buffer: its
    /// first frame was sent (`output_audio_buffer.started`) and its
    /// `response.done` has not gone out (§6.2). Never on a paced queue.
    pub fn speaking(&self, response_id: &str) -> bool {
        self.lock()
            .pulled
            .as_ref()
            .and_then(|p| p.spoke.as_ref())
            .is_some_and(|s| s.response_id == response_id)
    }

    /// `response_id`'s `response.done` goes out (§6.2): if its audio
    /// started on the call, the event that follows it — `cleared` when a
    /// purge cut it, else `stopped` — once. `None` for a response that
    /// never sent a frame, and on a paced queue.
    pub fn done(&self, response_id: &str) -> Option<ServerEvent> {
        let mut inner = self.lock();
        let pull = inner.pulled.as_mut()?;
        let s = pull.spoke.take_if(|s| s.response_id == response_id)?;
        let buffer = OutputAudioBuffer {
            response_id: s.response_id,
        };
        Some(if s.cut {
            ServerEvent::OutputAudioBufferCleared(buffer)
        } else {
            ServerEvent::OutputAudioBufferStopped(buffer)
        })
    }

    /// The session closes (module doc): the frame clock gets no more ticks,
    /// and what it would have released goes. The record and what was sent
    /// stay, for the heard of a response the session's end still settles.
    pub fn stop_pulled(&self) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        if let Some(p) = inner.pulled.as_mut() {
            p.stopped = true;
            p.clear();
            inner.queue.clear();
        }
    }

    /// A playout report that arrived at `arrived` (§5.3), the round trip
    /// `rtt`: what the playout made of it, for the session's log. Nothing on
    /// a paced queue.
    pub fn playout(
        &self,
        arrived: Instant,
        playout_ntp_ms: Option<f64>,
        jitter_buffer_ms: Option<f64>,
        rtt: Option<Duration>,
    ) -> Vec<Note> {
        let mut inner = self.lock();
        let Some(p) = inner.pulled.as_mut() else {
            return Vec::new();
        };
        let Pull {
            playout,
            line,
            record,
            ..
        } = p;
        playout.report(
            arrived,
            playout_ntp_ms,
            jitter_buffer_ms,
            line.as_ref(),
            record,
            rtt,
        )
    }
}

#[cfg(test)]
mod tests;
