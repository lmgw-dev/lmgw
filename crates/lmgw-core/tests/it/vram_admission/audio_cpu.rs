//! An audio row on the CPU (the per-row CPU switch): it claims no VRAM, is
//! never an eviction victim and never evicts, learns no residency, keeps
//! serving while the GPU hold is on — and a benchmark's lease still covers
//! it. The fake driver lists no process for its container and charges it
//! nothing, as a container that never opened the card would be.

use lmgw_core::runtime::Class;

use super::audio_residency::{add_audio_model, models_page, op, speak, start_idle};
use super::*;

/// An audio row switched to the CPU through the op, as the editor and the
/// MCP tool switch it: its directory is `on_disk` bytes, and its container
/// holds nothing on the card, before or after a request.
pub(super) async fn add_cpu_model(f: &Fixture, model_id: &str, on_disk: u64) -> i64 {
    let id = add_audio_model(f, model_id, on_disk, 0, None).await;
    {
        let mut w = f.world();
        w.size.insert(model_id.into(), 0);
        w.loaded_bytes.remove(model_id);
        w.unlisted.insert(model_id.into());
    }
    op(
        f,
        "audio_model_set",
        json!({"action": "update", "id": id, "backend": "cpu", "threads": 8}),
    )
    .await;
    id
}

/// The resident list `/api/vram` publishes, as `class/model` names.
fn residents(v: &Value) -> Vec<String> {
    v["resident"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{}/{}",
                r["container"].as_str().unwrap(),
                r["model"].as_str().unwrap()
            )
        })
        .collect()
}

/// A full card does not hold a CPU start back: no queue, no eviction, and
/// its container runs without the GPU passthrough the class args carry —
/// `label=disable` kept. A GPU admission that needs room evicts the idle
/// chat model, never the idle CPU container, and the ledger never shows it.
#[tokio::test]
async fn a_cpu_row_starts_on_a_full_card_and_is_never_evicted() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 0).await;
    add_cpu_model(&f, "asr", 4 * GIB).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);

    let waits = f.state.vram.waits_begun();
    assert_eq!(speak(&f, "asr").await, 200);
    assert_eq!(f.state.vram.waits_begun(), waits, "nothing queued");
    assert!(f.stops().is_empty(), "nothing evicted: {:?}", f.stops());
    assert_eq!(f.runs(), ["chat-model", "asr"]);
    let argv = f.world().argv["asr"].join(" ");
    assert!(!argv.contains("nvidia.com/gpu"), "{argv}");
    assert!(argv.contains("--security-opt label=disable"), "{argv}");
    let gpu_argv = f.world().argv["chat-model"].join(" ");
    assert!(gpu_argv.contains("nvidia.com/gpu=all"), "{gpu_argv}");

    let v = vram_status(&f.gateway).await;
    assert_eq!(residents(&v), ["chat/chat-model"], "{v}");
    assert_eq!(v["estimated_resident_bytes"], 6 * GIB, "{v}");
    let rt = f.state.runtime().list();
    let asr = rt.iter().find(|e| e.model_id == "asr").unwrap();
    assert_eq!(serde_json::to_value(asr).unwrap()["placement"], "cpu");
    let chat_rt = rt.iter().find(|e| e.model_id == "chat-model").unwrap();
    assert!(
        serde_json::to_value(chat_rt)
            .unwrap()
            .get("placement")
            .is_none(),
        "a GPU frame is what it was"
    );

    // 6 + 3 does not fit 8: the chat model goes, the CPU container stays.
    assert_eq!(embed(&f.gateway).await.status(), 200);
    assert_eq!(f.stops(), ["chat-model"]);
    assert!(f.state.runtime().contains(Class::Audio, "asr"));
}

/// No reading is ever taken of a CPU container — none after its answers,
/// none at rest — so nothing is learned and no "residency not read" line can
/// come of it; the models page says where it runs instead. And §4.7's
/// per-process share stays measurable while it runs: a member with no GPU
/// process would otherwise make every pass refuse.
#[tokio::test]
async fn a_cpu_row_learns_nothing_and_leaves_the_outside_share_measurable() {
    let f = fixture(16 * GIB, 6 * GIB, 3 * GIB, 0).await;
    f.attribute(GIB);
    add_cpu_model(&f, "asr", GIB).await;
    let readings = f.state.vram.residency_readings();
    let at_rest = f.state.vram.residency_at_rest_readings();
    assert_eq!(speak(&f, "asr").await, 200);
    assert_eq!(speak(&f, "asr").await, 200);
    assert_eq!(chat(&f.gateway).await.status(), 200);

    let v = until_vram(&f, "the chat model's share is measured", |v| {
        v["external_trigger_active"] == true && v["lmgw_share_bytes"] == 6 * GIB
    })
    .await;
    assert_eq!(v["outside_share_bytes"], GIB, "{v}");
    assert_eq!(f.state.vram.residency_readings(), readings);
    assert_eq!(f.state.vram.residency_at_rest_readings(), at_rest);
    let asked: Vec<String> = f.pid_inspects().concat();
    assert!(
        asked.iter().all(|n| !n.contains("asr")),
        "no PID of the CPU container is read: {asked:?}"
    );

    let page = models_page(&f, "asr").await;
    assert_eq!(page.model.residency, None);
    assert_eq!(page.residency_charged_bytes, None);
    assert_eq!(
        (
            page.runs_on.as_str(),
            page.threads_in_effect,
            page.threads_source.as_str()
        ),
        ("cpu", 8, "row")
    );
    let note = page.residency_note.unwrap_or_default();
    assert!(
        note.starts_with("runs on the CPU (8 threads, set on the row)"),
        "{note}"
    );
}

