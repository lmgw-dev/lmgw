//! What an audio or image upstream says a call used, and the images its
//! answer holds, for its `request_logs` row.
//!
//! OpenAI reports audio usage only inside the answer, never in a header:
//! - a transcription's JSON (`json`, `diarized_json`) carries
//!   `usage: {"type": "tokens", "input_tokens", "output_tokens", …}` for the
//!   token-billed models, and `{"type": "duration", "seconds"}` for
//!   `whisper-1`, which bills by the minute: no tokens, but the seconds,
//!   which become the row's `audio_in_ms`, rounded (billable-units design
//!   §4.2);
//! - a streamed transcription's `transcript.text.done` event carries the
//!   token usage;
//! - a speech request with `stream_format: "sse"` ends with
//!   `speech.audio.done`, whose `usage` has input and output tokens.
//!
//! A binary speech answer and a `text`/`srt`/`vtt` transcript carry none, and
//! their rows record none: unknown is NULL, never 0 and never an estimate
//! from text length or audio duration. What lmgw measures itself — the
//! characters it sent, an upload's WAV length — rides in
//! `MediaOutcome::measured` and is merged under what is reported here
//! ([`Reported::quantities`]): the provider bills on its own figure.
//!
//! [`UsageTap`] reads the relayed bytes as they pass, holding nothing back from
//! the client. Which answers it reads is the endpoint's to say ([`Answer`]),
//! not the content type's: a JSON body is read only on the transcription
//! routes, so the base64 audio a `/v1/tasks/*` or speech answer can carry in
//! JSON is never copied. A local audio.cpp row is free and what it reports is
//! not read: only a cloud route is (§4.7). An image answer's images are
//! counted on every route, local ones included, by a scanner that holds
//! none of the answer ([`images`]).

use reqwest::header;
use serde_json::Value;

use crate::config::Route;
use crate::ir::Usage;
use crate::pricing::Quantities;
use crate::sse::{SseDecoder, SseEvent};
use crate::state::AppState;

mod images;

/// Which answer a relay carries, as far as its usage goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    /// The transcription family (`/v1/audio/transcriptions`, its `/details`,
    /// `/v1/audio/alignments`): a JSON body or an event stream.
    Transcript,
    /// `/v1/audio/speech`: its event stream only.
    Speech,
    /// `/v1/images/generations` and `/v1/images/edits`: the images in the
    /// answer are counted (billable-units design §4.4). A gpt-image answer's
    /// `usage` is not read: its input bills text and image tokens at
    /// different rates, and a token sheet has one input rate (§4.8).
    Image,
    /// Anything else — the task routes: nothing is read.
    Other,
}

impl Answer {
    /// The answer of the audio upstream path `path` (without the base's
    /// `/v1`). The image routes say [`Answer::Image`] themselves.
    pub(crate) fn of_path(path: &str) -> Self {
        match path {
            "/audio/transcriptions" | "/audio/transcriptions/details" | "/audio/alignments" => {
                Self::Transcript
            }
            "/audio/speech" => Self::Speech,
            _ => Self::Other,
        }
    }
}

/// What an answer said it used, and the images it held: its tokens, the
/// seconds of audio a duration-billed transcription reports, and the images
/// counted in an image answer. Each `None` unless the answer said it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Reported {
    pub(crate) usage: Usage,
    /// `whisper-1`'s `{"type": "duration", "seconds"}`, in milliseconds,
    /// rounded (§4.1).
    pub(crate) audio_in_ms: Option<u64>,
    /// The images in an image answer ([`images`]).
    pub(crate) images_out: Option<u64>,
}

impl Reported {
    /// The quantities, to be merged over lmgw's own measurement of the same
    /// request with [`Quantities::over`]: reported wins (§4.1).
    pub(crate) fn quantities(&self) -> Quantities {
        Quantities {
            audio_in_ms: self.audio_in_ms,
            images_out: self.images_out,
            ..Default::default()
        }
    }
}

/// Reads the usage out of a relayed audio answer, and counts the images in
/// an image answer.
pub(crate) struct UsageTap {
    mode: Mode,
    reported: Reported,
}

