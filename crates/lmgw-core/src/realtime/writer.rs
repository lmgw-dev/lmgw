//! The writer (realtime design §4.1, §4.3, §8.2): the one task that writes
//! to the socket — the link's sending half (`link::Sink`; calls design
//! §7.1), a WebSocket's today.
//!
//! Everything the session says goes through [`WriterHandle`] in order, and
//! the writer turns each event into a [`ServerFrame`] with a freshly minted
//! `event_id` at the moment it leaves — so ids are in send order, and no
//! event can go out without one.
//!
//! **Two lanes.** Events travel on a **bounded** data lane, and
//! [`Outbox::flush`] waits for room on it. That wait is the session's
//! flow control: a client that stops reading fills the socket, the writer
//! stops taking events, the session core stops handling frames while it
//! waits — and TCP pushes back on the client. Without it every frame the
//! client sent (an invalid one answers with an error ~20× its size) would
//! queue in memory without end. The bound is a window, not a limit: nothing
//! is ever refused or dropped for it. Pings and the close travel on an
//! unbounded control lane the writer always reads first. (A peer that stops
//! reading for good is the liveness check's — `liveness` — not the
//! window's.)
//!
//! **Paced output** (§8.2, `paced`). A speaking response's audio — as PCM
//! ([`Outbox::send_audio`]), encoded as it leaves — and its transcript
//! deltas ([`Outbox::send_purgeable`]) are queued with their generation, and
//! the writer moves them off the lane into the paced queue as they arrive,
//! where they wait until due: the first `output_lead_ms` of a response's
//! audio at once ([`Outbox::pace`] names the lead), then as fast as it plays
//! ([`Pacer`](super::pacing::Pacer)), re-based on when each chunk really
//! left. The writer keeps reading both lanes while it waits (WP1c review
//! H2): pings, the close and every other event go out meanwhile, so neither
//! a barge-in's events nor the liveness check queue behind seconds of audio.
//! Text output is never purgeable, so it is never paced (§8.3).
//!
//! **Purges name exact generations** (§4.3). [`WriterHandle::purge`] drops
//! what of one generation's purgeable output has not left — and nothing of
//! any other, so a cancel can never reach a previous response's output — and
//! answers, synchronously, how much of its audio did, per item: what the
//! heard table keeps after a cancel (§7.3, review H4).
//!
//! **The drained acknowledgement.** When a response's output is all queued,
//! the core queues a marker behind it ([`Outbox::drained`]). The writer
//! reports the generation back on the channel [`spawn`] returns once every
//! event before it has left — for a speaking response, once its last paced
//! audio has left *and* the client has played it: the playing window's end
//! (`paced`, `playback`; owner's decision Q1) — and only then does the core
//! send the response's closing events and `response.done` (§4.3: a cancel or
//! a barge-in while the answer still plays has a response to cancel). The
//! window itself is readable ([`WriterHandle::playback`]): the barge-in gate
//! judges input against it (§6.4).
//!
//! **The end drops what is paced** (review m8). When the session closes —
//! the close frame, or every handle gone — the events already on the lane
//! still go out, but paced audio does not: its client is going away, and
//! minutes of it pushed at once would only hold the close back.
//!
//! **A peer's close quiets the writer, it does not end it** (WP11 binding
//! review M1). Once a send fails — the client's close came in, after which
//! the socket refuses every frame, or the socket broke — the writer sends
//! nothing more and drops what reaches it, paced output included, as a
//! writer that had ended would (what left stays recorded: the heard cut is
//! read from it). But it keeps the socket until every handle is gone —
//! which the session lets go of only after a bound session's journal
//! drained — and then closes it, which flushes the close reply the socket
//! queued when the client's close arrived. So a page that leaves mid-reply
//! gets the close only once its thread holds the reply's cut and the last
//! turn; a writer that ended at the failed send dropped the socket at once,
//! and the browser saw the connection drop (1006) before the drain.
//!
//! **A link whose own clock pulls the audio** (a WebRTC call's frame clock,
//! calls design §5.1): the writer hands it the paced queue at the start
//! ([`link::Sink::pull`], [`Pulled`]), and from then on no pacer times the
//! audio — each tick of the link's clock takes its 20 ms, with the
//! transcript deltas queued ahead of it — while purges, the playback record,
//! the drained marker and `progress()` keep their meaning (`paced`). The
//! writer stops that clock wherever its own sending ends: before a bound
//! call's `lmgw.session.closed` goes out (the session's last event), at the
//! close, at the end once every handle is gone, and when the client is gone.
//! Right after the `response.done` of a response whose audio started on the
//! call it sends `output_audio_buffer.stopped`, or `.cleared` for one that
//! was cut (calls design §6.2, `paced::pulled`).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use bytes::Bytes;

