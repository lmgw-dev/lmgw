//! The per-row CPU switch at the operator surface and at §4.7's
//! outside-VRAM fallback: under the GPU hold a container verb decides per
//! member — a CPU row starts and restarts, a GPU row is held — and a CPU row
//! never takes the outside-VRAM fallback, however full the card is.

use lmgw_core::runtime::Class;

use super::audio_cpu::add_cpu_model;
use super::audio_residency::{add_audio_model, op, speak};
use super::*;

const MIB: u64 = 1024 * 1024;

/// The hold as its sweep would not have left it: switched on straight
/// through the settings, so whatever runs keeps running and the verbs meet
/// it.
async fn hold_without_sweep(f: &Fixture, on: bool) {
    let mut s = f.state.snapshot().settings.clone();
    s.hold.active = on;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
}

/// `class/model` for each member a group verb's list names.
fn members(list: &Value) -> Vec<String> {
    let mut out: Vec<String> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            format!(
                "{}/{}",
                m["class"].as_str().unwrap(),
                m["model_id"].as_str().unwrap()
            )
        })
        .collect();
    out.sort();
    out
}

async fn verb(f: &Fixture, target: Option<&str>, model: Option<&str>, action: &str) -> Value {
    lmgw_core::ops::container(&f.state, target, model, action, false, None)
        .await
        .unwrap_or_else(|e| panic!("{action} on {target:?}/{model:?}: {e}"))
}

/// Under the hold: a group start starts only the CPU member and names the
/// GPU one as held; a group restart restarts the CPU member and leaves the
/// GPU ones running untouched; a group apply stops the GPU ones (held) and
/// recreates the CPU one; and the per-model verbs refuse the GPU row while
/// they act on the CPU row.
#[tokio::test]
async fn under_the_hold_the_container_verbs_act_on_the_cpu_member_and_hold_the_gpu_ones() {
    // Six starts: asr, tts, chat-model, then asr again for each verb.
    let f = fixture_n(16 * GIB, 6 * GIB, 3 * GIB, 0, 3).await;
    add_audio_model(&f, "tts", GIB, GIB, None).await;
    add_cpu_model(&f, "asr", GIB).await;
    sqlx::query("UPDATE audio_models SET warm_start = 1")
        .execute(&f.state.db)
        .await
        .unwrap();
    f.state.reload_snapshot().await.unwrap();
    hold_without_sweep(&f, true).await;

    let out = verb(&f, Some("audio"), None, "start").await;
    assert_eq!(members(&out["started"]), ["audio/asr"], "{out}");
    assert_eq!(members(&out["held"]), ["audio/tts"], "{out}");
    assert_eq!(f.runs(), ["asr"]);

    // Everything up, then the hold again.
    hold_without_sweep(&f, false).await;
    assert_eq!(speak(&f, "tts").await, 200);
    assert_eq!(chat(&f.gateway).await.status(), 200);
    hold_without_sweep(&f, true).await;

    let out = verb(&f, Some("all"), None, "restart").await;
    assert_eq!(members(&out["restarted"]), ["audio/asr"], "{out}");
    assert_eq!(
        members(&out["held"]),
        ["audio/tts", "chat/chat-model"],
        "{out}"
    );
    assert_eq!(f.stops(), ["asr"], "the GPU members are left as they are");

    let out = verb(&f, Some("all"), None, "apply").await;
    assert_eq!(members(&out["recreated"]), ["audio/asr"], "{out}");
    assert_eq!(
        members(&out["held"]),
        ["audio/tts", "chat/chat-model"],
        "{out}"
    );
    let mut stops = f.stops();
    stops.sort();
    assert_eq!(stops, ["asr", "asr", "chat-model", "tts"]);
    assert!(f.state.runtime().contains(Class::Audio, "asr"));
    assert!(!f.state.runtime().contains(Class::Audio, "tts"));

    let err = lmgw_core::ops::container(&f.state, None, Some("tts"), "start", false, None)
        .await
        .expect_err("a GPU row is held");
    assert!(err.contains("hold"), "{err}");
    let out = verb(&f, None, Some("asr"), "restart").await;
    assert_ne!(out["ok"], false, "{out}");
    assert_eq!(f.runs().iter().filter(|m| *m == "asr").count(), 4);
}

/// §4.7's outside-VRAM fallback never takes a CPU row's request: with the
/// switch on, the row's own fallback usable and the card all but full of
/// outside use, the CPU container — charged nothing — answers itself.
#[tokio::test]
async fn a_cpu_row_never_takes_the_outside_vram_fallback() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 512).await;
    f.attribute(8 * GIB - 100 * MIB);
    let asr = add_cpu_model(&f, "asr", GIB).await;
    let cloud = cloud_upstream(&f, "cloud-speech").await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "audio/wav")
                .set_body_bytes(vec![0u8; 64]),
        )
        .mount(&cloud)
        .await;
    op(
        &f,
        "audio_model_set",
        json!({"action": "update", "id": asr, "hold_fallback_mode": "alias",
               "hold_fallback": "cloud-speech"}),
    )
    .await;
    assert!(f.state.snapshot().settings.vram.fallback_on_external);

    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/audio/speech", f.gateway))
        .json(&json!({"model": "audio/asr", "input": "hello", "voice": "alba"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers().get("x-lmgw-fallback").is_none(),
        "{:?}",
        resp.headers()
    );
    assert_eq!(f.runs(), ["asr"]);
}

/// An operator start of a GPU row that waits for the admission gate — held
/// by a request queued for room — while the row is switched to the CPU and
/// the hold comes on. The start still starts the GPU descriptor it read
/// before the wait, so the check after the gate is the global hold, not the
/// row's: it is held, and nothing runs. Asked of the row as it reads now,
/// the start would go ahead onto the card the owner just took back.
#[tokio::test]
async fn an_operator_start_waiting_for_the_gate_is_held_after_its_row_moved_to_the_cpu() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 0).await;
    add_audio_model(&f, "tts", 3 * GIB, 3 * GIB, None).await;
    let asr = add_audio_model(&f, "asr", GIB, GIB, None).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    // 6 + 3 does not fit 8 and the chat model is generating: the TTS request
    // waits for room, holding the gate. 6 + 1 would fit.
    f.world().busy.insert("chat-model".into());
    let waiting = tokio::spawn({
        let f = f.gateway.clone();
        async move { speak_via(&f, "tts").await }
    });
    until_vram(&f, "the TTS start queues", |v| {
        v["queue"].as_array().is_some_and(|q| q.len() == 1)
    })
    .await;
    let start = tokio::spawn({
        let state = f.state.clone();
        async move { lmgw_core::ops::container(&state, None, Some("asr"), "start", false, None).await }
    });
    // The operator start is behind the gate by now.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!start.is_finished());

    op(
        &f,
        "audio_model_set",
        json!({"action": "update", "id": asr, "backend": "cpu"}),
    )
    .await;
    hold_without_sweep(&f, true).await;

    assert_eq!(waiting.await.unwrap(), 503);
    let err = start.await.unwrap().expect_err("the GPU start is held");
    assert!(err.contains("hold"), "{err}");
    assert_eq!(f.runs(), ["chat-model"], "no GPU start of the switched row");
    assert!(f.stops().is_empty(), "{:?}", f.stops());
}

/// [`speak`] on a gateway alone, for a spawned request.
async fn speak_via(gateway: &common::Gw, model: &str) -> u16 {
    gateway
        .client()
        .post(format!("{gateway}/v1/audio/speech"))
        .json(&json!({"model": format!("audio/{model}"), "input": "hello", "voice": "alba"}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}
