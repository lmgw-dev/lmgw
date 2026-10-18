//! A chat row's footprint through `PlanCache::local`, from a GGUF on disk
//! (decision-models design, Appendix C, chat): the KV cache laid out for the
//! row's slots, the MTP context's own cache and compute buffer, the
//! recurrent snapshots a draft window keeps, the input embeddings left in
//! host memory, and the wire form a ladder rung carries.

use lmgw_core::config::{LlamaParams, LocalModel};
use lmgw_core::gguf::synth;
use lmgw_core::vram::plan::PlanCache;

const REL: &str = "org/hybrid/hybrid-q4.gguf";
const EMBD: u64 = 256;
const VOCAB: usize = 1000;
const HEAD_BYTES: u64 = 4096;
const TOKEN_EMBD_BYTES: u64 = 8192;

/// A Qwen3.5-shaped hybrid: 8 layers (every 4th attends) plus one MTP
/// layer, an untied output head.
fn write_model(dir: &std::path::Path) -> u64 {
    let tokens: Vec<String> = (0..VOCAB).map(|i| format!("t{i}")).collect();
    let tokens: Vec<&str> = tokens.iter().map(String::as_str).collect();
    let mut h = synth::chat("qwen35", 4096);
    h.u32("qwen35.block_count", 9)
        .u32("qwen35.embedding_length", EMBD as u32)
        .u32("qwen35.feed_forward_length", 512)
        .u32("qwen35.attention.head_count", 4)
        .u32("qwen35.attention.head_count_kv", 2)
        .u32("qwen35.attention.key_length", 64)
        .u32("qwen35.attention.value_length", 64)
        .u32("qwen35.full_attention_interval", 4)
        .u32("qwen35.ssm.conv_kernel", 4)
        .u32("qwen35.ssm.state_size", 16)
        .u32("qwen35.ssm.group_count", 2)
        .u32("qwen35.ssm.inner_size", 128)
        .u32("qwen35.nextn_predict_layers", 1)
        .arr_str("tokenizer.ggml.tokens", &tokens)
        .tensor_sized("token_embd.weight", TOKEN_EMBD_BYTES)
        .tensor_sized("output.weight", HEAD_BYTES);
    let path = dir.join(REL);
    h.write_to(&path);
    std::fs::metadata(&path).unwrap().len()
}

fn row(params: LlamaParams, args: &[&str]) -> LocalModel {
    LocalModel {
        id: 1,
        model_id: "hybrid".into(),
        gguf_path: REL.into(),
        params,
        args: args.iter().map(|s| s.to_string()).collect(),
        idle_seconds: 0,
        enabled: true,
        public: true,
        image: None,
        extra_run_args: None,
        warm_start: false,
        hold_fallback_mode: Default::default(),
        hold_fallback: None,
        capabilities_override: None,
        ladder: vec![],
    }
}

