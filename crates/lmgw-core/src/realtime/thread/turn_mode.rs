//! `session.lmgw.turn_mode` (realtime calls design §2.2, C23): how a bound
//! session's turns end, resolved by lmgw so a client neither re-derives the
//! VAD rule nor reads `voice_resolved` for it.
//!
//! - **At the bind** the mode is the thread's (`voice_resolved.
//!   turn_detection`): `push_to_talk` there is `push_to_talk`, anything else
//!   `automatic` ([`of_rule`]). lmgw sets `audio.input.turn_detection` to
//!   match ([`detection`]): the thread's rule in automatic mode
//!   (`server_vad` or `semantic_vad`), `semantic_vad` where the thread names
//!   none (a push-to-talk thread switched to automatic), `null` in
//!   push-to-talk. `session.created` echoes both.
//! - **A `session.update`** ([`update`]) naming the mode switches it, and
//!   lmgw writes the turn detection it implies — unless the mode is the one
//!   in effect, which leaves the client's detection as it is. One that also
//!   names `audio.input.turn_detection` must agree with it (`null` with
//!   `push_to_talk`, a rule with `automatic`), as a client's echo of the
//!   whole session does; else `invalid_value`. Turn detection named alone
//!   works as ever, and the mode follows it.
//! - The thread's rule is the one the bind read: a change of the thread's
//!   turn detection applies at the next bind.
//! - **An unbound session** has no mode: the field is absent from its
//!   session, and a client's is refused (`invalid_value`).

use lmgw_api_types::realtime::TurnMode;
use serde_json::{json, Map, Value};

use super::super::protocol::{ErrorObject, Session};
use crate::store::TurnDetection;

const PARAM: &str = "session.lmgw.turn_mode";

/// The mode a session bound to a thread whose rule is `rule` starts in.
pub(in crate::realtime) fn of_rule(rule: TurnDetection) -> TurnMode {
    match rule {
        TurnDetection::PushToTalk => TurnMode::PushToTalk,
        TurnDetection::SemanticVad | TurnDetection::ServerVad => TurnMode::Automatic,
    }
}

/// `audio.input.turn_detection` in `mode` on a thread whose rule is `rule`
/// (module doc).
pub(in crate::realtime) fn detection(mode: &TurnMode, rule: TurnDetection) -> Value {
    match (mode, rule) {
        (TurnMode::PushToTalk, _) => Value::Null,
        (_, TurnDetection::ServerVad) => json!({"type": "server_vad"}),
        (_, TurnDetection::SemanticVad | TurnDetection::PushToTalk) => {
            json!({"type": "semantic_vad"})
        }
    }
}

/// A `session.update`'s turn mode (module doc). `rule`: the bound thread's
/// rule as the bind read it; `None` for an unbound session. `Ok(Some)`: the
/// update to merge, with the turn detection or the mode it implies written
/// in; `Ok(None)`: merge it as sent.
pub(in crate::realtime) fn update(
    rule: Option<TurnDetection>,
    current: &Session,
    update: &Map<String, Value>,
) -> Result<Option<Map<String, Value>>, ErrorObject> {
    let named_mode = update
        .get("lmgw")
        .and_then(Value::as_object)
        .and_then(|l| l.get("turn_mode"));
    let Some(rule) = rule else {
        return match named_mode {
            Some(_) => Err(ErrorObject::invalid(
                "invalid_value",
                "session.lmgw.turn_mode applies to a session bound to a Chat thread \
                 (chat_thread or chat_folder): this session's turns follow \
                 audio.input.turn_detection alone",
            )
            .with_param(PARAM)),
            None => Ok(None),
        };
    };
    let named_detection = update
        .get("audio")
        .and_then(Value::as_object)
        .and_then(|a| a.get("input"))
        .and_then(Value::as_object)
        .and_then(|i| i.get("turn_detection"));
    let mode = match named_mode {
        None => None,
        Some(v) => match serde_json::from_value::<TurnMode>(v.clone()) {
            Ok(m @ (TurnMode::Automatic | TurnMode::PushToTalk)) => Some(m),
            _ => {
                return Err(ErrorObject::invalid(
                    "invalid_value",
                    format!("session.lmgw.turn_mode is \"automatic\" or \"push_to_talk\", got {v}"),
                )
                .with_param(PARAM))
            }
        },
    };
    match (mode, named_detection) {
        (None, None) => Ok(None),
        (Some(mode), Some(td)) => {
            let agree = match mode {
                TurnMode::PushToTalk => td.is_null(),
                _ => !td.is_null(),
            };
            if agree {
                return Ok(None);
            }
            let said = if td.is_null() { "null" } else { "a rule" };
            Err(ErrorObject::invalid(
                "invalid_value",
                format!(
                    "session.lmgw.turn_mode is \"{mode}\" and audio.input.turn_detection is \
                     {said}: they must agree — null with \"push_to_talk\", a rule with \
                     \"automatic\" — or name only one of them"
                ),
            )
            .with_param(PARAM))
        }
        (Some(mode), None) => {
            let now = current.lmgw.as_ref().and_then(|l| l.turn_mode.as_ref());
            if now == Some(&mode) {
                return Ok(None);
            }
            let mut patched = update.clone();
            Ok(set(
                &mut patched,
                &["audio", "input", "turn_detection"],
                detection(&mode, rule),
            )
            .then_some(patched))
        }
        (None, Some(td)) => {
            let follows = if td.is_null() {
                TurnMode::PushToTalk
            } else {
                TurnMode::Automatic
            };
            let mut patched = update.clone();
            Ok(set(&mut patched, &["lmgw", "turn_mode"], json!(follows)).then_some(patched))
        }
    }
}

