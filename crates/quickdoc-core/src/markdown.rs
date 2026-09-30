//! `docs__query`'s answer shape (§7): markdown, because small models parse it
//! better than nested JSON.
//!
//! Three things are non-negotiable here and are what the tests pin:
//!
//! * the payload is emitted **verbatim** and unfenced — it is already markdown
//!   from the source document, and wrapping it in a code fence would corrupt
//!   every fence it contains;
//! * every chunk carries its **deep link**, so an answer built from this can
//!   cite where it came from;
//! * the header states the corpus's version, crawl date, embedding model, eval
//!   score and any degradation, and the footer states what the **token budget**
//!   did. A budget that trims silently is the context7 anti-pattern this whole
//!   surface exists to avoid.

use std::collections::HashMap;

use crate::retrieve::SearchResult;
use crate::store::Corpus;

/// Render one search result for a tool caller.
///
/// `urls` maps chunk id → source document URL ([`crate::store::document_urls`]);
/// a chunk missing from it is rendered without a link rather than dropped.
/// `warnings` are the corpus-level flags the caller should know about — the
/// re-embed and eval-regression badges — stated, never enforced.
pub fn render(
    corpus: &Corpus,
    query: &str,
    result: &SearchResult,
    urls: &HashMap<String, String>,
    warnings: &[String],
) -> String {
    let t = &result.trace;
    let mut out = String::new();

    out.push_str(&format!(
        "# {} — {} for {query:?}\n\n",
        corpus.corpus_id(),
        plural(result.hits.len(), "chunk", "chunks"),
    ));

    let mut facts = vec![
        format!("corpus `{}`", corpus.corpus_id()),
        format!("status `{}`", corpus.status),
        format!("{} chunks indexed", corpus.chunk_count),
    ];
    if !corpus.crawl_date.is_empty() {
        facts.push(format!("crawled {}", corpus.crawl_date));
    }
    if !corpus.source_kind.is_empty() {
        facts.push(format!("source `{}`", corpus.source_kind));
    }
    facts.push(format!("embedded with `{}`", t.embed_model));
    if let Some(m) = &t.rerank_model {
        facts.push(format!("reranked with `{m}`"));
    }
    match corpus.eval_score {
        Some(s) => facts.push(format!("eval hit@{} {:.2}", corpus.eval_k.max(1), s)),
        None => facts.push("eval not measured".into()),
    }
    out.push_str(&facts.join(" · "));
    out.push('\n');

    for w in warnings {
        out.push_str(&format!("\n> ⚠ {w}\n"));
    }
    if let Some(reason) = &t.rerank_skipped {
        out.push_str(&format!("\n> rerank stage skipped: {reason}\n"));
    }

    if result.hits.is_empty() {
        out.push_str(
            "\nNo chunk matched. Try other wording, or `docs__resolve` for another version of \
             this library.\n",
        );
        return out;
    }

    for (i, h) in result.hits.iter().enumerate() {
        let heading = if h.chunk.heading_path.is_empty() {
            h.chunk.derived_title.clone()
        } else {
            h.chunk.heading_path.clone()
        };
        out.push_str("\n---\n\n");
        out.push_str(&format!(
            "## {}. {}\n",
            i + 1,
            if heading.is_empty() {
                "(untitled section)".to_string()
            } else {
                heading
            }
        ));
        if let Some(url) = urls.get(&h.chunk.id) {
            out.push_str(&format!("source: {url}\n"));
        }
        out.push_str(&format!("{}\n\n", provenance(h)));
        out.push_str(h.chunk.payload.trim_end());
        out.push('\n');
    }

    out.push_str("\n---\n\n");
    out.push_str(&budget_line(result));
    out
}

/// Where a hit's rank came from, so a caller (or a human reading a trace) can
/// see whether it was the lexical or the vector stage that found it.
fn provenance(h: &crate::retrieve::Hit) -> String {
    let mut parts = Vec::new();
    match h.rerank_score {
        Some(s) => parts.push(format!("rerank {s:.3}")),
        None => parts.push(format!("rrf {:.4}", h.rrf_score)),
    }
    if let Some(r) = h.fts_rank {
        parts.push(format!("bm25 #{r}"));
    }
    if let Some(r) = h.knn_rank {
        parts.push(format!("knn #{r}"));
    }
    parts.push(format!("{} tokens", h.tokens));
    format!("_{}_", parts.join(" · "))
}

