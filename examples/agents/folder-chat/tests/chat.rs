//! The prompt builder offline: a synced fixture index, quickdoc's retriever,
//! and the budget edges that decide what the model is shown.

mod common;

use std::collections::HashSet;
use std::sync::Arc;

use common::*;
use folder_chat::budget::{BudgetError, DEFAULT_ANSWER_RESERVE_TOKENS, MESSAGE_FRAMING_TOKENS};
use folder_chat::chat::{
    self, default_search_params, ChatMessage, ChatRequest, Role, SYSTEM_PROMPT,
};
use folder_chat::chunk::estimator;
use folder_chat::gateway::ModelInfo;
use folder_chat::index::IndexDir;
use quickdoc_core::embed::FixtureEmbedder;
use quickdoc_core::retrieve::Retriever;

async fn indexed() -> (tempfile::TempDir, Retriever) {
    let tmp = tempfile::tempdir().unwrap();
    let para = "The zeta relabel flag belongs on every bind mount. ".repeat(12);
    write(
        tmp.path(),
        "notes.md",
        format!("# Setup\n\n## SELinux\n\n{para}\n\n## Podman\n\nRootless zeta containers.\n"),
    );
    write(tmp.path(), "todo.txt", "zeta errands\n");
    let idx = IndexDir::open(tmp.path()).await.unwrap();
    sync(&idx, Counting::new()).await.report();
    let r = Retriever::load_by_id(
        idx.pool().clone(),
        "folder@1",
        Arc::new(FixtureEmbedder::new(DIMS)),
    )
    .await
    .unwrap()
    .with_token_counter(estimator());
    (tmp, r)
}

fn model(ctx: u64, out: u64) -> ModelInfo {
    ModelInfo {
        alias: "m".into(),
        context_length: Some(ctx),
        max_output_tokens: Some(out),
    }
}

/// The chunk size the fixture index is cut with ([`sync`]'s 400), which is
/// one excerpt to the budget.
const CHUNK_TOKENS: usize = 400;

fn ask(q: &str) -> ChatRequest {
    ChatRequest {
        history: vec![],
        question: q.into(),
    }
}

#[tokio::test]
async fn the_rendered_excerpts_fit_the_retrieval_budget() {
    let (_tmp, r) = indexed().await;
    let tc = estimator();
    let p = chat::prepare(
        &r,
        &model(8192, 1024),
        &ask("zeta relabel flag"),
        tc.as_ref(),
        default_search_params(false),
        CHUNK_TOKENS,
    )
    .await
    .unwrap();
    let m = &p.meta;
    assert!(!m.citations.is_empty());
    assert_eq!(m.max_excerpts, chat::MAX_EXCERPTS);
    assert_eq!(
        m.budget.system_tokens,
        tc.count(SYSTEM_PROMPT) as u64 + MESSAGE_FRAMING_TOKENS
    );
    let last = &p.messages.last().unwrap().content;
    let excerpts = last
        .strip_prefix("Excerpts:\n\n")
        .unwrap()
        .split("\n\nQuestion: ")
        .next()
        .unwrap();
    assert!(tc.count(excerpts) as u64 <= m.budget.retrieval_tokens);
    for (i, c) in m.citations.iter().enumerate() {
        assert_eq!(c.n, i + 1);
        assert!(excerpts.contains(&format!("[{}] {}", c.n, c.path)));
    }
    assert_eq!(p.messages[0].content, SYSTEM_PROMPT);
}

#[tokio::test]
async fn a_budget_too_small_for_any_excerpt_says_so() {
    let (_tmp, r) = indexed().await;
    let tc = estimator();
    let system = tc.count(SYSTEM_PROMPT) as u64 + MESSAGE_FRAMING_TOKENS;
    let history = tc.count(&chat::user_message("", "zeta")) as u64 + MESSAGE_FRAMING_TOKENS;
    // Exactly five tokens left for excerpts: not even a header line fits.
    // (No max_output_tokens: a reported one this tight would leave no room
    // for an excerpt, and the default reserve would stand in.)
    let ctx = DEFAULT_ANSWER_RESERVE_TOKENS + system + history + 5;
    let p = chat::prepare(
        &r,
        &ModelInfo {
            max_output_tokens: None,
            ..model(ctx, 0)
        },
        &ask("zeta"),
        tc.as_ref(),
        default_search_params(false),
        CHUNK_TOKENS,
    )
    .await
    .unwrap();
    assert_eq!(p.meta.budget.retrieval_tokens, 5);
    assert!(p.meta.citations.is_empty());
    assert!(p.meta.excerpts_found > 0);
    assert_eq!(p.meta.excerpts_dropped_for_budget, p.meta.excerpts_found);
    assert!(
        p.meta.notes.iter().any(|n| n.contains("no room")),
        "{:?}",
        p.meta.notes
    );
    assert_eq!(
        p.messages.last().unwrap().content,
        "Excerpts: none.\n\nQuestion: zeta"
    );
}

