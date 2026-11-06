//! A plan from Hugging Face before any download (find-a-model design WP2,
//! §2.3, §3, §10): against a mock hub and a Range-capable mock CDN serving
//! the same synthetic GGUFs `plan_pin` writes to disk.
//!
//! - The hub plan of a file is the disk plan of the same bytes once they
//!   are where `hf_add` puts them, `source` aside: for a chat model with two
//!   projectors and a drafter, a split chat model and an embedder.
//! - Only the picked projector's and the drafter's headers are read, and a
//!   split file's later parts are read for its whole count and width (D131).
//! - The pick across a repo's quants, on the whole card and beside a program
//!   that holds part of it (D7, D120), unchanged by the GPU hold (D10).
//! - A header the CDN cuts is marked by its size alone and never picked (D8).
//!
//! The names are fake and nothing here reaches the network. Every test sets
//! `HF_ENDPOINT`, so each holds `common::process_env_lock` for its body.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use lmgw_api_types::model_find::{FitMark, FitMeasure, PickRule, PlanFrom};
use lmgw_core::config::Settings;
use lmgw_core::gguf::synth;
use lmgw_core::modelinfo::{self, hub_fit};
use lmgw_core::state::{AppState, SharedState};
use lmgw_core::store;
use lmgw_core::vram::{GpuMemory, GpuProbe};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use crate::plan_pin;

pub(crate) const REPO: &str = "demo-org/demo-GGUF";
const COMMIT: &str = "1111111111111111111111111111111111111111";
pub(crate) const MIB: u64 = 1 << 20;