use super::audio::pcm::encode_pcm16_le;
use super::audio::resample::INPUT_RATE;
use super::ids::Ids;
use super::link::{self, Close};
use super::liveness::RoundTrip;
use super::protocol::{PartRef, ServerEvent, ServerFrame};

mod outbox;
mod paced;
mod playback;
mod progress;
#[cfg(test)]
mod tests;

pub(crate) use outbox::Outbox;
pub(crate) use paced::{Cut, Pulled, Sent};
pub(crate) use playback::Playback;
pub(crate) use progress::Progress;

use paced::{Entry, Kind, Shared};

/// How many events may wait for the socket before a sender waits too. A
/// flow-control window (see the module doc), sized to hold one burst of
/// closing events and a few deltas — not a cap on anything a client sees.
const WINDOW: usize = 64;

/// The output rate audio is paced at: PCM16 at 24 kHz, the session's one
/// format (§2.2).
const OUTPUT_RATE: u32 = INPUT_RATE;

/// One entry of the data lane.
enum Data {
    Event {
        /// The generation a cancel of which drops this event; `None` for
        /// everything that is never taken back.
        purge: Option<u64>,
        event: ServerEvent,
    },
    /// Audio of generation `gen`'s item `at`: PCM16-LE, purgeable and paced.
    Audio { gen: u64, at: PartRef, pcm: Bytes },
    /// Generation `gen`'s audio runs `lead` ahead of real time.
    Pace { gen: u64, lead: Duration },
    /// Generation `g`'s output has all left: report it.
    Drained(u64),
}

/// The control lane.
enum Control {
    /// The liveness check's ping (`link::Sink::ping`).
    Ping,
    /// Send what the data lane already holds, then close the link.
    Close(Close),
}

/// The session's end of the writer. Cheap to clone; the writer ends once
/// every handle is dropped and the data lane is drained.
#[derive(Clone)]
pub(crate) struct WriterHandle {
    data: mpsc::Sender<Data>,
    control: mpsc::UnboundedSender<Control>,
    paced: Arc<Shared>,
    round_trip: Arc<RoundTrip>,
}

impl WriterHandle {
    /// Drop the purgeable output of `generation` that has not left yet —
    /// queued, or still on the lane — and nothing of any other generation;
    /// a cancel, before its `response.done` is queued. Answers the audio
    /// samples of `generation` that did leave, per item, and whether
    /// anything of it did (module doc).
    pub fn purge(&self, generation: u64) -> Sent {
        self.purge_cut(generation, Cut::Sent)
    }

    /// [`Self::purge`], where a link that pulls the audio counts what the
    /// listener heard as `cut` says (calls design §5.4); on a WebSocket the
    /// same as [`Self::purge`].
    pub fn purge_cut(&self, generation: u64, cut: Cut) -> Sent {
        self.paced.purge(generation, cut, self.round_trip.get())
    }

    /// Whether `response_id`'s audio is in a call's output buffer: its
    /// `output_audio_buffer.started` went out and its `response.done` has
    /// not (calls design §6.2). Never on a WebSocket.
    pub fn speaking(&self, response_id: &str) -> bool {
        self.paced.speaking(response_id)
    }

    /// A bound call's playout report (`lmgw.output_audio.playout`, calls
    /// design §5.3) that arrived at `arrived`: what the playout made of it,
    /// for the session's log. Nothing on a WebSocket.
    pub fn playout(
        &self,
        arrived: Instant,
        playout_ntp_ms: Option<f64>,
        jitter_buffer_ms: Option<f64>,
    ) -> Vec<crate::realtime::calls::audio::playout::Note> {
        self.paced.playout(
            arrived,
            playout_ntp_ms,
            jitter_buffer_ms,
            self.round_trip.get(),
        )
    }

    /// Whether anything of `generation`'s paced output — an audio chunk, a
    /// transcript delta — has left for the client (§4.3, B3 review 1).
    pub fn heard(&self, generation: u64) -> bool {
        self.paced.heard(generation)
    }

    /// The paced send as a speaker reads it — how much of each
    /// generation's audio has been released, which its synthesis waits on,
    /// and when the client runs dry (`progress`).
    pub fn progress(&self) -> Progress {
        Progress::new(self.paced.clone())
    }

    /// The latest speaking response's playing window, as released so far
    /// (`playback`): kept past its `response.done` until the next speaking
    /// response paces, so input captured inside it is judged by it however
    /// late it is processed (§6.4).
    pub fn playback(&self) -> Option<Playback> {
        self.paced.playback()
    }