#[tokio::test]
async fn history_is_never_truncated_to_make_room() {
    let (_tmp, r) = indexed().await;
    let tc = estimator();
    let req = ChatRequest {
        history: vec![
            ChatMessage {
                role: Role::User,
                content: "a".repeat(4000),
            },
            ChatMessage {
                role: Role::Assistant,
                content: "b".repeat(4000),
            },
        ],
        question: "zeta?".into(),
    };
    let e = chat::prepare(
        &r,
        &model(2048, 512),
        &req,
        tc.as_ref(),
        default_search_params(false),
        CHUNK_TOKENS,
    )
    .await
    .unwrap_err();
    match e {
        chat::AskError::Budget(BudgetError::ConversationTooLong { history, .. }) => {
            assert!(history >= 2000, "{history}")
        }
        other => panic!("{other}"),
    }
}

#[tokio::test]
async fn a_max_output_tokens_of_the_whole_context_keeps_the_default_reserve_and_says_so() {
    let (_tmp, r) = indexed().await;
    let tc = estimator();
    // What a llama.cpp model often reports: it may generate its whole
    // context. Kept free in full, no question could ever be asked.
    let p = chat::prepare(
        &r,
        &model(8192, 8192),
        &ask("zeta relabel flag"),
        tc.as_ref(),
        default_search_params(false),
        CHUNK_TOKENS,
    )
    .await
    .unwrap();
    let b = &p.meta.budget;
    assert_eq!(b.answer_reserve_tokens, DEFAULT_ANSWER_RESERVE_TOKENS);
    assert!(!b.answer_reserve_reported);
    assert_eq!(b.max_output_tokens, Some(8192));
    assert!(!p.meta.citations.is_empty(), "the question was answerable");
    assert!(
        p.meta.notes.iter().any(|n| n.contains(
            "reports max_output_tokens 8192, which is its whole context (8192 tokens); 2048 \
             tokens are kept free for the answer instead (DEFAULT_ANSWER_RESERVE_TOKENS)"
        )),
        "{:?}",
        p.meta.notes
    );

    // A reported reserve that leaves room is the reserve, and no note says
    // otherwise.
    let p = chat::prepare(
        &r,
        &model(8192, 1024),
        &ask("zeta relabel flag"),
        tc.as_ref(),
        default_search_params(false),
        CHUNK_TOKENS,
    )
    .await
    .unwrap();
    assert_eq!(p.meta.budget.answer_reserve_tokens, 1024);
    assert!(p.meta.budget.answer_reserve_reported);
    assert!(
        !p.meta.notes.iter().any(|n| n.contains("max_output_tokens")),
        "{:?}",
        p.meta.notes
    );
    // Every answer names the retrieval depths it ran with.
    assert!(
        p.meta.notes.iter().any(|n| n.contains("k_fts 50")
            && n.contains("k_vec 50")
            && n.contains("rrf_k 60")
            && n.contains("k_rerank 20")),
        "{:?}",
        p.meta.notes
    );

    // Too tight beside the system prompt and one excerpt, though under the
    // context: the default stands in, with the other wording.
    let system = p.meta.budget.system_tokens;
    let p = chat::prepare(
        &r,
        &model(8192, 8192 - system - CHUNK_TOKENS as u64 + 1),
        &ask("zeta relabel flag"),
        tc.as_ref(),
        default_search_params(false),
        CHUNK_TOKENS,
    )
    .await
    .unwrap();
    assert_eq!(
        p.meta.budget.answer_reserve_tokens,
        DEFAULT_ANSWER_RESERVE_TOKENS
    );
    assert!(
        p.meta
            .notes
            .iter()
            .any(|n| n.contains("leaves no room") && n.contains("one excerpt (chunk_tokens, 400)")),
        "{:?}",
        p.meta.notes
    );
}

#[test]
fn reading_order_groups_by_file_in_the_order_of_each_files_best_excerpt() {
    // Best first: b's second half, a's end, b's start, a's start, c.
    let picked = [("b", 50), ("a", 10), ("b", 5), ("a", 3), ("c", 0)];
    assert_eq!(chat::reading_order(&picked), vec![2, 0, 3, 1, 4]);
    assert!(chat::reading_order(&[]).is_empty());
}

#[tokio::test]
async fn excerpts_are_numbered_in_reading_order() {
    let (_tmp, r) = indexed().await;
    let tc = estimator();
    let p = chat::prepare(
        &r,
        &model(8192, 1024),
        &ask("zeta"),
        tc.as_ref(),
        default_search_params(false),
        CHUNK_TOKENS,
    )
    .await
    .unwrap();
    let c = &p.meta.citations;
    assert!(
        c.iter().filter(|c| c.path == "notes.md").count() >= 2,
        "{c:?}"
    );
    // One file's excerpts are adjacent and in their order in it.
    let paths: Vec<&str> = c.iter().map(|c| c.path.as_str()).collect();
    let mut seen: Vec<&str> = paths.clone();
    seen.dedup();
    let unique: HashSet<&str> = paths.iter().copied().collect();
    assert_eq!(seen.len(), unique.len(), "a file is not split: {paths:?}");
    for w in c.windows(2) {
        if w[0].path == w[1].path {
            assert!(w[0].span_start < w[1].span_start, "{c:?}");
        }
    }
    // The prompt shows them in that order, under those numbers.
    let last = &p.messages.last().unwrap().content;
    let mut at = 0;
    for c in c {
        let head = format!("[{}] {}", c.n, c.path);
        let found = last[at..]
            .find(&head)
            .unwrap_or_else(|| panic!("{head} after {at}"));
        at += found + head.len();
    }
}
