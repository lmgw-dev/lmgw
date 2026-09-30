# quickdoc — Hybrid Documentation Retrieval (lmgw) — Design

**Date:** 2026-08-29
**Status:** Draft v2 — spike-backed; v2 (2026-08-30) folds in owner review: the
four open questions resolved and agent-initiated **doc requests** added (§7, §11)

> Companion to [2026-06-09-llm-api-gateway-design.md](2026-06-09-llm-api-gateway-design.md)
> (refs like "§8" point there) and
> [2026-06-29-mcp-gateway-design.md](2026-06-29-mcp-gateway-design.md) ("MCP §x").
> This spec adds a **docs-retrieval plane**: an ingested, versioned corpus of
> library/framework documentation served to every agent on the network through
> the existing MCP plane, plus three gateway-level features it depends on
> (RerankModel, a generalized Jobs subsystem, VRAM admission control).

## 1. Summary

Small local models (qwen3-class) have closed much of the *reasoning* gap to
frontier models but cannot close the *knowledge* gap — they lack the parameter
space for memorized API surfaces. quickdoc turns that weakness into a routing
problem: ingest the documentation of the libraries actually in use into a
hybrid-retrieval corpus (BM25 + embeddings + rerank), and serve it as MCP tools
(`docs__resolve`, `docs__query`) on lmgw's `/mcp` plane. A local model with the
correct doc section in context beats the same model hallucinating from
pretraining, and beats websearch (searxng) on directness, latency, and privacy.

lmgw is the right host because everything quickdoc needs already lives here:
the embedding layer (in-process, no HTTP loopback), the aggregating MCP plane
(one registration → every agent), the server-side tool loop behind
`/v1/responses` (§21) that powers both LLM-driven ingestion and agentic
retrieval testing, the management UI for an ingestion wizard, and the Podman
container lifecycle for the model runtimes involved.

### Goals
- One MCP tool surface for versioned library docs, usable by any agent via `/mcp`.
- Hybrid retrieval: FTS5 BM25 + exact-KNN vector search + fusion + optional
  cross-encoder rerank, measurable end to end.
- LLM-driven ingestion (local models, batch, overnight-cheap) with a
  **verbatim-payload guarantee** — the corpus never contains model-rewritten text.
- An eval harness (golden queries, hit@k/MRR) from day one; retrieval tuning is
  measured, never vibes.
- Gateway features it rides on, built properly: `RerankModel`, generalized Jobs,
  VRAM admission control.

### Non-goals (v1)
- Billion-scale ANN indexing (corpus target is 10⁴–10⁵ chunks; exact KNN is
  milliseconds at that scale — spike-proven).
- Generic web crawling as a primary ingestion path (last-resort only, fenced).
- Multi-user corpus permissions (single owner, trusted LAN, per §2 of the base spec).
- Serving corpora to non-MCP consumers (no bespoke REST retrieval API beyond
  the debug/eval endpoints in §10).

## 2. Locked decisions

