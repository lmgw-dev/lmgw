//! An offer read and a call's `Rtc` made for it (calls design §2.4, §2.5,
//! §3.5): everything that can fail on the offer fails here, before any
//! `Rtc` exists.
//!
//! **The configuration** (§3.5's table): ICE-lite, so the server answers
//! checks and sends none; one host candidate, the address the client's POST
//! already proved (§3.2); Opus as the only audio codec, with RED for it when
//! the offer carries it (C7); str0m's reordering wait set to the reorder
//! window and its packet count to the window over the shortest packet plus
//! one, so the count never binds before the time does; the data channels'
//! send buffer sized so one message of the largest a call sends fits on
//! top of str0m's default ([`Checked::buffer`]); statistics every second;
//! raw packets on, the one way str0m shows the pair of the sender reports
//! lmgw sends (§5.3, §16: `audio::line`).
//! The MTU stays at str0m's 1150-byte target, which fits every WireGuard
//! MTU (§3.6). The DTLS role is what str0m answers the offer's `actpass`
//! with (passive, the DTLS server).
//!
//! **The largest message a call sends** is the smaller of the client's
//! `max-message-size` and `realtime.max_message_mb` (0: no bound). The
//! client's is str0m's assumed 64 KiB when its offer names none, and
//! sctp-proto's own largest (`u32::MAX` bytes, which str0m sets for it) when
//! it says 0, "any size" (RFC 8841). The client never sets how much lmgw
//! buffers: the writer keeps no more than str0m's default in flight before
//! it writes the next message (`driver::call`'s `flush`).
//!
//! **What the phone asks of lmgw's audio** ([`PhoneAudio`], §5.2): DTX when
//! its Opus format says `usedtx=1` (RFC 7587: the receiver's preference),
//! and its `maxaveragebitrate`, which caps `realtime.webrtc_output_kbps`.
//! str0m's own Opus matching reads `usedtx` and not `maxaveragebitrate`, so
//! that one is read from the offer's `a=fmtp` line for the Opus payload
//! type.

use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use str0m::change::SdpOffer;
use str0m::format::Codec;
use str0m::{Candidate, Rtc};

use super::handle::Limit;
use super::OfferError;

/// The data channels' send buffer str0m keeps by default (128 KiB at
/// `634e76bb`), read from str0m: what a call keeps in flight before it
/// writes its next message.
pub(super) fn sctp_window() -> usize {
    static WINDOW: OnceLock<usize> = OnceLock::new();
    *WINDOW.get_or_init(|| Rtc::builder().sctp_max_buffered_amount())
}

/// The `max-message-size` str0m assumes for a client whose offer names none
/// (its `DEFAULT_REMOTE_MAX_MESSAGE_SIZE`, not exported; §6.3).
pub(super) const ASSUMED_REMOTE_MAX_MESSAGE: usize = 64 * 1024;

/// The largest message sctp-proto sends, which str0m sets for a client
/// whose `max-message-size` is 0, "any size".
const SCTP_LARGEST_MESSAGE: usize = u32::MAX as usize;

/// The largest message str0m receives, which it advertises in every answer
/// (its `LOCAL_MAX_MESSAGE_SIZE`, not exported; §6.3): what a call's reader
/// may hold ahead of its session, one message at least.
pub const LOCAL_MAX_MESSAGE: usize = 256 * 1024;

/// The shortest Opus packet the answer lets the client send: str0m's Opus
/// format line says `minptime=10`. The reorder count is taken over it.
const SHORTEST_PACKET: Duration = Duration::from_millis(10);

/// The bounds a call takes from the settings (`CallParams`), as the
/// configuration needs them.
pub(super) struct Config {
    pub reorder: Duration,
}

/// What the phone's offer asks of lmgw's audio (module doc).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PhoneAudio {
    /// Its Opus format says `usedtx=1`.
    pub dtx: bool,
    /// Its Opus format's `maxaveragebitrate`, in bits per second.
    pub max_average: Option<u32>,
}

/// An offer, read and checked.
pub(super) struct Checked {
    offer: SdpOffer,
    /// The client offers RED for Opus.
    red: bool,
    /// What it asks of lmgw's audio.
    pub phone: PhoneAudio,
    /// The largest message the call sends (module doc), and whose bound it
    /// is.
    pub max_message: usize,
    pub by: Limit,
}

impl Checked {
    /// The data channels' send buffer: str0m's default, which the writer
    /// keeps in flight at most before a write, plus room for the largest
    /// message on top of it — str0m never takes a message larger than what
    /// is left (§6.3).
    pub fn buffer(&self) -> usize {
        sctp_window().saturating_add(self.max_message)
    }
}

