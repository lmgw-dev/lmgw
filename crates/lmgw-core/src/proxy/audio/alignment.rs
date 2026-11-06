//! `POST /v1/audio/alignments` answers one shape, whatever family aligned:
//! [`AudioAlignment`].
//!
//! audio.cpp's families each answer in their own dialect (module doc of
//! `lmgw_api_types::audio_alignment`). The route reads the answer here
//! instead of relaying it: the transcript is the client's own, the seconds
//! are always there, the sample rate is stated once, and every word says
//! where it sits in the transcript ([`offsets`]). An answer with no word
//! timings lmgw can read is a gateway error naming the family, never
//! something untyped passed on.
//!
//! Only a successful answer is read; a refusal is relayed as the other
//! audio routes relay theirs.

use axum::http::{header, Response};
use lmgw_api_types::audio_alignment::{AlignedWord, AudioAlignment, AudioTiming};
use serde_json::Value;

use super::MultipartField;
use crate::audio::rates::wav_rate;
use crate::config::Route;
use crate::error::GatewayError;
use crate::state::SharedState;

mod offsets;

/// What a family sends for one word, read leniently: the word and its times
/// are what the answer cannot do without, so a field of the wrong type is as
/// good as a missing one there (the answer is refused for it), and the
/// confidence, which is only incidental, is dropped when it is not a number.
struct RawWord {
    word: Option<String>,
    start: Option<f64>,
    end: Option<f64>,
    start_sample: Option<f64>,
    end_sample: Option<f64>,
    confidence: Option<f32>,
}

impl RawWord {
    fn of(v: &Value) -> RawWord {
        let num = |key: &str| v.get(key).and_then(Value::as_f64);
        RawWord {
            word: v.get("word").and_then(Value::as_str).map(str::to_owned),
            start: num("start"),
            end: num("end"),
            start_sample: num("start_sample"),
            end_sample: num("end_sample"),
            confidence: num("confidence").map(|c| c as f32),
        }
    }
}

/// The engine's `timing` object, field by field: one of the wrong type is
/// dropped, the rest stay.
fn timing_of(answer: &Value) -> Option<AudioTiming> {
    let t = answer.get("timing")?;
    let num = |key: &str| t.get(key).and_then(Value::as_f64);
    let timing = AudioTiming {
        wall_ms: num("wall_ms"),
        audio_duration_ms: num("audio_duration_ms"),
        rtf: num("rtf"),
    };
    (timing != AudioTiming::default()).then_some(timing)
}

/// `resp`, a successful upstream answer of `/audio/alignments`, read into
/// the typed shape and re-made as a JSON response (same status; its
/// content type and length are the typed body's).
pub(super) async fn typed(
    state: &SharedState,
    route: &Route,
    fields: &[MultipartField],
    resp: reqwest::Response,
) -> Result<reqwest::Response, GatewayError> {
    let status = resp.status();
    let body = resp
        .bytes()
        .await
        .map_err(|e| GatewayError::Transport(e.to_string()))?;
    // Matching the words to the transcript is CPU work that grows with a long
    // one, so it runs off the async threads.
    let upload = fields.to_vec();
    let read = tokio::task::spawn_blocking(move || read(&body, &upload))
        .await
        .map_err(|e| GatewayError::Internal(e.to_string()))?;
    let answer = match read {
        Ok(a) => a,
        Err(why) => {
            let family = family_of(state, route).await;
            return Err(GatewayError::Upstream {
                status: 502,
                provider_type: None,
                message: format!(
                    "the {family} alignment model's answer has no word timings lmgw can read \
                     ({why}), so /v1/audio/alignments has nothing to answer with"
                ),
            });
        }
    };
    let json = serde_json::to_vec(&answer).map_err(|e| GatewayError::Internal(e.to_string()))?;
    let made = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, json.len())
        .body(json)
        .map_err(|e| GatewayError::Internal(e.to_string()))?;
    Ok(made.into())
}

/// The name of what answered, for a message: the audio row's family
/// (`crisperwhisper`), else the model id the route sent to.
async fn family_of(state: &SharedState, route: &Route) -> String {
    let snap = state.snapshot();
    match crate::audio::voices::row_of_route(&snap, route) {
        Some(row) => {
            let profile = crate::audio::voices::row_profile(state, row).await;
            if profile.family.is_empty() {
                row.model_id.clone()
            } else {
                profile.family.clone()
            }
        }
        None => route.upstream_model.clone(),
    }
}