    /// Send a ping ahead of the queued data; when it left is noted for the
    /// round trip (`liveness::round_trip`).
    pub fn ping(&self) {
        let _ = self.control.send(Control::Ping);
    }

    /// The pings' round trip, which the socket's reader completes.
    pub fn round_trip(&self) -> Arc<RoundTrip> {
        self.round_trip.clone()
    }

    /// The latest ping round trip measured, if any (`liveness::round_trip`).
    pub fn rtt(&self) -> Option<Duration> {
        self.round_trip.get()
    }

    /// Close the link, saying `close`, once the events already queued have
    /// gone out.
    pub fn close(&self, close: Close) {
        let _ = self.control.send(Control::Close(close));
    }
}

/// The drained acknowledgements: one generation each.
pub(crate) type Drained = mpsc::UnboundedReceiver<u64>;

/// Start the writer on the link's sending half — handing a link whose own
/// clock pulls the audio the paced queue first (module doc).
pub(crate) fn spawn<S: link::Sink>(
    mut sink: S,
    ids: Arc<Ids>,
) -> (WriterHandle, Drained, tokio::task::JoinHandle<()>) {
    let (data_tx, data_rx) = mpsc::channel(WINDOW);
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let (drained_tx, drained_rx) = mpsc::unbounded_channel();
    let paced = Arc::new(Shared::default());
    let round_trip = Arc::new(RoundTrip::default());
    sink.pull(Pulled::new(paced.clone(), ids.clone(), round_trip.clone()));
    let w = Writer {
        sink,
        ids,
        paced: paced.clone(),
        drained: drained_tx,
        round_trip: round_trip.clone(),
    };
    let task = tokio::spawn(w.run(data_rx, control_rx));
    (
        WriterHandle {
            data: data_tx,
            control: control_tx,
            paced,
            round_trip,
        },
        drained_rx,
        task,
    )
}

struct Writer<S> {
    sink: S,
    ids: Arc<Ids>,
    paced: Arc<Shared>,
    drained: mpsc::UnboundedSender<u64>,
    round_trip: Arc<RoundTrip>,
}

impl<S: link::Sink> Writer<S> {
    /// Send one event now. `false`: the client is gone.
    ///
    /// A bound call's `lmgw.session.closed` (calls design §8.2) is the
    /// session's last event, whichever way it reached the lane (the close's
    /// `say_closed`, a verdict's `say_closed_now`): a link's own clock stops
    /// before it goes, so nothing the clock sends follows it (`paced::pulled`).
    ///
    /// A call's `output_audio_buffer.stopped` or `.cleared` (calls design
    /// §6.2) follows the `response.done` of a response whose audio started
    /// on the call (`paced::pulled`), also when the shaper dropped that
    /// `response.done`; an emptied one's `event_too_large` errors come
    /// between (§6.5).
    async fn send(&mut self, event: ServerEvent) -> bool {
        if matches!(event, ServerEvent::LmgwSessionClosed(_)) {
            self.paced.stop_pulled();
        }
        let after = match &event {
            ServerEvent::ResponseDone { response } => self.paced.done(&response.id),
            _ => None,
        };
        if !self.send_one(event).await {
            return false;
        }
        match after {
            Some(e) => self.send_one(e).await,
            None => true,
        }
    }

