//! `/v1/tasks/run` and `/v1/tasks/stream` on a row whose engine refuses a
//! character its package lacks (Supertonic, review TC-4): the task's text
//! is fitted to the vocabulary as `/v1/audio/speech`'s `input` is
//! ([`crate::audio::shape::fit_text`]), a text with nothing left to say is
//! `empty_input` before admission ([`refuse_unsayable`]), and
//! `x-lmgw-speech` says what changed. Only the characters: the generic
//! route relays everything else as it came, inline tags included. Any other
//! row and any other route gets the body untouched.

use std::time::Instant;

use serde_json::Value;

use super::{send_json, MediaOutcome};
use crate::audio::preflight::refuse_unsayable;
use crate::audio::profile::SpeechProfile;
use crate::audio::shape::{fit_text, ShapeReport};
use crate::audio::voices::{row_of_route, row_profile};
use crate::config::{AudioModel, Route};
use crate::error::GatewayError;
use crate::gate::GateHeaders;
use crate::proxy::synthesize::CharsSeen;
use crate::proxy::Failed;
use crate::state::SharedState;

/// What [`task_call`] produced: its outcome, and what shaping changed.
pub(super) struct Tasked {
    pub result: Result<MediaOutcome, Failed>,
    pub report: ShapeReport,
}

/// Resolve → shape-check → admit → shape → send, for a task body.
pub(super) async fn task_call(
    state: &SharedState,
    endpoint: &str,
    body: &Value,
    started: Instant,
) -> Tasked {
    let refused = |failed: Failed, report: ShapeReport| Tasked {
        result: Err(failed),
        report,
    };
    let Some(alias) = body.get("model").and_then(Value::as_str) else {
        let e = GatewayError::BadRequest("missing 'model'".into());
        return refused((None, GateHeaders::default(), e), ShapeReport::default());
    };
    let routed = match crate::gate::resolve(state, alias, crate::gate::RouteCheck::Audio).await {
        Ok(r) => r,
        Err(f) => return refused((f.route, f.headers, f.error), ShapeReport::default()),
    };
    // A text the resolved row would be sent nothing of is refused before
    // anything starts for it.
    let resolved = profile_on(state, routed.resolved()).await;
    if let Some((_, profile)) = &resolved {
        let mut trial = body.clone();
        let report = shape_task(profile, &mut trial);
        let vocab = profile.char_vocab.as_deref();
        if let Err(e) = refuse_unsayable(task_text(&trial), &report, alias, vocab) {
            note_chars(&report, alias);
            let at = Some(Box::new(routed.resolved().clone()));
            return refused((at, routed.headers().clone(), e), report);
        }
    }
    let crate::gate::Opened {
        route,
        hold,
        headers,
    } = match routed.admit(state).await {
        Ok(o) => o,
        Err(f) => return refused((f.route, f.headers, f.error), ShapeReport::default()),
    };
    let mut out = body.clone();
    out["model"] = Value::String(route.upstream_model.clone());
    // The row that answers: read again only when it is another one.
    let snap = state.snapshot();
    let same = |(row, _): &(AudioModel, std::sync::Arc<SpeechProfile>)| {
        row_of_route(&snap, &route).is_some_and(|r| r.model_id == row.model_id)
    };
    let answering = match resolved.filter(same) {
        Some(r) => Some(r),
        None => profile_on(state, &route).await,
    };
    let report = answering
        .as_ref()
        .map(|(_, p)| shape_task(p, &mut out))
        .unwrap_or_default();
    let name = headers.fallback().unwrap_or(alias).to_string();
    note_chars(&report, &name);
    let vocab = answering
        .as_ref()
        .and_then(|(_, p)| p.char_vocab.as_deref());
    if let Err(e) = refuse_unsayable(task_text(&out), &report, &name, vocab) {
        return refused((Some(Box::new(route)), headers, e), report);
    }
    Tasked {
        result: send_json(state, endpoint, (route, hold, headers), &out, started).await,
        report,
    }
}

/// The lmgw audio row `route` lands on and its profile, when the row's
/// engine refuses a character its package lacks; `None` otherwise.
async fn profile_on(
    state: &SharedState,
    route: &Route,
) -> Option<(AudioModel, std::sync::Arc<SpeechProfile>)> {
    let row = row_of_route(&state.snapshot(), route)?.clone();
    crate::audio::families::char_vocabulary(&row.family)?;
    let profile = row_profile(state, &row).await;
    Some((row, profile))
}

/// The text audio.cpp reads from a task body: `request.text`, or for a body
/// without `request` its own `text` (`handle_generic_run`).
fn text_slot(body: &mut Value) -> Option<&mut Value> {
    if body.get("request").is_some() {
        body.get_mut("request")?.get_mut("text")
    } else {
        body.get_mut("text")
    }
}

fn task_text(body: &Value) -> Option<&str> {
    match body.get("request") {
        Some(r) => r.get("text"),
        None => body.get("text"),
    }
    .and_then(Value::as_str)
}

/// The task's text fitted to `profile`'s vocabulary.
fn shape_task(profile: &SpeechProfile, body: &mut Value) -> ShapeReport {
    let mut report = ShapeReport::default();
    if let Some(change) = text_slot(body).and_then(|t| fit_text(profile, t)) {
        report.changes.push(change);
    }
    report
}

/// Logs every character shaping fitted out of the text, in full.
fn note_chars(report: &ShapeReport, name: &str) {
    CharsSeen::log_once(report, &format!("task: '{name}'"));
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_text_is_where_audio_cpp_reads_it() {
        let mut nested = json!({"model": "m", "request": {"text": "a"}, "text": "b"});
        assert_eq!(task_text(&nested), Some("a"));
        *text_slot(&mut nested).unwrap() = json!("c");
        assert_eq!(nested["request"]["text"], "c");
        assert_eq!(nested["text"], "b");
        let mut flat = json!({"model": "m", "text": "b"});
        assert_eq!(task_text(&flat), Some("b"));
        assert!(text_slot(&mut flat).is_some());
        let mut none = json!({"model": "m", "request": {"audio": "x.wav"}});
        assert_eq!(task_text(&none), None);
        assert!(text_slot(&mut none).is_none());
    }
}
