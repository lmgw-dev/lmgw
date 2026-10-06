//! A speaking response's decode and its back-pressure (realtime design
//! §8.2): the latter on the real writer, on tokio's paused clock, so a long
//! answer's minute of playback takes no time.

use std::time::Duration;

use tokio::time::Instant;

use super::*;
use crate::realtime::audio::pcm::write_wav_pcm16_mono;
use crate::realtime::ids::Ids;
use crate::realtime::protocol::PartRef;
use crate::realtime::writer::{self, Outbox};

use super::room::{Room, Unbind, Waited};
use crate::runtime::Placement;

#[test]
fn answers_become_24k_pcm() {
    let tone: Vec<i16> = (0..2205).map(|i| ((i % 50) * 100) as i16).collect();
    // 22.05 kHz: resampled, to exactly ceil(len · 24000 / 22050).
    let out = decode(&write_wav_pcm16_mono(&tone, 22_050).unwrap(), 0).unwrap();
    assert_eq!(out.len(), 2400);
    // 24 kHz passes through, faded in and out over 5 ms.
    let out = decode(&write_wav_pcm16_mono(&tone, 24_000).unwrap(), 0).unwrap();
    let mut faded = tone.clone();
    fade_edges(&mut faded, 120);
    assert_eq!(out, faded);
    // A rate no TTS answers with, and something that is no WAV.
    let e = decode(&write_wav_pcm16_mono(&tone, 999).unwrap(), 0).unwrap_err();
    assert!(e.to_string().contains("999 Hz"), "{e}");
    assert!(decode(b"RIFF....nope", 0).is_err());
}

const SECOND: u64 = 24_000;

fn part() -> PartRef {
    PartRef {
        response_id: "resp_1".into(),
        item_id: "item_a".into(),
        output_index: 0,
        content_index: 0,
    }
}

/// Queue `seconds` of generation `gen`'s audio with the writer, in 100 ms
/// deltas.
async fn queue(out: &writer::WriterHandle, gen: u64, seconds: u64) {
    let mut ob = Outbox::default();
    for _ in 0..seconds * 10 {
        ob.send_audio(gen, part(), bytes::Bytes::from(vec![0u8; 4800]));
    }
    ob.flush(out).await;
}

/// The GPU-side pressure of a response holding a local model, with none.
fn calm() -> Option<Unbind> {
    None
}

/// A long answer — a looping model: sixty one-second clauses, each
/// synthesized at once whenever the speaker may go on, paced with `lead`.
/// Answers when each clause started. The response holds a local model, so
/// the stall check runs throughout: a client that keeps reading is never
/// judged stalled, whatever the lead.
async fn sixty_clauses(lead: Duration) -> Vec<Duration> {
    let (tx, _frames) = futures::channel::mpsc::unbounded();
    let (out, _drained, _task) = writer::spawn(tx, std::sync::Arc::new(Ids::new()));
    let (_stop, signal) = crate::proxy::stop_pair();
    let mut progress = out.progress();
    let ahead = 2 * SECOND;
    let mut room = Room::new(Some(out.progress()), Some(ahead));
    let start = Instant::now();
    let mut ob = Outbox::default();
    ob.pace(1, lead);
    ob.flush(&out).await;
    let mut queued = 0;
    let mut started = Vec::new();
    for _ in 0..60 {
        let lifted = room.wait(1, queued, &signal, Some(&calm)).await.unwrap();
        assert_eq!(
            lifted,
            Waited::Room,
            "lead {lead:?}, clause {}",
            started.len()
        );
        started.push(start.elapsed());
        queue(&out, 1, 1).await;
        queued += SECOND;
        // Never more than the bound and the clause just made is waiting.
        let ahead_now = queued - progress.sent(1);
        assert!(
            ahead_now <= ahead + SECOND,
            "{} s queued ahead",
            ahead_now as f64 / SECOND as f64
        );
    }
    // Nothing was dropped: all of it is sent in the end.
    while progress.sent(1) < queued {
        assert!(progress.changed().await);
    }
    assert!(start.elapsed() >= Duration::from_secs(59));
    started
}