/// Read `sdp` and refuse what no call could serve: an offer that does not
/// parse (str0m's reason quoted), and one without Opus audio or without a
/// data channel (§2.5). `setting` is `realtime.max_message_mb` in bytes
/// (`None`: no bound).
pub(super) fn check(sdp: &str, setting: Option<usize>) -> Result<Checked, OfferError> {
    let offer = SdpOffer::from_sdp_string(sdp).map_err(|e| OfferError::Invalid(e.to_string()))?;
    let channel = offer.media_lines.iter().find(|m| m.typ.is_channel());
    let Some(channel) = channel else {
        return Err(OfferError::Unsupported(
            "the offer has no data channel: create the data channel `oai-events` before \
             createOffer",
        ));
    };
    let client = match channel.max_message_size() {
        None => ASSUMED_REMOTE_MAX_MESSAGE,
        Some(0) => SCTP_LARGEST_MESSAGE,
        Some(n) => n.min(SCTP_LARGEST_MESSAGE),
    };
    let (max_message, by) = match setting {
        Some(s) if s < client => (s, Limit::Setting),
        _ => (client, Limit::Client),
    };
    let opus = offer
        .media_lines
        .iter()
        .flat_map(|m| m.rtp_params())
        .find(|p| p.spec().codec == Codec::Opus);
    let Some(opus) = opus else {
        return Err(OfferError::Unsupported(
            "the offer has no Opus audio: a call speaks Opus only",
        ));
    };
    let phone = PhoneAudio {
        dtx: opus.spec().format.use_dtx == Some(true),
        max_average: fmtp(sdp, *opus.pt(), "maxaveragebitrate").and_then(|v| v.parse().ok()),
    };
    Ok(Checked {
        red: opus.red().is_some(),
        phone,
        offer,
        max_message,
        by,
    })
}

/// The value of `key` in the `a=fmtp` line of payload type `pt` in `sdp`,
/// if one names it (RFC 7587 §7: `key=value` pairs, `;`-separated).
fn fmtp<'a>(sdp: &'a str, pt: u8, key: &str) -> Option<&'a str> {
    let prefix = format!("a=fmtp:{pt} ");
    sdp.lines()
        .filter_map(|l| l.trim().strip_prefix(prefix.as_str()))
        .flat_map(|params| params.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(key))
        .map(|(_, v)| v.trim())
}

/// The `Rtc` for `checked`, its one candidate `candidate`, and the answer.
pub(super) fn answer(
    checked: Checked,
    candidate: SocketAddr,
    config: &Config,
    now: Instant,
) -> Result<(Rtc, String), OfferError> {
    let window = config.reorder;
    let count = (window.as_millis() / SHORTEST_PACKET.as_millis()) as usize + 1;
    let mut rtc = Rtc::builder()
        .set_ice_lite(true)
        .clear_codecs()
        .enable_opus(true, checked.red)
        .set_reordering_timeout_audio(Some(window))
        .set_reordering_size_audio(count)
        .set_sctp_max_buffered_amount(checked.buffer())
        .set_stats_interval(Some(Duration::from_secs(1)))
        .enable_raw_packets(true)
        .build(now);
    let host = Candidate::host(candidate, "udp")
        .map_err(|e| OfferError::Unavailable(format!("the candidate {candidate}: {e}")))?;
    rtc.add_local_candidate(host);
    let answer = rtc
        .sdp_api()
        .accept_offer(checked.offer)
        .map_err(|e| OfferError::Invalid(e.to_string()))?;
    Ok((rtc, answer.to_sdp_string()))
}

#[cfg(test)]
pub(super) mod tests {
    use str0m::media::{Direction, MediaKind};

    use super::*;

    /// A browser-like offer from a str0m client: audio send-receive with
    /// Opus (and RED when `red`), and `oai-events`.
    pub(crate) fn client_offer(red: bool, channel: bool) -> String {
        let mut rtc = Rtc::builder()
            .clear_codecs()
            .enable_opus(true, red)
            .build(Instant::now());
        rtc.add_local_candidate(
            Candidate::host("127.0.0.1:40000".parse().unwrap(), "udp").unwrap(),
        );
        let mut change = rtc.sdp_api();
        change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        if channel {
            change.add_channel("oai-events".into());
        }
        change.apply().unwrap().0.to_sdp_string()
    }

    fn config() -> Config {
        Config {
            reorder: Duration::from_millis(60),
        }
    }

