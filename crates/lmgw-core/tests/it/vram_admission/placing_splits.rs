//! Splits (device-ledger design §4.3, §5.2 steps 3 and 4, §5.4, §7.2,
//! WP6): a chat model with a set context larger than any card's free
//! memory starts over the fewest cards that hold it, each card handed by
//! UUID in the bus's order and told its layers with `--tensor-split`; each
//! card is charged its part, the container records the split, and boot
//! adopts it. A row that sets no context is never split (ruling 15). A
//! row's own `--tensor-split` is `placing_own_splits`' (ruling 11).

use super::llama_fit_review::{argv, flag, start};
use super::unheld_containers::{entry, forget_everything, pass};
use super::*;
use lmgw_core::config::LlamaParams;
use lmgw_core::gguf::synth;
use lmgw_core::runtime::argv::{FITTED_LABEL, HANDED_LABEL};
use lmgw_core::vram::charge::PLACEMENT_LABEL;
use lmgw_core::vram::headroom::LLAMA_MARGIN;
use lmgw_core::vram::CardCharge;

const MIB: u64 = 1024 * 1024;

/// A dense synthetic chat model: 8 layers of 4 MiB, an output head and
/// input embeddings of 2 MiB each, 2 KV heads of 64 (512 bytes of f16
/// cache a token and layer), trained on 32768 tokens: at a context of 4096
/// each layer holds 4 MiB of weights and 2 MiB of cache.
pub(super) fn write_model(f: &Fixture, file: &str) {
    write_model_with(f, file, 4);
}

/// [`write_model`] with layers of `layer` MiB of weights: a split's compute
/// buffers carry tens of MiB more than one card's (`by_layer`), so a model
/// that only a split holds is larger than [`write_model`]'s.
pub(super) fn write_model_with(f: &Fixture, file: &str, layer: u64) {
    let mut h = synth::chat("llama", 32768);
    h.u32("llama.block_count", 8)
        .u32("llama.embedding_length", 256)
        .u32("llama.feed_forward_length", 512)
        .u32("llama.attention.head_count", 4)
        .u32("llama.attention.head_count_kv", 2)
        .u32("llama.vocab_size", 1024);
    for i in 0..8 {
        h.tensor_sized(&format!("blk.{i}.attn_q.weight"), layer * MIB / 4);
        h.tensor_sized(&format!("blk.{i}.ffn_up.weight"), 3 * layer * MIB / 4);
    }
    h.tensor_sized("output.weight", 2 * MIB);
    h.tensor_sized("token_embd.weight", 2 * MIB);
    h.write_to(&f._models_dir.path().join(file));
}

/// Chat row `model_id` of [`write_model`]'s model, with `params` and
/// `args`.
pub(super) async fn add_row(f: &Fixture, model_id: &str, params: LlamaParams, args: &[&str]) {
    add_row_with(f, model_id, 4, params, args).await;
}

