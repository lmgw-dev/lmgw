//! Which turn detection a session runs (realtime design §5.1, §6.2, §6.3,
//! §6.6): the detector's parameters from the session, whether a commit
//! answers by itself, and the log line of what serves `semantic_vad`.

use super::super::protocol::{Eagerness, SemanticResolved, Session, TurnDetection};
use super::super::turn::barge_in::BargeInParams;
use super::super::turn::semantic::SemanticParams;
use super::super::turn::server_vad::ServerVadParams;
use crate::config::{BargeInCheck, RealtimeSettings, SemanticVadEngine};
use rows::row;

mod rows;

pub(crate) use rows::warn_unusable as warn_unusable_semantic_rows;

/// The Smart Turn rule `session` runs (§6.3): its eagerness's row, when
/// its turn detection is `semantic_vad` and `realtime.semantic_vad_engine`
/// is `smart_turn` — the built-in row when the stored one cannot run
/// (`rows`); echoed as `lmgw.resolved.semantic_vad`.
pub(crate) fn semantic_rule(s: &Session, settings: &RealtimeSettings) -> Option<SemanticResolved> {
    let Some(TurnDetection::SemanticVad { eagerness, .. }) = turn_detection(s) else {
        return None;
    };
    if settings.semantic_vad_engine != SemanticVadEngine::SmartTurn {
        return None;
    }
    let (r, floor_window_ms) = row(*eagerness, settings);
    Some(SemanticResolved {
        threshold: r.threshold,
        floor: r.floor,
        floor_window_ms,
        max_wait_ms: r.max_wait_ms,
        silence_duration_ms: r.silence_duration_ms,
    })
}

/// The detector's parameters for `session`'s `turn_detection`; `None` for
/// manual turns.
///
/// `semantic_vad` is the detector with the Smart Turn rule of its
/// eagerness (§6.3, [`semantic_rule`]) on the settings' threshold and
/// padding; the row's `silence_duration_ms` is where a pause that could not
/// be scored commits. With `realtime.semantic_vad_engine` `server_vad` it is
/// plain `server_vad` on that window — high 300 ms, medium and auto 500 ms,
/// low 800 ms by default, `semantic_vad` before Smart Turn — echoed as
/// `lmgw.resolved.turn_detection: "server_vad"` (`merge::normalize`).
pub(crate) fn detector_params(s: &Session, settings: &RealtimeSettings) -> Option<ServerVadParams> {
    let post_interrupt = s
        .lmgw
        .as_ref()
        .and_then(|l| l.post_interrupt_silence_ms)
        .unwrap_or(settings.post_interrupt_silence_ms);
    let td = s.audio.as_ref()?.input.as_ref()?.turn_detection.as_ref()?;
    let (threshold, prefix, silence) = match td {
        TurnDetection::ServerVad {
            threshold,
            prefix_padding_ms,
            silence_duration_ms,
            ..
        } => (
            threshold.unwrap_or(settings.threshold),
            prefix_padding_ms.unwrap_or(settings.prefix_padding_ms),
            silence_duration_ms.unwrap_or(settings.silence_duration_ms),
        ),
        TurnDetection::SemanticVad { eagerness, .. } => (
            settings.threshold,
            settings.prefix_padding_ms,
            row(*eagerness, settings).0.silence_duration_ms,
        ),
    };
    let semantic = semantic_rule(s, settings).map(|r| SemanticParams {
        threshold: r.threshold as f32,
        floor: r.floor as f32,
        floor_window_ms: r.floor_window_ms,
        max_wait_ms: r.max_wait_ms,
    });
    Some(ServerVadParams {
        threshold: threshold as f32,
        prefix_padding_ms: prefix,
        silence_duration_ms: silence,
        post_interrupt_silence_ms: post_interrupt,
        semantic,
        ..ServerVadParams::default()
    })
}

/// The log line of what serves `semantic_vad` (§6.3), when a session's
/// turn detection becomes it — at the start, or by a `session.update` that
/// changes it (live acceptance L6, §23): the Smart Turn rule in effect, or —
/// with `realtime.semantic_vad_engine` `server_vad` — the substitution (§5.1:
/// every substitution is echoed and logged; echoed as
/// `lmgw.resolved.turn_detection`). `before`: the session as it was, `None`
/// at the start.
pub(crate) fn log_turn_detection(
    session_id: &str,
    before: Option<&Session>,
    next: &Session,
    params: Option<&ServerVadParams>,
) {
    if let Some(note) = substitution(before, next, params) {
        tracing::info!("realtime {session_id}: {note}");
    }
}