#[tokio::test(start_paused = true)]
async fn synthesis_waits_while_too_much_is_queued_ahead() {
    let started = sixty_clauses(Duration::from_millis(500)).await;
    // The first three go at once (half a second leaves with the lead);
    // after that synthesis keeps pace with the paced send, a clause a
    // second, instead of holding the whole minute in memory.
    assert!(started[2] < Duration::from_millis(10), "{started:?}");
    assert!(started[3] >= Duration::from_millis(500), "{started:?}");
    assert!(started[59] >= Duration::from_secs(56), "{started:?}");
    // No lead: every delta leaves the moment the client would run dry —
    // still no stall.
    let started = sixty_clauses(Duration::ZERO).await;
    assert!(started[59] >= Duration::from_secs(56), "{started:?}");
}

#[tokio::test(start_paused = true)]
async fn the_wait_ends_with_the_response() {
    let (tx, _frames) = futures::channel::mpsc::unbounded();
    let (out, _drained, _task) = writer::spawn(tx, std::sync::Arc::new(Ids::new()));
    let (stop, signal) = crate::proxy::stop_pair();
    // No bound: never a wait.
    let mut room = Room::new(Some(out.progress()), None);
    assert_eq!(
        room.wait(1, u64::MAX, &signal, None).await.unwrap(),
        Waited::Room
    );
    // No writer (the Chat's read-aloud): never a wait either.
    let mut room = Room::new(None, None);
    assert_eq!(
        room.wait(1, u64::MAX, &signal, None).await.unwrap(),
        Waited::Room
    );
    let mut room = Room::new(Some(out.progress()), Some(10));
    let waiting = tokio::spawn(async move { room.wait(1, 1000, &signal, None).await });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    stop.stop();
    let e = waiting.await.unwrap().unwrap_err();
    assert!(crate::proxy::is_canceled(&e), "{e}");
    // A later response speaking means this one is over.
    let (_stop, signal) = crate::proxy::stop_pair();
    let mut room = Room::new(Some(out.progress()), Some(10));
    let mut ob = Outbox::default();
    ob.pace(2, Duration::ZERO);
    ob.flush(&out).await;
    queue(&out, 2, 1).await;
    assert_eq!(
        room.wait(1, 1000, &signal, None).await.unwrap(),
        Waited::Room
    );
}

/// A socket the client stopped reading: it takes `n` frames, then never
/// another.
struct Stuck(usize);

impl futures::Sink<axum::extract::ws::Message> for Stuck {
    type Error = ();

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), ()>> {
        if self.0 > 0 {
            std::task::Poll::Ready(Ok(()))
        } else {
            std::task::Poll::Pending
        }
    }

    fn start_send(
        mut self: std::pin::Pin<&mut Self>,
        _: axum::extract::ws::Message,
    ) -> Result<(), ()> {
        self.0 -= 1;
        Ok(())
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), ()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), ()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test(start_paused = true)]
async fn a_client_that_stops_reading_keeps_the_bound() {
    // The client reads the first 300 ms, then nothing.
    let (out, _drained, _task) = writer::spawn(Stuck(3), std::sync::Arc::new(Ids::new()));
    let (_stop, signal) = crate::proxy::stop_pair();
    let mut ob = Outbox::default();
    ob.pace(1, Duration::from_millis(500));
    ob.flush(&out).await;
    queue(&out, 1, 4).await;
    let start = Instant::now();
    let mut room = Room::new(Some(out.progress()), Some(SECOND));
    // The client plays its 300 ms and runs dry; one delta later the writer
    // has missed a whole delta's slot: stalled — said, so the speaker lets
    // the model go.
    let stalled = room
        .wait(1, 4 * SECOND, &signal, Some(&calm))
        .await
        .unwrap();
    assert_eq!(stalled, Waited::Stalled);
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_millis(400) && waited < Duration::from_millis(500),
        "{waited:?}"
    );
    // B2 review 3: the bound stays. The speaker waits on without the
    // GPU-side reasons (it holds nothing now), and nothing more is
    // synthesized into a queue nobody reads.
    let wait = room.wait(1, 60 * SECOND, &signal, None);
    assert!(tokio::time::timeout(Duration::from_secs(600), wait)
        .await
        .is_err());
    // Asked again while it still holds the model: stalled again, at once.
    assert_eq!(
        room.wait(1, 60 * SECOND, &signal, Some(&calm))
            .await
            .unwrap(),
        Waited::Stalled
    );

    // A route without a local claim (a cloud TTS) is never judged stalled:
    // it holds nothing on the GPU.
    let (out, _drained, _task) = writer::spawn(Stuck(3), std::sync::Arc::new(Ids::new()));
    let mut ob = Outbox::default();
    ob.pace(1, Duration::from_millis(500));
    ob.flush(&out).await;
    queue(&out, 1, 4).await;
    let mut room = Room::new(Some(out.progress()), Some(SECOND));
    let wait = room.wait(1, 4 * SECOND, &signal, None);
    assert!(tokio::time::timeout(Duration::from_secs(600), wait)
        .await
        .is_err());
}