fn sha(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// What the mock CDN serves, by repo path, and which paths it was asked for.
#[derive(Clone, Default)]
pub(crate) struct Cdn {
    files: Arc<HashMap<String, Vec<u8>>>,
    /// Paths whose body ends this many bytes in: a cut stream.
    cut_at: Arc<HashMap<String, usize>>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Cdn {
    fn answer(&self, r: &Request) -> ResponseTemplate {
        let file = r
            .url
            .path()
            .strip_prefix("/cdn/")
            .unwrap_or_default()
            .to_string();
        self.asked.lock().unwrap().push(file.clone());
        let Some(bytes) = self.files.get(&file) else {
            return ResponseTemplate::new(404);
        };
        let total = bytes.len();
        let end = self.cut_at.get(&file).copied().unwrap_or(total);
        ResponseTemplate::new(206)
            .insert_header("accept-ranges", "bytes")
            .insert_header(
                "content-range",
                format!("bytes 0-{}/{total}", total - 1).as_str(),
            )
            .set_body_bytes(bytes[..end].to_vec())
    }

    pub(crate) fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

/// A mock hub for `REPO` holding `files` (path, bytes), its CDN, and the CDN's
/// log. The hub's answers are shaped as WP1's tests record them: a tree
/// listing with `lfs` objects, and a `302` per file naming its hash and size.
pub(crate) async fn hub(
    files: &[(String, Vec<u8>)],
    cut: &[(&str, usize)],
) -> (MockServer, MockServer, Cdn) {
    let hub = MockServer::start().await;
    let cdn_server = MockServer::start().await;
    let cdn = Cdn {
        files: Arc::new(files.iter().cloned().collect()),
        cut_at: Arc::new(cut.iter().map(|(p, n)| (p.to_string(), *n)).collect()),
        asked: Default::default(),
    };
    let listing: Vec<Value> = files
        .iter()
        .map(|(p, b)| {
            json!({"type": "file", "path": p, "size": b.len(),
                   "lfs": {"oid": sha(b), "size": b.len()}})
        })
        .collect();
    Mock::given(method("GET"))
        .and(path(format!("/api/models/{REPO}/tree/main")))
        .respond_with(ResponseTemplate::new(200).set_body_json(listing))
        .mount(&hub)
        .await;
    for (p, b) in files {
        Mock::given(method("GET"))
            .and(path(format!("/{REPO}/resolve/main/{p}")))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("x-repo-commit", COMMIT)
                    .insert_header("x-linked-etag", format!("\"{}\"", sha(b)).as_str())
                    .insert_header("x-linked-size", b.len().to_string().as_str())
                    .insert_header("location", format!("{}/cdn/{p}", cdn_server.uri()).as_str()),
            )
            .mount(&hub)
            .await;
    }
    let c = cdn.clone();
    Mock::given(method("GET"))
        .respond_with(move |r: &Request| c.answer(r))
        .mount(&cdn_server)
        .await;
    std::env::set_var("HF_ENDPOINT", hub.uri());
    (hub, cdn_server, cdn)
}

/// A probe that reads one card of `total` with `used` taken by other
/// programs.
struct Card {
    total: u64,
    used: u64,
}

impl GpuProbe for Card {
    fn devices(&self) -> Result<Vec<GpuMemory>, String> {
        Ok(vec![GpuMemory {
            index: 0,
            name: "demo card".into(),
            total_bytes: self.total,
            used_bytes: self.used,
            free_bytes: self.total - self.used,
        }])
    }

    fn source(&self) -> String {
        "stated probe".into()
    }
}

/// `plan` without the key the two sources differ in.
fn but_source(plan: &Value) -> Value {
    let mut p = plan.as_object().unwrap().clone();
    p.remove("source");
    Value::Object(p)
}

/// Every file of `plan_pin` on the mock hub, and on disk where `hf_add`
/// would put it: the hub plan and the disk plan of the same bytes are the
/// same plan (§10), by both fits too.
#[tokio::test]
async fn a_hub_plan_is_the_disk_plan_of_the_same_bytes() {
    let _env = crate::common::process_env_lock().await;
    let (state, dir) = plan_pin::fixture(160).await;
    let mut served = Vec::new();
    for (class, rel, h) in plan_pin::files() {
        let bytes = h.bytes();
        let at = dir.path().join(class).join(REPO).join(rel);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(&at, &bytes).unwrap();
        served.push((rel.to_string(), bytes));
    }
    let (_hub, _cdn_server, cdn) = hub(&served, &[]).await;

    for (rel, target) in [
        (plan_pin::CHAT, "chat"),
        (plan_pin::SPLIT1, "chat"),
        (plan_pin::EMBED, "aux"),
    ] {
        let disk = modelinfo::local_model_plan(&state, &format!("{REPO}/{rel}"), Some(target))
            .await
            .unwrap();
        let from_hub = modelinfo::local_model_plan_of(&state, None, Some(REPO), Some(rel), None)
            .await
            .unwrap();
        assert_eq!(but_source(&from_hub), but_source(&disk), "{rel}");
        let source: lmgw_api_types::model_find::PlanSource =
            serde_json::from_value(from_hub["source"].clone()).unwrap();
        assert_eq!(source.from, PlanFrom::Hub);
        assert_eq!(source.file.as_deref(), Some(rel));
        assert_eq!(source.commit.as_deref(), Some(COMMIT));
        assert!(source.header_bytes_read > 0, "{source:?}");
        assert_eq!(disk["source"]["from"], "disk");
        // The typed answer reads both.
        let typed: lmgw_api_types::PlanResult = serde_json::from_value(from_hub.clone()).unwrap();
        assert_eq!(
            typed.fit.as_ref().unwrap().whole_card.mark,
            FitMark::FitsShorter,
            "{rel}: {typed:?}"
        );
    }
    // One projector of the two, by the rule, and the drafter: the wider
    // projector's header was never read.
    let asked = cdn.asked();
    assert!(asked.contains(&plan_pin::CHAT_F16.to_string()), "{asked:?}");
    assert!(asked.contains(&plan_pin::DRAFTER.to_string()), "{asked:?}");
    assert!(
        !asked.contains(&plan_pin::CHAT_F32.to_string()),
        "{asked:?}"
    );
}

/// A split file's whole count and width come from every part's header
/// (D131): the second part is read too, and the fit carries both parts'.
#[tokio::test]
async fn a_split_files_whole_count_reads_its_later_parts() {
    let _env = crate::common::process_env_lock().await;
    let (state, _dir) = plan_pin::fixture(64).await;
    let served: Vec<(String, Vec<u8>)> = [
        (plan_pin::SPLIT1, plan_pin::split_part1()),
        (plan_pin::SPLIT2, plan_pin::split_part2()),
    ]
    .into_iter()
    .map(|(p, h)| (p.to_string(), h.bytes()))
    .collect();
    let (_hub, _cdn_server, cdn) = hub(&served, &[]).await;
    let plan = modelinfo::local_model_plan_hub(&state, REPO, plan_pin::SPLIT1, None)
        .await
        .unwrap();
    let fit = &plan["fit"]["whole_card"];
    // Part 1: token_embd and two layers; part 2: two layers; 4 Mi each.
    assert_eq!(fit["parameters"], 5 * 1024 * 4096, "{fit}");
    // Every layer is 8 MiB over 4 Mi elements: 16 bits, over both parts.
    assert_eq!(fit["bits_per_weight"], 16.0, "{fit}");
    assert!(
        cdn.asked().contains(&plan_pin::SPLIT2.to_string()),
        "{:?}",
        cdn.asked()
    );
    let weights = plan["warnings"].to_string();
    assert!(!weights.contains("is one part of"), "{weights}");
}

/// A chat model with an untied head, whole or as two parts: the input
/// embeddings and layers 0 and 1 first, layers 2 and 3 and `output.weight`
/// (8 MiB each) in the second part, so only the later part says the head is
/// untied.
fn untied_set(split: bool) -> Vec<synth::Header> {
    let mut h = synth::chat("qwen3", 65_536);
    h.u32("qwen3.block_count", 4)
        .u32("qwen3.embedding_length", 1024)
        .u32("qwen3.feed_forward_length", 2048)
        .u32("qwen3.attention.head_count", 8)
        .u32("qwen3.attention.head_count_kv", 8)
        .u32("qwen3.attention.key_length", 128)
        .u32("qwen3.attention.value_length", 128);
    let mut later = synth::Header::default();
    if split {
        h.u64("split.no", 0)
            .u64("split.count", 2)
            .u64("split.tensors.count", 7);
        later
            .u64("split.no", 1)
            .u64("split.count", 2)
            .u64("split.tensors.count", 7);
    }
    let layer = |h: &mut synth::Header, name: &str| {
        h.tensor_shaped(name, &[1024, 4096], 8 * MIB);
    };
    layer(&mut h, "token_embd.weight");
    layer(&mut h, "blk.0.ffn_up.weight");
    layer(&mut h, "blk.1.ffn_up.weight");
    let target = if split { &mut later } else { &mut h };
    layer(target, "blk.2.ffn_up.weight");
    layer(target, "blk.3.ffn_up.weight");
    layer(target, "output.weight");
    if split {
        vec![h, later]
    } else {
        vec![h]
    }
}

/// A split set whose untied head sits in its later part costs what the
/// single file of the same model costs, from the hub as from the disk: the
/// input embeddings stay on the host (`plan_files::set_summary` over the
/// parts the hub read), and the weights differ by the parts' own headers.
#[tokio::test]
async fn a_hub_plan_of_a_split_set_with_a_later_head_is_the_disk_plan() {
    let _env = crate::common::process_env_lock().await;
    let (state, dir) = plan_pin::fixture(256).await;
    const ONE: &str = "untied-one/demo-chat-BF16.gguf";
    const P1: &str = "untied/demo-chat-BF16-00001-of-00002.gguf";
    const P2: &str = "untied/demo-chat-BF16-00002-of-00002.gguf";
    let mut served = Vec::new();
    for (rel, h) in [(ONE, &untied_set(false)[0])]
        .into_iter()
        .chain([(P1, &untied_set(true)[0]), (P2, &untied_set(true)[1])])
    {
        let bytes = h.bytes();
        let at = dir.path().join("chat").join(REPO).join(rel);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(&at, &bytes).unwrap();
        served.push((rel.to_string(), bytes));
    }
    let (_hub, _cdn_server, cdn) = hub(&served, &[]).await;

    let disk = modelinfo::local_model_plan(&state, &format!("{REPO}/{P1}"), Some("chat"))
        .await
        .unwrap();
    let from_hub = modelinfo::local_model_plan_hub(&state, REPO, P1, None)
        .await
        .unwrap();
    assert_eq!(but_source(&from_hub), but_source(&disk));
    assert!(cdn.asked().contains(&P2.to_string()), "{:?}", cdn.asked());

    let one = modelinfo::local_model_plan_hub(&state, REPO, ONE, None)
        .await
        .unwrap();
    let terms = |p: &Value| p["fit"]["whole_card"]["terms"].clone();
    let (split_terms, one_terms) = (terms(&from_hub), terms(&one));
    assert_eq!(split_terms["kv_cache_bytes"], one_terms["kv_cache_bytes"]);
    assert_eq!(split_terms["compute_bytes"], one_terms["compute_bytes"]);
    // Kept on the host, the 8 MiB of embeddings are off the card's weights
    // in both layouts: the split set's weights are the single file's plus
    // the extra header bytes of its second part, never 8 MiB more.
    let header = |rel: &str| {
        lmgw_core::gguf::summarize(&dir.path().join("chat").join(REPO).join(rel))
            .unwrap()
            .header_bytes
    };
    let extra = header(P1) + header(P2) - header(ONE);
    let weights = |t: &Value| t["weights_bytes"].as_u64().unwrap();
    assert_eq!(weights(&split_terms), weights(&one_terms) + extra);
    assert_eq!(weights(&one_terms), 48 * MIB - 8 * MIB + header(ONE));
}

/// The same chat model in five widths, 4 layers of 4 Mi weights each, as
/// llama.cpp's quantizer writes them: each layer's matrix of the file's
/// ggml type (Q3_K, Q4_K, Q8_0, BF16, F32) and its norm F32, so the Q8_0
/// reads Q8_0's own 8.5 bits with the norms beside it (D7's cap).
pub(crate) fn quants() -> Vec<(String, Vec<u8>)> {
    [
        ("Q3_K_M", 11),
        ("Q4_K_M", 12),
        ("Q8_0", 8),
        ("BF16", 30),
        ("F32", 0),
    ]
    .into_iter()
    .map(|(q, ty)| {
        let mut h = synth::chat("qwen3", 4096);
        h.u32("qwen3.block_count", 4)
            .u32("qwen3.embedding_length", 1024)
            .u32("qwen3.attention.head_count", 8)
            .u32("qwen3.attention.head_count_kv", 8)
            .u32("qwen3.attention.key_length", 128)
            .u32("qwen3.attention.value_length", 128);
        h.tensor_shaped("token_embd.weight", &[1024, 1024], MIB);
        for i in 0..4 {
            h.tensor_typed(&format!("blk.{i}.attn_norm.weight"), &[1024], 0)
                .tensor_typed(&format!("blk.{i}.ffn_up.weight"), &[1024, 4096], ty);
        }
        (format!("quants/demo-chat-{q}.gguf"), h.bytes())
    })
    .collect()
}

/// A state on a card of `total` bytes with `used` taken by other programs,
/// 32 MiB of headroom, no budget, and the hold as `hold`.
pub(crate) async fn on_card(total: u64, used: u64, hold: bool) -> SharedState {
    let state = AppState::init_for_tests().await.unwrap();
    let mut settings = Settings::default();
    settings.vram.headroom_mb = 32;
    settings.hold.active = hold;
    store::save_settings(&state.db, &settings).await.unwrap();
    state.vram.set_probe(Arc::new(Card { total, used }));
    state.reload_snapshot().await.unwrap();
    state
}

pub(crate) async fn fits_of(
    state: &SharedState,
    files: &[(String, Vec<u8>)],
) -> Vec<hub_fit::FileFits> {
    let listing = lmgw_core::hf::list_repo_files(&state.http, "", REPO)
        .await
        .unwrap();
    let mut out = Vec::new();
    for (p, _) in files {
        out.push(hub_fit::file_fit(state, REPO, p, &listing, None).await);
    }
    out
}

/// D7 and D120 across a repo's quants: on the whole card the widest file of
/// the Q8 class that keeps all 4096 tokens (the BF16 and F32 files fit too,
/// and keep their marks); beside a program holding part of the card the
/// Q8_0 fits only a shorter conversation, so the pick is the Q4_K_M. The
/// 3-bit file is never the pick. The hold changes neither (D10). The other
/// program's share is set between the Q4_K_M's and the Q8_0's footprints,
/// read on an empty card first.
#[tokio::test]
async fn the_pick_on_the_whole_card_and_beside_what_runs() {
    let _env = crate::common::process_env_lock().await;
    let files = quants();
    let (_hub, _cdn_server, _cdn) = hub(&files, &[]).await;
    let total = 192 * MIB;
    let empty = fits_of(&on_card(total, 0, false).await, &files).await;
    let (q4, q8) = (
        empty[1].fit.whole_card.uses_bytes,
        empty[2].fit.whole_card.uses_bytes,
    );
    assert!(q4 < q8, "{q4} {q8}");
    let used = total - 32 * MIB - (q4 + q8) / 2;
    for hold in [false, true] {
        let state = on_card(total, used, hold).await;
        let fits = fits_of(&state, &files).await;
        let mark = |i: usize, m: FitMeasure| match m {
            FitMeasure::WholeCard => fits[i].fit.whole_card.mark,
            FitMeasure::Beside => fits[i].fit.beside.mark,
        };
        for f in &fits {
            assert_eq!(f.fit.whole_card.mark, FitMark::Fits, "{f:?}");
        }
        assert_eq!(fits[2].fit.whole_card.bits_per_weight, Some(8.5));
        assert_eq!(
            mark(1, FitMeasure::Beside),
            FitMark::Fits,
            "{:?}",
            fits[1].fit.beside
        );
        assert_eq!(
            mark(2, FitMeasure::Beside),
            FitMark::FitsShorter,
            "{:?}",
            fits[2].fit.beside
        );
        let beside = &fits[2].fit.beside;
        assert_eq!(beside.beside.as_ref().unwrap().outside_bytes, Some(used));
        let (whole, by) = hub_fit::pick_both(&fits);
        assert_eq!(
            whole.file.as_deref(),
            Some("quants/demo-chat-Q8_0.gguf"),
            "{whole:?}"
        );
        assert_eq!(whole.rule, PickRule::WholeContext);
        assert_eq!(
            by.file.as_deref(),
            Some("quants/demo-chat-Q4_K_M.gguf"),
            "{by:?}"
        );
        assert!(
            by.why.contains("beside what's on your graphics card"),
            "{}",
            by.why
        );
        let held = fits[2]
            .fit
            .whole_card
            .notes
            .iter()
            .any(|n| n == modelinfo::fit::HOLD_LINE);
        assert_eq!(held, hold, "{:?}", fits[2].fit.whole_card.notes);
        assert_eq!(fits[2].fit.card.as_ref().unwrap().hold, hold);
    }
}

/// D8: a header the CDN cuts is marked by its listed size alone, says why,
/// and is never the pick, though it is the widest file that would fit.
#[tokio::test]
async fn an_unreadable_header_falls_back_to_its_size() {
    let _env = crate::common::process_env_lock().await;
    let files = quants();
    let cut = "quants/demo-chat-Q8_0.gguf";
    let (_hub, _cdn_server, _cdn) = hub(&files, &[(cut, 40)]).await;
    let state = on_card(192 * MIB, 0, false).await;
    let fits = fits_of(&state, &files).await;
    let q8 = &fits[2];
    assert!(q8.files.is_none());
    assert_eq!(q8.fit.whole_card.mark, FitMark::FitsBySize, "{:?}", q8.fit);
    assert_eq!(q8.fit.whole_card.uses_bytes, files[2].1.len() as u64);
    assert!(
        q8.fit
            .whole_card
            .notes
            .iter()
            .any(|n| n.contains("marked by its size")),
        "{:?}",
        q8.fit.whole_card.notes
    );
    let (whole, _) = hub_fit::pick_both(&fits);
    assert_eq!(
        whole.file.as_deref(),
        Some("quants/demo-chat-Q4_K_M.gguf"),
        "{whole:?}"
    );
}

/// The other files of a repo estimated from one header (§3.3): never below
/// the exact footprint, over a repo whose files keep untied input embeddings
/// on the host, each a different width.
#[tokio::test]
async fn an_estimate_from_another_files_header_is_never_below_the_exact_one() {
    let _env = crate::common::process_env_lock().await;
    let files: Vec<(String, Vec<u8>)> = [
        ("Q4_K_M", 4.5, 0.6),
        ("Q8_0", 8.5, 1.1),
        ("BF16", 16.0, 2.0),
    ]
    .into_iter()
    .map(|(q, bits, embd_mib)| {
        let mut h = synth::chat("qwen3", 4096);
        h.u32("qwen3.block_count", 2)
            .u32("qwen3.embedding_length", 1024)
            .u32("qwen3.attention.head_count", 8)
            .u32("qwen3.attention.head_count_kv", 8)
            .u32("qwen3.attention.key_length", 128)
            .u32("qwen3.attention.value_length", 128);
        h.tensor_shaped(
            "token_embd.weight",
            &[1024, 1024],
            (embd_mib * MIB as f64) as u64,
        );
        h.tensor_shaped("output.weight", &[1024, 1024], MIB);
        for i in 0..2 {
            let bytes = (bits * (1024.0 * 1024.0) / 8.0) as u64;
            h.tensor_shaped(&format!("blk.{i}.ffn_up.weight"), &[1024, 1024], bytes);
        }
        (format!("quants/demo-chat-{q}.gguf"), h.bytes())
    })
    .collect();
    let (_hub, _cdn_server, _cdn) = hub(&files, &[]).await;
    let state = on_card(4096 * MIB, 0, false).await;
    let fits = fits_of(&state, &files).await;
    for reference in &fits {
        let r = reference.files.as_ref().unwrap();
        for exact in &fits {
            let e = exact.files.as_ref().unwrap();
            assert!(
                e.summary.host_input_bytes > 0,
                "untied: {:?}",
                e.summary.host_input_bytes
            );
            let estimate = r.estimated(&e.rel, exact.bytes.unwrap());
            let params = json!({"ctx_size": 4096, "n_gpu_layers": 999, "flash_attn": "on",
                                "cache_type_k": "q8_0", "cache_type_v": "q8_0"});
            let params = params.as_object().unwrap();
            let est = modelinfo::fit::Sizer::chat(&estimate, params).at(4096);
            let real = modelinfo::fit::Sizer::chat(e, params).at(4096);
            assert!(
                est.total_bytes >= real.total_bytes,
                "{} from {}: {} < {}",
                exact.file,
                reference.file,
                est.total_bytes,
                real.total_bytes
            );
            assert_eq!(est.kv_cache_bytes, real.kv_cache_bytes);
        }
    }
}

/// The hub plan of an embedder asked for as chat is the disk plan of the
/// same bytes where `hf_add target=chat` puts them (D51): the aux plan, with
/// the location warning naming the repo and file to fetch again with
/// `target=aux`, and not ready to apply.
#[tokio::test]
async fn a_hub_plan_for_chat_of_an_embedder_is_the_disk_plan_in_the_chat_dir() {
    let _env = crate::common::process_env_lock().await;
    let (state, dir) = plan_pin::fixture(4096).await;
    let bytes = plan_pin::embedder().bytes();
    let rel = format!("{REPO}/{}", plan_pin::EMBED);
    let at = dir.path().join("chat").join(&rel);
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    std::fs::write(&at, &bytes).unwrap();
    store::upsert_hf_model(&state.db, REPO, plan_pin::EMBED, &rel, "chat")
        .await
        .unwrap();
    let (_hub, _cdn_server, _cdn) = hub(&[(plan_pin::EMBED.to_string(), bytes)], &[]).await;

    let disk = modelinfo::local_model_plan(&state, &rel, Some("chat"))
        .await
        .unwrap();
    let from_hub = modelinfo::local_model_plan_of(
        &state,
        None,
        Some(REPO),
        Some(plan_pin::EMBED),
        Some("chat"),
    )
    .await
    .unwrap();
    assert_eq!(but_source(&from_hub), but_source(&disk));
    assert_eq!(from_hub["class"], "aux", "{from_hub}");
    assert_eq!(from_hub["target"], "chat", "{from_hub}");
    assert_eq!(from_hub["apply_ready"], false, "{from_hub}");
    let warnings = from_hub["warnings"].to_string();
    assert!(
        warnings.contains("found in the chat models dir")
            && warnings.contains(&format!(
                "lmgw__hf_add repo={REPO} file={} target=aux",
                plan_pin::EMBED
            )),
        "{warnings}"
    );
    // Without a target it is the header's class, the aux dir, ready.
    let own = modelinfo::local_model_plan_of(&state, None, Some(REPO), Some(plan_pin::EMBED), None)
        .await
        .unwrap();
    assert_eq!(
        (own["target"].clone(), own["apply_ready"].clone()),
        (json!("aux"), json!(true))
    );
}

/// A split file whose first part declares another count than its name, or
/// whose listing lacks a part, doesn't load as it is: its later parts aren't
/// read (one header read, the first part's), and its fit is by its size
/// alone with why, unknown when a part's bytes are.
#[tokio::test]
async fn a_split_set_that_cant_load_reads_no_later_part() {
    let _env = crate::common::process_env_lock().await;
    let (state, _dir) = plan_pin::fixture(4096).await;
    let mut three = plan_pin::split_part1();
    three.u64("split.count", 3);
    let served = vec![
        (plan_pin::SPLIT1.to_string(), three.bytes()),
        (
            plan_pin::SPLIT2.to_string(),
            plan_pin::split_part2().bytes(),
        ),
    ];
    let listed: u64 = served.iter().map(|(_, b)| b.len() as u64).sum();
    let (_hub, _cdn_server, cdn) = hub(&served, &[]).await;
    let plan = modelinfo::local_model_plan_hub(&state, REPO, plan_pin::SPLIT1, None)
        .await
        .unwrap();
    assert_eq!(cdn.asked(), vec![plan_pin::SPLIT1.to_string()]);
    let fit = &plan["fit"]["whole_card"];
    assert_eq!(fit["mark"], "fits_by_size", "{fit}");
    assert_eq!(fit["uses_bytes"], listed, "{fit}");
    assert!(
        fit["notes"]
            .to_string()
            .contains("declares a GGUF split into 3"),
        "{fit}"
    );
    assert!(
        plan["warnings"]
            .to_string()
            .contains("declares a GGUF split into 3"),
        "{plan}"
    );
    drop((_hub, _cdn_server));

    // The second part not listed: nothing more read, the bytes unknown.
    let served = vec![(
        plan_pin::SPLIT1.to_string(),
        plan_pin::split_part1().bytes(),
    )];
    let (_hub, _cdn_server, cdn) = hub(&served, &[]).await;
    let plan = modelinfo::local_model_plan_hub(&state, REPO, plan_pin::SPLIT1, None)
        .await
        .unwrap();
    assert_eq!(cdn.asked(), vec![plan_pin::SPLIT1.to_string()]);
    assert_eq!(plan["fit"]["whole_card"]["mark"], "unknown", "{plan}");
    let listing = lmgw_core::hf::list_repo_files(&state.http, "", REPO)
        .await
        .unwrap();
    let f = hub_fit::file_fit(&state, REPO, plan_pin::SPLIT1, &listing, None).await;
    assert_eq!(f.bytes, None);
    assert_eq!(f.fit.whole_card.mark, FitMark::Unknown, "{f:?}");
}

/// A plan counts only the header bytes it fetched (D123): the same plan
/// again reads nothing, every header being cached by its hash.
#[tokio::test]
async fn a_cached_header_counts_no_bytes() {
    let _env = crate::common::process_env_lock().await;
    let (state, _dir) = plan_pin::fixture(64).await;
    let served: Vec<(String, Vec<u8>)> = [
        (plan_pin::SPLIT1, plan_pin::split_part1()),
        (plan_pin::SPLIT2, plan_pin::split_part2()),
    ]
    .into_iter()
    .map(|(p, h)| (p.to_string(), h.bytes()))
    .collect();
    let (_hub, _cdn_server, cdn) = hub(&served, &[]).await;
    let first = modelinfo::local_model_plan_hub(&state, REPO, plan_pin::SPLIT1, None)
        .await
        .unwrap();
    let again = modelinfo::local_model_plan_hub(&state, REPO, plan_pin::SPLIT1, None)
        .await
        .unwrap();
    assert!(
        first["source"]["header_bytes_read"].as_u64().unwrap() > 0,
        "{first}"
    );
    assert_eq!(again["source"]["header_bytes_read"], 0, "{again}");
    assert_eq!(cdn.asked().len(), 2, "{:?}", cdn.asked());
    assert_eq!(first["fit"], again["fit"]);
}

/// A target the hub plan doesn't read headers for is refused before any
/// read, and the file's mark by size says that refusal, not a read failure.
#[tokio::test]
async fn an_image_target_is_marked_by_size_with_its_refusal() {
    let _env = crate::common::process_env_lock().await;
    let files = quants();
    let (_hub, _cdn_server, cdn) = hub(&files, &[]).await;
    let state = on_card(192 * MIB, 0, false).await;
    let listing = lmgw_core::hf::list_repo_files(&state.http, "", REPO)
        .await
        .unwrap();
    let f = hub_fit::file_fit(&state, REPO, &files[1].0, &listing, Some("image")).await;
    assert!(cdn.asked().is_empty(), "{:?}", cdn.asked());
    assert_eq!(f.fit.whole_card.mark, FitMark::FitsBySize, "{f:?}");
    assert!(
        f.fit
            .whole_card
            .notes
            .iter()
            .any(|n| n.contains("plans from its own catalog")),
        "{:?}",
        f.fit.whole_card.notes
    );
}
