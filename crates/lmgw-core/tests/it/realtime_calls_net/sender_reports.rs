//! The check of str0m's sender-report access (calls design §16, WP2):
//!
//! - **The reports lmgw receives**: str0m hands each received RTCP sender
//!   report over with its NTP time and the RTP time it names
//!   (`Event::SenderFeedback`, and `MediaData::last_sender_info`); the call
//!   passes it on as `CallEvent::SenderReport`, equal to what the client
//!   sent.
//! - **The reports lmgw sends**: str0m makes its own report's pair from the
//!   `now` it is made at — the NTP time is str0m's fixed `Instant` →
//!   wall-clock mapping of that `now`, the RTP time the last written frame's
//!   media time carried forward by `now` minus that frame's wallclock — and
//!   exposes the pair only as `RawPacket::RtcpTx` with raw packets on. So a
//!   sender that writes each frame with its wallclock on the frame clock
//!   sends reports whose pairs lie on one line, NTP − RTP/48 kHz constant:
//!   reading one sent report gives the NTP time of every frame written.

use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime};

use str0m::change::SdpAnswer;
use str0m::format::Codec;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::rtp::rtcp::Rtcp;
use str0m::rtp::RawPacket;
use str0m::{Candidate, Event, Input, Output, Rtc};

use super::client::Client;
use super::{call_event, calls_on, connect, ip, CallEvent, LO4};

fn close(a: SystemTime, b: SystemTime) -> bool {
    let d = a
        .duration_since(b)
        .or_else(|_| b.duration_since(a))
        .unwrap();
    d < Duration::from_micros(1)
}

#[tokio::test]
async fn the_clients_sender_reports_reach_the_call_with_their_times() {
    let calls = calls_on("127.0.0.1:0").await;
    let builder = Rtc::builder()
        .enable_raw_packets(true)
        .set_rtcp_report_interval_audio(Duration::from_millis(100));
    let (mut c, sdp) = Client::with(ip(LO4), builder).await;
    let answered = calls.offer(super::offer(sdp, LO4)).await.unwrap();
    c.accept(&answered.sdp);
    let mut h = answered.handle;
    connect(&mut c, &mut h).await;

    // Frames, and the reports the client sends about them.
    let mut sent = Vec::new();
    while sent.is_empty() {
        c.send_frame(&[1; 40]);
        c.run_for(Duration::from_millis(20)).await;
        c.until("the client's raw packets", |_, e| match e {
            Event::RawPacket(p) => {
                if let RawPacket::RtcpTx(Rtcp::SenderReport(sr)) = *p {
                    sent.push(sr.sender_info);
                }
                Some(())
            }
            _ => None,
        })
        .await;
    }
    let got = call_event(
        &mut c,
        &mut h.events,
        "a sender report on the call",
        |e| match e {
            CallEvent::SenderReport(r) => Some(r),
            _ => None,
        },
    )
    .await;
    let mine = sent
        .iter()
        .find(|s| close(s.ntp_time, got.ntp_time))
        .unwrap_or_else(|| panic!("{got:?} among {sent:?}"));
    assert_eq!(got.rtp_time, mine.rtp_time.numer(), "{got:?} {mine:?}");
}

/// Two str0m peers on a network in memory, on a clock the test moves.
struct Pair {
    a: Rtc,
    b: Rtc,
    now: Instant,
    /// What each side's drain gave, besides its transmits.
    a_events: Vec<Event>,
    b_events: Vec<Event>,
}

impl Pair {
    fn drain(&mut self) {
        loop {
            let mut moved = false;
            for side in [0, 1] {
                let (from, to, events) = if side == 0 {
                    (&mut self.a, &mut self.b, &mut self.a_events)
                } else {
                    (&mut self.b, &mut self.a, &mut self.b_events)
                };
                loop {
                    match from.poll_output().unwrap() {
                        Output::Timeout(_) => break,
                        Output::Transmit(t) => {
                            moved = true;
                            let r =
                                Receive::new(Protocol::Udp, t.source, t.destination, &t.contents)
                                    .unwrap();
                            to.handle_input(Input::Receive(self.now, r)).unwrap();
                        }
                        Output::Event(e) => events.push(e),
                    }
                }
            }
            if !moved {
                return;
            }
        }
    }