/// [`add_row`] of [`write_model_with`]'s model of `layer` MiB layers.
pub(super) async fn add_row_with(
    f: &Fixture,
    model_id: &str,
    layer: u64,
    params: LlamaParams,
    args: &[&str],
) {
    let file = format!("{model_id}.gguf");
    write_model_with(f, &file, layer);
    f.world().size.insert(model_id.to_string(), 50 * MIB);
    store::insert_local_model(
        &f.state.db,
        &NewLocalModel {
            model_id: model_id.into(),
            gguf_path: file,
            params,
            args: args.iter().map(|a| a.to_string()).collect(),
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

/// A context of 4096, set.
pub(super) fn set_context() -> LlamaParams {
    LlamaParams {
        ctx_size: Some(4096),
        ..Default::default()
    }
}

/// Layers of [`BIG`] MiB: [`add_big`]'s model, 8 of them, the output
/// head and the cache about 290 MiB at 4096 tokens.
pub(super) const BIG: u64 = 32;

/// [`add_row_with`] of a model of [`BIG`] MiB layers.
pub(super) async fn add_big(f: &Fixture, model_id: &str, params: LlamaParams, args: &[&str]) {
    add_row_with(f, model_id, BIG, params, args).await;
}

/// What other programs hold on [`four_cards`]' cards, and so what is free
/// there: 160, 260, 270 and 120 MiB.
pub(super) const FOUR_OUTSIDE: [u64; 4] = [352, 252, 242, 392];

/// Four cards of 512 MiB with 160, 260, 270 and 120 MiB free: no card holds
/// [`add_big`]'s model whole (about 290 MiB), cards 2 and 1 together do. A
/// ledger read first, so lmgw knows the machine has several cards.
pub(super) async fn four_cards() -> Fixture {
    let f = fixture(2048 * MIB + LLAMA_MARGIN, 8 * MIB, 8 * MIB, 4).await;
    f.world().cards = (0..4)
        .map(|i| (format!("card {i}"), 512 * MIB + LLAMA_MARGIN))
        .collect();
    f.world().outside_on = (0..4).map(|i| (i, FOUR_OUTSIDE[i] * MIB)).collect();
    vram_status(&f.gateway).await;
    f
}

/// The NVIDIA devices an argv passes, as written after `--device`.
pub(super) fn devices(argv: &[String]) -> Vec<String> {
    argv.windows(2)
        .filter(|w| w[0] == "--device")
        .filter_map(|w| w[1].strip_prefix("nvidia.com/gpu="))
        .map(str::to_string)
        .collect()
}

/// The value of label `name` on an argv.
pub(super) fn label(argv: &[String], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    argv.iter()
        .find_map(|a| a.strip_prefix(prefix.as_str()))
        .map(str::to_string)
}

/// The verification of WP6 (§13): a model with a set context larger than
/// any card's free memory starts over the fewest cards, and each card holds
/// what the placement says, within the margin — here the fixed headroom of
/// 4 MiB kept free beside it on each card.
#[tokio::test]
async fn a_set_context_no_card_holds_starts_over_the_fewest_cards() {
    let f = four_cards().await;
    add_big(&f, "split-model", set_context(), &[]).await;
    start(&f, "split-model").await.unwrap();
    let a = argv(&f, "split-model");

    // Cards 1 and 2 by UUID, in the bus's order, with the engine told so.
    assert_eq!(
        devices(&a),
        vec![card_id(1).0.clone(), card_id(2).0.clone()],
        "{a:?}"
    );
    assert!(
        a.iter()
            .any(|x| x == &format!("NVIDIA_VISIBLE_DEVICES={},{}", card_id(1).0, card_id(2).0)),
        "{a:?}"
    );
    assert!(
        a.iter().any(|x| x == "CUDA_DEVICE_ORDER=PCI_BUS_ID"),
        "{a:?}"
    );
    assert!(label(&a, HANDED_LABEL).is_some(), "{a:?}");

    // The split: layer counts that sum to n_layer + 1, each card some.
    assert_eq!(flag(&a, "--split-mode").as_deref(), Some("layer"), "{a:?}");
    assert_eq!(flag(&a, "--fit").as_deref(), Some("off"), "{a:?}");
    let entries: Vec<u64> = flag(&a, "--tensor-split")
        .expect("a split")
        .split(',')
        .map(|e| e.parse().unwrap())
        .collect();
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert_eq!(
        entries.iter().sum::<u64>(),
        9,
        "8 layers and the output layer"
    );
    assert!(entries.iter().all(|e| *e > 0), "{entries:?}");
    assert!(
        entries[1] > entries[0],
        "card 2, the roomiest, is filled first: {entries:?}"
    );
    let fit = label(&a, FITTED_LABEL).expect("the split is recorded");
    assert!(
        fit.contains(&format!("split={}/{}", entries[0], entries[1])),
        "{fit}"
    );

    // Each card is charged its part, and it is within what was free there
    // beside the 4 MiB kept free.
    let charge = CardCharge::parse(&label(&a, PLACEMENT_LABEL).unwrap()).unwrap();
    let parts = charge.split_parts().expect("a part on each card");
    assert_eq!(parts[0].0, card_id(1));
    assert_eq!(parts[1].0, card_id(2));
    assert!(parts[0].1 <= (260 - 4) * MIB, "{parts:?}");
    assert!(parts[1].1 <= (270 - 4) * MIB, "{parts:?}");
    assert!(
        parts[0].1 + parts[1].1 > 8 * (BIG + 2) * MIB,
        "every layer's weights and cache: {parts:?}"
    );

    // The driver shows each part on its card, and the ledger charges it
    // there.
    let v = vram_status(&f.gateway).await;
    let used: Vec<u64> = v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["used_bytes"].as_u64().unwrap())
        .collect();
    assert_eq!(
        used,
        vec![
            FOUR_OUTSIDE[0] * MIB,
            FOUR_OUTSIDE[1] * MIB + parts[0].1,
            FOUR_OUTSIDE[2] * MIB + parts[1].1,
            FOUR_OUTSIDE[3] * MIB
        ],
        "{v}"
    );
    let resident = v["resident"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["model"] == "split-model")
        .cloned()
        .unwrap_or_else(|| panic!("{v}"));
    assert_eq!(
        resident["charge"].as_array().map(Vec::len),
        Some(2),
        "{resident}"
    );

    // The start says how it was split.
    let warnings = entry(&f, "split-model").unwrap().warnings;
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("lmgw splits it over 2 cards") && w.contains("--tensor-split")),
        "{warnings:?}"
    );
}

