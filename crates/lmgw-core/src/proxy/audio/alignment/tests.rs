use serde_json::{json, Value};

use super::*;

const TRANSCRIPT: &str = "Throughout the Vita Nuova, there is a strain like the first falling \
    murmur which reaches the ear in some remote meadow, and prepares us to look upon the sea.";

/// Qwen3's forced aligner, answering a 24 kHz clip
/// (trimmed to its first five words): seconds and samples, no sample rate,
/// punctuation stripped.
fn qwen3() -> Value {
    json!({
        "text": TRANSCRIPT,
        "language": "en",
        "words": [
            {"word": "Throughout", "start": 0.32, "end": 0.586667, "start_sample": 7680, "end_sample": 14080, "confidence": 0},
            {"word": "the", "start": 0.586667, "end": 0.64, "start_sample": 14080, "end_sample": 15360, "confidence": 0},
            {"word": "Vita", "start": 0.64, "end": 0.8, "start_sample": 15360, "end_sample": 19200, "confidence": 0},
            {"word": "Nuova", "start": 0.8, "end": 1.12, "start_sample": 19200, "end_sample": 26880, "confidence": 0},
            {"word": "there", "start": 1.12, "end": 1.22667, "start_sample": 26880, "end_sample": 29440, "confidence": 0}
        ]
    })
}

/// CrisperWhisper's aligner on the same clip: the same keys, punctuation
/// kept on the word.
fn crisperwhisper() -> Value {
    json!({
        "text": TRANSCRIPT,
        "language": "en",
        "words": [
            {"word": "Throughout", "start": 0.44, "end": 0.86, "start_sample": 10560, "end_sample": 20640, "confidence": 0},
            {"word": "the", "start": 0.86, "end": 0.96, "start_sample": 20640, "end_sample": 23040, "confidence": 0},
            {"word": "Vita", "start": 0.96, "end": 1.12, "start_sample": 23040, "end_sample": 26880, "confidence": 0},
            {"word": "Nuova,", "start": 1.12, "end": 1.68, "start_sample": 26880, "end_sample": 40320, "confidence": 0},
            {"word": "there", "start": 1.68, "end": 1.82, "start_sample": 40320, "end_sample": 43680, "confidence": 0}
        ]
    })
}

fn text(t: &str) -> MultipartField {
    MultipartField::Text("text".into(), t.into())
}

fn file(bytes: Vec<u8>) -> MultipartField {
    MultipartField::File("file".into(), "clip.wav".into(), None, bytes.into())
}

/// A WAV header (no samples) of `rate` Hz.
fn wav(rate: u32) -> Vec<u8> {
    let mut b = b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0".to_vec();
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(b"\x02\0\x10\0data\0\0\0\0");
    b
}

fn read_json(answer: &Value, fields: &[MultipartField]) -> Result<AudioAlignment, String> {
    read(answer.to_string().as_bytes(), fields)
}

#[test]
fn both_families_answer_one_shape_with_the_same_offsets() {
    let fields = [text(TRANSCRIPT)];
    let q = read_json(&qwen3(), &fields).unwrap();
    let c = read_json(&crisperwhisper(), &fields).unwrap();
    assert_eq!(q.text, TRANSCRIPT);
    assert_eq!(q.language.as_deref(), Some("en"));
    assert_eq!(q.words[3].word, "Nuova");
    assert_eq!(
        c.words[3].word, "Nuova,",
        "the word stays as the family wrote it"
    );
    for (a, b) in q.words.iter().zip(&c.words) {
        assert_eq!((a.char_start, a.char_end), (b.char_start, b.char_end));
    }
    let chars: Vec<char> = TRANSCRIPT.chars().collect();
    let cut: Vec<String> = c
        .words
        .iter()
        .map(|w| {
            chars[w.char_start.unwrap()..w.char_end.unwrap()]
                .iter()
                .collect()
        })
        .collect();
    assert_eq!(cut, ["Throughout", "the", "Vita", "Nuova", "there"]);
    // The family's seconds and samples come through as they are.
    assert_eq!(c.words[3].start, 1.12);
    assert_eq!(c.words[3].start_sample, Some(26880));
    assert_eq!(c.words[3].end_sample, Some(40320));
    assert_eq!(c.words[3].confidence, Some(0.0));
}

