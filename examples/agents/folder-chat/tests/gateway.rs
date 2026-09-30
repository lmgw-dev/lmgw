//! The gateway client and the whole agent against the fake lmgw in
//! `common/fake.rs`: model info, embeddings (with a `gpu_hold` 503, a 500, a
//! hold-fallback answer or another model's vectors on demand), rerank (and a
//! rerank answered by a hold fallback), a streaming chat completion cut into
//! awkward pieces, and an index loaded while the embedding model is held.

mod common;

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use common::fake::*;
use folder_chat::budget::{BudgetError, FALLBACK_CONTEXT_TOKENS};
use folder_chat::chat::{self, AskError, ChatMessage, ChatRequest, Role, SYSTEM_PROMPT};
use folder_chat::gateway::{ChatChunk, GatewayError, Usage, GPU_HOLD_MESSAGE};
use folder_chat::sync::{AbortKind, SyncError, SyncEvent};
use folder_chat::FolderChat;
use futures::StreamExt;
use serde_json::json;

async fn collect(
    mut s: futures::stream::BoxStream<'static, Result<ChatChunk, GatewayError>>,
) -> Vec<ChatChunk> {
    let mut out = Vec::new();
    while let Some(c) = s.next().await {
        out.push(c.unwrap());
    }
    out
}

#[tokio::test]
async fn model_info_reads_what_lmgw_reports_and_nothing_more() {
    let (base, fake) = serve(Fake {
        models: models(
            json!({"id": "chat-small", "context_length": 32768, "max_output_tokens": 4096}),
        ),
        ..Fake::default()
    })
    .await;
    let g = gateway(&base);
    let chat = g.model_info("chat-small").await.unwrap();
    assert_eq!(chat.context_length, Some(32768));
    assert_eq!(chat.max_output_tokens, Some(4096));
    let embed = g.model_info("embed/fixture").await.unwrap();
    assert_eq!(embed.context_length, Some(512));
    assert_eq!(embed.max_output_tokens, None, "absent means unknown");
    match g.model_info("nope").await.unwrap_err() {
        GatewayError::Http {
            status: 404, code, ..
        } => assert_eq!(code.as_deref(), Some("not_found")),
        other => panic!("{other:?}"),
    }
    assert!(fake
        .bearers
        .lock()
        .unwrap()
        .iter()
        .all(|b| b == &format!("Bearer {TOKEN}")));
}