fn q8(ctx: i64) -> LlamaParams {
    LlamaParams {
        ctx_size: Some(ctx),
        parallel: Some(2),
        cache_type_k: Some("q8_0".into()),
        cache_type_v: Some("q8_0".into()),
        flash_attn: Some("on".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_chat_row_is_charged_its_contexts_caches_and_compute() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_model(dir.path());
    let models = dir.path().display().to_string();
    let plans = PlanCache::default();

    let plain = plans.local(&models, &row(q8(4096), &["-ub", "256"])).await;
    assert_eq!(plain.note, None, "everything is sized: {plain:?}");
    // The weights less token_embd, which stays in host memory beside an
    // untied head.
    assert_eq!(plain.weights_bytes, file - TOKEN_EMBD_BYTES);
    // Two attending layers of the eight in the main graph, two slots in two
    // streams: 4096 cells × 2 layers × 2 heads × (64 + 64) at q8_0's 8.5
    // bits; six recurrent layers, a state per slot.
    let kv = 4096 * 2 * 2 * 128 * 272 / 256;
    let state = (3 * (128 + 2 * 2 * 16) + 16 * 128) * 4;
    assert_eq!(plain.kv_cache_bytes, kv + 6 * state * 2);
    // Masks (two streams of 2048 cells over a 256-token batch), the f16 copy
    // of the q8_0 cache, and the attention phase: 16 · ub · (n_embd + 4 ×
    // 64), which outweighs the FFN's 4 · ub · (4 × 256 + 3 × 512).
    let compute = 2 * 2048 * 256 + 2 * 2 * 128 * 4096 + 16 * 256 * (EMBD + 4 * 64);
    assert_eq!(plain.compute_bytes, compute);
    assert_eq!(
        plain.total_bytes,
        plain.weights_bytes + plain.kv_cache_bytes + plain.compute_bytes
    );

    // draft-mtp with a window of 2: the MTP layer's own f16 cache, a second
    // compute buffer, and two more recurrent snapshots per slot.
    let mtp = LlamaParams {
        spec_type: Some("draft-mtp".into()),
        spec_draft_n_max: Some(2),
        ..q8(4096)
    };
    let fp = plans.local(&models, &row(mtp, &["-ub", "256"])).await;
    assert_eq!(fp.note, None, "{fp:?}");
    let mtp_kv = 4096 * 2 * 128 * 2;
    assert_eq!(
        fp.kv_cache_bytes,
        plain.kv_cache_bytes + mtp_kv + 6 * state * 2 * 2
    );
    // One attending layer over the f16 cache: masks + attention, no copy.
    let mtp_compute = 2 * 2048 * 256 + 16 * 256 * (EMBD + 4 * 64);
    assert_eq!(fp.compute_bytes, compute + mtp_compute);

    // A ladder rung carries the same terms on the wire.
    let rung = lmgw_api_types::RungFootprint::from(fp.clone());
    assert_eq!(rung.compute_bytes, fp.compute_bytes);
    assert_eq!(rung.total_bytes, fp.total_bytes);

    // The compute buffer grows with the context: the masks and the copy.
    let longer = plans.local(&models, &row(q8(8192), &["-ub", "256"])).await;
    assert_eq!(
        longer.compute_bytes - plain.compute_bytes,
        2 * 2048 * 256 + 2 * 2 * 128 * 4096
    );
}

/// The header keys the chat footprint reads, through a real header: shared
/// KV layers, the MLA rank and head dims, the per-layer input width, the
/// expert widths (one per layer: the largest), the MTP layers, the
/// recurrent-state keys, and the input embeddings of a tied head (only the
/// per-layer table stays in host memory).
#[test]
fn the_chat_keys_parse_from_a_header() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.gguf");
    let mut h = synth::chat("probe", 4096);
    h.u32("probe.attention.shared_kv_layers", 18)
        .u32("probe.attention.kv_lora_rank", 512)
        .u32("probe.attention.key_length_mla", 192)
        .u32("probe.attention.value_length_mla", 128)
        .u32("probe.embedding_length_per_layer_input", 256)
        .u32("probe.expert_used_count", 8)
        .arr_u64("probe.expert_feed_forward_length", &[512, 704, 640, 704])
        .u32("probe.expert_shared_feed_forward_length", 512)
        .u32("probe.nextn_predict_layers", 1)
        .u32("probe.shortconv.l_cache", 3)
        .tensor_sized("token_embd.weight", 4096)
        .tensor_sized("per_layer_token_embd.weight", 2048);
    h.write_to(&path);
    let s = lmgw_core::gguf::summarize(&path).unwrap();
    assert_eq!(s.shared_kv_layers, Some(18));
    assert_eq!(s.kv_lora_rank, Some(512));
    assert_eq!(
        (s.key_length_mla, s.value_length_mla),
        (Some(192), Some(128))
    );
    assert_eq!(s.embedding_length_per_layer_input, Some(256));
    assert_eq!(s.expert_used_count, Some(8));
    assert_eq!(s.expert_feed_forward_length, Some(704));
    assert_eq!(s.expert_shared_feed_forward_length, Some(512));
    assert_eq!(s.nextn_predict_layers, Some(1));
    assert!(s.has_recurrent_keys);
    // Tied: token_embd is copied to the card for the logits.
    assert_eq!(s.host_input_bytes, 2048);

    // An untied head leaves token_embd in host memory; a plain header has
    // no recurrent keys and no MLA dims.
    let mut h = synth::chat("plain", 4096);
    h.u32("plain.attention.kv_lora_rank", 512)
        .tensor_sized("token_embd.weight", 4096)
        .tensor_sized("output.weight", 1024);
    h.write_to(&path);
    let s = lmgw_core::gguf::summarize(&path).unwrap();
    assert_eq!(s.host_input_bytes, 4096);
    assert!(!s.has_recurrent_keys);
    assert_eq!((s.key_length_mla, s.value_length_mla), (None, None));
}