/// The budget's own report. Always present, whether or not a budget was set:
/// "how much did this cost" is not a question a caller should have to guess at.
fn budget_line(result: &SearchResult) -> String {
    let t = &result.trace;
    match t.params.budget_tokens {
        Some(b) => {
            let mut s = format!(
                "{} of the {b}-token budget used ({} counter)",
                t.budget_used_tokens, t.token_counter
            );
            if !t.budget_dropped.is_empty() {
                s.push_str(&format!(
                    "; {} dropped by it — raise `budget_tokens` to see them",
                    plural(
                        t.budget_dropped.len(),
                        "lower-ranked chunk",
                        "lower-ranked chunks"
                    ),
                ));
            }
            s.push('\n');
            s
        }
        None => format!(
            "{} tokens returned ({} counter); no `budget_tokens` was set, so nothing was \
             trimmed.\n",
            t.budget_used_tokens, t.token_counter
        ),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{Embedder, FixtureEmbedder, FixtureReranker};
    use crate::retrieve::{Retriever, SearchParams};
    use crate::store::{self, NewChunk, NewCorpus};
    use std::sync::Arc;

    async fn seeded() -> (sqlx::SqlitePool, i64) {
        let pool = store::open_in_memory().await.unwrap();
        let embedder = FixtureEmbedder::new(32);
        let cid = store::insert_corpus(
            &pool,
            &NewCorpus {
                crawl_date: "2026-08-29T10:00:00Z".into(),
                source_kind: "markdown".into(),
                status: "ready".into(),
                ..NewCorpus::new("axum", "0.8", embedder.identity())
            },
        )
        .await
        .unwrap();
        let sid = store::insert_source(&pool, cid, "https://docs.rs/axum", "markdown", &[])
            .await
            .unwrap();
        let (did, _) = store::upsert_document(&pool, sid, "https://docs.rs/axum/routing", "h")
            .await
            .unwrap();
        // A payload that itself contains a fence — the case a code-fenced
        // renderer would corrupt.
        let payload = "Handlers return IntoResponse.\n\n```rust\nRouter::new()\n```";
        let mut c = NewChunk::new(cid, did, (0, 40), payload);
        c.heading_path = "Routing".into();
        c.embedding = Some(embedder.embed_one(payload));
        store::insert_chunks(&pool, "https://docs.rs/axum/routing", 32, &[c])
            .await
            .unwrap();
        (pool, cid)
    }

    #[tokio::test]
    async fn payload_is_verbatim_and_the_budget_reports_itself() {
        let (pool, cid) = seeded().await;
        let r = Retriever::load(pool.clone(), cid, Arc::new(FixtureEmbedder::new(32)))
            .await
            .unwrap()
            .with_reranker(Arc::new(FixtureReranker));
        let result = r
            .search("handlers", &SearchParams::default())
            .await
            .unwrap();
        let ids: Vec<String> = result.hits.iter().map(|h| h.chunk.id.clone()).collect();
        let urls = store::document_urls(&pool, &ids).await.unwrap();
        let corpus = store::get_corpus(&pool, cid).await.unwrap().unwrap();

        let md = render(
            &corpus,
            "handlers",
            &result,
            &urls,
            &["eval regression: below best".to_string()],
        );
        assert!(md.contains("# axum@0.8"), "{md}");
        assert!(md.contains("crawled 2026-08-29T10:00:00Z"), "{md}");
        assert!(md.contains("source: https://docs.rs/axum/routing"), "{md}");
        assert!(md.contains("## 1. Routing"), "{md}");
        assert!(
            md.contains("```rust\nRouter::new()\n```"),
            "the payload's own fence must survive: {md}"
        );
        assert!(md.contains("⚠ eval regression"), "{md}");
        assert!(md.contains("no `budget_tokens` was set"), "{md}");
    }

    #[tokio::test]
    async fn a_budget_that_trims_says_so() {
        let (pool, cid) = seeded().await;
        let r = Retriever::load(pool.clone(), cid, Arc::new(FixtureEmbedder::new(32)))
            .await
            .unwrap();
        let params = SearchParams {
            budget_tokens: Some(1),
            ..Default::default()
        };
        let result = r.search("handlers", &params).await.unwrap();
        let corpus = store::get_corpus(&pool, cid).await.unwrap().unwrap();
        let md = render(&corpus, "handlers", &result, &HashMap::new(), &[]);
        // The single hit is bigger than the whole budget and still goes out —
        // the overrun is stated rather than the answer being empty.
        assert!(md.contains("of the 1-token budget used"), "{md}");
        assert_eq!(result.hits.len(), 1);
    }
}