| Decision | Choice | Why |
|---|---|---|
| Placement | **`quickdoc-core` workspace crate** + thin adapters in `lmgw-core` (MCP tools, web routes, embedder impl) + UI tab | Retrieval iteration in `cargo test` with fixture embedders; no gateway boot needed |
| Corpus storage | **Separate SQLite file** via the existing **sqlx 0.9**; `embedding BLOB` (**f16**) on the chunk row; **FTS5** for BM25; **exact KNN as in-process scan** over a flat matrix | Spike: 7.9 ms KNN @50k (9× faster than sqlite-vec, 17× faster than LanceDB exact), +0.14 MB binary, zero new crates. f16 from day one halves resident memory. See §5 |
| Vector index escape hatch | **sqlite-vec via `sqlite3_auto_extension`** — proven working next to sqlx 0.9, not shipped in v1 | Same file, same transaction; +2 s build, +0.75 MB when needed. LanceDB and rusqlite rejected (§5) |
| Embedding integrity | **Pin resolved model identity + dimensions per corpus** at ingest; verify at query; refuse loudly on mismatch | Alias remap with same dims is otherwise silent and undetectable |
| Ingestion | **LLM-driven, span-based**: the model selects boundaries/metadata; code slices the *original* text and validates spans verbatim | Corpus must ground, not paraphrase. Context7's observed `use axum({` corruption is the cautionary tale (§7) |
| Tool surface | **`docs__resolve` + `docs__query`** built-in toolset on the main `/mcp` plane (selfadmin pattern, MCP §7 names) | Two-call shape proven by context7; simple surface suits small callers |
| Response budget | **Visible token-budget parameter** on `docs__query` (default from Settings) | No hidden caps — context7's server-side budget is the anti-pattern |
| Reranker | **`RerankModel`** class, sections in the **existing embed router container** (renamed *aux*), `reranking = true`, **no `pooling` key** | Spike-verified live: mixed embed+rerank sections work; flag forces rank pooling itself |
| Reranker acquisition | **Same HF flow as chat/embed models** (`hf_repo` → `hf_add` → downloads) | All three classes are llama.cpp GGUFs; no separate catalog |
| Doc requests | **`docs__request`** tool + a request queue in the Docs tab that prefills the ingest wizard | The agent that misses a corpus knows best that it's missing; the owner stays the ingestion gate |
| Container topology | **No new container.** Split stays by *operational profile*: chat (big, stateful) / aux (small, stateless) / audio | Router mode gives per-model child processes already; a third container is pure lifecycle overhead |
| Preset apply | Adopt **`GET /v1/models?reload=1`** (write preset, reload in place) replacing restart-to-apply | Spike-verified: only changed sections unload; resident models survive. Applies to chat + aux routers |
| Jobs | **Generalized Jobs subsystem** (persistent rows, typed progress events, cancel, SSE/poll); HF downloads ported onto it | Third bespoke job implementation would be one too many; owner preference: generalize now |
| VRAM scheduling | **lmgw is the admission controller.** Routers run **`--models-max 0`**; lmgw computes fit, evicts via `POST /models/unload`, then admits | Router's count-based LRU fights VRAM-based scheduling (counts sleeping models; failed loads consume evictions with no rollback) |
| Eval | Golden queries per corpus, **hit@k + MRR**, synthetic bootstrap via `/v1/responses`; score surfaced in `docs__resolve` and UI | Context7's "benchmark score" validated the idea: callers pick corpora by measured quality |

## 3. Architecture

```
crates/quickdoc-core          # no lmgw deps; the testable heart
  ├─ store/    corpus SQLite (own file, own migrations dir)
  ├─ chunk/    span-based chunking + verbatim validation
  ├─ retrieve/ fts + knn + fusion (RRF) + rerank orchestration
  ├─ ingest/   pipeline steps (pure logic; tool-loop driven from lmgw)
  └─ eval/     golden queries, hit@k / MRR scoring
crates/lmgw-core
  ├─ mcp: docs__ toolset (selfadmin pattern, served on /mcp)
  ├─ web: corpus CRUD, debug search, eval run, wizard routes
  └─ adapters: Embedder + Reranker impls over the gateway
crates/lmgw-ui: Docs tab (§11)
```

Traits at the boundary (all in `quickdoc-core`):

- **`Embedder`** — three impls: *in-process* (calls `embeddings_inner`,
  `proxy.rs` §6 — make it `pub(crate)`), *HTTP* (any OpenAI-compatible
  `/v1/embeddings`; used during crate development against the running prod
  gateway, so dev never needs a second instance or container), *fixture*
  (deterministic vectors for tests).
- **`Reranker`** — in-process via the aux router's `/v1/rerank`; no-op impl.
- **Storage** is behind the store module boundary, not a trait for v1 — the
  spike settled the engine; swapping later is a migration, not a polymorphism
  problem.

**DB layout:** corpus data lives in `quickdoc.db` next to the main DB (§9 path
conventions), WAL mode, **its own sqlx migrations directory**. Main-DB
migrations and corpus migrations never mix; nuke-and-reingest of the corpus
file is always safe. No secrets in the corpus DB → no 0600 requirement, and
`rsync` of one file is a full backup.

## 4. Corpus data model

A **corpus** is one library at one version — `axum@0.8` — mirroring context7's
version-pinned IDs. Multiple corpora may exist per library.