#[tokio::test(start_paused = true)]
async fn the_gpu_hold_lifts_the_bound_at_the_next_release() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (tx, _frames) = futures::channel::mpsc::unbounded();
    let (out, _drained, _task) = writer::spawn(tx, std::sync::Arc::new(Ids::new()));
    let (_stop, signal) = crate::proxy::stop_pair();
    let mut ob = Outbox::default();
    ob.pace(1, Duration::from_millis(500));
    ob.flush(&out).await;
    queue(&out, 1, 30).await;
    let held = std::sync::Arc::new(AtomicBool::new(false));
    let flag = held.clone();
    let pressure = move || flag.load(Ordering::SeqCst).then_some(Unbind::Hold);
    let mut room = Room::new(Some(out.progress()), Some(2 * SECOND));
    let start = Instant::now();
    let switch = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        held.store(true, Ordering::SeqCst);
    });
    // 30 s queued against a 2 s bound: without the hold this waits ~28 s;
    // the hold comes on at 5 s and lifts it at the next delta.
    let lifted = room
        .wait(1, 30 * SECOND, &signal, Some(&pressure))
        .await
        .unwrap();
    assert_eq!(lifted, Waited::Lifted(Unbind::Hold));
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_secs(5) && waited <= Duration::from_millis(5100),
        "{waited:?}"
    );
    switch.await.unwrap();
}

#[test]
fn every_clause_carries_what_the_model_wrote() {
    // B2 review 2: the speaker gets each clause as said and as written —
    // with the code before it — and what nothing says after the last one.
    let (tx, _core) = mpsc::unbounded_channel();
    let (work, mut queue) = mpsc::unbounded_channel();
    let (_stop, signal) = crate::proxy::stop_pair();
    let mut sink = Splitter::new(1, &tx, work, signal, "realtime sess_test", None);
    for d in [
        "Hier der **Befehl**:\n```sh\nls",
        " -la\n```\nFühr ihn aus.\n```sh\nrm -rf build\n```\n---",
    ] {
        sink.on_delta(&StreamDelta::TextDelta(d.into()));
    }
    sink.finish();
    drop(sink);
    let mut got = Vec::new();
    while let Ok(w) = queue.try_recv() {
        got.push(match w {
            Work::Clause { said, written, .. } => (Some(said), written.before, written.own),
            Work::Unspoken(raw) => (None, raw, String::new()),
            Work::Pass(_) | Work::Report(_) => panic!("no delta was passed"),
            Work::Break => continue,
        });
    }
    let s = |t: &str| t.to_string();
    assert_eq!(
        got,
        [
            (
                Some(s("Hier der Befehl:")),
                s(""),
                s("Hier der **Befehl**:\n")
            ),
            (
                Some(s("Führ ihn aus.")),
                s("```sh\nls -la\n```\n"),
                s("Führ ihn aus.")
            ),
            // The closing block, and a rule line with nothing to say.
            (None, s("\n```sh\nrm -rf build\n```\n---"), s("")),
        ]
    );
}