#[test]
fn the_transcript_is_the_clients_not_the_models_echo() {
    let mut answer = qwen3();
    answer["text"] = json!("something else the model made of it");
    let got = read_json(&answer, &[text(TRANSCRIPT)]).unwrap();
    assert_eq!(got.text, TRANSCRIPT);
    // Without a submitted text (the route needs one), the echo stands.
    let got = read_json(&answer, &[]).unwrap();
    assert_eq!(got.text, "something else the model made of it");
}

#[test]
fn the_sample_rate_is_the_familys_then_the_words_then_the_wavs() {
    let fields = [text(TRANSCRIPT)];
    let mut answer = qwen3();
    // Derived from the last word's samples and seconds: 29440 / 1.22667 = 24000.
    assert_eq!(
        read_json(&answer, &fields).unwrap().sample_rate,
        Some(24000)
    );
    // The words describe their own samples, so they beat the upload's header
    // (an engine that resamples leaves the header at another rate)...
    let with_wav = [text(TRANSCRIPT), file(wav(22050))];
    assert_eq!(
        read_json(&answer, &with_wav).unwrap().sample_rate,
        Some(24000)
    );
    // ...the family's own figure beats both...
    answer["sample_rate"] = json!(16000);
    assert_eq!(
        read_json(&answer, &with_wav).unwrap().sample_rate,
        Some(16000)
    );
    // ...and the header is what is left when no word has both.
    let seconds_only = json!({"words": [{"word": "the", "start": 0.2, "end": 0.4}]});
    assert_eq!(
        read_json(&seconds_only, &[text("the"), file(wav(22050))])
            .unwrap()
            .sample_rate,
        Some(22050)
    );
}

#[test]
fn the_rate_derived_from_the_words_comes_from_the_word_that_ends_last() {
    // An early word's seconds are rounded to a hundredth, which is a wrong
    // rate by itself (1600 / 0.07 = 22857); the last word is exact.
    let answer = json!({"words": [
        {"word": "a", "start": 0.0, "end": 0.07, "start_sample": 0, "end_sample": 1600},
        {"word": "b", "start": 0.07, "end": 8.0, "start_sample": 1600, "end_sample": 128000},
    ]});
    let got = read_json(&answer, &[text("a b")]).unwrap();
    assert_eq!(got.sample_rate, Some(16000));
}

#[test]
fn a_family_that_sends_only_samples_gets_seconds_from_the_rate() {
    // The 16 kHz shape of audio.cpp's earlier answers.
    let answer = json!({
        "sample_rate": 16000,
        "words": [{"word": "the", "start_sample": 3200, "end_sample": 6400, "confidence": 0.98}]
    });
    let got = read_json(&answer, &[text("The task")]).unwrap();
    let w = &got.words[0];
    assert_eq!((w.start, w.end), (0.2, 0.4));
    assert_eq!((w.start_sample, w.end_sample), (Some(3200), Some(6400)));
    assert_eq!(w.confidence, Some(0.98));
    assert_eq!((w.char_start, w.char_end), (Some(0), Some(3)));
    assert_eq!(got.language, None);
}

#[test]
fn a_family_that_sends_only_seconds_gets_samples_when_the_rate_is_known() {
    let answer = json!({"words": [{"word": "the", "start": 0.2, "end": 0.4}]});
    let known = read_json(&answer, &[text("the"), file(wav(16000))]).unwrap();
    assert_eq!(known.words[0].start_sample, Some(3200));
    assert_eq!(known.words[0].end_sample, Some(6400));
    let unknown = read_json(&answer, &[text("the")]).unwrap();
    assert_eq!(unknown.sample_rate, None);
    assert_eq!(unknown.words[0].start_sample, None);
    assert_eq!(unknown.words[0].start, 0.2, "the seconds are always there");
}