/// Boot adopts a split container from its labels: the command it renders
/// to compare has the split, so it is not started again, and it is charged
/// its part on each card as recorded.
#[tokio::test]
async fn a_split_container_is_adopted_with_its_parts() {
    let f = four_cards().await;
    f.world().reports = true;
    add_big(&f, "split-model", set_context(), &[]).await;
    start(&f, "split-model").await.unwrap();
    let recorded = label(&argv(&f, "split-model"), PLACEMENT_LABEL).unwrap();
    forget_everything(&f);
    assert!(pass(&f).await, "adopted");
    let e = entry(&f, "split-model").expect("adopted");
    assert!(e.adopted);
    assert_eq!(f.world().runs.len(), 1, "not started again");
    assert_eq!(
        e.card_charge.and_then(|c| c.label()),
        Some(recorded),
        "charged as recorded"
    );
}

/// A row that sets no context is never split (ruling 15): it goes on the
/// roomiest card, its context fitted to it.
#[tokio::test]
async fn a_row_without_a_context_stays_on_one_card() {
    let f = four_cards().await;
    add_big(&f, "split-model", LlamaParams::default(), &[]).await;
    start(&f, "split-model").await.unwrap();
    let a = argv(&f, "split-model");
    assert_eq!(devices(&a), vec![card_id(2).0.clone()], "{a:?}");
    assert_eq!(flag(&a, "--tensor-split"), None, "{a:?}");
    let ctx: u64 = flag(&a, "--ctx-size").unwrap().parse().unwrap();
    assert!(ctx < 32768, "fitted to the card: {ctx}");
}

/// A set-context row that does not adapt (`--n-gpu-layers 999`, so
/// `--fit` moves nothing) and is larger than either of two 520 MiB cards:
/// only a split holds it. While a busy model holds card 0, no split fits,
/// and `place` answers the roomiest card, which can never hold it (owner,
/// 2026-11-18): the request commits to no card, so it is not stuck there
/// until its timeout. Out of time, it names what it waited for — the busy
/// model on the cards the split needs, not "another process"; asked again,
/// it starts split over both cards once that model has finished.
#[tokio::test]
async fn a_model_only_a_split_holds_starts_split_once_the_busy_model_finishes() {
    // The busy model is ready and has served nothing, so its 327 MiB
    // first-prompt margin is kept on card 0 beside it (`ledger`) and only
    // its own bytes count as room once it is idle (`place`): cards and model
    // large enough that the split still fits then.
    let f = fixture(1040 * MIB, 30 * MIB, 8 * MIB, 4).await;
    f.world().cards = (0..2).map(|i| (format!("card {i}"), 520 * MIB)).collect();
    vram_status(&f.gateway).await;
    // A request held open on the chat model: busy as lmgw counts it.
    let route = f.state.snapshot().resolve("chat-model").unwrap();
    let hold = lmgw_core::vram::admit(&f.state, &route, "chat-model")
        .await
        .expect("the chat model is admitted")
        .expect("a managed local model gets a hold");
    assert_eq!(devices(&argv(&f, "chat-model")), vec![card_id(0).0.clone()]);
    add_row_with(
        &f,
        "split-model",
        64,
        LlamaParams {
            ctx_size: Some(4096),
            n_gpu_layers: Some(999),
            ..Default::default()
        },
        &[],
    )
    .await;
    let body = json!({"model": "split-model", "messages": [{"role": "user", "content": "hi"}]});

    // Busy throughout: out of time, naming the busy model and the cards.
    let r = chat_body(&f.gateway, body.clone()).await;
    assert_eq!(r.status(), 503);
    let text = r.text().await.unwrap();
    assert!(
        text.contains("no card holds it whole")
            && text.contains("chat/chat-model")
            && text.contains("card 0")
            && text.contains("card 1")
            && !text.contains("another process"),
        "{text}"
    );
    assert!(f.stops().is_empty(), "the busy model is never evicted");
    assert!(argv(&f, "split-model").is_empty(), "not started");

    // Asked again; the busy model finishes while it waits.
    let gw = f.gateway.clone();
    let waiter = tokio::spawn(async move {
        let r = chat_body(&gw, body).await;
        (r.status(), r.text().await.unwrap_or_default())
    });
    let mut waiting = false;
    for _ in 0..100 {
        let v = vram_status(&f.gateway).await;
        waiting = v["queue"].as_array().is_some_and(|q| {
            q.iter().any(|w| {
                w["phase"]["kind"] == "waiting_for_room" || w["phase"] == "waiting_for_room"
            })
        });
        if waiting {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(waiting, "{}", vram_status(&f.gateway).await);
    drop(hold);
    let (status, text) = waiter.await.unwrap();
    assert_eq!(status, 200, "{text}");
    let a = argv(&f, "split-model");
    assert_eq!(
        devices(&a),
        vec![card_id(0).0.clone(), card_id(1).0.clone()],
        "{a:?}"
    );
    assert!(flag(&a, "--tensor-split").is_some(), "{a:?}");
}