/// A clause the splitter handed the speaker: `(tts, said, before, own)`.
type Handed = (String, String, String, String);

/// Everything the splitter hands the speaker for `deltas`, in order, after
/// the stream ends.
fn handed(deltas: &[StreamDelta]) -> Vec<Work> {
    let (tx, _core) = mpsc::unbounded_channel();
    let (work, mut queue) = mpsc::unbounded_channel();
    let (_stop, signal) = crate::proxy::stop_pair();
    let mut sink = Splitter::new(1, &tx, work, signal, "realtime sess_test", None);
    for d in deltas {
        sink.on_delta(d);
    }
    sink.finish();
    drop(sink);
    std::iter::from_fn(|| queue.try_recv().ok()).collect()
}

fn texts(deltas: &[&str]) -> Vec<StreamDelta> {
    deltas
        .iter()
        .map(|d| StreamDelta::TextDelta((*d).into()))
        .collect()
}

/// What the splitter hands the speaker for `deltas`: each clause, and what
/// nothing says after the last one.
fn split(deltas: &[&str]) -> (Vec<Handed>, Vec<String>) {
    let (mut clauses, mut unspoken) = (Vec::new(), Vec::new());
    for w in handed(&texts(deltas)) {
        match w {
            Work::Clause {
                tts, said, written, ..
            } => clauses.push((tts, said, written.before, written.own)),
            Work::Unspoken(raw) => unspoken.push(raw),
            Work::Pass(_) | Work::Report(_) => panic!("no delta was passed"),
            Work::Break => {}
        }
    }
    (clauses, unspoken)
}

/// Each clause the splitter hands the speaker for `deltas`: what it says,
/// and its delivery cue.
fn cued(deltas: &[StreamDelta]) -> Vec<(String, Option<String>)> {
    handed(deltas)
        .into_iter()
        .filter_map(|w| match w {
            Work::Clause { said, cue, .. } => Some((said, cue)),
            _ => None,
        })
        .collect()
}

/// WP10 D9: the TTS gets a clause's tags, the transcript does not, and the
/// history keeps what the model wrote.
#[test]
fn a_clause_is_sent_with_its_tags_and_said_without_them() {
    let (clauses, _) = split(&["Ha *laughs* that's (laughs) funny [laughs]."]);
    let s = |t: &str| t.to_string();
    assert_eq!(
        clauses,
        [(
            s("Ha [laughs] that's [laughs] funny [laughs]."),
            s("Ha that's funny."),
            s(""),
            s("Ha *laughs* that's (laughs) funny [laughs]."),
        )]
    );
}

/// WP10 D10: a clause of nothing but tags is not sent — its tags go before
/// the next clause's words and its text with that clause; one at the end
/// is not voiced, and the history keeps it.
#[test]
fn a_clause_of_only_tags_goes_with_the_next_one_or_stays_unvoiced() {
    let (clauses, unspoken) = split(&["(laughs). ja. Gut [sighs]", "\n[sighs]"]);
    let s = |t: &str| t.to_string();
    assert_eq!(
        clauses,
        [
            (s("[laughs] ja."), s("ja."), s("(laughs). "), s("ja.")),
            (s("Gut [sighs]."), s("Gut."), s(" "), s("Gut [sighs]\n")),
        ]
    );
    assert_eq!(unspoken, ["[sighs]"], "the history keeps the last one");
    // Two tags carry together, and a tag the next clause opens with stays.
    let (clauses, _) = split(&["(laughs). (sighs). so.\n[sighs], gut."]);
    assert_eq!(clauses[0].0, "[laughs] [sighs] so.");
    assert_eq!(clauses[0].1, "so.");
    assert_eq!(clauses[1].0, "[sighs], gut.");
    assert_eq!(clauses[1].1, "gut.");
    // A tag that opens a sentence is that sentence's, not a clause of its
    // own (it was, at the first comma, before 2026-10-05).
    let (clauses, _) = split(&["[laughs], ja."]);
    assert_eq!(
        clauses,
        [(s("[laughs], ja."), s("ja."), s(""), s("[laughs], ja."))]
    );
}