/// Under the hold the GPU row is swept and refused, while the CPU row keeps
/// its container, answers itself — no fallback header, though it names a
/// cloud fallback — and is named in `kept_on_cpu`.
#[tokio::test]
async fn under_the_hold_a_cpu_row_keeps_serving_and_the_gpu_row_does_not() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 0).await;
    add_audio_model(&f, "tts", GIB, GIB, None).await;
    let asr = add_cpu_model(&f, "asr", GIB).await;
    let _cloud = cloud_upstream(&f, "cloud-speech").await;
    op(
        &f,
        "audio_model_set",
        json!({"action": "update", "id": asr, "hold_fallback_mode": "alias",
               "hold_fallback": "cloud-speech"}),
    )
    .await;
    assert_eq!(speak(&f, "tts").await, 200);
    assert_eq!(speak(&f, "asr").await, 200);

    let out = engage_hold(&f).await;
    assert_eq!(out["stopped"], json!(["audio/tts"]), "{out}");
    assert_eq!(out["kept_on_cpu"], json!(["audio/asr"]), "{out}");
    assert!(
        out["message"]
            .as_str()
            .unwrap()
            .contains("audio models that run on the CPU keep serving (audio/asr)"),
        "{out}"
    );
    let again = lmgw_core::ops::hold_set(&f.state, true).await.unwrap();
    assert_eq!(again["kept_on_cpu"], json!(["audio/asr"]), "{again}");

    let held = f
        .gateway
        .client()
        .post(format!("{}/v1/audio/speech", f.gateway))
        .json(&json!({"model": "audio/tts", "input": "hello", "voice": "alba"}))
        .send()
        .await
        .unwrap();
    assert_eq!(held.status(), 503);
    let body: Value = held.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_hold", "{body}");

    let served = f
        .gateway
        .client()
        .post(format!("{}/v1/audio/speech", f.gateway))
        .json(&json!({"model": "audio/asr", "input": "hello", "voice": "alba"}))
        .send()
        .await
        .unwrap();
    assert_eq!(served.status(), 200);
    assert!(served.headers().get("x-lmgw-fallback").is_none());
    assert_eq!(f.stops(), ["tts"]);

    // The reaper's sweep leaves it running too.
    lmgw_core::runtime::lifecycle::reap_idle(&f.state).await;
    assert!(f.state.runtime().contains(Class::Audio, "asr"));
    let v = vram_status(&f.gateway).await;
    assert_eq!(v["draining"], json!([]), "{v}");
}

/// A row switched to the CPU while its GPU container is still busy: under
/// the hold that container is still on the card, so a request is refused —
/// saying why — until the sweep has stopped it; the next one starts it on
/// the CPU.
#[tokio::test]
async fn a_row_switched_while_its_gpu_container_runs_is_refused_until_that_stops() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 0).await;
    add_audio_model(&f, "asr", GIB, GIB, None).await;
    let snap = f.state.snapshot();
    let rt = lmgw_core::runtime::descriptor::model_runtime(&snap, Class::Audio, "asr").unwrap();
    let spec = lmgw_core::runtime::lifecycle::acquire_spec(&f.state, &snap, &rt);
    let busy = f.state.runtime().acquire(&spec).await.unwrap();

    let out = engage_hold(&f).await;
    assert_eq!(out["draining"], json!(["audio/asr"]), "{out}");
    sqlx::query("UPDATE audio_models SET backend = 'cpu' WHERE model_id = 'asr'")
        .execute(&f.state.db)
        .await
        .unwrap();
    f.state.reload_snapshot().await.unwrap();

    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/audio/speech", f.gateway))
        .json(&json!({"model": "audio/asr", "input": "hello", "voice": "alba"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_hold", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("started on the GPU before that"),
        "{body}"
    );

    drop(busy);
    lmgw_core::runtime::lifecycle::reap_idle(&f.state).await;
    assert_eq!(f.stops(), ["asr"]);
    assert_eq!(speak(&f, "asr").await, 200);
    assert_eq!(f.runs(), ["asr", "asr"]);
    assert!(!f.world().argv["asr"].join(" ").contains("nvidia.com/gpu"));
}

/// The same container under the hold, for a row with a usable hold
/// fallback: the fallback answers, as it did a moment before the switch,
/// rather than a bare refusal — and nothing is sent to the GPU container.
#[tokio::test]
async fn a_row_switched_while_its_gpu_container_runs_falls_back_like_a_held_row() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 0).await;
    let id = add_audio_model(&f, "asr", GIB, GIB, None).await;
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
        json!({"action": "update", "id": id, "hold_fallback_mode": "alias",
               "hold_fallback": "cloud-speech"}),
    )
    .await;
    let snap = f.state.snapshot();
    let rt = lmgw_core::runtime::descriptor::model_runtime(&snap, Class::Audio, "asr").unwrap();
    let spec = lmgw_core::runtime::lifecycle::acquire_spec(&f.state, &snap, &rt);
    let busy = f.state.runtime().acquire(&spec).await.unwrap();
    engage_hold(&f).await;
    sqlx::query("UPDATE audio_models SET backend = 'cpu' WHERE model_id = 'asr'")
        .execute(&f.state.db)
        .await
        .unwrap();
    f.state.reload_snapshot().await.unwrap();

    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/audio/speech", f.gateway))
        .json(&json!({"model": "audio/asr", "input": "hello", "voice": "alba"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-lmgw-fallback")
            .and_then(|v| v.to_str().ok()),
        Some("cloud-speech")
    );
    assert!(
        f.world().audio_calls.is_empty(),
        "{:?}",
        f.world().audio_calls
    );
    drop(busy);
}

/// A boot under the hold warm-starts the CPU rows flagged for it, and only
/// those.
#[tokio::test]
async fn a_boot_under_the_hold_warm_starts_only_the_cpu_row() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 0).await;
    add_audio_model(&f, "tts", GIB, GIB, None).await;
    add_cpu_model(&f, "asr", GIB).await;
    sqlx::query("UPDATE audio_models SET warm_start = 1")
        .execute(&f.state.db)
        .await
        .unwrap();
    f.state.reload_snapshot().await.unwrap();
    engage_hold(&f).await;

    lmgw_core::runtime::lifecycle::boot(&f.state).await;
    assert_eq!(f.runs(), ["asr"]);
}