```
corpus      id, library, version, status,
            embed_upstream, embed_model, embed_dims,     -- pinned at ingest
            ingest_model, ingest_prompt_version,          -- corpus = f(two models)
            crawl_date, source_kind, eval_score, chunk_count
source      id, corpus_id, url/root, kind (llms_txt|markdown|rustdoc_json|html), fence (allowed domains)
document    id, source_id, url, content_hash, fetched_at  -- hash gates incremental re-ingest
chunk       id (stable: hash(document.url + span)), document_id,
            heading_path, span_start, span_end,
            payload TEXT,                                 -- VERBATIM source slice
            embedding BLOB,                               -- f32[dims]
            derived_title, derived_summary                -- LLM-derived, marked as such
chunk_fts   FTS5 external-content table over payload + heading_path + derived_*
golden_query id, corpus_id, query, expected_chunk_ids, origin (manual|synthetic)
doc_request  id, library, version?, reason?, client_name?,   -- from MCP initialize info
             count, first_requested_at, last_requested_at,
             status (pending|fulfilled|dismissed)
```

Stable chunk identity: the id is a content hash of `(document url, span text)`,
so unchanged chunks keep their ids across re-ingests — citations and golden
queries survive. Changed documents replace their chunks; orphaned golden
queries surface in the eval view instead of silently vanishing.

**Query-time verification:** every retrieval resolves the corpus's pinned embed
model through the router; identity mismatch → hard error naming the corpus and
both models, plus a "re-embed required" badge in the UI. Never limp along.

## 5. Storage engine — spike results

Full evidence: `~/workspace/scratch/quickdoc-storage-spike/` (projects kept,
`run.sh bench` re-runs). Benchmarks at 50k/100k chunks × 1024-dim f32:

| | sqlx+sqlite-vec | rusqlite+sqlite-vec | LanceDB | **BLOB + brute force** |
|---|---|---|---|---|
| Exact KNN top-10 @50k | 72 ms | 71 ms | 137 ms | **7.9 ms** |
| Exact KNN top-10 @100k | 141 ms | — | 273 ms | **16 ms** |
| FTS/BM25 top-10 @50k | 8.7 ms | 7.8 ms | 0.8 ms | 8.1 ms |
| Clean-build / binary Δ | +2 s / +0.75 MB | +1 s / +0.30 MB | **+384 s / +262 MB** | **+1 s / +0.14 MB** |

Decision: **BLOB + in-process exact scan.** Identical top-10 across all four
engines (cross-validated); the flat-matrix scan is simply the cheapest way to
the same answer. FTS5 is confirmed present in sqlx's bundled SQLite 3.51.3;
FTS5 + vector + metadata commit in one transaction in one file.

Costs made explicit (no hidden limits): the scan holds the matrix resident.
Embeddings are stored **f16 from day one** (owner-approved), so resident cost
is **~98 MiB @50k, ~195 MiB @100k** chunks (the table above measured f32; the
scan is memory-bandwidth-bound, so f16 should be no slower — step 1 verifies
with an f16-vs-f32 recall/latency test). The Docs tab shows per-corpus and
total resident size; if a corpus doesn't fit, that is a visible error, not a
silent cap. Remaining head-room levers behind the store boundary: mmap side
file, then the proven sqlite-vec drop-in.

Rejected: **LanceDB** (+262 MB RPM, +374 crates, protoc in CI, breaking 0.x
API every ~3 weeks — to be *slower* at exact KNN); **rusqlite route** (works,
but pins rusqlite 0.39.x forever against sqlx 0.9's `libsqlite3-sys <0.38`
window — 0.40 already breaks the build).

Footguns to honor if sqlite-vec is ever activated: `sqlite3_auto_extension`
before *any* pool opens (first line of `AppState::init`); KNN CTEs need
`AS MATERIALIZED` (10× penalty otherwise); set `cache_size` explicitly for
ingest (10 s → 1.6 s @50k).

## 6. Retrieval pipeline

```
query ─→ FTS5 BM25 (top K_f) ─┐
      └→ embed → exact KNN (top K_v) ─┴→ RRF fusion → [rerank top K_r] → budgeted response
```

- Every stage parameter (`K_f`, `K_v`, RRF constant, rerank on/off, `K_r`,
  response token budget) is a **per-request parameter** with defaults from
  Settings. Persisting new defaults is a config mutation (dashboard or
  self-admin plane) — mirroring the MCP §13 posture: external agents may
  *experiment* per-request, only the owner *commits*.
