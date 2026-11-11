//! The llama-fit review's findings (2026-11-11), end to end: a fitted start
//! records its placement on its container and a row that needs no fitting
//! renders nothing extra (1); an image without `--fit` is charged as written
//! (2); the hold teaches no growth and the learned growth can be forgotten
//! (3); a ready llama-server keeps its margin until its first request (4); a
//! busy resident is waited for rather than the row fitted smaller (5); the
//! command preview shows the placement and says where it comes from (6).

use super::*;
use lmgw_core::config::LlamaParams;
use lmgw_core::gguf::synth;
use lmgw_core::store::OutsideHour;

const MIB: u64 = 1024 * 1024;

/// The dense synthetic model of `llama_fit`: 8 layers of 4 MiB, trained on
/// 32768 tokens — 128 MiB of cache at that context.
fn write_model(f: &Fixture, file: &str) {
    let mut h = synth::chat("llama", 32768);
    h.u32("llama.block_count", 8)
        .u32("llama.embedding_length", 256)
        .u32("llama.feed_forward_length", 512)
        .u32("llama.attention.head_count", 4)
        .u32("llama.attention.head_count_kv", 2)
        .u32("llama.vocab_size", 1024);
    for i in 0..8 {
        h.tensor_sized(&format!("blk.{i}.attn_q.weight"), MIB);
        h.tensor_sized(&format!("blk.{i}.ffn_up.weight"), 3 * MIB);
    }
    h.tensor_sized("output.weight", 2 * MIB);
    h.tensor_sized("token_embd.weight", 2 * MIB);
    h.write_to(&f._models_dir.path().join(file));
}

pub(super) async fn add_row(f: &Fixture, model_id: &str, params: LlamaParams) {
    let file = format!("{model_id}.gguf");
    write_model(f, &file);
    f.world().size.insert(model_id.to_string(), 8 * MIB);
    store::insert_local_model(
        &f.state.db,
        &NewLocalModel {
            model_id: model_id.into(),
            gguf_path: file,
            params,
            args: vec![],
            idle_seconds: 0,
            enabled: true,
            public: true,
            image: None,
            extra_run_args: None,
            warm_start: false,
            fallback: None,
            capabilities_override: None,
            ladder: vec![],
        },
    )
    .await
    .unwrap();
    f.state.reload_snapshot().await.unwrap();
}

pub(super) async fn start(f: &Fixture, model_id: &str) -> Result<Value, String> {
    lmgw_core::ops::container(&f.state, None, Some(model_id), "start", false, None)
        .await
        .map(crate::common::container_wire)
        .map_err(|e| e.to_string())
}

pub(super) fn argv(f: &Fixture, model_id: &str) -> Vec<String> {
    f.world().argv.get(model_id).cloned().unwrap_or_default()
}

pub(super) fn flag(argv: &[String], name: &str) -> Option<String> {
    let at = argv.iter().rposition(|a| a == name)?;
    argv.get(at + 1).cloned()
}

fn label(argv: &[String]) -> Option<String> {
    argv.iter()
        .find_map(|a| a.strip_prefix("lmgw.llama-fit="))
        .map(str::to_string)
}

pub(super) fn now_hour() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    i64::try_from(secs / 3600).unwrap()
}

/// A day of other programs' use at a flat `bytes`: auto headroom learned,
/// at no growth.
fn learned_flat(f: &Fixture, bytes: u64) {
    let now = now_hour();
    f.state.vram.seed_outside_hours(
        (0..24)
            .map(|h| OutsideHour {
                hour: now - h,
                low_bytes: bytes,
                high_bytes: bytes,
                rise_bytes: 0,
            })
            .collect(),
    );
}

/// Finding 1: the fitted start's container carries its placement, which is
/// what boot adopts it by; a row the card holds whole is started exactly
/// as before, with nothing rendered over it.
#[tokio::test]
async fn a_fitted_start_records_its_placement_and_a_whole_one_renders_nothing() {
    let f = fixture(96 * MIB, 8 * MIB, 8 * MIB, 4).await;
    add_row(&f, "fit-model", LlamaParams::default()).await;
    start(&f, "fit-model").await.unwrap();
    let a = argv(&f, "fit-model");
    let l = label(&a).unwrap_or_else(|| panic!("no placement label: {a:?}"));
    let ctx = flag(&a, "--ctx-size").unwrap();
    assert!(
        l.contains(&format!("ctx={ctx},")) && l.ends_with("fit=off"),
        "{l}"
    );

    let f = fixture(512 * MIB, 8 * MIB, 8 * MIB, 4).await;
    add_row(&f, "fit-model", LlamaParams::default()).await;
    start(&f, "fit-model").await.unwrap();
    let a = argv(&f, "fit-model");
    assert_eq!(flag(&a, "--fit"), None, "{a:?}");
    assert_eq!(flag(&a, "--ctx-size"), None, "{a:?}");
    assert_eq!(label(&a), None, "{a:?}");
}

/// Finding 2: an image whose llama-server has no `--fit` loads the row as
/// written, so it is charged so — too large for this card — and no fit
/// flag is rendered anywhere.
#[tokio::test]
async fn an_image_without_fit_is_charged_as_written() {
    let f = fixture(96 * MIB, 8 * MIB, 8 * MIB, 4).await;
    f.world().llama_without_fit = true;
    add_row(&f, "fit-model", LlamaParams::default()).await;
    let err = start(&f, "fit-model").await.unwrap_err();
    assert!(err.contains("headroom"), "refused as too large: {err}");
    assert!(argv(&f, "fit-model").is_empty(), "never started");
}