/// [`log_turn_detection`]'s line, when there is one to write.
fn substitution(
    before: Option<&Session>,
    next: &Session,
    params: Option<&ServerVadParams>,
) -> Option<String> {
    let td = |s: &Session| {
        s.audio
            .as_ref()
            .and_then(|a| a.input.as_ref())
            .and_then(|i| i.turn_detection.clone())
    };
    let Some(TurnDetection::SemanticVad { eagerness, .. }) = td(next) else {
        return None;
    };
    if before.is_some_and(|b| td(b) == td(next)) {
        return None;
    }
    let eagerness = format!("{:?}", eagerness.unwrap_or(Eagerness::Auto)).to_lowercase();
    let silence = params.map_or(0, |p| p.silence_duration_ms);
    Some(match params.and_then(|p| p.semantic.as_ref()) {
        Some(r) => format!(
            "turn_detection semantic_vad (eagerness {eagerness}): Smart Turn — a pause scoring \
             at least {:.2} commits at once, at least {:.2} at {} ms of silence, anything \
             else at {} ms; a pause it cannot score at {silence} ms (§6.3)",
            r.threshold, r.floor, r.floor_window_ms, r.max_wait_ms
        ),
        None => format!(
            "turn_detection semantic_vad (eagerness {eagerness}) is served by server_vad with a \
             {silence} ms silence window (realtime.semantic_vad_engine is server_vad); echoed \
             as lmgw.resolved.turn_detection \"server_vad\""
        ),
    })
}

/// The barge-in knobs (§6.4, §12): the session's `lmgw` object, else the
/// settings.
pub(crate) struct Barge {
    pub params: BargeInParams,
    pub half_duplex: bool,
    /// `barge_in_check` is `"words"` — the session still needs an ASR alias
    /// for it (`turn::arbiter::check`).
    pub words: bool,
}

/// The session's [`Barge`] knobs.
pub(crate) fn barge_params(s: &Session, settings: &RealtimeSettings) -> Barge {
    let l = s.lmgw.as_ref();
    let params = BargeInParams {
        min_ms: l
            .and_then(|l| l.barge_in_min_ms)
            .unwrap_or(settings.barge_in_min_ms),
        guard_ms: l
            .and_then(|l| l.barge_in_guard_ms)
            .unwrap_or(settings.barge_in_guard_ms),
        ..BargeInParams::default()
    };
    let half_duplex = l
        .and_then(|l| l.half_duplex)
        .unwrap_or(settings.half_duplex);
    let check = l
        .and_then(|l| l.barge_in_check)
        .unwrap_or(settings.barge_in_check);
    Barge {
        params,
        half_duplex,
        words: check == BargeInCheck::Words,
    }
}

fn turn_detection(s: &Session) -> Option<&TurnDetection> {
    s.audio
        .as_ref()
        .and_then(|a| a.input.as_ref())
        .and_then(|i| i.turn_detection.as_ref())
}

/// `turn_detection.create_response` — off for manual turns.
pub(super) fn create_response(s: &Session) -> bool {
    match turn_detection(s) {
        Some(TurnDetection::ServerVad {
            create_response, ..
        })
        | Some(TurnDetection::SemanticVad {
            create_response, ..
        }) => create_response.unwrap_or(true),
        None => false,
    }
}

/// `turn_detection.interrupt_response`: whether a turn that starts during
/// a response cancels it (§6.4). A property of the turn, like
/// `create_response`, so the live session's — not the response's snapshot.
pub(in crate::realtime) fn interrupt_response(s: &Session) -> bool {
    match turn_detection(s) {
        Some(TurnDetection::ServerVad {
            interrupt_response, ..
        })
        | Some(TurnDetection::SemanticVad {
            interrupt_response, ..
        }) => interrupt_response.unwrap_or(true),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn session(td: serde_json::Value) -> Session {
        serde_json::from_value(json!({"type": "realtime",
            "audio": {"input": {"turn_detection": td}}}))
        .unwrap()
    }

    #[test]
    fn what_serves_semantic_vad_is_said_once_per_change() {
        let mut settings = RealtimeSettings::default();
        let semantic = session(json!({"type": "semantic_vad", "eagerness": "high"}));
        let params = detector_params(&semantic, &settings);
        assert_eq!(
            substitution(None, &semantic, params.as_ref()).unwrap(),
            "turn_detection semantic_vad (eagerness high): Smart Turn — a pause scoring at \
             least 0.50 commits at once, at least 0.10 at 500 ms of silence, anything else at \
             2000 ms; a pause it cannot score at 300 ms (§6.3)"
        );
        // The escape hatch: the substitution, as before Smart Turn.
        settings.semantic_vad_engine = SemanticVadEngine::ServerVad;
        let params = detector_params(&semantic, &settings);
        assert_eq!(params.as_ref().unwrap().semantic, None);
        let note = substitution(None, &semantic, params.as_ref()).unwrap();
        assert_eq!(
            note,
            "turn_detection semantic_vad (eagerness high) is served by server_vad with a 300 ms \
             silence window (realtime.semantic_vad_engine is server_vad); echoed as \
             lmgw.resolved.turn_detection \"server_vad\""
        );
        // From server_vad, and to another eagerness: said again.
        let plain = session(json!({"type": "server_vad"}));
        assert!(substitution(Some(&plain), &semantic, params.as_ref()).is_some());
        let low = session(json!({"type": "semantic_vad", "eagerness": "low"}));
        let note = substitution(Some(&semantic), &low, None).unwrap();
        assert!(note.contains("eagerness low"), "{note}");
        // Unchanged, or not semantic_vad at all: nothing to say.
        assert!(substitution(Some(&semantic), &semantic, params.as_ref()).is_none());
        assert!(substitution(None, &plain, None).is_none());
    }
}