- Rerank calls the aux router `/v1/rerank` with the corpus-selected reranker;
  skipped gracefully when no `RerankModel` is enabled.
- The **debug endpoint** (§10) returns per-stage traces: BM25 hits + scores,
  KNN hits + distances, fusion table, rerank scores, final budget trimming.

## 7. MCP tool contract

Built-in toolset, selfadmin pattern, served on the main `/mcp` plane so one
gateway registration reaches every agent. Names are `docs__resolve` and
`docs__query` (MCP §7: ≤64 chars, `[a-zA-Z0-9_-]`).

**`docs__resolve(library, query?)`** → matching corpora with decision metadata:
id (`library@version`), description, chunk count, crawl date, source kind,
**eval score** (our measured hit-rate — context7's "benchmark score" done
honestly), and status flags: embed-model (ok / re-embed required) and
**eval-regression** — the same badge the UI shows is surfaced to the requesting
model, which may prefer another corpus version or caveat its answer. Neither
flag blocks `docs__query`; degraded-but-stated beats refused.

**`docs__request(library, version?, reason?)`** → files a corpus request. If a
matching corpus already exists, the response says so (with its id) instead of
filing; duplicate pending requests for the same `library@version` increment a
counter rather than piling up. The MCP client name from `initialize` is
recorded when available. A `docs__resolve` miss hints at this tool in its
response, so agents discover the path naturally. Requests land in the Docs
tab's queue (§11) — ingestion always starts with the owner, never the agent.

*Addendum (2026-09-18).* The owner gained a second pair of hands: the
`lmgw__docs_*` self-admin tools (§20 plane, `/mcp/admin`, own token) read that
queue and create, ingest, re-embed and delete corpora, so a bulk import can run
unattended instead of forty passes through the wizard. The gate above is
unchanged — those tools are the owner's, behind the owner's token, and call the
same `web::api_docs` handlers the wizard does; nothing on the agent-facing
`/mcp` plane can start a crawl.

**`docs__query(corpus_id, query, budget_tokens?, params?)`** → ranked chunks as
**markdown** (small models parse it better than nested JSON): heading path,
verbatim payload, deep source link, corpus version + crawl date in the header.
`budget_tokens` is visible and defaulted from Settings — never a baked-in
constant. Optional `params` carries the §6 stage overrides (the playground and
optimization agents use it; ordinary callers never see it).

Lessons taken from probing context7 live: **steal** the two-call shape,
version-pinned IDs, metadata-rich resolve, deep links, markdown output.
**Fix** their three defects: payloads must be verbatim (their pipeline returned
`use axum({` — rewritten, corrupted code), prose sections are first-class
chunks (their code-only bias orphans explanations), and the response budget is
parameterized, not hidden.

## 8. Ingestion

**LLM-driven, code-fenced.** The engine is the existing server-side tool loop
(`agent.rs`, §21) — ingestion is a Job that drives a local model with a small
toolset (`fetch`, `emit_extraction`) under the loop's visible budgets.

The extraction contract (non-negotiable):
1. Code fetches; the model **never** free-crawls. Each source declares a domain
   fence; fetches outside it fail. Rate-limited, robots-respecting.
2. The model emits **spans + metadata** (section boundaries, boilerplate ranges
   to drop, heading paths, derived titles/summaries) as **schema-constrained
   output** (llama.cpp `json_schema` — reliable even from small models).
3. Code slices the *original* document by those spans and **validates every
   payload is a verbatim substring**. Validation failure discards the
   extraction, never "fixes" it.
4. Derived text is stored in `derived_*` columns — embedded for recall,
   surfaced only as labels, never as payload.

Source-kind heuristics the ingestion prompt encodes (cheapest first):
`llms-full.txt`/`llms.txt` → repo markdown/mdBook → rustdoc JSON → fenced HTML.
Incremental re-ingest is gated by `document.content_hash` — unchanged pages
never re-run the model. The corpus records `ingest_model` +
`ingest_prompt_version`; both showing in `docs__resolve` output.

