# llama.cpp gets its own egress: the `llama_cpp` protocol, `/props` facts, tool-result images

Requested by the owner and approved 2026-10-05. **v2, approved 2026-10-06**: v1 plus an
adversarial review against the code (appendix). The owner confirmed both open questions as
recommended and kept every "taken in this draft" decision (§11). Today llama-server is an
`openai`-protocol upstream of kind `llama_server`, and its quirks are
`kind == UpstreamKind::LlamaServer` branches inside the OpenAI egress. It gets a protocol and an
egress of its own, composed from the OpenAI wire pieces the two share, so the OpenAI egress speaks
only OpenAI. ik_llama.cpp's server uses the same egress. The first feature built on it is
tool-result images, which llama-server accepts and lmgw flattens to text today.

**Today** (lines at `main` 7862c23):
- Every local chat and aux request leaves through `egress::openai::OpenaiEgress`. The synthetic
  router and aux upstreams are `protocol: Openai, kind: LlamaServer` (`config/snapshot.rs:419-438`,
  `:477`), and the egress is picked by protocol alone (`egress/mod.rs:132-138`).
- The OpenAI egress branches on the kind three times (`egress/openai.rs:185`, `:270`, `:587`) and
  carries llama-only wire beyond that (§1.1).
- `upstreams` checks `protocol IN ('openai','anthropic','gemini')` and
  `kind IN ('generic','llama_server','audio_cpp')`, and nothing ties the two together
  (`migrations/0013_audio.sql:26-40`).
- lmgw reads `/props` only off the request path: the local model test's cross-check
  (`modelinfo.rs:3324-3374`) and the benchmark engine (`bench/facts.rs:1-80`, both engines).
- Every tool result is flattened to text (`egress/openai.rs:26-46`, `ir.rs:154-188`): an image
  becomes `[image/png image, N base64 bytes — omitted: …]` plus a WARN. llama-server at 0c6a6a7
  accepts `image_url` parts in any role, `tool` included (`server-common.cpp:1236-1285`), and keeps
  the media marker in place (`common/chat.cpp:187-220`). Its own Anthropic and Responses converters
  send tool-result images that way (`server-chat.cpp:453-513`, `:191-230`).

## Decisions

*Owner, 2026-10-05:*
1. **llama.cpp gets its own protocol and egress.** It is a core component, its future quirks must
   not depend on OpenAI, and OpenAI-shaped surfaces stay true to OpenAI.
2. **Compose, don't fork.** A pure move puts the shared OpenAI-wire pieces into a common module
   first (§2, checked with `scripts/move-check.py`). The llama egress then uses them and overrides
   only where llama.cpp differs (§3), and the OpenAI egress loses every `LlamaServer` branch.
3. **Ask the server, don't guess from config.** `/props` drives capability decisions such as
   tool-result images (§4).
4. **ik_llama.cpp gets the same egress.** Feature probing for ik only where it needs no constant
   updating; ik is not a priority (§6).
5. **Existing `llama_server` rows migrate automatically, external ones too.** Afterwards
   `protocol` and `kind` cannot disagree (§5).
6. **Kind checks outside the egress stay.** Code that asks "is this a llama-server?" keeps asking;
   only the wire dialect moves (§1.3).
7. **Every local chat request goes through this egress**, and byte-identical goldens prove
   everything except the intended changes (§9).
8. **The first feature is tool-result images**, confirmed by a live probe on a dev copy (§8).
9. **The order:** this spec, the owner's review, then the build.

*Taken in this draft; overturn in review:*
10. **A new `Protocol::LlamaCpp`, spelled `llama_cpp`, rather than choosing the egress by kind.**
    The stored row says which wire it speaks, and `for_protocol` stays a one-key dispatch at its 17
    call sites. Choosing by kind keeps `protocol = openai` on llama rows (the shape being replaced)
    and needs a `for_upstream` at every call site anyway. The cost, the `Protocol::Openai`
    comparisons a new variant changes without a compile error (§1.4), is paid once in WP2a. Not
    `llama`: Meta's hosted Llama API is OpenAI-compatible and belongs on a `generic` openai row;
    `llama_cpp` also matches `audio_cpp`. `Protocol` is `rename_all = "lowercase"`
    (`config/routes.rs:13-14`), so the variant gets an explicit `#[serde(rename = "llama_cpp")]`.
11. **The invariant.** `llama_cpp` implies `llama_server`, and `llama_server` implies `llama_cpp` or
    `anthropic`. `openai` with `llama_server` can no longer exist. `anthropic` stays allowed:
    llama-server serves `/v1/messages` (`server.cpp:268`), and the kind is what makes a row local
    and free (§1.3). Enforced three times: table CHECKs, `settle` on the patch, and the migration
    (§5).
12. **The migration maps** `openai`+`llama_server` → `llama_cpp`. `gemini`+`llama_server`, a wire
    llama-server does not serve, keeps its protocol and becomes `generic`. A start notice names
    each such row; `anthropic`+`llama_server` is left as it is.
13. **Facts belong to a server, not to a request.** A managed container is probed once per start,
    climb and adoption. An external row is probed in the background per (row, `base_url`, model)
    and cached until an event says it may be stale (§4). There is no TTL.
14. **Unknown means today.** With no facts, or a fact that is absent, the request carries today's
    bytes.
15. **Nothing is keyed on a build number.** `chat_template_kwargs.enable_thinking` stays the only
    thinking switch on every build (§7).