#[test]
fn a_word_the_transcript_lacks_is_kept_without_offsets() {
    let got = read_json(&qwen3(), &[text("Throughout Vita Nuova")]).unwrap();
    assert_eq!(got.words.len(), 5);
    assert_eq!(got.words[1].char_start, None, "`the` is not in the text");
    assert_eq!(got.words[2].char_start, Some(11));
    assert_eq!(got.words[4].char_start, None);
}

#[test]
fn an_answer_without_readable_word_timings_is_an_error() {
    let fields = [text("the")];
    for (answer, why) in [
        (json!({"text": "the"}), "no `words` list"),
        (json!({"words": [{"word": "the"}]}), "has no start time"),
        (
            json!({"words": [{"start": 0.1, "end": 0.2}]}),
            "has no `word`",
        ),
        (
            json!({"words": [{"word": "the", "start_sample": 5, "end_sample": 9}]}),
            "has no start time",
        ),
    ] {
        let err = read_json(&answer, &fields).unwrap_err();
        assert!(err.contains(why), "{answer}: {err}");
    }
    assert!(read(b"<html>", &fields).unwrap_err().contains("JSON"));
}

#[test]
fn an_empty_word_list_is_an_alignment_of_nothing() {
    let got = read_json(&json!({"words": []}), &[text("")]).unwrap();
    assert!(got.words.is_empty());
}

#[test]
fn the_engines_timing_comes_through() {
    let mut answer = qwen3();
    answer["timing"] = json!({"wall_ms": 288.04, "audio_duration_ms": 8335, "rtf": 0.0345579});
    let got = read_json(&answer, &[text(TRANSCRIPT)]).unwrap();
    let timing = got.timing.unwrap();
    assert_eq!(timing.wall_ms, Some(288.04));
    assert_eq!(timing.audio_duration_ms, Some(8335.0));
    assert_eq!(timing.rtf, Some(0.0345579));
    // The answer lmgw sends carries it where the Audio Lab reads it.
    let again = read_json(&answer, &[text(TRANSCRIPT)]).unwrap();
    let sent: Value = serde_json::to_value(&again).unwrap();
    assert_eq!(sent["timing"]["wall_ms"], 288.04);
    assert_eq!(sent["timing"]["audio_duration_ms"], 8335.0);
    assert_eq!(sent["timing"]["rtf"], 0.0345579);
    // None of it is invented when the engine sends none, and an odd field
    // is left out alone.
    assert!(read_json(&qwen3(), &[text(TRANSCRIPT)])
        .unwrap()
        .timing
        .is_none());
    answer["timing"] = json!({"wall_ms": "slow", "rtf": 0.5});
    let odd = read_json(&answer, &[text(TRANSCRIPT)])
        .unwrap()
        .timing
        .unwrap();
    assert_eq!((odd.wall_ms, odd.rtf), (None, Some(0.5)));
}

#[test]
fn an_incidental_field_of_an_odd_type_is_left_out_not_fatal() {
    let mut answer = qwen3();
    answer["language"] = json!(7);
    answer["sample_rate"] = json!("fast");
    answer["text"] = json!(["not", "a", "string"]);
    answer["timing"] = json!("slow");
    answer["words"][1]["confidence"] = json!("high");
    answer["words"][2]["confidence"] = json!(null);
    let got = read_json(&answer, &[text(TRANSCRIPT)]).unwrap();
    assert_eq!(got.language, None);
    assert_eq!(
        got.sample_rate,
        Some(24000),
        "derived, the stated one is odd"
    );
    assert_eq!(got.timing, None);
    assert_eq!(got.words[1].confidence, None);
    assert_eq!(got.words[2].confidence, None);
    assert_eq!(got.words[0].confidence, Some(0.0));
    assert_eq!(got.words.len(), 5);
    // Without a submitted text, an odd echo is no text.
    assert_eq!(read_json(&answer, &[]).unwrap().text, "");
}