Batch economics: ingestion is latency-insensitive and runs on local models
overnight; the VRAM scheduler (§9b) arbitrates it against interactive traffic.

## 9. Gateway features quickdoc rides on

### 9a. RerankModel + aux container (spike-verified)

- New model class rendered into the **existing embed-router preset**
  (container renamed *aux* in UI/tool naming): section emits
  `reranking = true` and **no `pooling`** (the flag forces rank pooling).
  `render_embed_preset` generalizes to `render_aux_preset` over both classes.
- Acquisition is the **standard HF flow** (`hf_repo` → `hf_add` → downloads →
  create model), identical to chat and embed models — all llama.cpp GGUFs, one
  path, no separate catalog. The spike-tested models (bge-reranker-v2-m3,
  Qwen3-Reranker-0.6B) are docs-level suggestions only.
- Add `("rerank", "reranking")` to `SHORT_ALIASES` (`router.rs`) — both are
  valid preset keys for the same option and the dedup guard must catch both.
- **Zero-vector footgun:** `/v1/embeddings` against a reranker section returns
  HTTP 200 with an all-zeros vector (reranking sets the embedding flag
  internally; verified live). lmgw's dispatch gates on its own model-kind so a
  misrouted alias can never poison a corpus.
- New ingress `/v1/rerank` proxied to the aux router; accept Jina and TEI
  request shapes (child auto-detects). Bigger rerank batches come from
  `ubatch-size`, not `batch-size` (child clamps `n_batch` to `n_ubatch`).
- Adopt `GET /v1/models?reload=1` for preset apply on both routers (replaces
  restart-to-apply, `router.rs` apply path): only changed sections unload.

### 9b. VRAM admission control

Problem (owner-stated): a resident chat model + an ingressing embed request =
OOM crash today. llama-server auto-unloads only passively; audio.cpp needs a
container restart to unload. lmgw is the sole ingress, so it is the only place
admission control can exist.

Spike findings that shape the design (full report:
`~/workspace/scratch/llama-router-spike/`):

- **Active eviction exists:** `POST /models/unload` frees the child's VRAM.
  Async — poll `/v1/models` or subscribe `/models/sse` until `unloaded`; the
  200 is *not* "memory freed" (~150 ms typical).
- **Run routers with `--models-max 0`** and disable the built-in LRU entirely.
  It is count-based and fights VRAM scheduling: sleeping models (measured
  718 → 165 MB RSS) count against the limit, and **a failed load still consumes
  an eviction with no rollback** — at capacity, one OOM-on-load costs a healthy
  model *and* leaves the slot empty. With `models-max 0` all of it no-ops.
- **Check `GET /slots?model=X` before evicting** — unload has no
  in-flight-request guard and will kill running generations.
- **No VRAM telemetry exists in the router.** lmgw's GGUF-based planning
  (`gguf.rs`, `local_model_plan`) plus NVML polling is the sizing truth.
- `sleeping` = warm tier: VRAM-free but wake is a full reload.
- audio.cpp eviction = container restart (lmgw already owns that lifecycle).

Scheduler loop: estimate request's model footprint → NVML free check → pick
victims (idle first, `/slots`-guarded) → unload + await `unloaded` → load +
await `loaded` → forward. Requests queue in lmgw, visibly (Traffic page), never
silently dropped. This is its own plane feature; quickdoc bulk embedding is
merely its first heavy customer.

### 9c. Jobs subsystem

One generalized subsystem: persistent job rows, typed progress events, cancel,
one SSE + poll surface. Kinds: `hf_download` (ported from the bespoke
`hf.rs` machinery), `ingest`, `re_embed`, `eval_run`, `golden_gen` (the §10
synthetic bootstrap). The Downloads page and the Docs tab consume the same
feed.

## 10. Web API surface (dashboard plane)

- `POST /api/docs/search` — the **debug endpoint**: query + full §6 param set →
  per-stage traces. Backs the playground and external optimization agents.
- `POST /api/docs/eval` — run a corpus's golden queries (as an `eval_run` job)
  → hit@k, MRR, per-query breakdown; history retained for regression tracking.
- Corpus CRUD + ingest/re-embed job triggers.
- **Export/import**: export streams the corpus DB file (post-`wal_checkpoint`)
  with a metadata manifest; import validates schema version + embed-model
  availability before accepting. Backs manual backup and machine transfer —
  the file is the unit of portability by design (§3).