/// The typed alignment of a family's answer `body` to the upload `fields`;
/// `Err` says in a few words why the answer has no readable word timings.
///
/// Only the words, their text and their times are required. The language,
/// the sample rate, the confidences, the echoed text and the timing are
/// incidental: one of an odd type is left out of the answer rather than
/// failing it.
fn read(body: &[u8], fields: &[MultipartField]) -> Result<AudioAlignment, String> {
    let answer: Value =
        serde_json::from_slice(body).map_err(|e| format!("not the expected JSON: {e}"))?;
    if !answer.is_object() {
        return Err("not the expected JSON: not an object".into());
    }
    let words: Vec<RawWord> = answer
        .get("words")
        .and_then(Value::as_array)
        .ok_or("no `words` list")?
        .iter()
        .map(RawWord::of)
        .collect();
    let transcript = fields
        .iter()
        .find_map(|f| match f {
            MultipartField::Text(name, value) if name == "text" => Some(value.clone()),
            _ => None,
        })
        .or_else(|| {
            answer
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let rate = sample_rate(
        answer.get("sample_rate").and_then(Value::as_f64),
        fields,
        &words,
    );

    let spans = {
        let texts: Vec<&str> = words
            .iter()
            .map(|w| w.word.as_deref().unwrap_or_default())
            .collect();
        offsets::locate(&transcript, &texts)
    };
    let mut out = Vec::with_capacity(words.len());
    for (n, (w, span)) in words.into_iter().zip(spans).enumerate() {
        let word = w.word.ok_or_else(|| format!("word {n} has no `word`"))?;
        let seconds = |secs: Option<f64>, samples: Option<f64>, which: &str| {
            secs.or_else(|| Some(samples? / f64::from(rate?)))
                .ok_or_else(|| format!("word {n} ({word:?}) has no {which} time"))
        };
        let start = seconds(w.start, w.start_sample, "start")?;
        let end = seconds(w.end, w.end_sample, "end")?;
        let samples = |given: Option<f64>, secs: f64| {
            given
                .filter(|s| *s >= 0.0)
                .or_else(|| rate.map(|r| secs * f64::from(r)))
                .map(|s| s.round() as u64)
        };
        out.push(AlignedWord {
            start_sample: samples(w.start_sample, start),
            end_sample: samples(w.end_sample, end),
            word,
            start,
            end,
            confidence: w.confidence,
            char_start: span.map(|(from, _)| from),
            char_end: span.map(|(_, to)| to),
        });
    }
    Ok(AudioAlignment {
        text: transcript,
        language: answer
            .get("language")
            .and_then(Value::as_str)
            .map(str::to_owned),
        sample_rate: rate,
        words: out,
        timing: timing_of(&answer),
    })
}

/// The rate the words' samples count in: the family's own figure, else the
/// one the seconds and samples of the words give (taken from the word that
/// ends last, whose seconds carry the most digits, so the division is the
/// least rounded), else the uploaded WAV's header.
///
/// The words come before the header because they are what the `*_sample`
/// fields count in: a family that resamples the clip (audio.cpp's analysis
/// tasks work at 16 kHz whatever was uploaded) leaves the header describing
/// a rate the samples are not in, and the derived one then is the one that
/// turns them back into the seconds beside them.
fn sample_rate(stated: Option<f64>, fields: &[MultipartField], words: &[RawWord]) -> Option<u32> {
    let whole =
        |r: f64| (r.is_finite() && r >= 1.0 && r <= f64::from(u32::MAX)).then(|| r.round() as u32);
    let derived = || {
        words
            .iter()
            .filter_map(|w| Some((w.end?, w.end_sample?)))
            .filter(|(secs, samples)| *secs > 0.0 && *samples > 0.0)
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .and_then(|(secs, samples)| whole(samples / secs))
    };
    let header = || {
        fields.iter().find_map(|f| match f {
            MultipartField::File(name, _, _, bytes) if name == "file" => wav_rate(bytes),
            _ => None,
        })
    };
    stated.and_then(whole).or_else(derived).or_else(header)
}

#[cfg(test)]
mod tests;