    /// [`Self::send`]'s one event, its `event_id` minted now.
    async fn send_one(&mut self, event: ServerEvent) -> bool {
        let frame = ServerFrame {
            event_id: self.ids.event(),
            event,
        };
        let text = match serde_json::to_string(&frame) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("realtime: a server event did not serialize: {e}");
                return true;
            }
        };
        self.sink.event(text).await
    }

    /// Take one lane entry: send it, pace it, or drop it. `false`: the
    /// client is gone.
    async fn take(&mut self, d: Data) -> bool {
        match d {
            Data::Event { purge: None, event } => self.send(event).await,
            Data::Event {
                purge: Some(gen),
                event,
            } => {
                self.paced.push(Entry {
                    gen,
                    kind: Kind::Other(event),
                });
                true
            }
            Data::Audio { gen, at, pcm } => {
                let ready = Instant::now();
                self.paced.push(Entry {
                    gen,
                    kind: Kind::Audio { at, pcm, ready },
                });
                true
            }
            Data::Pace { gen, lead } => {
                self.paced.pace(gen, lead);
                true
            }
            Data::Drained(gen) => {
                // Behind paced output, it is reported when the paced send has
                // ended (H1); otherwise everything before it has left, and
                // its schedule is gone with it (review m8).
                if !self.paced.marker(gen) {
                    let _ = self.drained.send(gen);
                }
                true
            }
        }
    }

    /// Send every paced entry that is due; `false`: the client is gone.
    async fn release_due(&mut self) -> bool {
        loop {
            let Some(entry) = self.paced.next(Instant::now()) else {
                return true;
            };
            if !self.release(entry).await {
                return false;
            }
        }
    }

    async fn release(&mut self, entry: Entry) -> bool {
        match entry.kind {
            Kind::Audio { at, pcm, .. } => {
                // Encoded here, as it leaves: the core never spends its time
                // on base64, and the queue holds the smaller PCM.
                let samples = Kind::samples(&pcm);
                let event = ServerEvent::OutputAudioDelta {
                    at,
                    delta: encode_pcm16_le(&pcm),
                };
                let alive = self.send(event).await;
                // When it really left: a stalled socket re-bases the rest of
                // the schedule (review m7).
                self.paced.left(entry.gen, samples, Instant::now());
                alive
            }
            Kind::Other(event) => self.send(event).await,
            Kind::Drained => {
                let _ = self.drained.send(entry.gen);
                true
            }
        }
    }

    /// The session is over: what is still on the lane is taken — the
    /// unpaced events go out, the paced output is dropped with what is
    /// queued (module doc). `false`: the client is gone.
    async fn finish_lane(&mut self, data: &mut mpsc::Receiver<Data>) -> bool {
        while let Ok(d) = data.try_recv() {
            let alive = match d {
                Data::Event { purge: None, event } => self.send(event).await,
                _ => true,
            };
            if !alive {
                return false;
            }
        }
        self.paced.clear();
        true
    }

    async fn run(
        mut self,
        mut data: mpsc::Receiver<Data>,
        mut control: mpsc::UnboundedReceiver<Control>,
    ) {
        let mut control_open = true;
        loop {
            // What is due goes first, without waiting on a timer for it.
            if !self.release_due().await {
                return self.quiet(data, control).await;
            }
            let due = self.paced.next_due();
            let paced = self.paced.clone();
            let next = tokio::select! {
                // Control first: a close must reach the events behind it.
                biased;
                c = control.recv(), if control_open => match c {
                    Some(c) => Step::Control(c),
                    None => {
                        control_open = false;
                        continue;
                    }
                },
                // A purge changed what is due.
                () = paced.wake.notified() => continue,
                () = tokio::time::sleep_until(due.unwrap_or_else(Instant::now)),
                    if due.is_some() => continue,
                d = data.recv() => match d {
                    Some(d) => Step::Data(d),
                    // Every handle dropped and nothing left on the lane: a
                    // normal close, without what is paced (module doc).
                    None => {
                        self.paced.stop_pulled();
                        self.paced.clear();
                        self.sink.end().await;
                        return;
                    }
                },
            };
            let alive = match next {
                Step::Data(d) => self.take(d).await,
                // Noted on the round trip as it is sent (`link::ws`).
                Step::Control(Control::Ping) => self.sink.ping(&self.round_trip).await,
                Step::Control(Control::Close(close)) => {
                    // What was queued before the close still goes out — but
                    // not the paced audio (module doc). A link's own clock
                    // stops first (if `lmgw.session.closed` has not stopped
                    // it already), so nothing it releases follows the last
                    // events (`paced::pulled`).
                    self.paced.stop_pulled();
                    if !self.finish_lane(&mut data).await {
                        return self.quiet(data, control).await;
                    }
                    self.sink.close(close).await;
                    return;
                }
            };
            if !alive {
                // The client is gone; the reader ends the session.
                return self.quiet(data, control).await;
            }
        }
    }

    /// The client closed or the socket broke (module doc): send nothing
    /// more, drop what comes — both lanes are still read, so no sender
    /// waits on a writer that has nowhere to write — and close the socket
    /// once every handle is gone. A link's own clock stops at once: it has
    /// nowhere to send either.
    async fn quiet(
        mut self,
        mut data: mpsc::Receiver<Data>,
        mut control: mpsc::UnboundedReceiver<Control>,
    ) {
        self.paced.stop_pulled();
        let mut control_open = true;
        loop {
            tokio::select! {
                c = control.recv(), if control_open => control_open = c.is_some(),
                d = data.recv() => if d.is_none() {
                    break;
                },
            }
        }
        self.paced.clear();
        self.sink.end().await;
    }
}

/// What the writer's loop took.
enum Step {
    Control(Control),
    Data(Data),
}