enum Mode {
    /// Nothing to read: another route, a local one, an error, a binary or
    /// plain-text body.
    Off,
    /// One JSON document, read once it is whole.
    Json(Vec<u8>),
    /// An event stream, read event by event; the last usage seen wins.
    Sse(SseDecoder),
    /// An image answer's JSON body, its images counted as it passes.
    Images(images::DataCount),
    /// An image answer's event stream, its final images counted.
    ImageEvents(images::FinalEvents),
}

impl UsageTap {
    /// The tap for `resp`, the answer `route` gave on a route of `answer`.
    pub(crate) fn new(
        state: &AppState,
        route: &Route,
        resp: &reqwest::Response,
        answer: Answer,
    ) -> Self {
        if answer == Answer::Other || !resp.status().is_success() {
            return Self::with(Mode::Off);
        }
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let events = content_type.starts_with("text/event-stream");
        // lmgw's own count, so a local sd-server row has it too (§4.7).
        if answer == Answer::Image {
            return Self::with(if events {
                Mode::ImageEvents(images::FinalEvents::default())
            } else {
                Mode::Images(images::DataCount::default())
            });
        }
        // What a provider reports is read on a cloud route only.
        if state.snapshot().is_local_upstream(route.upstream.id) {
            return Self::with(Mode::Off);
        }
        Self::with(if events {
            Mode::Sse(SseDecoder::new())
        } else if answer == Answer::Transcript && content_type.starts_with("application/json") {
            Mode::Json(Vec::new())
        } else {
            Mode::Off
        })
    }

    fn with(mode: Mode) -> Self {
        Self {
            mode,
            reported: Reported::default(),
        }
    }

    /// One relayed chunk.
    pub(crate) fn feed(&mut self, chunk: &[u8]) {
        match &mut self.mode {
            Mode::Off => {}
            Mode::Json(body) => body.extend_from_slice(chunk),
            Mode::Sse(decoder) => {
                let events = decoder.feed(chunk);
                self.note(&events);
            }
            Mode::Images(count) => count.feed(chunk),
            Mode::ImageEvents(count) => count.feed(chunk),
        }
    }

    /// What the answer reported; [`Reported::default`] when it reported
    /// nothing, or ended before it did. `whole` is whether the relay ended
    /// with the upstream's last byte: an image stream that broke off may
    /// have had more images coming, so its count is unknown then.
    pub(crate) fn finish(mut self, whole: bool) -> Reported {
        match std::mem::replace(&mut self.mode, Mode::Off) {
            Mode::Json(body) => of_body(&body),
            // A last event the upstream did not end with a blank line is still
            // its last event: the end of the stream terminates it.
            Mode::Sse(mut decoder) => {
                let events = decoder.feed(b"\n\n");
                self.note(&events);
                self.reported
            }
            Mode::Images(count) => Reported {
                images_out: count.images(),
                ..self.reported
            },
            Mode::ImageEvents(count) => Reported {
                images_out: count.images(whole),
                ..self.reported
            },
            Mode::Off => self.reported,
        }
    }

    fn note(&mut self, events: &[SseEvent]) {
        for event in events {
            if let Some(r) = event_usage(&event.data) {
                self.reported = r;
            }
        }
    }
}

/// What a whole transcription answer that `route` gave reported — what an
/// in-process caller, which reads the body itself, records.
pub(crate) fn of_answer(state: &AppState, route: &Route, body: &[u8]) -> Reported {
    if state.snapshot().is_local_upstream(route.upstream.id) {
        return Reported::default();
    }
    of_body(body)
}

/// The top-level `usage` of a JSON body; nothing for any other body.
fn of_body(body: &[u8]) -> Reported {
    serde_json::from_slice::<Value>(body)
        .map(|v| reported(v.get("usage")))
        .unwrap_or_default()
}