/// WP9b C3: a leading cue covers the clause it opens — a whole sentence or
/// line, since 2026-10-05, where the first-comma cut once made "[laughing]
/// Oh no," a clause of its own and the cue had to be carried on — and no
/// other.
#[test]
fn a_cue_covers_the_clause_it_opens() {
    let pair = |said: &str, cue: Option<&str>| (said.to_string(), cue.map(str::to_string));
    assert_eq!(
        cued(&texts(&["[laughing] Oh no, that is funny. Next."])),
        [
            pair("Oh no, that is funny.", Some("laughing")),
            pair("Next.", None),
        ]
    );
    // One written inside a clause is no cue: the clause has the one it
    // opens with.
    assert_eq!(
        cued(&texts(&[
            "[laughing] Oh, [whispering] that is a secret. So."
        ])),
        [
            pair("Oh, that is a secret.", Some("laughing")),
            pair("So.", None),
        ]
    );
    // Several open a clause together; one written inside a clause is no
    // cue.
    assert_eq!(
        cued(&texts(&["[excited] [laughing] Wow. Oh [sighs] fine."])),
        [
            pair("Wow.", Some("excited, laughing")),
            pair("Oh fine.", None)
        ]
    );
    // The clause after a cued one has none of its own, whatever the one
    // before it had.
    assert_eq!(
        cued(&texts(&["[whispering] One. Two. [laughing] Three. Four."])),
        [
            pair("One.", Some("whispering")),
            pair("Two.", None),
            pair("Three.", Some("laughing")),
            pair("Four.", None),
        ]
    );
}

/// WP9b C3: a `!`, a `?`, a line end and a tool call's flush end a clause as
/// a full stop does, and nothing outlives it; a clause of nothing but a cue
/// rides the carry into the next one (WP10 D10).
#[test]
fn nothing_outlives_the_sentence_or_the_flush() {
    let pair = |said: &str, cue: Option<&str>| (said.to_string(), cue.map(str::to_string));
    assert_eq!(
        cued(&texts(&[
            "Hi. [excited] Really? Yes. [laughing] Wow! Fine."
        ])),
        [
            pair("Hi.", None),
            pair("Really?", Some("excited")),
            pair("Yes.", None),
            pair("Wow!", Some("laughing")),
            pair("Fine.", None),
        ]
    );
    // A closed clause: a line end, the heading's.
    assert_eq!(
        cued(&texts(&["[sighs] Well, a heading\nNext."])),
        [pair("Well, a heading.", Some("sighs")), pair("Next.", None),]
    );
    // The carry: "(laughs)" is a clause of nothing but a tag, and cues the
    // next clause.
    assert_eq!(
        cued(&texts(&["(laughs). It is a secret. Ok."])),
        [pair("It is a secret.", Some("laughs")), pair("Ok.", None)]
    );
    // A tool call flushes the text: the clause it closes has its cue, the
    // text after the call none.
    let call = StreamDelta::ToolCallStart {
        index: 0,
        id: "call_1".into(),
        name: "lookup".into(),
    };
    let mut deltas = texts(&["[laughing] Oh no, "]);
    deltas.push(call);
    deltas.extend(texts(&["that is funny."]));
    assert_eq!(
        cued(&deltas),
        [
            pair("Oh no,", Some("laughing")),
            pair("that is funny.", None)
        ]
    );
}

/// A TTS route's claim, as `before_clause` sees it: whether it holds a
/// local model, where that runs, and what was done to it.
#[derive(Default)]
struct FakeClaim {
    holds: bool,
    on: Placement,
    let_go: u32,
    pending: bool,
    regained: u32,
}

