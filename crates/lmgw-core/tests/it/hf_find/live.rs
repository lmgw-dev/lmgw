//! `model_find` against the live hub, through a served gateway as the
//! dashboard and the tool call it (the op test path), on a declared 24 GiB
//! and 8 GiB card with 1 GiB headroom: a live check of a rule against what
//! Hugging Face answers today, beside the recorded regression set.
//!
//! Ignored, and a no-op without `LMGW_HF_FIND_LIVE` (the queries, separated
//! by `;`), so no test run reaches the network. It sends no token, starts
//! no model and no container, and prints one line a query and card:
//!
//! `LMGW_HF_FIND_LIVE='qwen3 1.7b;gemma 4' cargo nextest run --workspace
//! --run-ignored only --no-capture hf_find::live`

use serde_json::json;

use super::op::{find, state_with};

#[tokio::test]
#[ignore = "reads the live hub; set LMGW_HF_FIND_LIVE to the queries"]
async fn model_find_on_the_live_hub() {
    let Ok(queries) = std::env::var("LMGW_HF_FIND_LIVE") else {
        eprintln!("LMGW_HF_FIND_LIVE is unset: nothing read");
        return;
    };
    let _env = crate::common::process_env_lock().await;
    // The real hub, whatever this shell has set; never a token.
    std::env::remove_var("HF_ENDPOINT");
    for gib in [24, 8] {
        let state = state_with("", gib).await;
        for q in queries.split(';').map(str::trim).filter(|q| !q.is_empty()) {
            let a = find(&state, json!({ "query": q })).await;
            let card = a
                .card
                .as_ref()
                .map(|c| format!("{} ({:?}, {} {})", c.origin, c.kind, c.upload, c.file))
                .unwrap_or_default();
            let others: Vec<String> = a
                .others
                .iter()
                .take(5)
                .map(|o| format!("{} [{}]", o.origin, o.pipeline.as_deref().unwrap_or("-")))
                .collect();
            eprintln!(
                "LIVE {q:20} {gib:2} GiB {:?} {card}; {} calls; others: {}",
                a.answer,
                a.hub_budget.as_ref().map(|b| b.calls).unwrap_or_default(),
                others.join(", ")
            );
            eprintln!("     {}", a.sentence);
        }
    }
}