    fn tick(&mut self, d: Duration) {
        self.now += d;
        self.a.handle_input(Input::Timeout(self.now)).unwrap();
        self.b.handle_input(Input::Timeout(self.now)).unwrap();
        self.drain();
    }
}

#[test]
fn the_pair_lmgws_own_reports_carry_is_readable_and_follows_its_writes() {
    let start = Instant::now();
    let a_addr: SocketAddr = "192.0.2.1:1000".parse().unwrap();
    let b_addr: SocketAddr = "192.0.2.2:2000".parse().unwrap();
    // `a` is the client; `b` answers as a call does, in ICE-lite, with raw
    // packets on to read the reports it sends.
    let mut a = Rtc::builder()
        .clear_codecs()
        .enable_opus(true, false)
        .build(start);
    a.add_local_candidate(Candidate::host(a_addr, "udp").unwrap());
    let mut change = a.sdp_api();
    change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    change.add_channel("oai-events".into());
    let (offer, pending) = change.apply().unwrap();
    let mut b = Rtc::builder()
        .set_ice_lite(true)
        .clear_codecs()
        .enable_opus(true, false)
        .enable_raw_packets(true)
        .set_rtcp_report_interval_audio(Duration::from_millis(500))
        .build(start);
    b.add_local_candidate(Candidate::host(b_addr, "udp").unwrap());
    let answer = b.sdp_api().accept_offer(offer).unwrap();
    a.sdp_api()
        .accept_answer(
            pending,
            SdpAnswer::from_sdp_string(&answer.to_sdp_string()).unwrap(),
        )
        .unwrap();
    let mut p = Pair {
        a,
        b,
        now: start,
        a_events: Vec::new(),
        b_events: Vec::new(),
    };
    p.drain();
    while !p.b.is_connected() {
        p.tick(Duration::from_millis(10));
        assert!(p.now - start < Duration::from_secs(30), "the pair connects");
    }
    let mid: Mid = p
        .b_events
        .iter()
        .find_map(|e| match e {
            Event::MediaAdded(m) => Some(m.mid),
            _ => None,
        })
        .expect("the audio media");

    // `b` writes a frame every 20 ms, each with its wallclock on the frame
    // clock, as the frame clock (WP3) will.
    let t0 = p.now;
    for k in 0..300u64 {
        let wallclock = t0 + Duration::from_millis(20 * k);
        while p.now < wallclock {
            p.tick(Duration::from_millis(5));
        }
        let mut w = p.b.writer(mid).unwrap();
        let pt = w
            .payload_params()
            .find(|x| x.spec().codec == Codec::Opus)
            .unwrap()
            .pt();
        w = p.b.writer(mid).unwrap();
        w.write(
            pt,
            wallclock,
            MediaTime::new(1000 + 960 * k, Frequency::FORTY_EIGHT_KHZ),
            vec![0u8; 40],
        )
        .unwrap();
        p.drain();
    }
    let sent: Vec<_> = p
        .b_events
        .iter()
        .filter_map(|e| match e.as_raw_packet() {
            Some(RawPacket::RtcpTx(Rtcp::SenderReport(sr))) => Some(sr.sender_info),
            _ => None,
        })
        .filter(|s| s.rtp_time.numer() >= 1000)
        .collect();
    let heard: Vec<_> = p
        .a_events
        .iter()
        .filter_map(|e| match e {
            Event::SenderFeedback(f) => Some(f.sender_info),
            _ => None,
        })
        .filter(|s| s.rtp_time.numer() >= 1000)
        .collect();
    assert!(sent.len() >= 5, "{sent:?}");
    // What the client heard is what `b` sent, pair for pair.
    for h in &heard {
        let s = sent
            .iter()
            .find(|s| close(s.ntp_time, h.ntp_time))
            .unwrap_or_else(|| panic!("{h:?} among {sent:?}"));
        assert_eq!(s.rtp_time.numer(), h.rtp_time.numer());
    }
    assert!(!heard.is_empty());
    // The pairs lie on one line: NTP − RTP / 48 kHz is the same for every
    // report, to the RTP clock's resolution.
    let offset = |s: &str0m::rtp::rtcp::SenderInfo| {
        let ntp = s
            .ntp_time
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        ntp - s.rtp_time.numer() as f64 / 48_000.0
    };
    let first = offset(&sent[0]);
    for s in &sent {
        assert!(
            (offset(s) - first).abs() < 1e-4,
            "{s:?}: {} against {first}",
            offset(s)
        );
    }
}