/// Finding 5: a resident that is busy will finish, so a row that would
/// need less than its target to start beside it waits — and here runs out
/// of time — instead of starting with a smaller context for its life.
#[tokio::test]
async fn a_busy_resident_is_waited_for_rather_than_the_row_fitted_smaller() {
    let f = fixture(96 * MIB, 40 * MIB, 8 * MIB, 4).await;
    start(&f, "chat-model").await.unwrap();
    set_slot_busy(&f, true);
    set_queue_timeout(&f, 1).await;
    add_row(&f, "fit-model", LlamaParams::default()).await;
    let err = start(&f, "fit-model").await.unwrap_err();
    assert!(
        err.contains("chat-model") || err.contains("waited"),
        "a queue timeout naming what it waited for: {err}"
    );
    assert!(argv(&f, "fit-model").is_empty(), "not started smaller");
}

/// Finding 4: with the learned headroom in use, a llama-server that is
/// ready keeps its class's margin free beside it until its first request
/// has run; then the driver's figure has what the first prompt put there.
#[tokio::test]
async fn a_ready_llama_server_keeps_its_margin_until_its_first_request() {
    let f = fixture(4096 * MIB, 8 * MIB, 8 * MIB, 4).await;
    learned_flat(&f, 0);
    start(&f, "chat-model").await.unwrap();
    let note = |v: &Value| {
        v["resident"]
            .as_array()
            .and_then(|rs| rs.iter().find(|r| r["model"] == "chat-model").cloned())
            .map(|r| r["note"].as_str().unwrap_or_default().to_string())
            .unwrap_or_default()
    };
    let v = vram_status(&f.gateway).await;
    assert!(
        note(&v).contains("807 MiB is kept free beside it until its first request has run"),
        "{v}"
    );
    let resp = sized_chat(&f.gateway, "hello", 8).await;
    assert_eq!(resp.status(), 200);
    // The send ends with its response's body.
    resp.bytes().await.unwrap();
    let mut wait = crate::common::patience::Wait::new("the margin dropped after the request");
    loop {
        let v = vram_status(&f.gateway).await;
        if !note(&v).contains("until its first request") {
            break;
        }
        wait.again(Some(&v)).await;
    }
}

/// Finding 3 (a) and (c): no reading is taken while the hold keeps lmgw off
/// the card, and the owner's op forgets what was learned.
#[tokio::test]
async fn the_hold_teaches_nothing_and_the_growth_can_be_forgotten() {
    let f = fixture(4096 * MIB, 8 * MIB, 8 * MIB, 4).await;
    learned_flat(&f, 0);
    engage_hold(&f).await;
    assert_eq!(
        lmgw_core::vram::headroom::sample(&f.state).await,
        None,
        "no reading under the hold"
    );
    let snap = f.state.snapshot();
    assert!(f.state.vram.headroom_now(&snap).learned_in_use);
    let now: Value = f
        .gateway
        .client()
        .post(format!("{}/api/op/headroom_forget", f.gateway))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(now["hours_seen"], 0, "{now}");
    assert_eq!(now["learned_in_use"], false, "{now}");
    assert_eq!(now["sample_seconds"], 30, "{now}");
}

/// Finding 6: the command line `local_model_get` shows is the one the
/// running container was told, with where it comes from.
#[tokio::test]
async fn the_preview_shows_the_fitted_command() {
    let f = fixture(96 * MIB, 8 * MIB, 8 * MIB, 4).await;
    add_row(&f, "fit-model", LlamaParams::default()).await;
    // The card's total, as the dashboard's status poll reads it.
    vram_status(&f.gateway).await;
    let before = lmgw_core::ops::local_model_get(&f.state, None, Some("fit-model"), None)
        .await
        .unwrap();
    let lmgw_api_types::LocalModelRead::Chat(before) = before else {
        panic!("a chat row");
    };
    assert!(
        before
            .advisories
            .iter()
            .any(|a| a.contains("decided at each start")),
        "{:?} / {}",
        before.advisories,
        before.command_line
    );
    start(&f, "fit-model").await.unwrap();
    let got = lmgw_core::ops::local_model_get(&f.state, None, Some("fit-model"), None)
        .await
        .unwrap();
    let lmgw_api_types::LocalModelRead::Chat(got) = got else {
        panic!("a chat row");
    };
    assert!(
        got.command_line.contains("--fit off"),
        "{}",
        got.command_line
    );
    assert!(
        got.advisories
            .iter()
            .any(|a| a.contains("what its running container was told")),
        "{:?}",
        got.advisories
    );
}

/// Seen on a 24 GiB test GPU (2026-11-11): with a declared budget the card's
/// total is known before any device read, but not how many cards the
/// engine sees. A footprint fitted then was charged as written for good,
/// and the first start of a row was refused as too large.
#[tokio::test]
async fn a_first_start_under_a_declared_budget_is_fitted() {
    let f = fixture(512 * MIB, 8 * MIB, 8 * MIB, 4).await;
    let mut s = f.state.snapshot().settings.clone();
    s.vram.budget_mb = 96;
    store::save_settings(&f.state.db, &s).await.unwrap();
    f.state.reload_snapshot().await.unwrap();
    add_row(&f, "fit-model", LlamaParams::default()).await;
    let out = start(&f, "fit-model").await;
    assert!(out.is_ok(), "{out:?}");
    assert_eq!(
        flag(&argv(&f, "fit-model"), "--fit").as_deref(),
        Some("off")
    );
}
