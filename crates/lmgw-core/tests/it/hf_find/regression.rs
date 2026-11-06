//! The ranking regression set (find-a-model design §4.4, §10, WP4's entry
//! gate): the 28 queries over the recorded hub, on a 24 GiB card (a large
//! desktop card) and an 8 GiB one (a gamer's), with 1 GiB headroom (the
//! setting's default). The expectations are §4.4's table's cards: test
//! expectations over recordings (D38), not a list lmgw consults.
//!
//! The card here is the walk's by size (D123); §4.4's notes on files the
//! header decides (phi 4 on 8 GB, bge m3, embeddinggemma, the four at the
//! card's edge) are the header fit's, which `hf_repo` with `fit` reads.
//!
//! `LATER` holds queries outside §4.4's set that a live finding changed a
//! rule for, recorded and checked the same way.

use lmgw_api_types::model_find::FindAnswer;
use lmgw_core::find::{self, card::DIGITS_HINT, Room};

use super::fixtures::{self, Recorded, Recording};

const GIB: u64 = 1 << 30;

pub fn cards() -> [(&'static str, Room); 2] {
    [
        (
            "24 GB",
            Room {
                capacity: 24 * GIB,
                headroom: GIB,
            },
        ),
        (
            "8 GB",
            Room {
                capacity: 8 * GIB,
                headroom: GIB,
            },
        ),
    ]
}

/// What §4.4's table says a card should be.
#[derive(Debug, Clone, Copy)]
pub enum Want {
    /// A card of this original (any case), from this upload when given.
    Card(&'static str, Option<&'static str>),
    /// `NothingFits`, the top original's card naming this one when given.
    NothingFits(Option<&'static str>),
    NoRunnableFile,
    /// §4.4's known miss: the card the rules give, which isn't the right
    /// one. Kept so a rule that fixes it shows up here.
    KnownMiss(&'static str),
}

use Want::*;

/// §4.4's 28 queries: the 24 GB card, then the 8 GB one.
pub const QUERIES: [(&str, Want, Want); 28] = [
    (
        "qwen 3.6",
        Card("Qwen/Qwen3.6-35B-A3B", Some("unsloth/Qwen3.6-35B-A3B-GGUF")),
        NothingFits(None),
    ),
    (
        "gemma 4",
        Card("google/gemma-4-31B-it", Some("unsloth/gemma-4-31B-it-GGUF")),
        Card("google/gemma-4-E4B-it", None),
    ),
    ("gemma four", NoRunnableFile, NoRunnableFile),
    (
        "llama 3 8b",
        Card("meta-llama/Meta-Llama-3-8B-Instruct", None),
        Card("meta-llama/Meta-Llama-3-8B-Instruct", None),
    ),
    (
        "deepseek v4 flash",
        // The card is the original closest to fitting (D17, refined in WP4's review
        // round): the base V4-Flash is smaller than the 0731 one the walk meets first.
        NothingFits(Some("deepseek-ai/DeepSeek-V4-Flash")),
        NothingFits(Some("deepseek-ai/DeepSeek-V4-Flash")),
    ),
    (
        "mistral small",
        Card(
            "mistralai/Mistral-Small-3.1-24B-Instruct-2503",
            Some("bartowski/mistralai_Mistral-Small-3.1-24B-Instruct-2503-GGUF"),
        ),
        NothingFits(None),
    ),
    (
        "qwen3 embedding",
        Card(
            "Qwen/Qwen3-Embedding-0.6B",
            Some("Qwen/Qwen3-Embedding-0.6B-GGUF"),
        ),
        Card(
            "Qwen/Qwen3-Embedding-0.6B",
            Some("Qwen/Qwen3-Embedding-0.6B-GGUF"),
        ),
    ),
    (
        "hermes",
        Card("NousResearch/Hermes-3-Llama-3.1-8B", None),
        Card("NousResearch/Hermes-3-Llama-3.1-8B", None),
    ),
    (
        "llama",
        Card("meta-llama/Llama-3.1-8B-Instruct", None),
        Card("meta-llama/Llama-3.1-8B-Instruct", None),
    ),
    (
        "mistral",
        Card("mistralai/Mistral-7B-Instruct-v0.3", None),
        Card("mistralai/Mistral-7B-Instruct-v0.3", None),
    ),
    (
        "phi 4",
        Card("microsoft/phi-4", Some("microsoft/phi-4-gguf")),
        Card("microsoft/phi-4", None),
    ),
    (
        "gpt oss",
        Card("openai/gpt-oss-20b", None),
        NothingFits(None),
    ),
    (
        "deepseek",
        Card("deepseek-ai/DeepSeek-R1-Distill-Qwen-32B", None),
        Card("deepseek-ai/DeepSeek-R1-Distill-Qwen-1.5B", None),
    ),
    (
        "qwen",
        Card("Qwen/Qwen3.8-27B", None),
        Card("Qwen/Qwen2.5-7B-Instruct", None),
    ),
    (
        "gemma",
        Card("google/gemma-4-31B-it", None),
        Card("google/gemma-4-E4B-it", None),
    ),
    (
        "nomic embed",
        Card(
            "nomic-ai/nomic-embed-text-v1.5",
            Some("nomic-ai/nomic-embed-text-v1.5-GGUF"),
        ),
        Card(
            "nomic-ai/nomic-embed-text-v1.5",
            Some("nomic-ai/nomic-embed-text-v1.5-GGUF"),
        ),
    ),
    (
        "bge m3",
        Card("BAAI/bge-m3", None),
        Card("BAAI/bge-m3", None),
    ),
    (
        "embeddinggemma",
        Card(
            "google/embeddinggemma-2",
            Some("unsloth/embeddinggemma-2-GGUF"),
        ),
        Card("google/embeddinggemma-2", None),
    ),
    (
        "llama 3.2 3b",
        Card("meta-llama/Llama-3.2-3B-Instruct", None),
        Card("meta-llama/Llama-3.2-3B-Instruct", None),
    ),
    (
        "granite",
        KnownMiss("ibm-granite/granite-docling-258M"),
        KnownMiss("ibm-granite/granite-docling-258M"),
    ),
    (
        "smollm",
        Card("HuggingFaceTB/SmolLM3-3B", Some("ggml-org/SmolLM3-3B-GGUF")),
        Card("HuggingFaceTB/SmolLM3-3B", None),
    ),
    (
        "olmo",
        Card("allenai/Olmo-3-32B-Think", None),
        Card("allenai/Olmo-3-7B-Instruct", None),
    ),
    (
        "glm",
        Card("zai-org/GLM-4.7-Flash", None),
        Card("zai-org/GLM-4.1V-9B-Thinking", None),
    ),
    // On 8 GB §4.4 had NVIDIA-Nemotron-Nano-9B-v2 (532 likes): its spike read the first 100
    // repos by downloads, which didn't hold Nemotron-Orchestrator-8B (601 likes, 2.8k
    // downloads). The hub's whole page (D70) does, and D15's likes rank it first.
    (
        "nemotron",
        Card("nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B-BF16", None),
        Card("nvidia/Nemotron-Orchestrator-8B", None),
    ),
    (
        "gemma 3",
        Card("google/gemma-3-27b-it", Some("unsloth/gemma-3-27b-it-GGUF")),
        Card("google/gemma-3-4b-it", None),
    ),
    (
        "qwen 2.5 coder",
        Card(
            "Qwen/Qwen2.5-Coder-32B-Instruct",
            Some("Qwen/Qwen2.5-Coder-32B-Instruct-GGUF"),
        ),
        Card("Qwen/Qwen2.5-Coder-7B-Instruct", None),
    ),
    (
        "ministral",
        Card("mistralai/Ministral-3-14B-Reasoning-2512", None),
        Card("mistralai/Ministral-3-3B-Reasoning-2512", None),
    ),
    (
        "kimi",
        Card("moonshotai/Kimi-VL-A3B-Thinking-2506", None),
        NothingFits(None),
    ),
];

/// Queries outside §4.4's set, each a live finding a rule changed for: the
/// 24 GB card, then the 8 GB one.
pub const LATER: [(&str, Want, Want); 1] = [
    // WP8's live round (D114): Qwen3-TTS-12Hz-1.7B-CustomVoice (text-to-speech,
    // more likes) matches every word as Qwen3-1.7B does; the owner's ruling of
    // 2026-11-04 puts the kind Find a model sets up first (D15).
    (
        "qwen3 1.7b",
        Card("Qwen/Qwen3-1.7B", Some("Qwen/Qwen3-1.7B-GGUF")),
        Card("Qwen/Qwen3-1.7B", Some("Qwen/Qwen3-1.7B-GGUF")),
    ),
];

/// What one query gave on one card, in a line.
fn line(found: &find::Found) -> String {
    let origin = found
        .origin
        .as_ref()
        .map(|o| o.id().to_string())
        .unwrap_or_default();
    let upload = found
        .chosen
        .as_ref()
        .map(|c| c.upload.id.clone())
        .unwrap_or_default();
    format!(
        "{:?} {origin} ({upload}), {} calls",
        found.answer, found.calls
    )
}

fn check(q: &str, card: &str, want: Want, found: &find::Found) -> Result<(), String> {
    let origin = found.origin.as_ref().map(|o| o.id().to_ascii_lowercase());
    let upload = found
        .chosen
        .as_ref()
        .map(|c| c.upload.id.to_ascii_lowercase());
    let same = |a: Option<&String>, b: &str| a.is_some_and(|a| a == &b.to_ascii_lowercase());
    let ok = match want {
        Card(o, up) => {
            found.answer == FindAnswer::Card
                && same(origin.as_ref(), o)
                && up.is_none_or(|u| same(upload.as_ref(), u))
        }
        NothingFits(o) => {
            found.answer == FindAnswer::NothingFits && o.is_none_or(|o| same(origin.as_ref(), o))
        }
        NoRunnableFile => found.answer == FindAnswer::NoRunnableFile,
        KnownMiss(o) => found.answer == FindAnswer::Card && same(origin.as_ref(), o),
    };
    match ok {
        true => Ok(()),
        false => Err(format!(
            "{q} on {card}: wanted {want:?}, got {}",
            line(found)
        )),
    }
}

/// Every query on both cards over the recording: the card §4.4 names.
#[tokio::test]
async fn the_28_queries_pick_section_4_4s_cards() {
    let hub = Recorded::load();
    let mut wrong = Vec::new();
    for (q, big, small) in QUERIES {
        for ((card, room), want) in cards().into_iter().zip([big, small]) {
            let found = find::find(&hub, q, None, Some(room)).await.unwrap();
            hub.take_asked();
            // The answer's size on the wire with the lists' default windows
            // (D142), and what every entry would add.
            let answer = |lists| {
                let a = find::card::answer(&found, &super::op::no_card_inputs(), &lists);
                serde_json::to_vec(&a).unwrap().len()
            };
            let all = find::card::Lists {
                others_count: u32::MAX,
                did_you_mean_count: u32::MAX,
                other_uploads_count: u32::MAX,
                ..Default::default()
            };
            eprintln!(
                "{q:20} {card:5} {}, {} B ({} B with every list entry)",
                line(&found),
                answer(Default::default()),
                answer(all)
            );
            if let Err(e) = check(q, card, want, &found) {
                wrong.push(e);
            }
            // The walk stays with the top original's author and kind (D17,
            // D126).
            if let (Some(o), Some(top)) = (&found.origin, found.ranked.originals.first()) {
                assert_eq!(o.model.author(), top.model.author(), "{q} on {card}");
                assert_eq!(o.kind, top.kind, "{q} on {card}");
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of 56 wrong:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// The later queries on both cards over the recording. "qwen3 1.7b" (D15,
/// ruled 2026-11-04): the most-liked repo matching every word is a speech
/// model, and the card is the chat model, with every kind Find a model sets
/// up ranked before the other kinds and the speech model under "Other
/// models with this name".
#[tokio::test]
async fn a_kind_find_a_model_sets_up_leads_another_kind() {
    let hub = Recorded::load();
    let mut wrong = Vec::new();
    for (q, big, small) in LATER {
        for ((card, room), want) in cards().into_iter().zip([big, small]) {
            let found = find::find(&hub, q, None, Some(room)).await.unwrap();
            eprintln!("{q:20} {card:5} {}", line(&found));
            if let Err(e) = check(q, card, want, &found) {
                wrong.push(e);
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    let found = find::find(&hub, "qwen3 1.7b", None, Some(cards()[0].1))
        .await
        .unwrap();
    let originals = &found.ranked.originals;
    let first_other = originals
        .iter()
        .position(|o| !o.kind.sets_up())
        .expect("another kind among the originals");
    assert!(
        originals[first_other..].iter().all(|o| !o.kind.sets_up()),
        "{:?}",
        originals.iter().map(|o| o.id()).collect::<Vec<_>>()
    );
    let lists = find::card::Lists {
        others_count: u32::MAX,
        ..Default::default()
    };
    let answer = find::card::answer(&found, &super::op::no_card_inputs(), &lists);
    let speech = "qwen/qwen3-tts-12hz-1.7b-customvoice";
    assert!(
        answer
            .others
            .iter()
            .any(|c| c.origin.to_ascii_lowercase() == speech),
        "{:?}",
        answer.others.iter().map(|c| &c.origin).collect::<Vec<_>>()
    );
}

/// A hub whose every listing holds one text-to-speech repo (no network).
struct OnlySpeech;

impl lmgw_core::hf::search::Hub for OnlySpeech {
    async fn get(&self, path: &str) -> Result<lmgw_core::hf::search::HubAnswer, String> {
        assert!(path.starts_with("/api/models?"), "unexpected read {path}");
        let repo = lmgw_core::hf::search::HubModel {
            id: "maker-demo/Demo-Speech-1.7B".into(),
            likes: 5,
            gated: serde_json::Value::Bool(false),
            pipeline_tag: Some("text-to-speech".into()),
            ..Default::default()
        };
        Ok(lmgw_core::hf::search::HubAnswer {
            status: 200,
            body: serde_json::to_string(&[repo]).unwrap(),
            ..Default::default()
        })
    }
}

/// D149: when the only originals matching every word are of a kind Find a
/// model doesn't set up, the answer is `OtherKind` and the top original is
/// that repo, with no walk of its uploads.
#[tokio::test]
async fn only_another_kind_answers_other_kind() {
    let found = find::find(&OnlySpeech, "demo 1.7b", None, Some(cards()[0].1))
        .await
        .unwrap();
    assert_eq!(found.answer, FindAnswer::OtherKind);
    let origin = found.origin.as_ref().expect("the top original");
    assert_eq!(origin.id(), "maker-demo/Demo-Speech-1.7B");
    assert!(!origin.kind.sets_up());
    assert!(found.chosen.is_none());
}

/// D18: "gemma four" is searched as typed; the exact match has no GGUF and
/// the answer asks for digits, without a second search.
#[tokio::test]
async fn number_words_are_searched_as_typed() {
    let hub = Recorded::load();
    let found = find::find(&hub, "gemma four", None, Some(cards()[0].1))
        .await
        .unwrap();
    assert_eq!(found.answer, FindAnswer::NoRunnableFile);
    let asked = hub.take_asked();
    assert!(
        asked
            .iter()
            .all(|p| !p.contains("search=") || p.contains("search=gemma+four")),
        "{asked:?}"
    );
    let inputs = super::op::no_card_inputs();
    let answer = find::card::answer(&found, &inputs, &Default::default());
    assert!(
        answer.sentence.ends_with(DIGITS_HINT),
        "{}",
        answer.sentence
    );
}

/// D17's pass-over: "deepseek v4 flash" on 24 GB asks for no original's
/// uploads, only the two searches and the "by …" call, and its card says
/// the size it would need.
#[tokio::test]
async fn an_original_too_big_at_4_bits_costs_no_call() {
    let hub = Recorded::load();
    let found = find::find(&hub, "deepseek v4 flash", None, Some(cards()[0].1))
        .await
        .unwrap();
    let asked = hub.take_asked();
    assert_eq!(found.answer, FindAnswer::NothingFits);
    assert!(found.chosen.is_none());
    assert!(!asked.iter().any(|p| p.contains("base_model")), "{asked:?}");
    assert!(!asked.iter().any(|p| p.contains("/tree/")), "{asked:?}");
    assert_eq!(found.calls, 3, "{asked:?}");
    let answer = find::card::answer(&found, &super::op::no_card_inputs(), &Default::default());
    let card = answer.card.unwrap();
    assert!(card.upload.is_empty() && card.file.is_empty(), "{card:?}");
    assert!(card.fit.uses_bytes > 24 * GIB, "{card:?}");
}

/// The maker's own GGUF is the file of its original (D14), and comes first
/// among its uploads (D16).
#[tokio::test]
async fn the_makers_own_gguf_is_the_upload() {
    let hub = Recorded::load();
    let found = find::find(&hub, "qwen3 embedding", None, Some(cards()[1].1))
        .await
        .unwrap();
    let origin = found.origin.as_ref().unwrap();
    assert!(origin.makers_gguf.is_some(), "{origin:?}");
    assert_eq!(
        found
            .chosen
            .as_ref()
            .unwrap()
            .upload
            .id
            .to_ascii_lowercase(),
        "qwen/qwen3-embedding-0.6b-gguf"
    );
    assert_eq!(origin.kind, find::Kind::Embed);
}

/// Records the queries' reads from the live hub, without a token: run by
/// `scripts/hf-find-record.sh`, which sets `LMGW_HF_FIND_RECORD`. Ignored,
/// and a no-op without the variable, so no test run reaches the network.
/// With `LMGW_HF_FIND_ADD` set (the script's `--add`) it starts from the
/// last recording and reads only what that lacks, so the cards recorded
/// before stay as they were.
#[tokio::test]
#[ignore = "reads the live hub; scripts/hf-find-record.sh runs it"]
async fn record_the_queries() {
    let Ok(mut recorded) = std::env::var("LMGW_HF_FIND_RECORD") else {
        eprintln!("LMGW_HF_FIND_RECORD is unset: nothing recorded");
        return;
    };
    let mut kept = Default::default();
    if std::env::var_os("LMGW_HF_FIND_ADD").is_some() {
        let before = fixtures::recorded();
        if before != recorded {
            recorded = format!("{before}, reads added {recorded}");
        }
        kept = Recorded::load().into_answers();
    }
    let hub = Recording {
        inner: lmgw_core::hf::search::LiveHub {
            http: reqwest::Client::new(),
            base: lmgw_core::hf::HF_BASE.to_string(),
            // Never a token: the recording is what anyone is answered.
            token: String::new(),
        },
        kept: std::sync::Mutex::new(kept),
    };
    let before = hub.kept.lock().unwrap().len();
    for (q, _, _) in QUERIES.into_iter().chain(LATER) {
        for (card, room) in cards() {
            let found = find::find(&hub, q, None, Some(room)).await.unwrap();
            eprintln!("{q:20} {card:5} {}", line(&found));
            assert!(
                !matches!(
                    found.answer,
                    FindAnswer::RateLimited | FindAnswer::Unreachable
                ),
                "{q}: {:?} {:?} — nothing saved; run the script again later",
                found.answer,
                found.notes
            );
        }
    }
    let kept = hub.kept.lock().unwrap();
    eprintln!("{} answers, {} read now", kept.len(), kept.len() - before);
    fixtures::save(&kept, &recorded);
}