/// A benchmark's lease covers the CPU row too: a request is refused, and the
/// run's drain stops its container.
#[tokio::test]
async fn a_benchmark_lease_refuses_and_drains_the_cpu_row() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 0).await;
    add_cpu_model(&f, "asr", GIB).await;
    start_idle(&f, "asr").await;
    f.state
        .set_gpu_lease(Some(lmgw_core::bench::lease::lease(7, "chat-model")));

    let resp = f
        .gateway
        .client()
        .post(format!("{}/v1/audio/speech", f.gateway))
        .json(&json!({"model": "audio/asr", "input": "hello", "voice": "alba"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "gpu_benchmark", "{body}");

    struct Quiet;
    #[async_trait::async_trait]
    impl lmgw_core::bench::BenchSink for Quiet {
        async fn progress(&self, _: lmgw_core::bench::Progress) {}
        async fn phase_done(&self, _: lmgw_api_types::bench::Phase, _: &lmgw_core::bench::Record) {}
    }
    let drained = lmgw_core::bench::drain::empty_the_card(
        &f.state,
        &lmgw_core::agent::Cancel::none(),
        &Quiet,
    )
    .await;
    assert_eq!(drained, lmgw_core::bench::drain::Drained::Empty);
    assert_eq!(f.stops(), ["asr"]);
}

/// A GPU row switched to the CPU while its request queued for room, with
/// the hold engaged before the room came: the request still starts the GPU
/// descriptor it was admitted with, so the hold refuses it. The re-checks
/// after the wait go by what is started, not by the row as it reads now —
/// otherwise the weights load onto the card the owner just took back.
#[tokio::test]
async fn a_row_switched_to_the_cpu_while_its_gpu_start_queued_is_refused_under_the_hold() {
    let f = fixture(8 * GIB, 6 * GIB, 3 * GIB, 0).await;
    let asr = add_audio_model(&f, "asr", 3 * GIB, 3 * GIB, None).await;
    assert_eq!(chat(&f.gateway).await.status(), 200);
    // 6 + 3 does not fit 8, and the chat model is generating: the GPU start
    // waits for room.
    f.world().busy.insert("chat-model".into());
    let gateway = f.gateway.clone();
    let waiting = tokio::spawn(async move {
        let resp = gateway
            .client()
            .post(format!("{}/v1/audio/speech", gateway))
            .json(&json!({"model": "audio/asr", "input": "hello", "voice": "alba"}))
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        (status, resp.json::<Value>().await.unwrap_or_default())
    });
    until_vram(&f, "the GPU start queues", |v| {
        v["queue"].as_array().is_some_and(|q| q.len() == 1)
    })
    .await;

    op(
        &f,
        "audio_model_set",
        json!({"action": "update", "id": asr, "backend": "cpu"}),
    )
    .await;
    engage_hold(&f).await;
    f.world().busy.remove("chat-model");

    let (status, body) = waiting.await.unwrap();
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["code"], "gpu_hold", "{body}");
    assert_eq!(f.runs(), ["chat-model"], "no GPU start of the switched row");
    // The decision after the wait refused rather than made room: nothing
    // was evicted for a start the hold refuses anyway.
    assert!(f.stops().is_empty(), "{:?}", f.stops());
}
