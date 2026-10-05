//! Warm early, never evict (realtime design §9.1), on the GPU-world fake: a
//! session starts its local models in the background with the admission
//! warm starts use — resident, not claimed — and skips rather than evicts,
//! or starts nothing under the GPU hold.

use std::time::Duration;

use lmgw_core::config::{BargeInCheck, Settings};
use lmgw_core::runtime::registry::RuntimeState;
use lmgw_core::server::build_router;
use lmgw_core::store;

use crate::support::gpu_world::{Gpu, GIB};
use crate::support::realtime_fakes::{next_event, open};

/// Serve `g`'s gateway with its settings adjusted by `tweak`.
async fn serve(g: &Gpu, tweak: impl FnOnce(&mut Settings)) -> String {
    let mut s = g.state.snapshot().settings.clone();
    tweak(&mut s);
    store::save_settings(&g.state.db, &s).await.unwrap();
    g.state.reload_snapshot().await.unwrap();
    let app = build_router(g.state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr.to_string()
}

async fn until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// A session on `model`, past `session.created` (where the warm starts).
async fn connect(addr: &str, model: &str) -> crate::support::realtime_fakes::Ws {
    let mut ws = open(addr, &format!("/v1/realtime?model={model}"), &[]).await;
    assert_eq!(next_event(&mut ws).await["type"], "session.created");
    ws
}

#[tokio::test]
async fn a_session_warms_its_model_without_claiming_it() {
    let g = Gpu::new(10 * GIB, 2, 5).await;
    g.model("voice", 4 * GIB).await;
    let addr = serve(&g, |_| {}).await;
    let _ws = connect(&addr, "voice").await;
    until("the warm start", || g.runs() == ["voice"]).await;
    // Resident, not held: the quiet session pins nothing.
    until("ready and unclaimed", || {
        g.state
            .runtime()
            .list()
            .iter()
            .any(|v| v.model_id == "voice" && v.state == RuntimeState::Ready && v.in_flight == 0)
    })
    .await;

    // Off: nothing starts until a request needs it.
    let g = Gpu::new(10 * GIB, 2, 5).await;
    g.model("voice", 4 * GIB).await;
    let addr = serve(&g, |s| s.realtime.warm_on_connect = false).await;
    let _ws = connect(&addr, "voice").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(g.runs().is_empty(), "{:?}", g.runs());
}

#[tokio::test]
async fn a_warm_skips_rather_than_evicts_and_starts_nothing_under_the_hold() {
    let g = Gpu::new(10 * GIB, 3, 5).await;
    g.model("big", 8 * GIB).await;
    g.model("voice", 4 * GIB).await;
    let addr = serve(&g, |_| {}).await;
    let _big = connect(&addr, "big").await;
    until("big is up", || g.runs() == ["big"]).await;
    // `voice` does not fit beside the idle `big`: skipped, not evicted for.
    let _voice = connect(&addr, "voice").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(g.runs(), ["big"]);
    assert!(g.stops().is_empty(), "{:?}", g.stops());

    let g = Gpu::new(10 * GIB, 2, 5).await;
    g.model("voice", 4 * GIB).await;
    let addr = serve(&g, |s| s.hold.active = true).await;
    let _ws = connect(&addr, "voice").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(g.runs().is_empty(), "{:?}", g.runs());
}

#[tokio::test]
async fn a_session_s_models_are_warmed_side_by_side() {
    // §23: warmed one after the other, the chat model's cold start held the
    // ASR model's back.
    let g = Gpu::new(10 * GIB, 2, 5).await;
    g.model("voice", 3 * GIB).await;
    g.model("ears", 2 * GIB).await;
    // `ears` is the session's speech-to-text model.
    sqlx::query("UPDATE local_models SET capabilities_override = ?1 WHERE model_id = 'ears'")
        .bind(
            serde_json::json!({ "capabilities": {
                "task": "asr", "endpoints": ["/v1/audio/transcriptions"], "source": "owner"
            } })
            .to_string(),
        )
        .execute(&g.state.db)
        .await
        .unwrap();
    g.state.reload_snapshot().await.unwrap();
    let release = g.gate_runs();
    let addr = serve(&g, |s| s.realtime.asr_alias = "ears".into()).await;
    let _ws = connect(&addr, "voice").await;
    // Both starts are under way while neither has come up.
    until("both starts in flight", || {
        let mut runs = g.runs();
        runs.sort();
        runs == ["ears", "voice"]
    })
    .await;
    release.send(true).unwrap();
    until("both up", || {
        let list = g.state.runtime().list();
        ["voice", "ears"].iter().all(|m| {
            list.iter()
                .any(|v| v.model_id == *m && v.state == RuntimeState::Ready)
        })
    })
    .await;
}

/// A TTS row that can never speak this session's answers is not warmed:
/// a voice-design row with no description from the session's speech
/// instructions or its own defaults, or a package whose variant does not
/// run the row's task. It is judged as a response's open judges it
/// (`proxy::synthesize::refuse_route`), so no container starts — and later
/// evicts nothing — for a voice the response would only refuse. With a
/// description it is warmed like any model.
#[tokio::test]
async fn a_voice_that_cannot_speak_is_not_warmed() {
    for (task, style, warmed) in [
        ("vdes", "", false),
        ("tts", "a calm, low voice", false),
        ("vdes", "a calm, low voice", true),
    ] {
        let g = Gpu::new(10 * GIB, 2, 5).await;
        g.model("voice", GIB).await;
        let root = g.models_dir().join("design");
        std::fs::create_dir_all(&root).unwrap();
        crate::support::audiocpp_gguf::qwen3(&root, "voice_design");
        let mut row = crate::support::audio_world::tts_row("design", "qwen3_tts");
        row.task = task.into();
        store::insert_audio_model(&g.state.db, &row).await.unwrap();
        let models = g.models_dir().display().to_string();
        let addr = serve(&g, |s| {
            s.audio.models_dir = models;
            s.realtime.tts_alias = "audio/design".into();
            s.realtime.speech_instructions = style.into();
        })
        .await;
        let _ws = connect(&addr, "voice").await;
        until("the chat model's warm", || {
            g.runs().contains(&"voice".into())
        })
        .await;
        if warmed {
            until("the voice's warm", || g.runs().len() == 2).await;
        } else {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let mut runs = g.runs();
        runs.sort();
        let want: &[&str] = if warmed {
            &["design", "voice"]
        } else {
            &["voice"]
        };
        assert_eq!(runs, want, "task {task}, style {style:?}");
    }
}

/// Make `ids` the GPU world's speech-to-text models.
async fn asr_rows(g: &Gpu, ids: &[&str]) {
    for id in ids {
        sqlx::query("UPDATE local_models SET capabilities_override = ?1 WHERE model_id = ?2")
            .bind(
                serde_json::json!({ "capabilities": {
                    "task": "asr", "endpoints": ["/v1/audio/transcriptions"], "source": "owner"
                } })
                .to_string(),
            )
            .bind(id)
            .execute(&g.state.db)
            .await
            .unwrap();
    }
    g.state.reload_snapshot().await.unwrap();
}

/// Live run 3, D3: the barge-in word check's own model was never warmed,
/// so the first barge's check timed out while its container loaded, and
/// the duration rule decided. With `barge_in_check: words` and a check
/// model other than the session's ASR model, the session warms it with the
/// rest — under the same never-evict rule. With `duration` it is no stage.
#[tokio::test]
async fn the_word_check_s_own_model_is_warmed_with_the_session() {
    for (check, want) in [
        (BargeInCheck::Words, &["check", "ears", "voice"][..]),
        (BargeInCheck::Duration, &["ears", "voice"][..]),
    ] {
        let g = Gpu::new(10 * GIB, 3, 5).await;
        g.model("voice", GIB).await;
        g.model("ears", GIB).await;
        g.model("check", GIB).await;
        asr_rows(&g, &["ears", "check"]).await;
        let addr = serve(&g, |s| {
            s.realtime.asr_alias = "ears".into();
            s.realtime.barge_in_check_alias = "check".into();
            s.realtime.barge_in_check = check;
        })
        .await;
        let _ws = connect(&addr, "voice").await;
        for _ in 0..500 {
            if g.runs().len() >= want.len() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut runs = g.runs();
        runs.sort();
        assert_eq!(runs, want, "{check:?}");
    }
}

/// An audio row `id` of `task` (`family`'s package, a stand-in weights file
/// for a speech-to-text one), loading lazily unless `lazy` says otherwise.
async fn audio_row(g: &Gpu, id: &str, family: &str, task: &str, lazy: Option<bool>) {
    let root = g.models_dir().join(id);
    std::fs::create_dir_all(&root).unwrap();
    if task == "tts" {
        crate::support::audiocpp_gguf::with_options(&root, family, &["speed"], &["offline"]);
    } else {
        std::fs::write(root.join("model.gguf"), vec![0u8; 4096]).unwrap();
    }
    let mut row = crate::support::audio_world::tts_row(id, family);
    row.task = task.into();
    row.lazy = lazy;
    store::insert_audio_model(&g.state.db, &row).await.unwrap();
}

/// Live run 3b, D3': audio.cpp loads a lazy row's weights on its first
/// request, and the warm only started the containers — the word check's
/// model held no GPU memory, and the first barge's check loaded it and
/// timed out. The warm now sends each lazy audio row the smallest real
/// request once its container is up: half a second of silence to the ASR
/// and the word check's model, one word in the session's voice to the TTS
/// — before any request of the session, with no request row, every claim
/// let go after. A row that loads at start (`lazy: false`) and a model that
/// has loaded already are sent nothing.
#[tokio::test]
async fn the_warm_loads_a_lazy_audio_model_before_its_first_request() {
    for eager_check in [false, true] {
        let g = Gpu::new(10 * GIB, 4, 5).await;
        g.model("voice", GIB).await;
        audio_row(&g, "ears", "qwen3_asr", "asr", None).await;
        audio_row(
            &g,
            "check",
            "qwen3_asr",
            "asr",
            eager_check.then_some(false),
        )
        .await;
        audio_row(&g, "speaker", "kokoro_tts", "tts", None).await;
        let voices = g.models_dir().join("voices");
        std::fs::create_dir_all(&voices).unwrap();
        std::fs::write(
            voices.join("alba.wav"),
            crate::support::audio_world::wav_bytes(24_000, 480),
        )
        .unwrap();
        let models = g.models_dir().display().to_string();
        let addr = serve(&g, |s| {
            s.audio.models_dir = models;
            s.realtime.asr_alias = "audio/ears".into();
            s.realtime.tts_alias = "audio/speaker".into();
            s.realtime.default_voice = "alba".into();
            s.realtime.barge_in_check = BargeInCheck::Words;
            s.realtime.barge_in_check_alias = "audio/check".into();
        })
        .await;
        let _ws = connect(&addr, "voice").await;
        let mut want_asr = vec!["ears".to_string()];
        if !eager_check {
            want_asr.push("check".into());
        }
        until("the loads", || {
            let w = g.world();
            let mut heard = w.transcriptions.clone();
            heard.sort();
            let mut want = want_asr.clone();
            want.sort();
            heard == want && w.speeches == ["speaker"]
        })
        .await;
        let mut runs = g.runs();
        runs.sort();
        assert_eq!(runs, ["check", "ears", "speaker", "voice"]);
        // The clause a response would send: the session's voice.
        let body = g.world().speech_bodies[0].clone();
        assert_eq!(
            (&body["input"], &body["voice"]),
            (&serde_json::json!("Hello."), &serde_json::json!("alba")),
            "{body}"
        );
        // Every claim let go, and nothing for the owner's Usage page.
        until("the claims let go", || {
            g.state.runtime().list().iter().all(|v| v.in_flight == 0)
        })
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_logs")
            .fetch_one(&g.state.db)
            .await
            .unwrap();
        assert_eq!(rows, 0, "a warm-up is no request");

        // A second session finds them loaded: nothing is sent again.
        let _again = connect(&addr, "voice").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let w = g.world();
        assert_eq!(
            w.transcriptions.len(),
            want_asr.len(),
            "eager check {eager_check}"
        );
        assert_eq!(w.speeches.len(), 1);
    }
}

/// The per-row CPU switch: under the GPU hold a session's ASR row on the
/// CPU is still warmed and loaded at connect — the hold has no claim on it,
/// and its cold load lands then rather than at the first turn — while the
/// TTS row on the GPU and the chat model start nothing.
#[tokio::test]
async fn under_the_hold_a_cpu_asr_row_is_warmed_and_loaded_and_the_gpu_ones_are_not() {
    let g = Gpu::new(10 * GIB, 4, 5).await;
    g.model("voice", GIB).await;
    audio_row(&g, "ears", "qwen3_asr", "asr", None).await;
    audio_row(&g, "speaker", "kokoro_tts", "tts", None).await;
    sqlx::query("UPDATE audio_models SET backend = 'cpu' WHERE model_id = 'ears'")
        .execute(&g.state.db)
        .await
        .unwrap();
    let voices = g.models_dir().join("voices");
    std::fs::create_dir_all(&voices).unwrap();
    std::fs::write(
        voices.join("alba.wav"),
        crate::support::audio_world::wav_bytes(24_000, 480),
    )
    .unwrap();
    let models = g.models_dir().display().to_string();
    let addr = serve(&g, |s| {
        s.audio.models_dir = models;
        s.realtime.asr_alias = "audio/ears".into();
        s.realtime.tts_alias = "audio/speaker".into();
        s.realtime.default_voice = "alba".into();
        s.realtime.barge_in_check = BargeInCheck::Duration;
        s.hold.active = true;
    })
    .await;
    let _ws = connect(&addr, "voice").await;
    until("the CPU ASR row is loaded", || {
        g.world().transcriptions == ["ears"]
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(g.runs(), ["ears"]);
    assert!(g.world().speeches.is_empty());
}

/// A row switched to the CPU while its container, started on the GPU, is
/// up but has not loaded yet; then the hold comes on before the reaper
/// stops that container. A session's warm finds it running and does not
/// start it, and its load must not join it: the first request of a lazy
/// row is what loads the weights, and that container is on the card the
/// owner took back.
#[tokio::test]
async fn the_warm_load_does_not_join_a_gpu_container_of_a_row_switched_to_the_cpu() {
    let g = Gpu::new(10 * GIB, 4, 5).await;
    g.model("voice", GIB).await;
    audio_row(&g, "ears", "qwen3_asr", "asr", None).await;
    let models = g.models_dir().display().to_string();
    let addr = serve(&g, |s| {
        s.audio.models_dir = models;
        s.realtime.asr_alias = "audio/ears".into();
        s.realtime.barge_in_check = BargeInCheck::Duration;
    })
    .await;
    let snap = g.state.snapshot();
    let rt = lmgw_core::runtime::descriptor::model_runtime(
        &snap,
        lmgw_core::runtime::Class::Audio,
        "ears",
    )
    .unwrap();
    let spec = lmgw_core::runtime::lifecycle::acquire_spec(&g.state, &snap, &rt);
    drop(g.state.runtime().acquire(&spec).await.unwrap());

    sqlx::query("UPDATE audio_models SET backend = 'cpu' WHERE model_id = 'ears'")
        .execute(&g.state.db)
        .await
        .unwrap();
    // The hold as its sweep left it: the container was still starting then,
    // so it was marked draining rather than stopped.
    let mut s = g.state.snapshot().settings.clone();
    s.hold.active = true;
    store::save_settings(&g.state.db, &s).await.unwrap();
    g.state.reload_snapshot().await.unwrap();

    let _ws = connect(&addr, "voice").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        g.world().transcriptions.is_empty(),
        "nothing is sent to the GPU container"
    );
    assert_eq!(g.runs(), ["ears"]);
}

/// Two sessions on one cold start of a lazy GPU row: the first session's
/// warm starts it, and the second's finds it in the registry, starts
/// nothing, and its load parks on the start in flight. The hold comes on
/// while the start is still under way. Once the container is up, the
/// parked load must not claim it and send it its first request — that
/// request is what puts a lazy row's weights on the card the owner just
/// took back. Without the hold the same parked load does send it.
#[tokio::test]
async fn a_warm_load_parked_on_a_start_is_refused_when_the_hold_comes_on_meanwhile() {
    for hold in [false, true] {
        let g = Gpu::new(10 * GIB, 4, 5).await;
        g.model("voice", GIB).await;
        audio_row(&g, "ears", "qwen3_asr", "asr", None).await;
        let models = g.models_dir().display().to_string();
        let addr = serve(&g, |s| {
            s.audio.models_dir = models;
            s.realtime.asr_alias = "audio/ears".into();
            s.realtime.barge_in_check = BargeInCheck::Duration;
        })
        .await;
        let release = g.gate_runs();
        let _first = connect(&addr, "voice").await;
        until("the cold start in flight", || {
            g.runs().contains(&"ears".to_string())
        })
        .await;
        let _second = connect(&addr, "voice").await;
        // The second session's load is parked on the start by now.
        tokio::time::sleep(Duration::from_millis(300)).await;
        if hold {
            let mut s = g.state.snapshot().settings.clone();
            s.hold.active = true;
            store::save_settings(&g.state.db, &s).await.unwrap();
            g.state.reload_snapshot().await.unwrap();
        }
        release.send(true).unwrap();
        until("ears is up", || {
            g.state
                .runtime()
                .list()
                .iter()
                .any(|v| v.model_id == "ears" && v.state == RuntimeState::Ready)
        })
        .await;
        if hold {
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert!(
                g.world().transcriptions.is_empty(),
                "nothing is sent to the GPU container under the hold: {:?}",
                g.world().transcriptions
            );
        } else {
            until("the parked load", || !g.world().transcriptions.is_empty()).await;
        }
        assert_eq!(g.runs().iter().filter(|m| *m == "ears").count(), 1);
        until("every claim let go", || {
            g.state.runtime().list().iter().all(|v| v.in_flight == 0)
        })
        .await;
    }
}