/// The usage of the one event of each stream that carries it.
fn event_usage(data: &str) -> Option<Reported> {
    // A speech stream is mostly base64 audio deltas: only an event that
    // mentions usage is worth parsing.
    if !data.contains("\"usage\"") {
        return None;
    }
    let v: Value = serde_json::from_str(data).ok()?;
    match v.get("type").and_then(Value::as_str) {
        Some("transcript.text.done") => Some(reported(v.get("usage"))),
        // A speech request's input is text: seconds there would be the
        // audio it made, never input audio, so only its tokens are read.
        Some("speech.audio.done") => Some(Reported {
            usage: tokens(v.get("usage")),
            ..Default::default()
        }),
        _ => None,
    }
}

/// A transcription's usage object as reported: its tokens, or its seconds
/// of input audio.
fn reported(u: Option<&Value>) -> Reported {
    Reported {
        usage: tokens(u),
        audio_in_ms: duration_ms(u),
        images_out: None,
    }
}

/// A duration usage's seconds in milliseconds, rounded (§4.1): `seconds` is
/// a JSON number and may carry a fraction. `None` for any other usage, and
/// for a value no duration can have.
fn duration_ms(u: Option<&Value>) -> Option<u64> {
    let u = u.filter(|u| u.get("type").and_then(Value::as_str) == Some("duration"))?;
    let seconds = u.get("seconds").and_then(Value::as_f64)?;
    (seconds.is_finite() && seconds >= 0.0).then(|| (seconds * 1000.0).round() as u64)
}

