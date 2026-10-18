//! A bound client's `lmgw.response.stop` drops the `response.create` a
//! response would start after the tap (calls design §6.2): the one held
//! while the user speaks, and the one queued behind the active response.

use tokio::time::Instant;

use super::*;
use crate::realtime::audio_in::Detected;
use crate::realtime::turn::server_vad::TurnEvent;

#[tokio::test]
async fn a_stop_drops_a_response_create_held_while_the_user_speaks() {
    let mut r = Rig::new().await;
    r.core.on_judged(Detected {
        event: TurnEvent::SpeechStarted {
            audio_start_ms: 0,
            onset_ms: 300,
        },
        speech_end: None,
        at: Instant::now(),
        barge_in: false,
        end: None,
    });
    r.core.response_create(Some("tool".into()), None);
    r.events().await;
    assert!(r.core.pending.is_some(), "the create is carried");

    r.core.response_stop(None);
    assert!(r.core.pending.is_none(), "the carried create is dropped");
    assert!(r.core.active.is_none());
    assert_eq!(r.events().await, Vec::<Value>::new(), "a stop says nothing");
}

#[tokio::test]
async fn a_stop_drops_a_response_create_queued_behind_the_active_response() {
    let mut r = Rig::new().await;
    r.core.response_create(None, None);
    r.delta(1, "eins");
    r.finish(1, Ok(completion("eins")));
    r.events().await;
    // Closing: generation is over, response.done waits for the drained ack.
    r.core.response_create(Some("next".into()), None);
    assert_eq!(r.events().await, Vec::<Value>::new(), "queued");
    assert!(r.core.active.as_ref().unwrap().queued.is_some());

    r.core.response_stop(None);
    assert!(r.core.active.as_ref().unwrap().queued.is_none());

    r.core.on_drained(1);
    let ev = r.events().await;
    assert_eq!(types(&ev), ["response.done"], "no new response starts");
    assert!(r.core.active.is_none());
}