impl super::room::Claim for FakeClaim {
    fn held_on(&self) -> Option<Placement> {
        self.holds.then_some(self.on)
    }
    fn let_go(&mut self) -> bool {
        let had = std::mem::replace(&mut self.holds, false);
        self.let_go += u32::from(had);
        self.pending |= had;
        had
    }
    fn let_go_pending(&self) -> bool {
        self.pending
    }
    async fn regain(
        &mut self,
        _stop: Option<&crate::proxy::StopSignal>,
    ) -> Result<(), GatewayError> {
        self.regained += 1;
        self.pending = false;
        self.holds = true;
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn a_stall_lets_the_model_go_keeps_the_bound_and_takes_it_again_when_the_client_reads() {
    stall_lets_go(Placement::Gpu).await;
}

/// A model on the CPU is let go on a stall too: the hold has no claim on
/// it, but a benchmark's drain waits for its claim, and a client that never
/// reads again would hold that drain for good.
#[tokio::test(start_paused = true)]
async fn a_stall_lets_a_model_on_the_cpu_go_as_well() {
    stall_lets_go(Placement::Cpu).await;
}

async fn stall_lets_go(on: Placement) {
    // B2 review 3: a client that stops reading. A socket nobody reads takes
    // nothing — until the client reads again.
    use futures::StreamExt;
    let (tx, mut rx) = futures::channel::mpsc::channel(0);
    let (out, _drained, _task) = writer::spawn(tx, std::sync::Arc::new(Ids::new()));
    let (_stop, signal) = crate::proxy::stop_pair();
    let mut ob = Outbox::default();
    ob.pace(1, Duration::from_millis(500));
    ob.flush(&out).await;
    queue(&out, 1, 4).await;
    let mut room = Room::new(Some(out.progress()), Some(SECOND));
    let mut claim = FakeClaim {
        holds: true,
        on,
        ..FakeClaim::default()
    };
    let reader = {
        let waiting = super::room::before_clause(
            &mut room,
            Some(&mut claim),
            (1, 4 * SECOND),
            &signal,
            &|_| calm(),
            ("say", "sess_test"),
        );
        tokio::pin!(waiting);
        // Stalled: the model is let go, and the bound stays — nothing more
        // is synthesized while the client does not read, however long.
        assert!(
            tokio::time::timeout(Duration::from_secs(600), &mut waiting)
                .await
                .is_err(),
            "the bound stays"
        );
        // The client reads again: what is queued plays, and once the bound
        // has room the model is taken again before the next clause.
        let reader = tokio::spawn(async move { while rx.next().await.is_some() {} });
        tokio::time::timeout(Duration::from_secs(10), &mut waiting)
            .await
            .expect("room once the client reads")
            .unwrap();
        reader
    };
    assert_eq!((claim.let_go, claim.regained), (1, 1));
    assert!(claim.holds && !claim.pending);
    reader.abort();
}

#[tokio::test(start_paused = true)]
async fn the_gpu_reasons_lift_the_bound_only_for_a_route_that_holds_a_model() {
    // Item 9 of B2's review: the lift is judged only while the response's
    // TTS route holds a local model — a cloud route or a fallback keeps its
    // bound under the GPU hold.
    let (tx, _frames) = futures::channel::mpsc::unbounded();
    let (out, _drained, _task) = writer::spawn(tx, std::sync::Arc::new(Ids::new()));
    let (_stop, signal) = crate::proxy::stop_pair();
    let mut ob = Outbox::default();
    ob.pace(1, Duration::from_millis(500));
    ob.flush(&out).await;
    queue(&out, 1, 30).await;
    let held = |_| Some(Unbind::Hold);

    let mut room = Room::new(Some(out.progress()), Some(2 * SECOND));
    let mut cloud = FakeClaim::default();
    let wait = super::room::before_clause(
        &mut room,
        Some(&mut cloud),
        (1, 30 * SECOND),
        &signal,
        &held,
        ("say", "sess_test"),
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .is_err(),
        "a cloud route keeps its bound under the hold"
    );

    let mut room = Room::new(Some(out.progress()), Some(2 * SECOND));
    let mut local = FakeClaim {
        holds: true,
        ..FakeClaim::default()
    };
    let start = Instant::now();
    super::room::before_clause(
        &mut room,
        Some(&mut local),
        (1, 30 * SECOND),
        &signal,
        &held,
        ("say", "sess_test"),
    )
    .await
    .unwrap();
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "lifted at once"
    );
    assert_eq!(local.let_go, 0, "a lift keeps the claim to its last clause");
}

#[tokio::test]
async fn gpu_pressure_reads_the_hold_the_benchmark_and_the_draining_mark() {
    // Item 9 of B2's review: the three things that lift the bound, read
    // from the state they live in.
    // A model on the CPU answers to the lease only: the hold has no claim
    // on it, and no admission evicts it.
    use super::room::gpu_pressure;
    use Placement::{Cpu, Gpu};
    let state = crate::state::AppState::init_for_tests().await.unwrap();
    assert_eq!(gpu_pressure(&state, Gpu), None);
    // An owner admission waiting for room: the registry's draining mark.
    let drain = state.runtime().drain_for_owner();
    assert_eq!(gpu_pressure(&state, Gpu), Some(Unbind::RoomWanted));
    assert_eq!(gpu_pressure(&state, Cpu), None);
    drop(drain);
    assert_eq!(gpu_pressure(&state, Gpu), None, "the mark cleared");
    // A benchmark's lease — read before the mark.
    let drain = state.runtime().drain_for_owner();
    let lease = crate::bench::lease::LeaseGuard::take(&state, 7, "qwen");
    assert_eq!(gpu_pressure(&state, Gpu), Some(Unbind::Benchmark));
    assert_eq!(gpu_pressure(&state, Cpu), Some(Unbind::Benchmark));
    drop(lease);
    drop(drain);
    // The owner's GPU hold.
    let mut s = state.snapshot().settings.clone();
    s.hold.active = true;
    crate::store::save_settings(&state.db, &s).await.unwrap();
    state.reload_snapshot().await.unwrap();
    assert_eq!(gpu_pressure(&state, Gpu), Some(Unbind::Hold));
    assert_eq!(gpu_pressure(&state, Cpu), None);
    // Both: the CPU model still answers to the lease.
    let lease = crate::bench::lease::LeaseGuard::take(&state, 8, "qwen");
    assert_eq!(gpu_pressure(&state, Cpu), Some(Unbind::Benchmark));
    drop(lease);
}

/// chat-voice §7.5: `flush` says the clause in progress at once, as a tool
/// call would — a preamble is heard before the tool runs — and the splitter
/// goes on cutting what comes after.
#[test]
fn a_flush_says_the_clause_in_progress() {
    let (tx, _core) = mpsc::unbounded_channel();
    let (work, mut queue) = mpsc::unbounded_channel();
    let (_stop, signal) = crate::proxy::stop_pair();
    let mut sink = Splitter::new(1, &tx, work, signal, "realtime sess_test", None);
    let said = |queue: &mut mpsc::UnboundedReceiver<Work>| -> Vec<String> {
        std::iter::from_fn(|| queue.try_recv().ok())
            .filter_map(|w| match w {
                Work::Clause { said, .. } => Some(said),
                _ => None,
            })
            .collect()
    };
    sink.on_delta(&StreamDelta::TextDelta("Ich schaue kurz nach".into()));
    assert!(said(&mut queue).is_empty(), "an open clause waits");
    sink.flush();
    // Closed as a sentence, as the end of a stream closes one.
    assert_eq!(said(&mut queue), ["Ich schaue kurz nach."]);
    sink.on_delta(&StreamDelta::TextDelta("Gefunden: es sind zwölf.".into()));
    sink.finish();
    assert_eq!(said(&mut queue), ["Gefunden: es sind zwölf."]);
}

mod announce;
