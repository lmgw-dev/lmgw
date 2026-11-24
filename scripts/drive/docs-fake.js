// In-page fakes for the Docs page's drives and shots (modern design WP9): populated corpora, a
// request queue, golden queries, candidates, an eval history, a corpus browser and a playground
// answer, without an ingest, an embedding model or a GPU. It answers, in the page:
//   GET  /api/docs/corpora               three corpora (ready, re-embed required + regression, empty)
//   GET  /api/docs/requests              two pending requests and a dismissed one
//   GET  /api/docs/golden, /eval, /golden/candidates   for the first corpus
//   GET  /api/docs/corpora/<id>/documents, /api/docs/chunks   three documents, two chunks each
//   POST /api/docs/search                a ranked answer with its trace
//   POST /api/docs/corpora/<id>/ingest, /re-embed, /api/docs/eval   "job queued" (nothing runs)
// Everything else goes to the dev instance as it is.
//
// Loaded with `scripts/ui-drive.py --init-script scripts/drive/docs-fake.js` (or ui-matrix.py).
// The scenario is on while sessionStorage's "lmgw.fake.docs" is "1"; `?fake-docs=1` on any URL
// sets it. window.__docsFakeAnswered counts the answers by key, window.__docsFakePosts lists the
// POSTs it swallowed.
//
// The data is made up and looks it: libraries are *-demo, URLs are under docs.example.
(() => {
  "use strict";
  try {
    if (new URL(location.href).searchParams.get("fake-docs") === "1") {
      sessionStorage.setItem("lmgw.fake.docs", "1");
    }
    if (sessionStorage.getItem("lmgw.fake.docs") !== "1") return;
  } catch (_) {
    return;
  }
  const answered = (window.__docsFakeAnswered = {});
  const posts = (window.__docsFakePosts = []);
  const count = (k) => { answered[k] = (answered[k] || 0) + 1; };
  const json = (v, status = 200) => new Response(JSON.stringify(v), {
    status, headers: { "content-type": "application/json" },
  });
  const T = "2026-01-01T12:00:00Z";
  const src = (id, corpus, root, kind) => ({ id, corpus_id: corpus, root, kind, fence: [], created_at: T });
  const corpora = [
    {
      id: 1, corpus_id: "router-demo@0.8", library: "router-demo", version: "0.8", status: "ready",
      chunk_count: 1840, unembedded_chunks: 0, resident_bytes: 5_652_480,
      embed_upstream: "embed-demo", embed_model: "bow-demo", embed_dims: 768,
      embed_identity: "embed-demo/bow-demo@768", ingest_model: "extract-demo",
      ingest_prompt_version: "v3", crawl_date: "2026-01-01T12:00:00Z", source_kind: "markdown",
      eval_score: 0.82, eval_best: 0.85, eval_k: 10, eval_at: "2026-01-02T09:30:00Z",
      eval_regression: false, embed_status: "ok", eval_status: "ok", flags: [], warnings: [],
      sources: [src(1, 1, "https://docs.example/router-demo/0.8/", "markdown"),
                src(2, 1, "https://docs.example/router-demo/llms.txt", "llms_txt")],
      created_at: T, updated_at: T,
    },
    {
      id: 2, corpus_id: "parser-demo@2.1", library: "parser-demo", version: "2.1", status: "ready",
      chunk_count: 412, unembedded_chunks: 37, resident_bytes: 1_265_664,
      embed_upstream: "embed-demo", embed_model: "old-demo", embed_dims: 384,
      embed_identity: "embed-demo/old-demo@384", ingest_model: "extract-demo",
      ingest_prompt_version: "v3", crawl_date: "2025-12-20T08:00:00Z", source_kind: "html",
      eval_score: 0.61, eval_best: 0.74, eval_k: 10, eval_at: "2026-01-01T18:00:00Z",
      eval_regression: true, embed_status: "re_embed_required", eval_status: "regression",
      flags: ["re_embed_required", "regression"],
      warnings: ["the pinned embedding model no longer resolves here: searches use keywords only",
                 "the last eval scored below this corpus's best"],
      sources: [src(3, 2, "https://docs.example/parser-demo/2.1/index.html", "html")],
      created_at: T, updated_at: T,
    },
    {
      id: 3, corpus_id: "queue-demo@1.0", library: "queue-demo", version: "1.0", status: "ready",
      chunk_count: 0, unembedded_chunks: 0, resident_bytes: 0,
      embed_upstream: "embed-demo", embed_model: "bow-demo", embed_dims: 768,
      embed_identity: "embed-demo/bow-demo@768", ingest_model: "extract-demo",
      ingest_prompt_version: "", crawl_date: "", source_kind: "llms_txt",
      eval_score: null, eval_best: null, eval_k: null, eval_at: null, eval_regression: false,
      embed_status: "ok", eval_status: "unmeasured", flags: ["unmeasured"], warnings: [],
      sources: [src(4, 3, "https://docs.example/queue-demo/llms.txt", "llms_txt")],
      created_at: T, updated_at: T,
    },
  ];
  const requests = [
    { id: 1, library: "cache-demo", version: "3.2", reason: "the agent was asked how eviction works and found no corpus",
      client_name: "assistant-demo", count: 3, first_requested_at: "2026-01-01T10:00:00Z",
      last_requested_at: "2026-01-02T08:15:00Z", status: "pending" },
    { id: 2, library: "http-demo", version: "", reason: null, client_name: null, count: 1,
      first_requested_at: "2026-01-02T07:00:00Z", last_requested_at: "2026-01-02T07:00:00Z", status: "pending" },
    { id: 3, library: "log-demo", version: "0.4", reason: "asked twice about structured fields",
      client_name: "assistant-demo", count: 2, first_requested_at: "2025-12-28T10:00:00Z",
      last_requested_at: "2025-12-29T10:00:00Z", status: "dismissed" },
  ];
  const chunk = (id, doc, path, text) => ({
    id, document_id: doc, corpus_id: 1, heading_path: path, span_start: 0, span_end: text.length,
    payload: text, derived_title: "Adding a layer", derived_summary: "How middleware wraps a route.",
  });
  const chunks = {
    1: [chunk("c1a0b1c2d3e4f5a6", 1, "Router › Middleware", "## Middleware\n\nWrap a route in a layer with `.layer(...)`; layers run outside in."),
        chunk("c1b0b1c2d3e4f5a6", 1, "Router › Middleware › Order", "Layers added later run first on the request and last on the response.")],
    2: [chunk("c2a0b1c2d3e4f5a6", 2, "Extractors › Path", "A path extractor reads the captured segments into a tuple or a struct.")],
    3: [],
  };
  const documents = [
    { id: 1, source_id: 1, url: "https://docs.example/router-demo/0.8/middleware.md", content_hash: "ab12cd34ef56ab12cd34ef56", fetched_at: T },
    { id: 2, source_id: 1, url: "https://docs.example/router-demo/0.8/extract/path.md", content_hash: "cd34ef56ab12cd34ef56ab12", fetched_at: T },
    { id: 3, source_id: 1, url: "https://docs.example/router-demo/0.8/extract/", content_hash: "ef56ab12cd34ef56ab12cd34", fetched_at: T },
  ];
  const stage = (ids) => ids.map((chunk_id, i) => ({ chunk_id, rank: i + 1, score: 0.9 - i * 0.1, skipped: false }));
  const search = {
    corpus_id: "router-demo@0.8",
    hits: chunks[1].map((c, i) => ({ chunk: c, score: 0.8 - i * 0.1, rrf_score: 0.0325 - i * 0.002,
      fts_rank: i + 1, knn_rank: 2 - i, knn_score: 0.71 - i * 0.05, rerank_score: null, rerank_skipped: true, tokens: 42 })),
    trace: {
      corpus_id: "router-demo@0.8", embed_model: "embed-demo/bow-demo@768",
      params: { k_fts: 50, k_vec: 50, rrf_k: 60, fts_weights: { payload: 1, heading_path: 1.5, derived_title: 1.5, derived_summary: 1 },
                rerank: true, k_rerank: 20, limit: 8, budget_tokens: null },
      fts_query: "\"middleware\" OR \"layer\"", fts: stage(["c1a0b1c2d3e4f5a6", "c1b0b1c2d3e4f5a6"]),
      knn_skipped: null, knn: stage(["c1b0b1c2d3e4f5a6", "c1a0b1c2d3e4f5a6"]),
      fused: [{ chunk_id: "c1a0b1c2d3e4f5a6", rrf_score: 0.0325, fts_rank: 1, knn_rank: 2 },
              { chunk_id: "c1b0b1c2d3e4f5a6", rrf_score: 0.0323, fts_rank: 2, knn_rank: 1 }],
      rerank_model: null, rerank_skipped: "no reranker is attached to this gateway", rerank: [],
      token_counter: "tiktoken", budget_used_tokens: 84, budget_dropped: [], resident_vectors: 1840,
      resident_bytes: 5652480, knn_kernel: "exact cosine (f16)",
      timings: { embed_ms: 3.1, fts_ms: 0.8, knn_ms: 1.9, fuse_ms: 0.1, fetch_ms: 0.4, rerank_ms: 0, total_ms: 6.5 },
    },
    urls: { c1a0b1c2d3e4f5a6: documents[0].url, c1b0b1c2d3e4f5a6: documents[0].url },
    status: { warnings: [] },
    markdown: "### router-demo@0.8 · Router › Middleware\n\nWrap a route in a layer with `.layer(...)`; layers run outside in.\n\n```rust\nlet app = Router::new().layer(TraceLayer::new());\n```\n",
  };
  const golden = [
    { id: 1, corpus_id: 1, query: "how do I add a middleware layer?", expected_chunk_ids: ["c1a0b1c2d3e4f5a6"], origin: "manual" },
    { id: 2, corpus_id: 1, query: "in which order do layers run?", expected_chunk_ids: ["c1b0b1c2d3e4f5a6"], origin: "synthetic" },
  ];
  const runs = [
    { id: 1, corpus_id: 1, k: 10, queries: 2, hit_at_k: 0.85, mrr: 0.72, orphaned_queries: 0, regression: false,
      params: { k_fts: 50, k_vec: 50 }, report: [{ query: 1, hit: true }], created_at: "2026-01-01T09:00:00Z" },
    { id: 2, corpus_id: 1, k: 10, queries: 2, hit_at_k: 0.82, mrr: 0.70, orphaned_queries: 1, regression: true,
      params: { k_fts: 50, k_vec: 50 }, report: [{ query: 1, hit: true }], created_at: "2026-01-02T09:30:00Z" },
  ];
  const candidates = {
    candidates: [{ id: 1, corpus_id: 1, query: "what does a path extractor read?", expected_chunk_ids: ["c2a0b1c2d3e4f5a6"],
      rationale: "the section explains the captured segments", model: "extract-demo", status: "pending",
      golden_query_id: null, created_at: T, decided_at: null }],
    chunks: { c2a0b1c2d3e4f5a6: chunks[2][0] },
  };

  const real = window.fetch.bind(window);
  window.fetch = async (input, init) => {
    const url = new URL(typeof input === "string" ? input : input.url, location.href);
    const method = ((init && init.method) || (typeof input === "string" ? "GET" : input.method) || "GET").toUpperCase();
    const p = url.pathname;
    if (!p.startsWith("/api/docs")) return real(input, init);
    if (method === "GET") {
      if (p === "/api/docs/corpora") {
        count("corpora");
        return json({ corpora, pending_requests: requests.filter((r) => r.status === "pending").length,
          rerank_model: null, schema_version: 3 });
      }
      if (p === "/api/docs/requests") {
        count("requests");
        const s = url.searchParams.get("status");
        return json({ requests: s ? requests.filter((r) => r.status === s) : requests });
      }
      let m = p.match(/^\/api\/docs\/corpora\/(\d+)\/documents$/);
      if (m) { count("documents"); return json({ documents: +m[1] === 1 ? documents : [] }); }
      if (p === "/api/docs/chunks") { count("chunks"); return json({ chunks: chunks[+url.searchParams.get("document_id")] || [] }); }
      if (p === "/api/docs/golden") { count("golden"); return json({ golden_queries: url.searchParams.get("corpus_id") === "1" ? golden : [] }); }
      if (p === "/api/docs/eval") { count("eval"); return json({ eval_runs: url.searchParams.get("corpus_id") === "1" ? runs : [] }); }
      if (p === "/api/docs/golden/candidates") {
        count("candidates");
        return json(url.searchParams.get("corpus_id") === "1" ? candidates : { candidates: [], chunks: {} });
      }
      if (p === "/api/docs/export/manifest") {
        count("manifest");
        return json({ schema_version: 3, exported_at: T, byte_size: 7_340_032,
          corpora: corpora.map((c) => ({ corpus_id: c.corpus_id, embed_upstream: c.embed_upstream,
            embed_model: c.embed_model, embed_dims: c.embed_dims, chunk_count: c.chunk_count })) });
      }
      return real(input, init);
    }
    posts.push(p);
    if (p === "/api/docs/search") { count("search"); return json(search); }
    if (/^\/api\/docs\/corpora\/\d+\/(ingest|re-embed)$/.test(p) || p === "/api/docs/eval" || p === "/api/docs/golden/generate") {
      count("job");
      return json({ ok: true, job_id: 0, already_running: false });
    }
    // Anything else that would write (delete, request status, golden edits) is refused here, so
    // a drive cannot change the instance by accident.
    count("refused");
    return json({ error: { message: "docs-fake: writes are not faked" } }, 400);
  };
})();