16. **The move is byte-identical. Intended changes come after it, one commit each** (§3.3).
17. **The OpenAI egress keeps** the sampler extensions (`top_k`, `min_p`, `repeat_penalty`, the
    client's own fields), the `reasoning_content` replay (DeepSeek reads it too), embeddings'
    `dimensions`, the Jina-shape rerank (Q1) and the `timings` read (Q2).
18. **A tool image is sent only where it is safe**, by a conservative predicate decided once per
    request. Otherwise it goes as the placeholder with its reason and a WARN (§8.2). A tool loop
    that works today never becomes a refusal.
19. **No native `/v1/responses` passthrough on `llama_cpp` rows** (CHECK and `settle`), so no llama
    chat request bypasses the egress (`responses.rs:125`).

## 1. Inventory

### 1.1 The egress layer

| What | Where | Goes to |
|---|---|---|
| Reasoning control, llama half: `enable_thinking` kwarg deep-merge, scalar even beside an OpenRouter object, `reasoning_budget_tokens`, no level while off | `openai.rs:175-251` (`:185`, `:193`, `:206-224`, `:236-250`) | llama |
| Strip a passthrough `reasoning_effort` while off (kind branch) | `:270-272` | llama |
| Rewrite a passthrough `thinking_budget_tokens` (llama's older name), today on every OpenAI route | `:277-284` | llama (I2) |
| Reconcile OpenRouter's `reasoning` object | `:285-303` | shared |
| Count plan `/tokenize {model, content}` (kind branch), `parse_count`, `tokenize_request` | `:587-591`, `:606-613`, `:788-795` | llama |
| tiktoken count | `:597-602`, `:760-778` | OpenAI |
| `exceed_context_size_error` → `ContextExceeded` (the ladder backstop) | `:403-429`, `:515-539` | llama; generic error mapping shared |
| `timings` on completions and streams, `parse_timings` | `:505-507`, `:876-881`, `:733-747` | shared (Q2) |
| `reasoning_content` replay on assistant turns | `:83-90` | shared, unchanged |
| Reading `reasoning_content` / `reasoning` | `:147-151` | shared |
| Messages, user content, tools, `tool_choice`, `stream_options`, the passthrough loop | `:21-140`, `:316-401` | shared |
| `input_audio` raw base64 with `format` (ik requires `wav`/`mp3`) | `:125-135` | shared |
| Tool-result flattening | `:26-46` | shared default; llama renderer (§8) |
| `top_k`, `min_p`, `repeat_penalty` | `:332-342` | shared, unchanged |
| Embeddings (llama-server ignores `dimensions`) | `:559-573`, `:615-641` | shared |
| Rerank: Jina request, Jina/TEI response | `:643-704` | `openai_wire::aux`, both egresses (Q1) |
| Usage, SSE decoder, finish reasons | `:707-726`, `:797-884`, `mod.rs:185-193` | shared |
| `has_reasoning_object` (`web/chat_reasoning.rs:92`, `proxy/chat.rs:199`, `responses.rs:201`) | `:156-160` | shared |
| Docs that name llama.cpp | `openai.rs:1-2`, `mod.rs:56-58`, `:77-85` | reworded |

### 1.2 Wire dialect outside the egress

| What | Where | Then |
|---|---|---|
| The count: `/apply-template`, then `/tokenize` with `add_special: true` | `gate/count.rs:155-189`, `:243-265` | request builders and readers move to the llama module; the gate keeps the policy |
| The counted body is the posted body: `egress::openai::chat_body(.., kind)` | `gate/fit.rs:416-422`, `gate/send.rs:509-515`, `proxy/count_messages.rs:255-261` | the llama egress's `chat_body(.., &Upstream)` |
| `parse_exceed_context` on local sends | `gate/send.rs:262`, `:658` | llama module path |
| The `/tokenize` passthrough route | `proxy/tokenize.rs:111-137` | llama module path; guard on the protocol |
| `/v1/completions` error mapping hard-wired to the OpenAI egress | `proxy/legacy.rs:352`, `:393` | the route's egress; otherwise llama's context refusal stops mapping |
| `timings_per_token` for the stats panel | `web/chat.rs:1080-1093` | stays (the caller's intent), kind check |
| `continue_final_message` / `add_generation_prompt` on Continue | `web/chat_turn.rs:874-896` | stays; its `protocol == Openai` test (`:875-876`) is re-keyed |
| The client's `enable_thinking` folded into the control | `proxy/recording.rs:691-697`, `ir.rs:427-440` | stays (ingress side), kind check |
| A raw `n_predict` folded into `max_tokens` | `gate/clamp.rs:65-85` | stays (gate) |
| `parse_timings` for the benchmark's `/completion` | `bench/stream.rs:74` | unchanged (shared) |

### 1.3 `UpstreamKind::LlamaServer` outside the egress

There are 75 uses, about 30 of them outside tests. **These stay** (decision 6), and they are why the
kind is more than a label: `catalog.rs:272` (no price sync), `web/chat.rs:1088`,
`web/wiring.rs:44`/`:66`, `web/api_usage.rs:208`, `capabilities/exposed.rs:119`/`:192`/`:604`/`:666`
(the free local price, the backing row), `ops/realtime_budget.rs:63`, `config/snapshot.rs:181`
(`is_local_upstream`), `proxy/audio.rs:509`, `proxy/recording.rs:691`, `proxy/embeddings.rs:291`,
`proxy/count_messages.rs:196`, `knowledge/limit.rs:150`, `proxy/reasoning_fit.rs:253`
(`!= Generic`) and the doc at `ir.rs:437`. **These are re-keyed to `Protocol::LlamaCpp`**, because
they pair the kind with `Protocol::Openai`: `web/chat_sampling.rs:86`, `web/chat_turn.rs:800`/
`:875-876`, `proxy/recording.rs:568` and `proxy/tokenize.rs:111-112`. `config/snapshot.rs:425`/
`:435` (the synthetic router and aux upstreams) get `protocol: LlamaCpp`.

### 1.4 `Protocol::Openai` sites a new variant changes silently

These are the `==`/`!=` tests, wildcard arms, defaults and strings:
- **Guards:** `gate/open.rs:150` (the legacy `/v1/completions` guard), `proxy/audio.rs:59`
  (llama-server serves `/v1/audio/transcriptions`, `server.cpp:266`), `proxy/image.rs:92`.
- **Capabilities:** `capabilities/task.rs:68` (chat endpoints), `capabilities/mod.rs:882-885` (task
  by name), `capabilities/exposed.rs:816` (image row, stays `Openai`), `:932` (local chat row,
  becomes `LlamaCpp`), `ops/common.rs:119`.
- **Re-keyed tests:** `web/chat_turn.rs:876`, `proxy/tokenize.rs:111`.
- **Engine checks:** `realtime/voice/fallback.rs:52-60` and its caller `realtime/voice.rs:372`.
- **Hard-wired `map_error`:** `proxy/legacy.rs:352`/`:393`, `proxy/audio.rs:93`,
  `proxy/image.rs:50`.
- **Defaults:** `store/rows.rs:66` reads an unknown protocol as `Openai`, and `Protocol::parse` is a
  string match (`config/routes.rs:30-37`), so the compiler does not flag a missing arm. Also
  `web/api.rs:1195` (create default) and the UI's new-row default (`upstreams.rs:160`).
- **Strings:** `ops/routing.rs:44`/`:51` (`openai|anthropic|gemini`); the MCP `supports_responses`
  doc (`mcp/selfadmin/catalog/routing.rs:64-70`) carries the stale "llama-server has no such
  endpoint".

`Protocol::speaks_openai_http()` (true for `Openai` and `LlamaCpp`) answers the "OpenAI-shaped HTTP
surface" question. Each site picks it or `LlamaCpp` explicitly, and the WP2a commit lists every
`Protocol::Openai` in `src/` with its choice. A test round-trips every variant through `as_str`,
`parse` and serde. The compiler flags the exhaustive matches: `catalog.rs:319`/`:369`,
`web/admin.rs:19`, `capabilities/mod.rs:920`, `capabilities/notes.rs:667`, `proxy/count.rs:326`,
`proxy/count_messages.rs:158`, `proxy/reasoning_fit.rs:105-113`, `reasoning_fit/rule.rs:45-61`,
`reasoning_fit/refusal.rs:104`, `proxy/recording.rs:566`, `web/chat_sampling.rs:85`,
`web/chat_turn.rs:794` and `egress/mod.rs:133`.

## 2. The shared module

**WP1, a pure move of whole functions.** `egress/openai_wire.rs` is the root and re-exports its
children:
- `messages.rs`: `messages_json`, `user_content_json`, the default tool-result rendering;
- `body.rs`: `has_reasoning_object`, the OpenRouter object reconcile;
- `decode.rs`: `OpenaiDecoder`, `reasoning_text`, `parse_usage`, `parse_timings`;
- `aux.rs`: the embeddings and rerank builders and parsers, the generic error mapping.

Trait methods, `chat_body`, `apply_reasoning`, `reconcile_reasoning_passthrough` and the count plan
stay in `openai.rs` and call them. `scripts/move-check.py <WP0> <WP1>` must show only `mod` and
`super::openai_wire::` path residuals; the suite and the WP0 corpus (§9.1) pass with nothing
re-blessed. `egress/openai/tests.rs` (1-48) moves with the decoder.

**WP1b, the seams.** The body builder takes a reasoning step and a tool-result renderer as
arguments. Its order is kept: modelled fields, then reasoning, then the passthrough loop, then
reconcile (`openai.rs:390-399`; the loop only fills keys that are absent). The trait methods'
bodies become shared functions. This is not a move; the WP0 corpus proves it byte for byte.

## 3. The llama.cpp egress

### 3.1 What it is

`egress/llama_cpp.rs` holds `LlamaCppEgress` and implements the whole `Egress` trait on top of
`openai_wire`. Its children are `reasoning.rs` (today's llama half of the reasoning control and of
the reconcile, unchanged), `errors.rs` (`ExceedContext`, `parse_exceed_context`), `count.rs`
(`/tokenize`, `/apply-template`, `tokenize_request`), `props.rs` (§4), `tool_results.rs` (§8) and
`tests.rs`.
- `build_chat` posts the shared body plus the llama reasoning to `{base}/chat/completions` with the
  bearer (managed rows set none). `pub fn chat_body(ir, model, params, stream, &Upstream)` is the
  body the gate counts.
- `map_error` maps `exceed_context_size_error` first and then falls back to the shared mapping.
- The count is `/tokenize {model, content}`, as today. Parsing, decoding, embeddings and rerank are
  the shared ones.

After WP2c the OpenAI egress no longer imports `UpstreamKind`. `apply_reasoning` keeps the generic
half, the count plan is tiktoken only, and the module doc says "OpenAI and OpenAI-compatible
providers".

### 3.2 Facts and the frozen decision travel on the route

`Upstream` gains `llama: Option<Arc<LlamaRoute>>` (`#[serde(skip)]`, runtime only), with
`LlamaRoute {facts: Arc<LlamaFacts>, tool_images: ToolImages}`. It is `None` on every non-llama
route and while nothing is known.
- **Decided once.** `gate::fit_chat` (`gate/fit.rs:339-352`, before the unguarded early return)
  decides it for every chat send: `/v1/chat/completions`, `/v1/messages`, in-process turns, the
  Chat. It uses the hold's entry facts (managed) or the external cache (§4.2), plus §8.2's
  predicate. The count paths that build a body without a send (`proxy/count_messages.rs:255`) call
  the same function.
- **Never refilled mid-request.** One `LocalHold` helper becomes the only writer of a held
  endpoint. It rewrites `base_url` and copies `llama` unchanged. Today nine sites write the
  endpoint: `vram/local_hold.rs:565-569`, `gate/open.rs:722-723`, `gate/candidate/walk.rs:658-659`,
  `gate/send.rs:458-460`, `quickdoc/ingest.rs:440`, `quickdoc/golden.rs:318`,
  `knowledge/sections.rs:307`, `realtime/warm/load.rs:126` and `proxy/audio.rs:494`. So a climb
  (`gate/send.rs:423-437`, its send in `pair` at `:452-462`) and a dead-container retry
  (`gate/send.rs:406-422`, `local_hold.rs:501`) send what was counted.
- **Rechecked per attempt, only downward** (review R3). A climb or a retry lands on another
  container, and a retry's is re-admitted from the row as it is now. Before each attempt, and
  before the count that attempt is judged by (the ladder's `pair`, the pool count's retry,
  `count_tokens`' retry), `gate::tool_images::recheck` holds a decision that lets tool images go
  against that container's facts, advisory and projector: where it no longer sees, carries an
  advisory or reads no projector, the images go as that placeholder; where it says nothing, as
  today's bytes; webp only where both containers decode it. It never lets more go. This is the
  one deliberate case where a send posts other than what was counted, and it posts less (a
  placeholder of about 25 tokens where the count held the image's bound).
- **A candidate re-pick** lands on another model and is fitted for it, so it decides once for
  itself and never inherits another model's yes.

### 3.3 Intended changes

| | Change | WP |
|---|---|---|
| I2 | The OpenAI egress stops rewriting a passthrough `thinking_budget_tokens`; it goes verbatim like any unknown key | WP3, recommended (Q2) |
| I3 | The llama egress sends the resolved budget under both names. Official builds read `reasoning_budget_tokens` first (`server-common.cpp:1390-1391`); ik reads only `thinking_budget_tokens` (§6), so lmgw's budget has never reached ik | WP3, recommended (Q2) |
| I1 | The OpenAI side would stop reading `timings`. It costs a `generic` row in front of llama-server or llama-swap its speeds, for no gain | not recommended (Q2) |
| I4 | `request_logs.egress_proto` says `llama_cpp` for llama rows (`proxy/recording.rs:216`, `:962`), as shown on the Traffic page | WP2a |

## 4. `/props`

### 4.1 What the server says

| | official (0c6a6a7) | ik_llama.cpp (7ff619c) |
|---|---|---|
| handler | `server-context.cpp:4610-4655`, `:4815-4825` | `examples/server/server.cpp:1063-1102` |
| `modalities` | `vision`, `audio`, `video` | `vision`, `audio` |
| `chat_template_caps` | 9 booleans (`common/jinja/caps.cpp:87-98`) | `{}` when read live (`bench/facts.rs:5-12`) |
| context | `default_generation_settings.n_ctx` (per slot) | that, plus a top-level `n_ctx` (total) |
| `build_info` | `b<number>-<commit>` (`common/build-info.cpp.in:27-30`) | absent |
| router mode | without `?model=`: `role: "router"` and no model facts (`server-models.cpp:1931-1953`) | none |

Only `/health` is public (`server-http.cpp:251-258`), so `/props` needs the key when the server has
one. A sleeping server answers from a cache (`server-context.cpp:4818-4820`). One reader,
`egress/llama_cpp/props.rs`, reads `LlamaFacts {vision, audio, video, caps, n_ctx_slot, build_info,
raw}` in both shapes. Every field is optional, and a missing one is unknown (decision 14).

### 4.2 When it is asked

- **Managed containers:** once per start and climb, right after `/health` answers. `await_ready`
  (`runtime/registry/acquire.rs:503-515`) gains a llama branch, as the image class already reads
  its capabilities there (`:547-560`). The probe is bounded by what is left of
  `vram.load_timeout_seconds`, the readiness budget.
  - **Adoption:** `Registry::adopt` (`runtime/registry/reconcile.rs:353-530`), shared by boot and
    the re-adopt passes, probes after its `probe_once` (`:435`) and before the insert (`:488`),
    under the same bound, so a pass cut short loses nothing.
  - **Storage:** the facts are kept on the registry entry beside `capabilities`
    (`runtime/registry/state.rs:119-122`), together with the started row's projector ubatch
    advisory (§8.2), and dropped with the entry. A restart, climb, image update or adoption reads
    them again. A failed read is an entry warning (`state.rs:113-118`), never a failed start. The
    local model test probes live and refreshes the entry, so a failed read does not last for the
    container's life, and a live read that fails leaves facts read before on it (a log warning); this also drops the test's hard-coded 10 s (`modelinfo.rs:3342`).
- **External `llama_cpp` rows:** in the background, one probe per row at a time, keyed by (row,
  `base_url`). A llama-server that is no router serves one model whatever a request names, so its
  facts are the server's: one key however many names an `expose_all` row passes through. Only under
  a router's answer is a model a key of its own, (row, `base_url`, model). The body is not kept:
  the resolver and the surfaces read the build, modalities, caps and slot context only.
  - **First use:** the first request goes out with unknown facts (decision 14) and does not wait.
  - **The request:** `GET <root>/props` with the row's bearer and headers, as `tokenize_request`
    builds them, bounded by the row's own `request_timeout()` (0 means none, as for its requests).
  - **Router mode:** a `role: "router"` answer is kept, and each model is asked as
    `?model=<m>&autoload=false` (`server-models.cpp:1878-1885`, `:1966-1968`), which never loads a
    model. "Model is not loaded" (`:1871-1873`), a 5xx (llama-server's 503 "Loading model" until a
    model is ready, a reverse proxy's 502/504) and a probe that reached nothing are never cached.
    A server that answered otherwise (a 404 from an old build, a 401) is cached as unknown together
    with its answer.
  - **Invalidation, four events** — each also aborts the row's probe in flight, so a server that
    accepts and never answers (under `timeout_ms = 0`, no deadline) holds the row only until the
    next one:
    - an edit (where `state.catalog.invalidate` runs: `ops/routing.rs:74`/`:117`/`:134`,
      `web/api.rs:1215`/`:1276`);
    - a transport failure on the row;
    - its media refusals (`server-common.cpp:1240`, `:1253`);
    - the row's Test button (`web/admin.rs:17`), which then asks at once — a row no request has
      asked yet too: its server, and a router about the models the row's aliases name.
- *Rejected:* a probe per request, a round trip for facts that change only when the process does.

**Shown:** the container view and `lmgw__container` show `build_info`, the modalities and the slot
context, or the warning. The Upstreams page and `lmgw__upstreams` show an external row's facts per
model, when they were read, or why they are unknown.

## 5. The protocol and the migration

- `Protocol::LlamaCpp`, serialized as `llama_cpp`; `for_protocol` maps it to `LLAMA_CPP`. The
  router and aux synthetic upstreams take it (`snapshot.rs:477` becomes a parameter); audio and
  image stay `Openai`.
- **Migration 0058** follows SQLite's twelve-step table rebuild. It is `-- no-transaction`, with
  `PRAGMA foreign_keys = OFF` before and `ON` after a `BEGIN … COMMIT` that holds the rebuild, so a
  crash leaves either the old table or the new one, never neither.
  - **New CHECKs:** the protocol CHECK gains `llama_cpp`, plus
    `CHECK (protocol <> 'llama_cpp' OR (kind = 'llama_server' AND supports_responses = 0))` and
    `CHECK (kind <> 'llama_server' OR protocol IN ('llama_cpp','anthropic'))`.
  - **The copy:** `INSERT … SELECT` keeps every column (`supports_responses` from 0015 included)
    and every id. Its `CASE`s are idempotent: `openai`+`llama_server` → `llama_cpp` with
    `supports_responses = 0`, and `gemini`+`llama_server` → kind `generic` (decision 12). A replay
    after a commit that sqlx did not record maps nothing twice.
  - **The id high-water mark:** the old `sqlite_sequence` value is carried over to the new table
    before the rename. Otherwise a deleted top id could be handed out again, and prices
    (`'<upstream_id>:<model>'`, `migrations/0026_prices.sql:19`) and usage rollups
    (`0028_usage_rollup.sql:19`), which `delete_upstream` (`store/upstreams.rs:124-130`) cleans
    neither of, would attach to the new row.
  - **The FK check runs in Rust:** `store::run_migrations` (`store.rs:85-89`) runs
    `PRAGMA foreign_key_check` after the migrator and refuses to start with the offending rows
    named. Inside the file under sqlx it would only return rows.
  - **The notice:** a pre-migration notice in the 0055 style (`store/migration_guards.rs:1-8`)
    names every row decision 12 changes and every `supports_responses` it clears.
- **`settle(patch.protocol, patch.kind, current)`** decides from what the caller sent, not from the
  merged row. Both write paths call it: `ops/routing.rs:48-56`/`:88-95` and
  `web/api.rs:1169-1180`. The dashboard path today parses the kind with `UpstreamKind::parse`, so
  `sd_cpp` reaches a raw CHECK failure, which `ops/common.rs:245-253` refuses properly. `settle`
  also refuses `supports_responses` on a `llama_cpp` row. Its rules:

  | protocol sent | kind sent | result |
  |---|---|---|
  | `llama_cpp` | none or `llama_server` | `llama_cpp` + `llama_server` |
  | `llama_cpp` | anything else | refused, by name |
  | `openai` | `llama_server` | `llama_cpp` (the old spelling), said in the response |
  | `anthropic` | `llama_server` | as sent |
  | `gemini` | `llama_server` | refused |
  | `openai`/`gemini` | none, current kind `llama_server` | kind `generic` (leaving llama) |
  | `anthropic` | none, current kind `llama_server` | kind kept |
  | none | `generic`/`audio_cpp`, current `llama_cpp` | refused: change the protocol |
  | none | `llama_server`, current protocol `openai`/`gemini` | as the `openai`/`gemini` rows above |
  | anything else | | as sent, against the CHECKs |

- **The UI** (`lmgw-ui/src/pages/upstreams.rs:475-494`, `:521-535`) offers the protocol "llama.cpp
  (llama-server, ik_llama.cpp)". It hides the kind select for that protocol and resets the kind to
  `generic` when the protocol leaves `llama_cpp`. The UI always posts `kind` (`:526`), so without
  the reset a switch away would come back as the old spelling. The MCP schema
  (`mcp/selfadmin/catalog/routing.rs:30-44`, `:64-70`), the `ops/routing.rs:44`/`:51` strings and
  the `lmgw-api-types/src/upstreams.rs:20-23` doc gain `llama_cpp`.
- **Tests** (`tests/it/migrations.rs`, `db_at_version(57)`): every row shape; the CHECK refusals; ids,
  aliases and the sequence kept (insert after deleting the top id); a crash mid-rebuild and a
  replay after commit; the FK check; the notice. `settle` row by row on both paths, and the serde
  round-trip.

## 6. ik_llama.cpp

ik uses the same protocol and egress, with no kind of its own. Everything below was read at the
pool's ik `main` 7ff619c, in `examples/server/`:
- **`/props`:** as in §4.1.
- **Images:** `image_url` parts are taken in any role (`server-common.cpp:721-773`).
- **Audio:** `input_audio` needs a `format` of `wav` or `mp3` (`:783-785`), which lmgw always
  sends.
- **Reasoning:** the `enable_thinking` kwarg is honoured (`:828-838`), and `reasoning_effort` is
  read nowhere in its server.
- **The budget:** read only as `thinking_budget_tokens`, and only while the server's own default
  is -1 (`:907-909`), hence I3.
- **Context refusal:** the same `exceed_context_size_error` (`:1106`).

Probing needs no ik code: one tolerant reader, and nothing keyed on `build_info`, which ik does not
send. So lmgw cannot tell the two engines apart and does not need to. ik sends no `video`, so a webp
tool image is never sent to it (§8.2). Tool-result images are tested against fixtures in ik's shape
and not probed live (decision 4).

## 7. Reasoning controls across builds

On image `official-67672dc5b`, per-request `reasoning_effort: "none"` and budgets were ignored, and
only `chat_template_kwargs.enable_thinking` switched thinking. The code follows that today: off is
the kwarg alone and never `"none"` (`openai.rs:206-224`), and a passthrough level is stripped
(`:270-272`). At 0c6a6a7 `"none"` works (`server-common.cpp:1348-1353`), but the kwarg still
switches both ways (`:1338-1346`), on ik as well. So the llama egress keeps the kwarg as the only
switch on every build. Keying on `build_info` would need a table of builds that is never finished,
the kind of probing decision 4 rules out; `build_info` is shown for diagnosis only. Later,
`chat_template_caps.supports_reasoning_effort` could stop a level from reaching a template that
raises on one (`openai.rs:211-212`).

## 8. Tool-result images (the first feature)

### 8.1 Rendering

When the frozen decision allows it, a tool result with at least one sendable `Image` block goes as
a `content` array:
- a sendable `Image` becomes
  `{"type":"image_url","image_url":{"url":"data:<mime>;base64,<data>"}}`;
- an image that is not sendable becomes its placeholder, naming why;
- `Text` becomes a text part, and `Json` its serialization;
- a `Resource` becomes its text, or `[<mime> resource: <uri>]`;
- `Audio` stays today's placeholder (audio tool results are later).

A result with nothing sendable stays today's string byte for byte. llama-server's own converter
collapses text-only results the same way (`server-chat.cpp:499-505`). It joins text parts with
newlines and sets each marker in without one (`common/chat.cpp:197-220`), so one part per block
renders as today's text does, with the image in its place.

### 8.2 The predicate, decided once

Tool images arrive unasked, so `tool_images` holds only when all of these do:
- **vision:** the route's facts say `vision: true`;
- **managed rows:** the started row has no projector ubatch advisory (`modelinfo.rs:594-620`). A
  non-causal projector (Gemma 4) aborts llama-server on an image larger than the ubatch, which kills
  every request in flight. A projector lmgw cannot read counts as unknown attention: one named but
  not in the models dir gets the advisory an unknown projector gets, and one the row loads where
  lmgw never sees it (`--mmproj-url`/`-mmu`, the projector `-hf` fetches by itself) refuses unless
  batch and ubatch both reach an unknown projector's floor (Gemma 4's measured image ubatch, or the
  row's own larger `--image-max-tokens`; "lmgw cannot read the projector it loads, …");
- **guarded rows** (shared pool, ladder): the per-image bound is known (`gate/fit.rs:367-377`);
- **candidate aliases:** the alias has the Vision facet enabled — on every route the request takes:
  a candidate's hold names the alias, and a route without one (the alias fallback under the GPU
  hold, a climb's or an outside-VRAM verdict's fallback) is judged by the alias the request names.

Each image then has to pass a format check: png, jpeg, gif or bmp, which stb_image decodes
(`tools/mtmd/mtmd-helper.cpp:409-415`). webp passes only when `/props` says `video: true`. webp
decodes only through ffmpeg in a `MTMD_VIDEO` build (`:419-429`), and `video` is exactly
`mtmd_helper_support_video` (`:509-516`). An SVG would fail the whole request with a 500.
The declared mime and the magic numbers have to agree, and the base64 is decoded whole and the
file walked the way stb_image v2.30 reads it (`egress/llama_cpp/tool_results/stb*`, read at
b062ba735): a truncated file, a 12-bit, lossless or arithmetic-coded JPEG, an RLE BMP or a GIF
without its trailer is its placeholder, naming the check. The walk is structural; a whole file
whose deflate, Huffman or LZW data is corrupt still fails at the server.

Anything that fails becomes the placeholder, with a reason naming the failed condition ("this
model's server has no vision", "its projector can abort above the batch size", "no per-image
bound", "svg is not a format llama.cpp decodes") and a WARN. Nothing is refused. *Cost, stated:*
an image costs the row's per-image tokens, about 1.1k on Gemma 4 (`modelinfo.rs:237-239`), against
about 25 for the placeholder, and it is re-sent on every loop iteration while it stays in the
history.

*Changed 2026-10-06:* the same placeholder shape (`ir::image_placeholder`) also stands in for
every image a request carries, a user message's included, to a configured fallback whose exposed
capabilities say `vision: false` (the owner's ruling: a configured fallback is always used, with
no exception by content; `gate::fallback_images`). Its reason names no model, since the text goes
to a provider: "omitted: the answering model cannot see images". It is decided once per send at
the top of `fit_chat`, before this predicate, so a blind fallback's tool images are already text
here. Unknown vision follows decision 14: the images go as they are.

**Counting.** `gate::media_parts` (`gate/count.rs:67-79`) stays the count of user media. It runs
before any route exists (`gate/candidate/uses.rs:48`, `web/agentchat.rs:639`,
`web/chat_turn/out.rs:62`), and tool images never make a request need Vision. A new
`gate::tool_media(ir)` counts the tool images that pass the format check. The fit
(`gate/fit.rs:368`) and `count_on_template` (`proxy/count_messages.rs:199`) add it only when the
frozen decision holds, at the same per-image bound as a user image. `without_media`
(`proxy/count_messages.rs:286-296`) renders them as placeholders when it leaves images out.
The count is never lowered afterwards: an attempt whose container the recheck finds unable to
take the image (§3.2) posts the placeholder against the image's reserved bound, so posted ≤
counted, and that is the only place the two differ.

**Limits: two paths the predicate does not cover.** The guarantee is the llama.cpp egress's: a
tool image reaches a llama-server only where this predicate and the format check let it, and only
`llama_cpp` rows go through that egress.
- An `anthropic` + `llama_server` row (decision 11) speaks llama-server's own `/v1/messages`. The
  Anthropic egress sends a `tool_result` image natively, and llama-server converts it itself, with
  no vision, ubatch or format check by lmgw: a server without vision, a projector that aborts above
  the ubatch, or a file stb_image cannot decode fails or aborts there as it would for any client.
- Native `/v1/responses` passthrough (`supports_responses`, `responses.rs:125`) forwards the client's
  body verbatim. It is closed for `llama_cpp` rows (decision 19), but a row of another protocol in
  front of a llama-server (a `generic` row on llama-swap) forwards `input_image` tool outputs to
  llama-server's own Responses converter unchecked.
Both are the owner's explicit choice of wire; moving such a row to `llama_cpp` brings it under the
predicate.

### 8.3 Where tool images come from, and two ingress fixes (WP5)

Tool images come from MCP results (`mcp/exec.rs:928-950`: the Chat's tool loop, and server-side MCP
on `/v1/responses` and `/v1/realtime`) and from Anthropic `tool_result` image blocks
(`ingress/anthropic.rs:328`); one whose source is a URL is the resource naming it, as a Responses
URL image is, not an image without bytes (review R3). WP5 also fixes two ingresses:
- The Responses ingress turns an array `output` into its JSON text (`ingress/responses.rs:460-462`),
  so the base64 of an `input_image` lands in the prompt. It now parses `input_text` and
  `input_image` items into blocks, as llama-server's own converter does (`server-chat.cpp:204-229`).
  A base64 image becomes an image block only as png, jpeg, gif or webp whose bytes are that format,
  with the format's lowercase mime: what Anthropic's `media_type` takes, so an Anthropic upstream
  never gets a block it refuses with a 400 where the item used to go as text (review R3). An SVG,
  a BMP, a mislabelled image or base64 that does not decode stays its JSON text, with the WARN.
- The OpenAI ingress keeps only the text of a tool message (`ingress/openai.rs:239-242`,
  `:321-331`), as OpenAI's API defines it, but it now WARNs about every part it drops.

### 8.4 The live probe (WP5)

The probe runs on a dev copy (`scripts/dev-copy.sh`) with the copy's local vision row (a Gemma 4 or
Qwen VL row with its projector and no ubatch advisory), driven by a committed
`scripts/tool-image-check.py`.
- **Image:** the driver draws each run's PNG itself, with the standard library only: a random
  six-digit number and a coloured shape.
- **Request:** `/v1/messages` with an assistant `tool_use` and a user `tool_result` that carries
  the image, at temperature 0, asking what the tool's image shows. The same image sent as a user
  image is the baseline.
- **Runs:** ≥5 against the build before WP5 and ≥5 against WP5.
- **Pass:** before, the model answers from the placeholder and cannot read the number. With WP5 it
  reads the number as well as from the baseline, and its `prompt_tokens` are within a few of the
  baseline's (the projector ran).
- **Placeholders:** an SVG tool image and a guarded row without a bound each still get the
  placeholder with its reason.
- **Rules:** generated images only, no cloud model, the GPU hold is never switched, and the numbers
  go to the owner. The first answer with a non-empty `x-lmgw-fallback` header ends the probe: a
  fallback alias answered, possibly a cloud model, and it is never asked again.

## 9. Tests

### 9.1 The egress corpus (WP0, before any change)

`tests/it/egress_golden.rs` writes `tests/fixtures/egress/<case>.json`. Each file holds:
- the request's method, URL and header names (keys as `<set>`);
- the body as its exact serialized string (`serde_json::Map` is a `BTreeMap` here, so the bytes are
  deterministic);
- the count plan, `map_error` on recorded error bodies, and the deltas decoded from recorded SSE.

Cases name their upstream abstractly (`llama`, `generic`), so the files survive the protocol
switch; `LMGW_BLESS=1` rewrites them (`chat_golden.rs:1-13`). **The matrix**, on both upstreams
where it applies:
- text, system and history, tools and every `tool_choice`;
- tool results as text, JSON, resource, image (png and svg) and audio;
- reasoning replay, user image as URL and as base64, user audio;
- the reasoning control: off, on, effort, budget, both, off with a passthrough level, kwargs
  deep-merge, the OpenRouter object, a passthrough `thinking_budget_tokens`;
- every sampler field, stop, seed, stream on and off;
- passthrough `grammar`, `response_format`, `n_predict`, `timings_per_token`,
  `continue_final_message`;
- embeddings with and without `dimensions`, rerank with `top_n`, the count plan,
  `tokenize_request`;
- the context refusal and other 400s;
- a llama stream with `reasoning_content`, tool calls and `timings`, an OpenRouter stream, and a
  whole completion with `timings`.

### 9.2 The route corpus (WP0)

The upstream body of every route, on a managed row (`gpu_world`) and an external llama row
(wiremock):
- `/v1/chat/completions` (stream, whole, tools), `/v1/messages`, `/v1/responses` (synthesized);
- the Chat's send, Continue and tool thread;
- `/v1/embeddings`, `/v1/rerank`, `/v1/count_tokens`, `/v1/messages/count_tokens`, `/tokenize`;
- `/v1/completions`, for the mapping of the context refusal.

The existing goldens keep guarding:
- `fixtures/chat_sse/*.sse` (`chat_golden.rs:349`, `:708` on llama rows);
- `fixtures/chat_requests/managed_off.json`, `page_and_voice.json` and `voice_tools.json`;
- the counted-equals-posted checks (`vram_admission/request_gate.rs:599-623`,
  `ladder_http.rs:326-360`).

### 9.3 Units and ITs

- The props reader on both shapes: a 0c6a6a7 body, ik's from `support/llama_fake.rs:196-205`, and
  a router body.
- The probe at ready and at adoption with `gpu_world`'s container (`support/gpu_world.rs:316-345`)
  serving `/props`, and not serving it: facts unknown, a warning, and the start still succeeds.
- The external cache: background probing, the router re-probe, never caching "not loaded", the
  four invalidations.
- The predicate condition by condition, `tool_media`, and placeholders with reasons.
- **The frozen decision:** a ladder climb, a dead-container retry and a candidate re-pick, each
  posting exactly what it counted.
- An MCP tool loop end to end: `support/mcp_stub.rs` returns an image to a `gpu_world` vision row.
- Both ingress fixes, the migration and `settle` (§5).

## 10. Work packages (build order)

Each WP ends green on `bash ci/check.sh`, is committed with explicit paths, and is green against the
corpora. "Re-blessed" means a §9.1/§9.2 fixture rewritten with `LMGW_BLESS=1`.

| WP | Delivers | Proven by |
|---|---|---|
| WP0 | §9.1 and §9.2 corpora captured on `main`; no source change | the corpora pass on `main` |
| WP1 | `egress/openai_wire/`, whole functions moved | move-check: only `mod` and path residuals; nothing re-blessed |
| WP1b | the seams: reasoning step, tool-result renderer, shared method bodies | nothing re-blessed |
| WP2a | `Protocol::LlamaCpp` and its serde, `LlamaCppEgress`, the synthetic upstreams, the count's wire builders moved (§1.2), the §1.4 audit, I4. Stored `openai`+`llama_server` rows still take the unchanged OpenAI egress, so both paths run in one build | each llama case renders the same through both paths; nothing re-blessed except I4's log value |
| WP2b | migration 0058 and its notice, the Rust FK check, `settle`, UI, MCP and API surfaces | §5's tests; nothing re-blessed (tests that build a llama upstream as `Openai`+`LlamaServer`, 19 files, change that spelling, not their expectations) |
| WP2c | the OpenAI egress without kind branches | no `UpstreamKind` in `egress/openai*`; nothing re-blessed |
| WP3 | I2 and I3 as confirmed (Q2), one commit each | each re-blesses only its own cases |
| WP4 | `LlamaFacts`, the probes at ready and adoption, the external background cache, `Upstream.llama`, the single endpoint writer, the surfaces | §9.3's props tests; nothing re-blessed; a case shows that known facts with vision and no tool image give today's bytes |
| WP5 | the predicate and frozen decision, rendering, `tool_media`, `without_media`, both ingress fixes, the live probe | only the tool-image and ingress cases re-blessed; the frozen-decision and MCP ITs; §8.4's report |

## 11. Open questions for the owner

*Answered 2026-10-06: both as recommended. Q1 keep; Q2 I2 and I3, not I1.*

1. **Rerank on the OpenAI egress (recommended: keep).** Keep the Jina `/rerank` as one shared
   builder in `openai_wire::aux`, documented as a de-facto extension on OpenAI-protocol rows. vLLM,
   TEI, Infinity and Jina serve it on generic rows, and it is not a llama quirk. Confirm?
2. **The intended changes (recommended: I2 and I3, not I1).** Take I2 (stop rewriting a client's
   `thinking_budget_tokens` on cloud routes) and I3 (send both budget names, the only way a budget
   reaches ik). Drop I1: reading `timings` is harmless and serves generic rows in front of
   llama-server or llama-swap. Confirm?

## Later

- Tool-result audio as `input_audio` parts when `/props` says `audio` (the same predicate shape).
- `/props` caps and `n_ctx` in place of static derivations (reasoning levels, external rows'
  context).
- llama-server's own `/v1/chat/completions/input_tokens` (`server.cpp:282`) as a one-call count.
- Two stale comments: `config/snapshot.rs:492-494` and the MCP doc at `catalog/routing.rs:64-70`
  say llama-server has no `/v1/responses`, but 0c6a6a7 serves it (`server.cpp:264`). The MCP doc is
  fixed in WP2b; the synthetic upstreams' `supports_responses: false` stays a choice.

## Appendix: Review finding → resolution (2026-10-06)

Every finding was checked against the code at 7862c23 and the llama.cpp sources first. All hold,
except for two citation slips, noted where they occur.

| Finding | Resolution |
|---|---|
| 1 `settle` loses the caller's intent | Holds: the UI always posts `kind` (`upstreams.rs:526`), and an MCP update fills `cur.kind` (`ops/routing.rs:92-95`). `settle(patch.protocol, patch.kind, current)` with a rule table; the UI resets the kind to `generic` (§5) |
| 2 Migration replay, sequence, FK check | Holds: a crash between `DROP` and `RENAME` loses the table on replay; prices and rollups are keyed by `upstream_id`, and `delete_upstream` cleans neither; `foreign_key_check` only returns rows under sqlx. Fixed by the twelve-step rebuild in `BEGIN…COMMIT`, idempotent `CASE`s, the carried `sqlite_sequence`, and the check in Rust (§5) |
| 3 The kind matters | Holds (§1.3's list). `anthropic`+`llama_server` allowed by the CHECKs; only `openai`+`llama_server` is mapped, and `gemini`+`llama_server` (a wire llama-server does not serve) becomes `generic` and is named (decisions 11, 12) |
| 4 Ladders: counted ≠ posted | Holds. Decided once in `fit_chat`, carried on `Upstream.llama` through the climb and the dead-container retry by the single endpoint writer (§3.2); the v1 contradiction is gone. *Citation:* `vram/climb.rs:787-788` is the drain loop. The climb's re-send is `gate/send.rs:423-437`/`:452-462`, cited instead. *Narrowed:* a candidate re-pick lands on another model and decides once for itself at its own fit; carrying the first model's yes would send images to a server that may not decode them |
| 5 A conservative predicate | Holds (`modelinfo.rs:594-620`, `mtmd-helper.cpp:409-429`). Vision, no ubatch advisory, a known bound on guarded rows, Vision on candidate aliases, formats png, jpeg, gif and bmp; placeholder with a reason and a WARN; decision 18 replaced; cost stated (§8.2). *Refined:* webp only when `/props` says `video`, because its decode needs `MTMD_VIDEO` and ffmpeg (`mtmd-helper.cpp:419-429`, `:509-516`) |
| 6 `media_parts` has no upstream | Holds (`candidate/uses.rs:48`, `agentchat.rs:639`, `chat_turn/out.rs:62`). `media_parts` kept for user media; the new `tool_media` is added by the fit and `count_on_template` under the frozen decision; Vision on candidate aliases (§8.2) |
| 7 External `/props`: routers, llama-swap | Holds (`server-models.cpp:1871-1885`, `:1931-1968`). Key (row, `base_url`, model), router re-probe with `autoload=false`, "not loaded" never cached, background probe one per row, first request unknown, the local model test refreshes the entry (§4.2) |
| 8 Missed §1.4 sites, serde | Holds, all sites confirmed. Added to §1.4; explicit `#[serde(rename = "llama_cpp")]` and a round-trip test (decision 10) |
| 9 Decision 7's holes | Holds (`responses.rs:125`; nine endpoint writers). `supports_responses` refused on `llama_cpp` rows by CHECK, `settle` and migration (decision 19); one `LocalHold` helper is the only endpoint writer (§3.2) |
| 10 Ingress | Holds (`ingress/responses.rs:460-462`, `ingress/openai.rs:321-331`). Both fixed in WP5 (§8.3) |
| 11 `egress_proto` | Holds (`recording.rs:216`, `:962`). Listed as I4 (§3.3) |
| 12 Adoption | Holds. `Registry::adopt` probes after `probe_once` and before the insert, under its bound (§4.2). *Citation:* v1 cited `reconcile.rs:193`, the runtime view, not adoption |
| WPs | WP1 whole functions only, WP1b seams, WP2 split into 2a, 2b and 2c (§10) |
| Q1, Q2 | Rewritten as recommendations to confirm (§11); I1 not recommended, so `timings` stays shared (§1.1) |