/// Write `value` at `path` in `map`, making the objects on the way; `false`
/// when something that is no object is in the way (the merge refuses the
/// update for it).
fn set(map: &mut Map<String, Value>, path: &[&str], value: Value) -> bool {
    let Some((last, ancestors)) = path.split_last() else {
        return false;
    };
    let mut at = map;
    for key in ancestors {
        let next = at.entry(*key).or_insert_with(|| json!({}));
        match next.as_object_mut() {
            Some(obj) => at = obj,
            None => return false,
        }
    }
    at.insert((*last).to_string(), value);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RealtimeSettings;
    use crate::realtime::merge::{apply_update, initial_session};

    /// A bound session started on `rule`, as the bind shapes it.
    fn bound(rule: TurnDetection) -> Session {
        let s = initial_session(&RealtimeSettings::default(), "sess_1".into(), None);
        let mode = of_rule(rule);
        let patch = json!({"type": "realtime",
            "audio": {"input": {"turn_detection": detection(&mode, rule)}},
            "lmgw": {"turn_mode": mode}});
        apply_update(&s, patch.as_object().unwrap(), &RealtimeSettings::default()).unwrap()
    }

    /// `patch` through [`update`] and the merge.
    fn apply(rule: Option<TurnDetection>, s: &Session, patch: Value) -> Result<Session, String> {
        let mut patch = patch.as_object().unwrap().clone();
        patch.insert("type".into(), json!("realtime"));
        let patched = update(rule, s, &patch).map_err(|e| {
            assert_eq!(e.code.as_deref(), Some("invalid_value"));
            assert_eq!(e.param.as_deref(), Some(PARAM));
            e.message
        })?;
        Ok(apply_update(s, patched.as_ref().unwrap_or(&patch), &Default::default()).unwrap())
    }

    fn td(s: &Session) -> Option<&'static str> {
        s.audio
            .as_ref()
            .and_then(|a| a.input.as_ref())
            .and_then(|i| i.turn_detection.as_ref())
            .map(|t| t.type_name())
    }

    fn mode(s: &Session) -> Option<TurnMode> {
        s.lmgw.as_ref().and_then(|l| l.turn_mode.clone())
    }

    #[test]
    fn the_bind_starts_in_the_thread_s_mode() {
        let ptt = bound(TurnDetection::PushToTalk);
        assert_eq!((mode(&ptt), td(&ptt)), (Some(TurnMode::PushToTalk), None));
        let server = bound(TurnDetection::ServerVad);
        assert_eq!(
            (mode(&server), td(&server)),
            (Some(TurnMode::Automatic), Some("server_vad"))
        );
        let semantic = bound(TurnDetection::SemanticVad);
        assert_eq!(
            (mode(&semantic), td(&semantic)),
            (Some(TurnMode::Automatic), Some("semantic_vad"))
        );
        // The echo carries the mode.
        let echo = serde_json::to_value(&ptt).unwrap();
        assert_eq!(echo["lmgw"]["turn_mode"], "push_to_talk");
        assert!(echo["audio"]["input"]["turn_detection"].is_null());
    }

    #[test]
    fn a_switch_writes_the_turn_detection_it_implies() {
        let rule = Some(TurnDetection::PushToTalk);
        let ptt = bound(TurnDetection::PushToTalk);
        // To automatic on a push-to-talk thread: semantic_vad.
        let auto = apply(rule, &ptt, json!({"lmgw": {"turn_mode": "automatic"}})).unwrap();
        assert_eq!(
            (mode(&auto), td(&auto)),
            (Some(TurnMode::Automatic), Some("semantic_vad"))
        );
        // And back.
        let back = apply(rule, &auto, json!({"lmgw": {"turn_mode": "push_to_talk"}})).unwrap();
        assert_eq!((mode(&back), td(&back)), (Some(TurnMode::PushToTalk), None));
        // A server_vad thread switched back to automatic gets its own rule.
        let server = Some(TurnDetection::ServerVad);
        let off = apply(
            server,
            &bound(TurnDetection::ServerVad),
            json!({"lmgw": {"turn_mode": "push_to_talk"}}),
        )
        .unwrap();
        let on = apply(server, &off, json!({"lmgw": {"turn_mode": "automatic"}})).unwrap();
        assert_eq!(td(&on), Some("server_vad"));
        // The mode in effect named again leaves the client's detection as it
        // is.
        let tuned = apply(
            server,
            &on,
            json!({"audio": {"input": {"turn_detection": {"type": "semantic_vad",
                "eagerness": "high"}}}}),
        )
        .unwrap();
        let same = apply(server, &tuned, json!({"lmgw": {"turn_mode": "automatic"}})).unwrap();
        assert_eq!(same.audio, tuned.audio);
    }

    #[test]
    fn turn_detection_named_alone_moves_the_mode() {
        let rule = Some(TurnDetection::SemanticVad);
        let s = bound(TurnDetection::SemanticVad);
        let manual = apply(
            rule,
            &s,
            json!({"audio": {"input": {"turn_detection": null}}}),
        )
        .unwrap();
        assert_eq!(mode(&manual), Some(TurnMode::PushToTalk));
        let vad = apply(
            rule,
            &manual,
            json!({"audio": {"input": {"turn_detection": {"type": "server_vad"}}}}),
        )
        .unwrap();
        assert_eq!(
            (mode(&vad), td(&vad)),
            (Some(TurnMode::Automatic), Some("server_vad"))
        );
    }

    #[test]
    fn a_mode_and_a_turn_detection_must_agree() {
        let rule = Some(TurnDetection::SemanticVad);
        let s = bound(TurnDetection::SemanticVad);
        let e = apply(
            rule,
            &s,
            json!({"lmgw": {"turn_mode": "push_to_talk"},
                   "audio": {"input": {"turn_detection": {"type": "server_vad"}}}}),
        )
        .unwrap_err();
        assert!(e.contains("must agree"), "{e}");
        let e = apply(
            rule,
            &s,
            json!({"lmgw": {"turn_mode": "automatic"},
                   "audio": {"input": {"turn_detection": null}}}),
        )
        .unwrap_err();
        assert!(e.contains("must agree"), "{e}");
        // Agreeing: as named.
        let ok = apply(
            rule,
            &s,
            json!({"lmgw": {"turn_mode": "automatic"},
                   "audio": {"input": {"turn_detection": {"type": "server_vad"}}}}),
        )
        .unwrap();
        assert_eq!(td(&ok), Some("server_vad"));
        // The whole session echoed back agrees with itself.
        let mut echo = serde_json::to_value(&s).unwrap();
        echo.as_object_mut().unwrap().remove("type");
        assert!(apply(rule, &s, echo).is_ok());
        // Neither null nor an unknown mode.
        for bad in [json!(null), json!("hybrid"), json!(3)] {
            let e = apply(rule, &s, json!({"lmgw": {"turn_mode": bad}})).unwrap_err();
            assert!(e.contains("\"automatic\" or \"push_to_talk\""), "{e}");
        }
    }

    #[test]
    fn an_unbound_session_has_no_mode_and_refuses_one() {
        let s = initial_session(&RealtimeSettings::default(), "sess_1".into(), None);
        assert_eq!(mode(&s), None);
        assert!(serde_json::to_value(&s).unwrap()["lmgw"]
            .get("turn_mode")
            .is_none());
        let e = apply(None, &s, json!({"lmgw": {"turn_mode": "automatic"}})).unwrap_err();
        assert!(e.contains("bound to a Chat thread"), "{e}");
        // Turn detection alone: as ever, and no mode appears.
        let manual = apply(
            None,
            &s,
            json!({"audio": {"input": {"turn_detection": null}}}),
        )
        .unwrap();
        assert_eq!((mode(&manual), td(&manual)), (None, None));
    }
}