- Doc-request queue: list / dismiss; a request is auto-marked `fulfilled` when
  a matching corpus finishes ingesting.
- Synthetic golden-query bootstrap: an `/v1/responses` run over sampled chunks
  generates candidate queries; owner curates in the eval view.

Agentic end-to-end testing needs **no new surface**: `/v1/responses` +
`docs__*` tools already compose (§21's loop executes MCP tools server-side).

## 11. UI — Docs tab

- **Corpus list**: status badges (ok / ingesting / re-embed required / eval
  regression — the same flags `docs__resolve` reports), chunk count,
  resident-memory size (§5 — the visible cost), eval score, crawl date,
  export/import actions.
- **Request queue**: pending `docs__request` entries (library, version, reason,
  requester, count, age) with a tab badge; **"Start ingest" prefills the
  wizard** from the request; dismiss removes it; fulfillment is automatic on
  ingest completion.
- **Ingestion wizard** (wizard.rs pattern): source URL + kind + fence + model
  choices → job progress via the Jobs feed.
- **Corpus browser**: documents → chunks, verbatim payload with derived
  fields shown as labels.
- **Search playground**: query box + §6 param controls + per-stage trace
  visualization (the debug endpoint's face).
- **Eval dashboard**: golden queries, scores over time, synthetic-candidate
  curation queue.

## 12. Build order

1. **`quickdoc-core`**: store + chunking + retrieval + eval, fixture embedder.
   Pure `cargo test` territory. *(independent)*
2. **RerankModel + aux rename + `?reload=1` apply** *(independent, small)*
3. **Jobs subsystem** + port HF downloads *(independent)*
4. **Ingestion pipeline** (needs 1, 3; uses HTTP embedder against prod during dev)
5. **MCP toolset + web API** — incl. `docs__request` and export/import
   (needs 1; rerank stage lights up with 2)
6. **Docs tab UI** — incl. request queue + wizard prefill (needs 5)
7. **VRAM admission control** *(own track; before 4 hits real overnight loads)*

## 13. Open questions

None. The v1 draft's four questions were resolved by owner review (2026-08-30)
and folded into the body:

- Reranker acquisition → standard HF flow, no catalog (§2, §9a).
- Eval regression → badge in the UI **and** in `docs__resolve` metadata; never
  blocks queries (§7, §11).
- f16 embeddings → from day one (§5).
- Corpus portability → export/import in API + UI (§10, §11).

Question 1 from the implementation's verification pass is **resolved**
(2026-08-30), as a side effect of the gateway tool-inventory work:

1. ~~**`docs__*` inside `/v1/responses` tool loops.**~~ **Resolved: they
   compose, opt-in, by label.** `mcp::exec::resolve` now answers two reserved
   labels ahead of the registered servers — **`lmgw`** (self-admin) and
   **`docs`** (quickdoc) — so a `/v1/responses` client writes
   `{"type":"mcp","server_label":"docs"}` and a Chat thread ticks the toolset in
   its settings, both with the same `allowed_tools` narrowing a registered
   server gets. Dispatch is the `SplitExecutor` pattern extended to three
   planes, keyed on **the built-in names this run actually resolved** rather
   than on the `lmgw__`/`docs__` prefix, so a run that never attached the
   toolset cannot reach it by guessing a name.
   **Opt-in, never always-on**: a caller that does not name the label gets none
   of those tools — the same reason `lmgw__*` moved off `/mcp` in §21 stage 2.
   `lmgw` stays bound by the `self_admin` mode gate wherever it is attached
   from, and a refusal names the Setting. The labels remain reserved from
   user-registered servers (`ops::validate_tool_prefix`), so nothing can shadow
   them. The register-lmgw's-own-`/mcp`-as-an-upstream workaround is no longer
   needed.

Open question:

2. **Degraded queries on an unresolvable embed pin.** §4 says fail, §7 says
   degraded-but-stated never blocks; the implementation follows §4
   (`docs__query` hard-errors, and the resolve hint now states that
   truthfully). Deciding for §7 means building a BM25-only degraded path that
   labels its output as vector-free.