#[tokio::test]
async fn a_held_gpu_stops_the_sync_before_it_starts() {
    let (base, _) = serve(Fake {
        models: models(json!({"id": "chat-small"})),
        hold_embeddings_from: Some(0),
        ..Fake::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let app = FolderChat::open(config(tmp.path(), false), gateway(&base))
        .await
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    match app.sync(tx).await {
        Err(SyncError::Aborted { kind, reason, .. }) => {
            assert_eq!(kind, AbortKind::GpuHold);
            assert!(reason.starts_with(GPU_HOLD_MESSAGE), "{reason}");
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
    match rx.recv().await {
        Some(SyncEvent::Aborted {
            kind: AbortKind::GpuHold,
            ..
        }) => {}
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_hold_that_lands_mid_sync_through_the_real_http_embedder() {
    // Call 0 is the embedder's width probe (also the model's fingerprint);
    // call 1 is the first file's batch.
    let (base, _) = serve(Fake {
        models: models(json!({"id": "chat-small"})),
        hold_embeddings_from: Some(1),
        ..Fake::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let app = FolderChat::open(config(tmp.path(), false), gateway(&base))
        .await
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let err = app.sync(tx).await.unwrap_err();
    assert!(
        matches!(
            err,
            SyncError::Aborted {
                kind: AbortKind::GpuHold,
                ..
            }
        ),
        "{err}"
    );
    let mut last = None;
    while let Ok(e) = rx.try_recv() {
        last = Some(e);
    }
    assert!(matches!(
        last,
        Some(SyncEvent::Aborted {
            kind: AbortKind::GpuHold,
            ..
        })
    ));
    // Nothing was half-stored: the index answers nothing yet.
    let status = app.status().await;
    assert_eq!(status.chunks, Some(0));
}

#[tokio::test]
async fn a_chunk_size_over_the_embedding_context_is_refused_through_the_agent() {
    let (base, _) = serve(Fake {
        models: HashMap::from([(
            "embed/fixture".to_string(),
            json!({"id": "embed/fixture", "context_length": 128}),
        )]),
        ..Fake::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let app = FolderChat::open(config(tmp.path(), false), gateway(&base))
        .await
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    match app.sync(tx).await {
        Err(SyncError::Aborted {
            kind: AbortKind::ChunkTooLarge,
            reason,
            ..
        }) => {
            assert!(reason.contains("128"), "{reason}")
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn chat_streams_text_reasoning_finish_and_usage() {
    let (base, fake) = serve(Fake {
        fallback: Some("anthropic/claude-small".into()),
        ..Fake::default()
    })
    .await;
    let g = gateway(&base);
    let s = g
        .chat_stream(
            "chat-small",
            &[folder_chat::gateway::WireMessage {
                role: "user".into(),
                content: "hi".into(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(s.fallback.as_deref(), Some("anthropic/claude-small"));
    let chunks = collect(s.chunks).await;
    let text: String = chunks
        .iter()
        .filter_map(|c| match c {
            ChatChunk::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Use the zeta flag [1].");
    assert!(chunks.contains(&ChatChunk::Reasoning {
        text: "thinking".into()
    }));
    assert!(chunks.contains(&ChatChunk::Finish {
        reason: "stop".into()
    }));
    assert_eq!(
        chunks.last(),
        Some(&ChatChunk::Usage {
            usage: Usage {
                prompt_tokens: Some(120),
                completion_tokens: Some(7),
                total_tokens: Some(127)
            }
        })
    );
    let body = fake.chat_bodies.lock().unwrap()[0].clone();
    assert_eq!(body["stream"], json!(true));
    assert!(
        body.get("max_tokens").is_none(),
        "the answer is never capped"
    );
}

#[tokio::test]
async fn ask_end_to_end_with_citations_budget_and_rerank() {
    let (base, fake) = serve(Fake {
        models: models(json!({"id": "chat-small", "context_length": 4096})),
        ..Fake::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let app = FolderChat::open(config(tmp.path(), true), gateway(&base))
        .await
        .unwrap();
    assert!(matches!(
        app.ask(&ChatRequest {
            history: vec![],
            question: "zeta?".into()
        })
        .await,
        Err(AskError::NoIndex)
    ));

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let report = app.sync(tx).await.unwrap();
    assert_eq!(report.new_files, 2);
    assert_eq!(report.embed_context_length, Some(512));
    assert!(app.status().await.ready);

    let answer = app
        .ask(&ChatRequest {
            history: vec![
                ChatMessage {
                    role: Role::User,
                    content: "What is in this folder?".into(),
                },
                ChatMessage {
                    role: Role::Assistant,
                    content: "Setup notes and a todo list.".into(),
                },
            ],
            question: "Which relabel flag do bind mounts need?".into(),
        })
        .await
        .unwrap();
    let meta = &answer.meta;
    assert_eq!(meta.chat_model, "chat-small");
    assert_eq!(meta.budget.context_tokens, 4096);
    assert!(meta.budget.context_reported);
    assert!(!meta.budget.answer_reserve_reported);
    assert!(meta
        .notes
        .iter()
        .any(|n| n.contains("DEFAULT_ANSWER_RESERVE_TOKENS")));
    assert_eq!(
        meta.budget.retrieval_tokens,
        4096 - meta.budget.answer_reserve_tokens
            - meta.budget.system_tokens
            - meta.budget.history_tokens
    );
    assert_eq!(meta.rerank_model.as_deref(), Some("rerank/fixture"));
    assert!(fake.rerank_calls.load(Ordering::SeqCst) >= 1);
    // Numbered in reading order: the best excerpt's file first, and in it
    // the excerpt the rerank put on top after the one above it in the file.
    assert_eq!(meta.citations[0].path, "notes.md");
    let best = meta
        .citations
        .iter()
        .max_by(|a, b| a.score.total_cmp(&b.score))
        .unwrap();
    assert_eq!(best.path, "notes.md");
    assert_eq!(best.heading_path, "# Setup > ## SELinux");
    let file = std::fs::read_to_string(tmp.path().join("notes.md")).unwrap();
    assert_eq!(
        &file[best.span_start as usize..best.span_end as usize],
        best.text
    );

    let text: String = collect(answer.chunks)
        .await
        .into_iter()
        .filter_map(|c| match c {
            ChatChunk::Text { text } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Use the zeta flag [1].");

    // What was actually sent: system prompt, both earlier turns verbatim, and
    // the numbered excerpts before the question.
    let body = fake.chat_bodies.lock().unwrap()[0].clone();
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[0]["content"], json!(SYSTEM_PROMPT));
    assert_eq!(msgs[2]["content"], json!("Setup notes and a todo list."));
    let last = msgs[3]["content"].as_str().unwrap();
    assert!(
        last.starts_with(&format!(
            "Excerpts:\n\n[1] notes.md — {}\n",
            meta.citations[0].heading_path
        )),
        "{last}"
    );
    assert!(
        last.contains(&format!("[{}] notes.md — # Setup > ## SELinux\n", best.n)),
        "{last}"
    );
    assert!(last.ends_with("Question: Which relabel flag do bind mounts need?"));
}

#[tokio::test]
async fn no_context_length_falls_back_visibly_and_a_long_history_is_refused() {
    let (base, _) = serve(Fake {
        models: models(json!({"id": "chat-small"})),
        ..Fake::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let app = FolderChat::open(config(tmp.path(), false), gateway(&base))
        .await
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    app.sync(tx).await.unwrap();

    let answer = app
        .ask(&ChatRequest {
            history: vec![],
            question: "zeta".into(),
        })
        .await
        .unwrap();
    assert_eq!(answer.meta.budget.context_tokens, FALLBACK_CONTEXT_TOKENS);
    assert!(!answer.meta.budget.context_reported);
    assert!(answer
        .meta
        .notes
        .iter()
        .any(|n| n.contains("reports no context_length")));
    assert_eq!(answer.meta.rerank_model, None);

    // ~10k estimated tokens of history against an assumed 8192-token context.
    let long = ChatRequest {
        history: vec![ChatMessage {
            role: Role::User,
            content: "word ".repeat(8000),
        }],
        question: "and now?".into(),
    };
    match app.ask(&long).await {
        Err(AskError::Budget(e @ BudgetError::ConversationTooLong { .. })) => {
            assert!(e.to_string().contains("start a new chat"), "{e}")
        }
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("a conversation over the context must not be answered"),
    }
}

async fn aborted(app: &FolderChat) -> (AbortKind, String, Vec<SyncEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let res = app.sync(tx).await;
    let mut evs = Vec::new();
    while let Ok(e) = rx.try_recv() {
        evs.push(e);
    }
    match res {
        Err(SyncError::Aborted { kind, reason, .. }) => (kind, reason, evs),
        other => panic!("expected a stop, got {:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn a_500_on_one_file_is_that_file_s_error_and_the_rest_is_indexed() {
    let (base, fake) = serve(Fake {
        models: models(json!({"id": "chat-small"})),
        fail_embeddings_containing: Some("POISONPILL".into()),
        ..Fake::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    common::write(tmp.path(), "bad.md", "# Bad\n\nPOISONPILL\n");
    let app = FolderChat::open(config(tmp.path(), false), gateway(&base))
        .await
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let r = app.sync(tx).await.unwrap();
    assert_eq!(r.new_files, 2);
    assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
    assert_eq!(r.errors[0].file, "bad.md");
    let m = &r.errors[0].message;
    assert!(
        m.contains("500") && m.contains("upstream exploded") && m.contains("probe"),
        "{m}"
    );
    // connect probe, then bad.md ×2 and a probe, notes.md, todo.txt.
    assert_eq!(fake.embed_calls.load(Ordering::SeqCst), 6);
    assert!(app.status().await.ready);
}

#[tokio::test]
async fn a_gateway_that_fails_even_the_probe_stops_the_sync() {
    // Call 0 is the connect probe; everything after it fails.
    let (base, _) = serve(Fake {
        models: models(json!({"id": "chat-small"})),
        fail_embeddings_from: Some(1),
        ..Fake::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let app = FolderChat::open(config(tmp.path(), false), gateway(&base))
        .await
        .unwrap();
    let (kind, reason, evs) = aborted(&app).await;
    assert_eq!(kind, AbortKind::Embedder);
    assert!(
        reason.contains("a probe embedding failed too") && reason.contains("EMBED_BATCH_ATTEMPTS"),
        "{reason}"
    );
    assert!(matches!(
        evs.last(),
        Some(SyncEvent::Aborted {
            kind: AbortKind::Embedder,
            ..
        })
    ));
    assert_eq!(app.status().await.chunks, Some(0));
}

#[tokio::test]
async fn an_embedding_answered_by_a_hold_fallback_stops_the_sync_and_writes_nothing() {
    let fallback = "openai/text-embedding-3-small";
    let (base, fake) = serve(Fake {
        models: models(json!({"id": "chat-small"})),
        embed_fallback_from: Some((0, fallback.into())),
        ..Fake::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let app = FolderChat::open(config(tmp.path(), false), gateway(&base))
        .await
        .unwrap();
    let before = common::fingerprint(tmp.path(), "");
    let (kind, reason, _) = aborted(&app).await;
    assert_eq!(kind, AbortKind::GpuHold);
    assert!(
        reason.starts_with(&format!(
            "lmgw answered with the fallback alias '{fallback}' — the embedding model is held; \
             the sync stops rather than mixing models or sending files elsewhere"
        )),
        "{reason}"
    );
    assert_eq!(fake.embed_calls.load(Ordering::SeqCst), 1, "the probe only");
    assert_eq!(
        common::fingerprint(tmp.path(), ""),
        before,
        "not a byte written, index included"
    );
    assert!(
        quickdoc_core::store::get_corpus_by_id(app.index().pool(), "folder@1")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_fallback_that_lands_mid_sync_stores_nothing_from_it() {
    // The connect probe is answered by the local model; the first batch by
    // the fallback.
    let (base, _) = serve(Fake {
        models: models(json!({"id": "chat-small"})),
        embed_fallback_from: Some((1, "openai/text-embedding-3-small".into())),
        ..Fake::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let app = FolderChat::open(config(tmp.path(), false), gateway(&base))
        .await
        .unwrap();
    let (kind, reason, _) = aborted(&app).await;
    assert_eq!(kind, AbortKind::GpuHold);
    assert!(reason.contains("fallback alias"), "{reason}");
    assert!(reason.contains("stopped at notes.md"), "{reason}");
    assert_eq!(app.status().await.chunks, Some(0));
}

#[tokio::test]
async fn an_alias_re_pointed_to_another_model_of_the_same_width_is_a_model_change() {
    let (base, fake) = serve(Fake {
        models: models(json!({"id": "chat-small"})),
        ..Fake::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let app = FolderChat::open(config(tmp.path(), false), gateway(&base))
        .await
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    assert_eq!(app.sync(tx).await.unwrap().index_reset, None);

    // Same alias, same width, other vectors.
    fake.negate_vectors.store(true, Ordering::SeqCst);
    // A restart does not load the old index for answering…
    let restarted = FolderChat::open(config(tmp.path(), false), gateway(&base))
        .await
        .unwrap();
    assert!(!restarted.load_existing().await.unwrap());
    // …and the next sync rebuilds it.
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let r = restarted.sync(tx).await.unwrap();
    let reason = r.index_reset.unwrap();
    assert!(reason.contains("PROBE_SAME_MODEL_MIN_COSINE"), "{reason}");
    assert_eq!(r.new_files, 2);
    assert!(restarted.load_existing().await.unwrap());
}

#[tokio::test]
async fn a_containment_stop_through_the_agent_writes_nothing_at_all() {
    let (base, _) = serve(Fake {
        models: models(json!({"id": "chat-small"})),
        ..Fake::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let app = FolderChat::open(config(tmp.path(), false), gateway(&base))
        .await
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    app.sync(tx).await.unwrap();

    let dir = tmp.path().join(folder_chat::config::INDEX_DIR_NAME);
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::rename(&dir, tmp.path().join("moved")).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), &dir).unwrap();
    let moved = common::fingerprint(&tmp.path().join("moved"), "");
    let (kind, _, _) = aborted(&app).await;
    assert_eq!(kind, AbortKind::IndexContainment);
    // Not even the "last sync finished" clock line.
    assert_eq!(common::fingerprint(&tmp.path().join("moved"), ""), moved);
    assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn a_question_embedded_by_a_hold_fallback_is_refused_as_a_hold() {
    // The sync makes three calls (the connect probe, notes.md, todo.txt);
    // the question's embedding is the fourth, answered by the fallback.
    let (base, fake) = serve(Fake {
        models: models(json!({"id": "chat-small", "context_length": 4096})),
        embed_fallback_from: Some((3, "openai/text-embedding-3-small".into())),
        ..Fake::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let app = FolderChat::open(config(tmp.path(), false), gateway(&base))
        .await
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    app.sync(tx).await.unwrap();
    assert_eq!(fake.embed_calls.load(Ordering::SeqCst), 3);
    match app
        .ask(&ChatRequest {
            history: vec![],
            question: "zeta?".into(),
        })
        .await
    {
        Err(AskError::Gateway(g)) => assert!(g.is_gpu_hold(), "{g}"),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("answered from vectors of another model"),
    }
    assert!(fake.chat_bodies.lock().unwrap().is_empty());
}

fn question(q: &str) -> ChatRequest {
    ChatRequest {
        history: vec![],
        question: q.into(),
    }
}

/// An app over `tmp` synced once with the fake at `base`, then a second one
/// — a restart — over the same folder, not loaded yet.
async fn synced_then_restarted(base: &str, tmp: &tempfile::TempDir) -> FolderChat {
    let app = FolderChat::open(config(tmp.path(), false), gateway(base))
        .await
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    assert_eq!(app.sync(tx).await.unwrap().new_files, 2);
    drop(app);
    FolderChat::open(config(tmp.path(), false), gateway(base))
        .await
        .unwrap()
}

#[tokio::test]
async fn a_hold_at_start_up_does_not_hide_a_complete_index() {
    let (base, fake) = serve(Fake {
        models: models(json!({"id": "chat-small", "context_length": 4096})),
        ..Fake::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let restarted = synced_then_restarted(&base, &tmp).await;

    // The probe cannot run under the hold; the index is loaded as its corpus
    // row pins it, and says it is not checked yet.
    fake.hold_embeddings.store(true, Ordering::SeqCst);
    assert!(restarted.load_existing().await.unwrap());
    let st = restarted.status().await;
    assert!(st.ready, "chat and MCP see the index");
    assert_eq!(st.documents, Some(2));
    assert!(st.unverified.as_deref().unwrap().contains("held"), "{st:?}");

    // The question's own embedding reports the hold; so does MCP's search.
    match restarted.ask(&question("zeta?")).await {
        Err(AskError::Gateway(g)) => assert!(g.is_gpu_hold(), "{g}"),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("answered while the embedding model is held"),
    }
    match restarted.search("zeta", 3).await {
        Err(AskError::Gateway(g)) => assert!(g.is_gpu_hold(), "{g}"),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("searched while the embedding model is held"),
    }
    // A sync under the hold stops before it starts, and leaves the index
    // loaded.
    let (kind, _, _) = aborted(&restarted).await;
    assert_eq!(kind, AbortKind::GpuHold);
    assert!(restarted.status().await.ready);
    assert!(fake.chat_bodies.lock().unwrap().is_empty());

    // Released: the first question probes, finds the model the index was
    // built with, and answers.
    fake.hold_embeddings.store(false, Ordering::SeqCst);
    let answer = restarted
        .ask(&question("Which relabel flag do bind mounts need?"))
        .await
        .unwrap();
    assert_eq!(answer.meta.citations[0].path, "notes.md");
    assert_eq!(restarted.status().await.unverified, None);
}

#[tokio::test]
async fn a_model_change_behind_a_hold_is_caught_by_the_first_probe_after_it() {
    let (base, fake) = serve(Fake {
        models: models(json!({"id": "chat-small", "context_length": 4096})),
        ..Fake::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let restarted = synced_then_restarted(&base, &tmp).await;
    fake.hold_embeddings.store(true, Ordering::SeqCst);
    assert!(restarted.load_existing().await.unwrap());

    // While it was held, the alias was re-pointed: same width, other vectors.
    fake.negate_vectors.store(true, Ordering::SeqCst);
    fake.hold_embeddings.store(false, Ordering::SeqCst);
    match restarted.ask(&question("zeta?")).await {
        Err(e @ AskError::IndexModelChanged { .. }) => {
            let m = e.to_string();
            assert!(
                m.contains("embed/fixture") && m.contains("PROBE_SAME_MODEL_MIN_COSINE"),
                "{m}"
            );
        }
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("answered from an index built by another model"),
    }
    assert!(fake.chat_bodies.lock().unwrap().is_empty());
    let st = restarted.status().await;
    assert!(!st.ready, "the index built by another model is unloaded");
    assert!(matches!(
        restarted.search("zeta", 3).await,
        Err(AskError::NoIndex)
    ));

    // The next sync rebuilds it, and the new index answers.
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let r = restarted.sync(tx).await.unwrap();
    assert!(r.index_reset.is_some());
    assert!(restarted.ask(&question("zeta?")).await.is_ok());
}

#[tokio::test]
async fn a_rerank_answered_by_a_hold_fallback_is_dropped_with_a_note() {
    let fallback = "cohere/rerank-v3.5";
    let (base, fake) = serve(Fake {
        models: models(json!({"id": "chat-small", "context_length": 4096})),
        rerank_fallback: Some(fallback.into()),
        ..Fake::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path());
    let app = FolderChat::open(config(tmp.path(), true), gateway(&base))
        .await
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    app.sync(tx).await.unwrap();

    let q = question("Which relabel flag do bind mounts need?");
    let answer = app.ask(&q).await.unwrap();
    let meta = &answer.meta;
    assert!(
        fake.rerank_calls.load(Ordering::SeqCst) >= 1,
        "it was asked"
    );
    assert_eq!(meta.rerank_model, None, "its scores were not used");
    assert!(
        meta.rerank_skipped
            .as_deref()
            .unwrap()
            .contains("rerank/fixture is held"),
        "{meta:?}"
    );
    assert!(
        meta.notes.iter().any(|n| n.contains(&format!(
            "the excerpts of this question reached the fallback '{fallback}'"
        ))),
        "{:?}",
        meta.notes
    );
    // The fused order, exactly as a question without a reranker gets it.
    let plain = app
        .ask_with(&q, chat::default_search_params(false))
        .await
        .unwrap();
    let ids = |m: &chat::AnswerMeta| {
        m.citations
            .iter()
            .map(|c| c.chunk_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(meta), ids(&plain.meta));
    assert!(!meta.citations.is_empty());

    // The MCP search says the same.
    let r = app.search("zeta relabel", 3).await.unwrap();
    assert_eq!(r.rerank_model, None);
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains(&format!("reached the fallback '{fallback}'"))),
        "{:?}",
        r.notes
    );
}