    #[test]
    fn the_answer_is_ice_lite_opus_only_with_one_candidate() {
        let checked = check(&client_offer(false, true), None).unwrap();
        assert_eq!(checked.max_message, LOCAL_MAX_MESSAGE, "a str0m client's");
        let cand: SocketAddr = "127.0.0.1:8787".parse().unwrap();
        let (_rtc, sdp) = answer(checked, cand, &config(), Instant::now()).unwrap();
        assert!(sdp.contains("a=ice-lite"), "{sdp}");
        assert!(sdp.contains("opus/48000/2"), "{sdp}");
        assert!(!sdp.contains("red/48000"), "no RED unless offered: {sdp}");
        assert!(!sdp.contains("PCMU") && !sdp.contains("PCMA"), "{sdp}");
        let candidates: Vec<&str> = sdp
            .lines()
            .filter(|l| l.starts_with("a=candidate"))
            .collect();
        assert_eq!(candidates.len(), 1, "{sdp}");
        assert!(candidates[0].contains("127.0.0.1 8787 typ host"), "{sdp}");
        assert!(sdp.contains("webrtc-datachannel"), "{sdp}");
        assert!(sdp.contains("a=setup:passive"), "{sdp}");
        // The bound a call's reader holds ahead is the one str0m advertises.
        assert!(
            sdp.contains(&format!("a=max-message-size:{LOCAL_MAX_MESSAGE}")),
            "{sdp}"
        );
    }

    #[test]
    fn what_the_phone_asks_of_lmgws_audio_is_read_from_its_opus_format() {
        let plain = check(&client_offer(false, true), None).unwrap();
        assert_eq!(plain.phone, PhoneAudio::default());
        // libwebrtc's line, with the phone's preferences on it.
        let sdp = client_offer(false, true);
        let fmtp = sdp
            .lines()
            .find(|l| l.starts_with("a=fmtp:111 "))
            .expect("str0m's Opus format line")
            .to_string();
        let asks = sdp.replace(&fmtp, &format!("{fmtp};usedtx=1; maxaveragebitrate=20000"));
        let c = check(&asks, None).unwrap();
        assert_eq!(
            c.phone,
            PhoneAudio {
                dtx: true,
                max_average: Some(20_000)
            }
        );
    }

    #[test]
    fn red_is_answered_when_offered() {
        let checked = check(&client_offer(true, true), None).unwrap();
        let (_rtc, sdp) = answer(
            checked,
            "127.0.0.1:8787".parse().unwrap(),
            &config(),
            Instant::now(),
        )
        .unwrap();
        assert!(sdp.contains("red/48000/2"), "{sdp}");
    }

    #[test]
    fn an_offer_without_a_channel_or_opus_or_that_does_not_parse_is_refused() {
        assert!(matches!(
            check(&client_offer(false, false), None),
            Err(OfferError::Unsupported(m)) if m.contains("oai-events")
        ));
        assert!(matches!(
            check("v=0\r\nnonsense", None),
            Err(OfferError::Invalid(_))
        ));
        // An application-only offer: no audio at all.
        let mut rtc = Rtc::new(Instant::now());
        rtc.add_local_candidate(
            Candidate::host("127.0.0.1:40000".parse().unwrap(), "udp").unwrap(),
        );
        let mut change = rtc.sdp_api();
        change.add_channel("oai-events".into());
        let sdp = change.apply().unwrap().0.to_sdp_string();
        assert!(matches!(
            check(&sdp, None),
            Err(OfferError::Unsupported(m)) if m.contains("Opus")
        ));
    }

    #[test]
    fn the_largest_message_is_the_smaller_bound_and_the_buffer_stays_finite() {
        let mb = |n: usize| n << 20;
        let any = client_offer(false, true).replace(
            &format!("a=max-message-size:{LOCAL_MAX_MESSAGE}"),
            "a=max-message-size:0",
        );
        let c = check(&any, Some(mb(7))).unwrap();
        assert_eq!((c.max_message, c.by), (mb(7), Limit::Setting));
        // "Any size" and no bound of lmgw's: sctp-proto's own largest, and a
        // buffer that is a number, never usize::MAX.
        let c = check(&any, None).unwrap();
        assert_eq!((c.max_message, c.by), (u32::MAX as usize, Limit::Client));
        assert_eq!(c.buffer(), sctp_window() + u32::MAX as usize);
        let c = check(&client_offer(false, true), Some(mb(64))).unwrap();
        assert_eq!((c.max_message, c.by), (LOCAL_MAX_MESSAGE, Limit::Client));
        assert_eq!(c.buffer(), sctp_window() + LOCAL_MAX_MESSAGE);
        let c = check(&client_offer(false, true), Some(1000)).unwrap();
        assert_eq!((c.max_message, c.by), (1000, Limit::Setting));
        let none = client_offer(false, true)
            .replace(&format!("a=max-message-size:{LOCAL_MAX_MESSAGE}\r\n"), "");
        assert_eq!(
            check(&none, None).unwrap().max_message,
            ASSUMED_REMOTE_MAX_MESSAGE
        );
        assert_eq!(sctp_window(), 128 * 1024, "str0m's default at 634e76bb");
    }
}