/// A usage object's tokens. Transcription usage names its kind in `type`
/// (`tokens` or `duration`); speech's has no `type` and is always tokens.
/// OpenAI's `input_tokens` is the billed input (text and audio), and there is
/// no cache on these routes.
fn tokens(u: Option<&Value>) -> Usage {
    let Some(u) = u else {
        return Usage::default();
    };
    if u.get("type").is_some_and(|t| t != "tokens") {
        return Usage::default();
    }
    Usage {
        prompt_tokens: u.get("input_tokens").and_then(Value::as_u64),
        completion_tokens: u.get("output_tokens").and_then(Value::as_u64),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn token_usage_is_read_as_tokens_and_duration_usage_as_audio_in_ms() {
        let tokens_body = json!({"text": "hi", "usage": {"type": "tokens", "input_tokens": 14,
            "input_token_details": {"text_tokens": 10, "audio_tokens": 4},
            "output_tokens": 101, "total_tokens": 115}});
        let r = of_body(tokens_body.to_string().as_bytes());
        assert_eq!(
            (r.usage.prompt_tokens, r.usage.completion_tokens),
            (Some(14), Some(101))
        );
        assert_eq!(r.audio_in_ms, None, "tokens are no duration");

        let duration = json!({"text": "hi", "usage": {"type": "duration", "seconds": 27}});
        let r = of_body(duration.to_string().as_bytes());
        assert_eq!(r.usage, Usage::default(), "seconds are no tokens");
        assert_eq!(r.audio_in_ms, Some(27_000));
        assert_eq!(r.quantities().audio_in_ms, Some(27_000));
        assert_eq!(of_body(b"plain text transcript"), Reported::default());
    }

    /// `seconds` is a JSON number: a fraction rounds to the millisecond.
    #[test]
    fn a_fraction_of_a_second_rounds_to_the_millisecond() {
        let ms = |seconds: Value| {
            let body = json!({"text": "hi", "usage": {"type": "duration", "seconds": seconds}});
            of_body(body.to_string().as_bytes()).audio_in_ms
        };
        assert_eq!(ms(json!(27.4)), Some(27_400));
        assert_eq!(ms(json!(0.0004)), Some(0));
        assert_eq!(ms(json!(0.0006)), Some(1));
        assert_eq!(ms(json!(1.2346)), Some(1_235));
        assert_eq!(ms(json!(0)), Some(0), "a reported 0 is a measured 0");
        assert_eq!(ms(json!(-1)), None);
        assert_eq!(ms(json!("27")), None, "not a number");
        let untyped = json!({"text": "hi", "usage": {"seconds": 27}});
        assert_eq!(of_body(untyped.to_string().as_bytes()).audio_in_ms, None);
    }

    #[test]
    fn only_the_done_events_carry_usage() {
        let done = json!({"type": "speech.audio.done",
            "usage": {"input_tokens": 14, "output_tokens": 101, "total_tokens": 115}});
        let u = event_usage(&done.to_string()).unwrap().usage;
        assert_eq!(
            (u.prompt_tokens, u.completion_tokens),
            (Some(14), Some(101))
        );
        assert!(event_usage(r#"{"type":"speech.audio.delta","audio":"AAAA"}"#).is_none());
        let other = json!({"type": "something.else", "usage": {"input_tokens": 1}});
        assert!(event_usage(&other.to_string()).is_none());
    }

    /// Seconds are input audio only on a transcription; on speech they would
    /// be the audio made, which `audio_in_ms` must never hold.
    #[test]
    fn a_duration_is_read_from_a_transcript_only() {
        let duration = json!({"type": "duration", "seconds": 4.2});
        let speech = json!({"type": "speech.audio.done", "usage": duration});
        assert_eq!(event_usage(&speech.to_string()), Some(Reported::default()));
        let transcript = json!({"type": "transcript.text.done", "text": "hi", "usage": duration});
        let r = event_usage(&transcript.to_string()).unwrap();
        assert_eq!(r.audio_in_ms, Some(4_200));
    }

    fn sse_tap() -> UsageTap {
        UsageTap::with(Mode::Sse(SseDecoder::new()))
    }

    const DONE: &str = r#"data: {"type":"speech.audio.done","usage":{"input_tokens":14,"output_tokens":101,"total_tokens":115}}"#;

    #[test]
    fn an_event_split_across_chunks_is_read_whole() {
        let mut tap = sse_tap();
        tap.feed(br#"data: {"type":"speech.audio.delta","audio":"AAAA"}"#);
        tap.feed(b"\n\n");
        let (head, tail) = DONE.split_at(40);
        tap.feed(head.as_bytes());
        tap.feed(tail.as_bytes());
        tap.feed(b"\n");
        tap.feed(b"\n");
        let u = tap.finish(true).usage;
        assert_eq!(
            (u.prompt_tokens, u.completion_tokens),
            (Some(14), Some(101))
        );
    }

    #[test]
    fn an_unterminated_last_event_still_counts() {
        for end in ["", "\n"] {
            let mut tap = sse_tap();
            tap.feed(format!("{DONE}{end}").as_bytes());
            let u = tap.finish(true).usage;
            assert_eq!(
                (u.prompt_tokens, u.completion_tokens),
                (Some(14), Some(101)),
                "ended with {end:?}"
            );
        }
    }

    #[test]
    fn only_the_transcription_family_and_speech_are_read() {
        for p in [
            "/audio/transcriptions",
            "/audio/transcriptions/details",
            "/audio/alignments",
        ] {
            assert_eq!(Answer::of_path(p), Answer::Transcript, "{p}");
        }
        assert_eq!(Answer::of_path("/audio/speech"), Answer::Speech);
        for p in ["/tasks/run", "/tasks/stream"] {
            assert_eq!(Answer::of_path(p), Answer::Other, "{p}");
        }
    }

    /// An image answer's images reach [`Reported`] through the tap, its
    /// usage left unread (§4.4), and a broken-off stream's count is unknown.
    #[test]
    fn an_image_answer_reports_its_images_and_no_tokens() {
        let body = json!({"data": [{"b64_json": "QQ=="}, {"b64_json": "Qg=="}],
            "usage": {"input_tokens": 50, "output_tokens": 4000}});
        let mut tap = UsageTap::with(Mode::Images(images::DataCount::default()));
        tap.feed(body.to_string().as_bytes());
        let r = tap.finish(false);
        assert_eq!(r.images_out, Some(2), "a whole document counts");
        assert_eq!(r.usage, Usage::default());
        assert_eq!(r.quantities().images_out, Some(2));

        let done = format!(
            "data: {}\n\n",
            json!({"type": "image_generation.completed", "b64_json": "QQ=="})
        );
        for (whole, want) in [(true, Some(1)), (false, None)] {
            let mut tap = UsageTap::with(Mode::ImageEvents(images::FinalEvents::default()));
            tap.feed(done.as_bytes());
            assert_eq!(tap.finish(whole).images_out, want, "whole: {whole}");
        }
    }
}
