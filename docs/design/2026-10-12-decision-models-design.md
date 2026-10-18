# Decision models: `POST /v1/systemone` and a decision kind for llama.cpp rows

The owner reviewed this on 2026-10-12 and took every recommendation of §7; slice 1 (§6.1) was built
and accepted live on 2026-10-13. What its final review left to the owner is listed at the end of
§7; the owner decided those items on 2026-10-13, and they were built the same day. Slice 2
(§6.2) was approved on 2026-10-14 and its free part built the same day; its paid acceptance waits
for a funded OpenRouter account. The owner asked for this on 2026-10-03 and put it in the sprint after chat voice. A decision
model does not generate text. It answers typed questions about a `state` (pick one option, rate on a
scale, yes or no) with probabilities that the model's training calibrates. llama.cpp serves these
models since b11361 (ggml-org/llama.cpp#29818, 2026-10-02) on TypeSafe's "System One" wire, which
TypeSafe and OpenRouter serve for the closed model Jev. This design adds the route to lmgw, makes
decision GGUFs a third aux kind next to embed and rerank, and lets a cloud upstream answer the same
route. A CPU probe of Laya backs the wire claims about llama.cpp (§1.4, Appendix A); llama.cpp's
source backs the per-type facts the probe could not reach (§1.5).

**Today** (lines at `main` 9e5ca003):
- lmgw has no `/v1/systemone`. `CAPABILITY_TABLE` (`server.rs:199-228`) has no row, so a request
  is a 404.
- A decision GGUF is misclassified. `class_for` (`modelinfo.rs:168-180`) tests the classifier
  head before anything else. Laya, Julia-1 and Kev-4B carry `cls.output*` tensors and plan as
  **rerank** aux rows. lev, OpenJev and Clef plan as **chat** rows. The rerank argv
  (`--reranking`, `runtime/argv.rs:715-760`) crashes llama-server at load on Laya:
  `GGML_ASSERT(ggml_can_mul_mat(a, b))` in `build_pooling`, exit 139 (Appendix A.10).
- An aux row has no field for `--ubatch-size`, `--batch-size`, a projector or CPU placement
  (`config/models.rs:172-216`). All four ride in its freeform `args`. The aux plan writes a sibling
  projector there (`modelinfo.rs:2037-2068`), and the VRAM footprint ignores it
  (`vram/plan.rs:302-322` passes no companions).
- An aux row made CPU-only by `--n-gpu-layers 0` is charged no VRAM (`vram/plan.rs:307-321`). It
  is still paused by the GPU hold, because `Snapshot::placement` answers `Gpu` for every class but
  audio (`bench/lease.rs:111-121`). Unless the row overrides its run args, its container also gets
  the class's GPU passthrough (`config/settings_classes.rs:220-225`).
- The route needs a llama.cpp build at b11361 or later. The shipped class default is
  `ghcr.io/ggml-org/llama.cpp:server-cuda` (`config/settings_classes.rs:215`), whose tag moves, so
  an install's pulled copy may be older; §3.6's image check covers that. Two images were probed on
  the CPU and serve the route: a master build at b11549 (aa94f20) and
  `ghcr.io/ggml-org/llama.cpp:server` at b11561 (e937b17a2).

## Decisions

*Owner, 2026-10-03:*
1. **Decision models are core** and come in the sprint after chat voice.
2. **A class or a kind of their own**, detected from GGUF metadata where the file marks them, with a
   public prefix of their own like `embed/` (§3).
3. **A thin passthrough `/v1/systemone` through admission**: the GPU hold, the VRAM queue, usage
   rows and fallbacks (§2).
4. **API docs and MCP coverage, a `local_model_test` probe that asks one `choice` question, and a
   check that the image has the route** (§2.8, §3.6, §3.7).

*Owner, 2026-10-12* (the recommendations of §7, taken as written):
5. **OpenRouter waits for slice 2** (§7 Q1). Its catalog option and price sync come with slice 2;
   slice 1 forwards to an OpenAI-protocol upstream and prices it from a manual row (§6.1).
6. **Decision models are a third aux kind, `AuxKind::Decision`** (§7 Q2, §3.1). Not a class of their
   own, and not a flag on chat rows.
7. **The prefix is `decide/`** (§7 Q3), a second prefix on the aux class (§3.10).
8. **Existing aux rows' args are hoisted into the new fields automatically, with a start notice
   naming each row, after the measurement** (§7 Q4, §3.3, WP2b). *Refined by the owner on
   2026-10-12 after the measurement:* `--n-gpu-layers 0` becomes `backend: cpu` only on a row whose
   effective image has no GPU backend; a row on a CUDA image keeps GPU placement, and its notice
   names the trade-off with the measured numbers. The other four flags hoist as before.
9. **The protocol types live in `lmgw-api-types::systemone`, and the schema is generated from them**
   (§7 Q5). The README typed-answers paragraph is amended to say so (§2.8).
10. **llama.cpp's 500 for a prompt over the ubatch is relayed verbatim** (§7 Q6, §2.7).

*Taken in the draft, standing after the owner's review of 2026-10-12:*
11. **The wire is TypeSafe's System One as llama.cpp serves it.** OpenAI has no equivalent (§1.3).
    lmgw copies the wire: the client's object goes upstream with only `model` rewritten, and the
    answer comes back as the upstream sent it, `model` included (§2.1).
12. **`/v1/systemone` is the one decision mechanism.** No lmgw-own classify route. The idea of a
    classification API on the gateway is this route.
13. **Detection is `{arch}.decision.type` in the header**, the key llama.cpp itself reads
    (`common.cpp:1189-1199`). It is tested before the rerank and embed rules (§3.2).
14. **The aux row gains five real fields** for every aux kind: `ubatch_size`, `batch_size`,
    `mmproj_path`, `backend` (only `cpu`) and `threads` (§3.3). Both placement predicates, the row's
    and the descriptor's, read `backend`, so CPU aux rows serve under the hold, as audio CPU rows
    already do (Q4).
15. **Planner rules come from the header, not from a table of llama.cpp's types** (§3.4). A
    non-causal model gets `ubatch = batch = ctx`. A causal one with a decision head gets
    `ubatch = batch = 2048`, lmgw's choice. A causal one that reads label logits keeps llama.cpp's
    defaults. An overrun is llama.cpp's own visible error.
16. **A route check for decisions, and the reverse guard.** `/v1/systemone` refuses a route that
    cannot answer it, and the generation, embedding and rerank routes refuse a decision row, both
    before a container starts. The token counters stay open (§2.2).
17. **A decision row's fallback must be able to answer System One.** Checked when the fallback is
    set and on every request (§2.4).
18. **Upstream errors are relayed verbatim**, with two exceptions: a 401 or 403 is lmgw's credential
    and answers 502, as on `/tokenize`, and a 404 from a local decision row's container answers a
    501 that names its image. lmgw's own refusals use llama.cpp's error shape with `lmgw_code`
    (§2.7).
19. **Logged as class `aux` with the new ingress label `systemone`**, and priced per token on input.
    Output is priced at whatever the price row says, which is 0 for Jev (§2.6).
20. **`/v1/models` publishes the kind through the existing capabilities object**: task `decision`,
    endpoints `["/v1/systemone"]`, output modalities `["decisions"]`. There is no OpenRouter-style
    `architecture` object (§2.9).
21. **No new op, header or client method.** Existing ops gain the kind and the fields; the fallback
    headers' route list gains the route (§2.8).
22. **Cloud upstreams need no new flag.** The route forwards to `openai` and `llama_cpp` upstreams,
    as `/v1/rerank` does, and refuses `anthropic` and `gemini` ones (§4.1).

*Owner, 2026-10-13* (the open items of §7, all approved):
23. **A plan sets the ubatch for an encoder llama-server cannot split** (item a). Such an embed or
    rerank row gets `ubatch_size` = `batch_size` = its trained context, so one input is no longer
    bounded by the default 512.
24. **The aux footprint charges the compute buffer** (item b), derived from the header, and an aux
    row at `-ngl 0` with a GPU device is charged for it. Headroom covers what the formula does not.
25. **A plan that cannot fit the card says so and never lowers the context.** It warns with the
    estimate, the capacity and the largest context that fits, and keeps the trained one.
26. **`/v1/models` notes carry no host path** (item c).
27. **The plan-time probe is gone** (item d). The owner ruled on 2026-10-13 that the Test
    button (`local_model_test`) is the only thing in lmgw that loads a model on request. Plan and
    inspect read the header only, so the self-admin gate on the probe is moot.
28. **`Retry-After` is relayed** on `/v1/systemone` and `/tokenize` (item e).
29. **The model test keeps the option order** (item f).
30. **The Models page shows a licence badge** read from `general.license` (item g).
31. **`embed_model_set` is removed** (item h).

*Owner, 2026-10-14:*
32. **Slice 2 is approved** (§6.2), except its paid live acceptance, which needs a
    funded OpenRouter account.

*Taken in the slice 2 build (2026-10-14):*
33. **`catalog_all_modalities` is a per-upstream option, on by default**, because Gemini's
    OpenAI-compatible endpoint refuses `output_modalities=all` (§6.2's check). Migration 0080
    turns it off for an existing `openai` row on Gemini's host; a new row starts on, and a refusal
    names the option.
34. **OpenRouter's non-text outputs take the tasks lmgw has**: `rerank`, `speech` → `tts`,
    `transcription` → `asr` and `video` → `video_generation`, beside `decisions` and `image`, so
    the full list adds no false chat models (§6.2).
35. **A cloud catalog's image or video model publishes no route** (§6.2, from the slice 2
    review): OpenRouter documents `POST /images` and `POST /videos`, and lmgw's image routes post
    to `/images/generations`. This revises image-generation design §5; an alias's
    `capabilities_override` states the routes of a provider that serves them.
36. **Price sync leaves unpriced what its token fields cannot price** (§6.2, from the review):
    speech, transcription, image, video and rerank models, and 0/0 token prices beside a
    non-zero field no row takes. On an `expose_all` upstream an aliased model is priced under its
    upstream scope too, so its passthrough name prices.

## 1. Context

### 1.1 What a decision model is

A request carries a `state` (text or JSON) and a map of questions. There are three question types:
- **`choice`**: one of N named options (with optional descriptions), answered with the winner,
  a probability per option and a confidence.
- **`score`**: 2 to 10 ordered levels, answered with the expected level index (it can fall
  between levels), a legend, probabilities and a confidence.
- **`noul`**: yes or no, answered with P(yes).

The model reads its answer from a head or from label logits, never from sampled text. A question
costs one forward pass, or less where the state is shared (§1.5), and there are no output tokens to
wait for. On a CPU, Laya answers one question in 33 ms (Appendix A.9). The medians in ggml-org's
announcement (https://huggingface.co/blog/ggml-org/decision-models-in-llamacpp, an RTX PRO 6000)
run from 3 ms for Julia-1 and 5 ms for Laya to 12 ms for Kev-4B, 36 ms for lev and 43 ms for
OpenJev.

The probabilities are scaled by temperatures stored in the GGUF (`decision.temperature.*`). The
llama.cpp README says they are "not guaranteed to be calibrated for your data". Julia-1's header
has no temperature keys at all (Appendix B). A vague ticket scored 0.25 on Julia-1 and 0.80 on
Kev-4B, and option descriptions move accuracy a lot. lmgw relays these numbers and never adjusts
them.

### 1.2 The wire, three ways

| | TypeSafe (`api.typesafe.ai`) | OpenRouter | llama.cpp (b11549) |
|---|---|---|---|
| Route | `POST /v1/systemone` | `POST /api/v1/systemone` | `POST /v1/systemone`; `/systemone` is a 404 |
| Auth | Bearer | Bearer | none, or `--api-key` |
| `model` | required; `jev-latest`, `jev-preview`, `jev-1.13.0` | `typesafe/jev-1.13`, `~typesafe/jev-latest`; the TypeSafe SDK's bare ids are mapped onto `typesafe/` | optional; picks the model in router mode, ignored by a single-model server (A.6) |
| `state` | required: string, object or array | same | same; non-strings go to the model as JSON text; `null` only for lfm2-d1 and lfm2-d1-omni, a 400 on Laya (A.5) |
| `images`, `files` | none: "Text only … No image, audio, or video input" | not documented | data URLs (`files`, `images` is an alias); needs `--mmproj` and a capable type; 501 otherwise (A.6) |
| `choice` options | up to 255 | as TypeSafe | per type: 52 for openjev, 255 for the others the README lists; a 400 above (A.6) |
| `score` levels | 2 to 10 | as TypeSafe | 2 to 10, a 400 otherwise |
| `noul` criteria | optional `{true, false}` descriptions | as TypeSafe | the same |
| Streaming | none | none | none; `stream: true` is ignored (A.6) |
| Answer `model` | the versioned id that answered (`jev-1.13.0`) | dated id (`typesafe/jev-1.13-20260917`) | the server's `--alias` |
| Answer extras | header `x-typesafe-request-id` | `id`, `provider`, `usage.cost` (USD) | none |
| Every answer has `type` | yes | yes | yes |
| `usage` | `input_tokens`, `output_tokens` (non-zero in the examples, e.g. 20) | the same plus `cost` | `input_tokens`; `output_tokens` always 0 |
| Validation error | 422 | not documented | 400 |
| Other errors | 401, 429, 529 | the provider's own | 501 not a decision model or no image support; 500 when the prompt exceeds the ubatch (A.8) |
| Context | 64k per request; 32k for the state plus the longest question | the catalog says 64,000, the guide page 32,000 | the GGUF's `context_length`, and for some types the ubatch |
| Price | $0.042 per Mtok input; output free | prompt $0.000000042 per token, completion 0 | local |
| `GET /v1/models` | TypeSafe's own shape, lists the aliases | Jev only with `?output_modalities=all` (§4.2) | `architecture.output_modalities: ["decisions"]` |

Sources: https://docs.typesafe.ai/api.md and `/models.md`;
https://openrouter.ai/docs/guides/community/typesafe-sdk.md and `jev.md`; the llama-server README
("POST `/v1/systemone`: TypeSafe-compatible System One API", which says it "Follows the TypeSafe
API … Multimodal input is an extension"); OpenRouter's live `GET /api/v1/models`. All were read
2026-10-12.

### 1.3 Why OpenAI is not the reference

OpenAI has no route that takes typed questions and returns calibrated choices. The nearest things
are `/v1/moderations` (fixed categories), `logprobs` on chat (token-level, uncalibrated, and the
caller has to build the classifier) and the evals/RFT graders (offline). The protocol-fidelity rule
says an external protocol's route copies that protocol. For this capability the protocol is System
One, and its references are TypeSafe's API and llama.cpp's implementation of it. Where the two
differ (the status of a validation error, `images`), lmgw relays what the upstream it routed to
says (§2.7).

### 1.4 The probe

Laya Q8_0 ran on the CPU in a llama.cpp master build at b11549, and again in
`ghcr.io/ggml-org/llama.cpp:server` at b11561: no GPU device, `NVIDIA_VISIBLE_DEVICES=void`,
`-ngl 0`, 16 threads on a 16-core CPU. The run covered the four question shapes, the state
shapes, the validation and support errors, the other routes, the ubatch boundary, latency, today's
two aux argvs and an image from before the route. Appendix A has the trimmed JSON. The findings
that shape this design:
- **The server turns on embedding mode for the decision types that read embeddings, and forces
  `n_batch = n_ubatch`** ("decision model reads the embeddings output, enabling embedding mode";
  "setting n_batch = n_ubatch = 512"). Those servers cannot generate. Chat and `/completion`
  answer 500 "the current context does not support logits computation" (A.7).
- **On Laya the whole prompt must fit in one ubatch.** At the default 512, any request over 512
  tokens fails with a **500** `server_error`, not a 400 (A.8). With `--ubatch-size 8192 -b 8192`
  a 7,669-token state answers, in 14.9 s on the CPU. An 8,519-token state still fails, now naming
  "current batch size: 8192". 8192 is Laya's trained context.
- **`--reranking` crashes the server and `--embeddings` is harmless** (A.10). A decision row
  renders no kind flag.
- **An image from before the route does not answer 404.** It cannot load Laya at all: a tensor
  shape check fails and the container exits 1 (A.11).
- **CPU latency:** one question in 33 ms, four in 156 ms. The questions are separate passes on
  Laya (A.9).

### 1.5 The models

Read from the first 32 MB of each GGUF (Appendix B) and from llama.cpp's source
(`common/common.cpp:1291-1305`, `tools/server/server-decision.h:39-70`). Every llama.cpp line in
this document is at `master` as of 2026-10-12; `common.cpp` is under `common/`, the `server-*`
files under `tools/server/`.

| Model | Size | Licence | `arch` / `decision.type` | Attention, head | ctx (header) | Server mode | Today's lmgw class |
|---|---|---|---|---|---|---|---|
| Laya | 421M | Apache-2.0 | `modern-bert` / `laya` | non-causal; `cls.*` | 8192 | embedding mode; whole prompt in one ubatch | aux rerank (crashes) |
| Julia-1 | 144M | Apache-2.0 | `modern-bert` / `laya` | non-causal; `cls.*` | 8192 | the same | aux rerank |
| Kev-4B | 4.2B | Apache-2.0 | `qwen35` / `kev` | causal; `cls.output [2560, 512]`, no `output.weight` | 262144 | embedding mode; the state is shared across questions | aux rerank |
| lev | 4.2B | Apache-2.0 | `qwen35` / `lev` | causal; tied embeddings, no `cls` | 262144 | label logits; the state is shared | chat |
| OpenJev | 27B | CC BY-NC 4.0 | `qwen35` / `openjev`, plus `clip` mmproj `qwen3vl_merger` | causal; `output.weight` | 262144 | label logits; the state is shared; images | chat (projector found) |
| Clef | 27B | Apache-2.0 | `clef` / `clef`, plus `clip` mmproj `qwen3vl_merger` | causal plus a `dec.blk.*` cross-attention decoder; `decision.*` tensors | 262144 | embedding mode; joint (all questions in one prompt); whole prompt in one ubatch; images | chat (projector found) |

Every one of them carries `tokenizer.chat_template.systemone`. The server refuses to load a
decision model without it (`server-decision.cpp:46-49`). The four causal ones also ship an
ordinary chat template, so `has_chat_template` cannot tell them apart. llama.cpp's
`/v1/models` says `["decisions"]` for all eleven known types, including the label-logit types
whose context could still generate (`server-common.cpp:150-167`). ggml-org's "Decision models"
collection lists 16 repos: the six above, Clef-Flash, Bespoke-Nimble-9B-v3, LiquidAI d1-3B and
d1-omni-600M, and six vllm-d3 sizes. Licences are shown on the row and never filter or down-rank
anything.

## 2. The route `POST /v1/systemone`

### 2.1 Passthrough and model resolution

The new `proxy/systemone.rs` gets a `CAPABILITY_TABLE` row
`("POST", "/v1/systemone", Cap::Inference)`. The handler follows `/tokenize`
(`proxy/tokenize.rs`) and `/v1/rerank` (`proxy/embeddings.rs:361-439`):
1. Read the body as JSON whatever its `Content-Type`. The `/v1` group wrappers take
   `Json<Value>` (`server.rs:1653-1666`), which refuses a request without that header, so the
   route takes `Bytes` and reuses `/tokenize`'s `read_body` (`proxy/tokenize.rs:193-208`). It
   keeps the body limit's 413 and makes non-JSON a 400. A body that is not a JSON object is a 400.
2. `model` must be a string. lmgw routes by it, as TypeSafe requires it and llama.cpp's router
   mode needs it. Its absence is a 400 that says so. Nothing else is validated: the upstream
   validates its own wire, and a pre-check would answer 400 where TypeSafe answers 422.
3. `policy_refusal` (`proxy/recording.rs:421`): the key's alias scope and budgets, as for
   embeddings. A decision on Jev costs money, so budgets apply. `/tokenize`, a counter, skips them.
4. `gate::open(state, alias, RouteCheck::Decision)`: the candidate pick, the hold swap, the route
   check (§2.2) and admission. A candidate alias picks local chat rows, which the route check
   refuses by name. Its fallback may be a cloud alias, and the route check refuses that only on an
   `anthropic` or `gemini` upstream, or when the upstream's catalog states output modalities for
   the model without `decisions`. Any other cloud route is forwarded, and the upstream's 404
   relayed.
5. Build `POST {upstream.base}/systemone` with the upstream's bearer and extra headers. The body is
   the client's object with `model` set to the route's upstream model. The builder is a new child
   module `egress/openai_wire/systemone.rs`, beside the shared rerank builder
   (`egress/openai_wire/aux.rs:149`). The `openai` and `llama_cpp` protocols both use it. A local
   route goes through `vram::send_local`, as rerank does.
6. Relay status, content type, `Retry-After` and body as the upstream sent them (§2.7). The
   response is non-streaming. `Retry-After` is relayed by `proxy/relay.rs` (`RelayedHead`), shared
   with `/tokenize` (owner item e). Other response headers are not relayed, so TypeSafe's
   `x-typesafe-request-id` does not reach the client. Its SDK reads it as an optional
   `request_id`; relaying it is a later one-line change if a client needs it.

*As built (WP3):* steps 1 and 5 do not go through a `Value`. `serde_json::Map` is a `BTreeMap` in
this build, so a `Value` sorts an object's members, and llama.cpp reads a `choice` question's
`criteria` in the order sent: that order is the option order. The handler reuses the byte half of
`/tokenize`'s reader (`read_bytes`, with its 413) and parses the body once into
`SystemOneBody` (`egress/openai_wire/systemone.rs`): the top-level members in order, each value
kept as the raw bytes it came as. The upstream body is those members with only `model`'s value
replaced. Only the whitespace between top-level members changes. `/v1/messages` shares the chat
handler, and its `RouteCheck::Text` now names `/v1/messages`, not `/v1/chat/completions`, so a
refusal names the route the client called. A refusal by the shared `/v1` layers on this path is
logged as `systemone` too (`server.rs` `refusal_label`), as a realtime upgrade's says `realtime`.

`model` in the answer stays as the upstream wrote it. TypeSafe defines it as the versioned id that
answered and tells clients to log it. On chat routes lmgw writes the requested alias back. Here,
which alias answered is the `x-lmgw-fallback` header's job, and a client that sent `decide/laya`
reads `laya`, as a TypeSafe client that sent `jev-latest` reads `jev-1.13.0`.

### 2.2 The route check

A new `RouteCheck::Decision` (`gate/open.rs:55-86`, run at `:132-174`) answers whether the resolved
route can serve System One, before admission starts anything:
- **Local aux route:** the row's kind must be `Decision`. Otherwise the request is refused with
  a message that names the row and its kind, in llama.cpp's words ("is not a decision model").
  This is the mirror of `refuse_embedder_for_rerank` (`proxy/route_guards.rs:83-100`).
- **Local chat, audio or image route:** refused by name. `refuse_media_route`
  (`route_guards.rs:22-47`) covers audio and image; a chat row is never a decision model once
  §3.2 classifies them.
- **Cloud route:** `openai` and `llama_cpp` protocols go through, unless the upstream's cached
  catalog states `output_modalities` for the model and they lack `decisions`. The check reads the
  raw list, as `cloud_image_guard` does (`proxy/image.rs:134-174`), not the parsed task:
  `parse_task_openai` (`catalog.rs:860-873`) turns any stated list other than `[embedding]` or
  images-only into `chat`, so a `llama_cpp` upstream listing `["decisions"]` would read as chat.
  No stated list means the route goes through. Slice 1 also maps `decisions` to the task
  `decision` in `parse_task_openai`, so such an upstream publishes correctly; that needs no
  `?output_modalities=all` (§4.2). `anthropic` and `gemini` are refused: neither serves the wire.

These refusals answer **501** `not_supported_error`, the status llama.cpp gives the same refusal
(A.7, `server-context.cpp:5776-5778`). The check returns
`GatewayError::Refused { status: 501, code: "not_supported", .. }`, whose status is its own
(`error.rs:27-31`, `:367-369`). `Unsupported` would be a 400 (`error.rs:370-373`), and the handler
only sees the `GatewayError` the gate hands back, so the status has to come from the check. The
variant's doc comment, which names only the principal gate (`error.rs:22-25`), widens to "a refusal
whose status the refusing layer decides".

**The reverse guard.** The generation checks refuse a decision row by name, pointing at
`/v1/systemone`, with those routes' existing status (`Unsupported`, a 400, as
`refuse_media_route`). That covers `RouteCheck::Text` as used by `/v1/chat/completions` and
`/v1/messages` (`proxy/chat.rs:125`), `/v1/responses` (`responses.rs:96`) and realtime
(`realtime/responder.rs:390`), plus `LegacyCompletions` (`gate/open.rs:146-157`), `Embeddings`
and `Rerank`. The token counters use `RouteCheck::Text` too: `/tokenize` (`proxy/tokenize.rs:99`),
`/v1/count_tokens` (`proxy/count.rs:247`, `:312`) and `/v1/messages/count_tokens`
(`count_messages.rs:128`). They stay open. `/tokenize` works on Laya (A.7) and is how a client sizes
a state against the ubatch. So the three counters move to a new `RouteCheck::Count(&'static str)`
that runs only the media guard, and the decision guard is a match arm, not a string compare. On
Laya, chat is a 500 and embeddings a 400 (A.7), and starting a container for a request it cannot
answer is the thing `refuse_media_route` exists to prevent. The label-logit types (lev, OpenJev)
could perhaps still generate. llama.cpp publishes them as `decisions` only, and lmgw follows that.

*As built (WP3 review):* the decision guard reaches past the routes above.
- `RouteCheck::None`, the check of the in-process chat callers (the dashboard's chats, agent runs,
  MCP sampling, chat voice, a profile's Try), runs it, because it reads only the snapshot.
- Realtime's `is_chat_alias` runs it, so a session's model, `realtime.default_model` and
  `model_map` cannot name a decision row, and the background warm never starts one.
- The audio and tasks routes' `require_openai_audio` refuses every local aux row. An aux row speaks
  `llama_cpp`, which the protocol check had passed.

### 2.3 Admission, the hold, CPU rows

Nothing new. An aux decision row is a `Target` of class `Aux`. `resolve_for_request`
(`config/snapshot.rs:207-243`) swaps a held GPU row to its fallback or refuses with `gpu_hold`.
Admission charges the row's footprint, queues on VRAM, and the registry starts the container under
its lease. A row with `backend: cpu` (§3.3) is `Placement::Cpu`. It is charged nothing, never
evicted, and serves under the hold. That is the owner's reading of the hold as zero VRAM, and audio
CPU rows already work this way (`runtime/placement.rs`). The aux class's request timeout
(60 s, `config/settings_classes.rs:249`) bounds the request. The longest CPU answer measured,
one question on a 7,669-token state, took 14.9 s. Time scales with the question count on types
that do not share the state (Laya: four questions cost four passes, A.9), so four questions on
that state take about 60 s and reach the timeout. The timeout is the class's visible setting, and
the CPU plan's rationale names it (§3.4).

### 2.4 Aliases and fallbacks

Local rows are not aliasable (per-model containers §5). A cloud alias (`jev` → OpenRouter
`typesafe/jev-1.13`) is an ordinary alias. Candidate aliases are chat-only, and their candidate
lists never contain an aux row.

A decision row's GPU-hold fallback is the aux row's existing `hold_fallback_mode` and
`hold_fallback`. `Inherit` means "none" for the aux class (`config/snapshot.rs:799-805`), so a
decision row never inherits the global chat fallback. A configured fallback always applies, with no
content-based exception. Privacy means choosing a row without one. The fallback must answer System
One:
- **When it is set**, `aux_model_set` runs `validate_fallback_alias`
  (`ops/settings_patch.rs:142`) and, for a decision row, §2.2's predicate on the fallback's route. A
  fallback whose catalog states output modalities without `decisions` is refused with that reason.
  A fallback whose catalog states none is accepted.
- **On every request** the route check runs on the swapped route (`gate/open.rs:501`), so a fallback
  that changed since it was set fails by name, never by a confusing upstream 404.

*As built (WP4):* the save-time check is `ops/aux_model/fallback.rs`. It runs on create, and on an
update that changes the kind or the fallback. So a row turned into a decision row keeps no fallback
that cannot answer, while an enable, a disable or a rename needs no catalog fetch: the gate checks
the fallback on every request. The refusal is §2.2's own message under `hold_fallback:`.

### 2.5 Keys and auth

`Cap::Inference`, as `/v1/rerank`. The key's alias scope and budgets apply through
`policy_refusal`. Tool scope does not apply, because a System One request carries no tools. The
shared `/v1` layers answer their own 401, 403, 413 and 429, OpenAI-shaped, as they do for
`/tokenize` (`proxy/tokenize.rs:221-226`).

### 2.6 Request log, usage and price

`Unanswered::open` writes the 499 row if the client goes away. Every other outcome writes one row
through `record` with:
- `class: RequestClass::Aux` (`telemetry.rs:240-260`): an encoder-like call with input tokens and
  no generation.
- A new `ClientProto::SystemOne` (`ingress/mod.rs:13-34`), spelled `systemone`, so the Traffic
  page tells decisions from `openai` traffic. `ClientProto::Realtime` is the precedent.
- `prompt_tokens = usage.input_tokens` and `completion_tokens = usage.output_tokens`, as reported.
  The row is unpriced (NULL) when the upstream reported no usage.

Price follows the billable-units design unchanged: a `per_mtok` row. A local row costs a real 0
(free local). Jev's catalog price is $0.042 per Mtok in and 0 out (§4.4), so its non-zero
`output_tokens` cost nothing. OpenRouter's `usage.cost` is relayed to the client and not read.
lmgw prices from its own rows everywhere and has no mechanism for provider-reported cost; adding
one is out of scope here.

### 2.7 Errors

| Case | Status | Body |
|---|---|---|
| Upstream answered (2xx or error) | the upstream's | relayed verbatim, content type included |
| Upstream 401 or 403 | 502 | lmgw's credential, as `/tokenize` (`proxy/tokenize.rs:161-170`) |
| Body not a JSON object, `model` missing | 400 | lmgw's, llama.cpp shape |
| Unknown alias; key scope or budget | 404; 403 (`error.rs:362-400`) | lmgw's, llama.cpp shape |
| Key rate | 429 | raised by the shared auth layer before the handler (`policy.rs:377-400`), OpenAI-shaped (`server.rs:1440-1461`), as §2.5 says |
| §2.2 refusals | 501 | lmgw's, llama.cpp shape |
| Hold with no usable fallback | 503 `gpu_hold` | lmgw's, llama.cpp shape |
| A local decision row's container answers 404 on the route | 501 | lmgw's: the row, its image and that the image lacks `/v1/systemone` (§3.6) |

lmgw's own errors use `llama_error` (`proxy/tokenize.rs:227-240`):
`{"error": {"code", "message", "type", "lmgw_code"}}`. That is llama.cpp's envelope (A.6) plus
lmgw's code, so a client checking for `gpu_hold` still can. The probe's 500 for a prompt over the
ubatch is relayed as llama.cpp sent it (Q6).

*As built (WP3):* the 404 mapping and every §2.2 refusal carry `lmgw_code` `not_supported`. A
relayed upstream error still writes its row as a failure, with the upstream's message. `Refused`
with a 501 reads `invalid_request_error` in the OpenAI and Anthropic dialects, which had mapped
every status but 401 to `permission_error`.

### 2.8 Documentation, schema, MCP

- **Schema.** A `DocRoute` in `openapi/planes/inference.rs` under the `llamacpp` tag with
  `Dialect::LlamaCpp`, as `/tokenize` (`:246-266`). It gets `endpoints: [other]`, a new
  `model_task::DECISION` (`lmgw-api-types/src/openapi_ext.rs:187-195`), `writes: Some(false)` and
  an example. The request and response schemas come from protocol types in a new
  `lmgw-api-types::systemone` (Q5): `SystemOneRequest`, a `Question` tagged by `type`, an `Answer`
  tagged by `type`, and `SystemOneResponse` with `usage`. The types allow unknown fields, so
  OpenRouter's `id`, `provider` and `usage.cost` and llama.cpp's `images` are documented as
  extensions and pass. The relay never re-serializes through them; they describe and check, and
  lmgw's own callers (§3.7, §5) build and read with them.
  *As built (WP5):* every map in the types is an order-keeping `Ordered<V>` (a `Vec` of members,
  written as an object), never a `BTreeMap`, for §2.1's reason. A question's `instructions` is any
  JSON value, since TypeSafe takes a string, an object or an array. The Rust names are `Question`,
  `Answer` and `Usage` as above. Their schema components are `SystemOneQuestion`,
  `SystemOneAnswer` and `SystemOneUsage`, because `Usage` is already a component
  (`self_admin::usage`). The response is the component `SystemOneResponse`. The extensions are
  fields of their own: `images` and `files` on the request, and `id`, `provider` and `usage.cost`
  on the answer. Unknown fields still pass.
- **Drift tests.** In `tests/it/openapi_v1_wire.rs` style: a wiremock upstream answers with the
  Appendix A.4 body (llama.cpp) and with TypeSafe's and OpenRouter's documented examples. Each
  answer is read into `SystemOneResponse` and validated against the documented component
  (`validates_against_component_of`). *As built:* each answer is also written back and compared
  (`round_trips`), so a field the type drops fails. TypeSafe's three examples (one per type) and
  OpenRouter's are the ones their docs showed on 2026-10-12 (`tests/it/support/decision_world.rs`).
- **Headers.** No new `x-lmgw-*` header. The route joins `GATED_ROUTES`
  (`openapi/headers.rs:100`), the scope of `x-lmgw-fallback` and `x-lmgw-fallback-reason`.
- **No op, no `UNTYPED` entry, no ratchet change.** Inference routes have no ops. `aux_model_set`,
  `local_model_plan`, `local_model_test`, `local_model_get`, `gguf_files` and `models` gain the
  kind and the fields in their `OpDoc` args and descriptions (`openapi/ops/table.rs:44`;
  `tests/it/openapi_ops.rs:211` keeps names and arms in step). The self-admin instructions' aux
  paragraph gets one sentence on decision rows. The handler builds no JSON in `web/` or `ops/`, so
  `ci/json-ratchet.toml` is unaffected. The pre-slice `embed_model_set` op name, with
  `OpArgs::AliasOf`, the `x-lmgw-alias-of` extension and the API page's alias chip, is gone
  (owner item h).

### 2.9 `/v1/models`

lmgw's `/v1/models` carries the model-capabilities object (`lmgw-api-types/src/capabilities.rs:20`),
not an `architecture` object. A local decision row's capabilities come from `capabilities::for_aux`
(`capabilities/mod.rs:551-597`), whose exhaustive `AuxKind` match (`:564-573`) gains the arm:
- `task: "decision"` and `endpoints: ["/v1/systemone"]`;
- `input_modalities: ["text"]`, plus `"image"` when the row has a projector. This changes
  `for_aux`'s projector rule. Today it withholds the list when a projector is present (`:552-559`),
  because it reads no projector header. With `mmproj_path` a field, it reads the projector's
  summary as the chat builder does (`local_modalities`, `capabilities/mod.rs:302`): `"image"` when
  `has_vision_encoder`, `"audio"` when `has_audio_encoder`. That applies to VL embedders too;
- `output_modalities: ["decisions"]`, llama.cpp's and OpenRouter's word.

Cloud entries and owner overrides take their endpoints from `capabilities::task::endpoints`
(`capabilities/task.rs:65-85`). Today an unknown task falls through to `/v1/tasks/run` (`:83`), so
it gains `"decision"` → `["/v1/systemone"]` for `openai` and `llama_cpp`, and nothing otherwise.
`KNOWN_TASKS` (`capabilities/mod.rs:1029`) gains `decision` and `KNOWN_MODALITIES` (`:1053`) gains
`decisions`. Without them, `validate_vocabulary` (`:980`) refuses an override that states either,
and §4.3's TypeSafe alias needs exactly such an override. The doc comments on
`ModelCapabilities::task` and the modality lists (`lmgw-api-types/src/capabilities.rs:21-30`) name
both.

The notes carry what the header knows and the engine enforces: the decision type and, for Laya and
Julia-1, `decision.max_head_tokens` (192 and 256), the budget past which llama.cpp truncates long
questions and options. lmgw cannot change that truncation; it can say so. The per-type option limits
(52 or 255) live in llama.cpp's code, not the header, so lmgw does not publish them. llama.cpp's 400
names the limit when a request crosses it (A.6).

An `architecture` object would be a second spelling of facts the capabilities already carry, on a
route whose reference is OpenAI's list. A client that follows llama.cpp's README check
(`architecture.output_modalities.includes("decisions")`) reads `capabilities.output_modalities`
instead. TypeSafe's SDK `models.list()` expects TypeSafe's own list shape and cannot read lmgw's,
OpenRouter's or llama.cpp's. That does not matter, because the SDK's `ask` does not depend on it.

*As built (WP4):* `for_aux(model, summary, projector_status, projector)` takes the projector the
chat builder's way. `capabilities::aux_projector` names it: the `mmproj_path` field, else a
`--mmproj` in the args, readable only under `/models/`. `source` is `gguf+config` when a projector
header was read, `config` otherwise. An unreadable projector withholds the input list and says so
in a note. `parse_task_openai` maps `decisions` when the outputs do not also include `text`, the
image rule's shape, so a chat model that can also decide stays a chat model.

### 2.10 Clients

`lmgw-client` covers the client-app routes (Chat, feed, realtime, MCP host; `lib.rs:1-20`), not
the inference routes, so it gets no method. A client app that wants decisions uses
`lmgw-api-types::systemone` and its own HTTP.

## 3. The decision kind

### 3.1 Class, aux kind or chat flag

| | Own class | `AuxKind::Decision` | Flag on chat rows |
|---|---|---|---|
| Fits the models | rows would need the chat param set for 4B–27B causal models | the aux class already spans 300M encoders to 8B causal embedders with projectors; it needs the five fields of §3.3 either way | full params, VRAM fit and projectors come for free |
| Kept out of chat surfaces | yes | yes: aux rows are never in chat pickers, candidate lists or the realtime cascade | no: every chat consumer (Chat picker, candidate aliases, realtime, the global chat fallback a row `Inherit`s) needs a new filter |
| Fallback semantics | new | aux's `Inherit` already means none, which is right for decisions | chat's `Inherit` reads the global chat alias, which is wrong |
| Server mode | | Laya, Kev and Clef run llama-server in embedding mode, the aux class's mode | |
| Cost | the fifth class: a table, settings, a synthetic upstream, labels, adoption, the reaper and every target list (`Class::Image` has 54 non-test sites in 24 files) | 19 non-test `AuxKind::Rerank` sites in 9 files, and the five fields | small, plus a filter at every chat consumer |

**Decided: `AuxKind::Decision`** (Q2, 2026-10-12). The probe's evidence cuts both ways on resource
shape: Laya and Julia are 144–421M non-causal encoders, while Kev, lev, OpenJev and Clef are causal
4–27B models, two of them with projectors. The aux class already has that spread among its
embedders: it serves 300M encoders, 8B causal embedders and vision-language embedders with
projectors. It handles the spread badly today only because ubatch, the projector and CPU placement
live in freeform args. Fixing that (§3.3) serves embed rows too. A class of its own would duplicate
the aux machinery for a kind that differs from rerank by one flag and one route.

### 3.2 Detection

`ModelSummary` (`gguf.rs:1560-1685`) gains four fields read from the header:
- `decision_type`: `{arch}.decision.type`;
- `decision_max_head_tokens`: `{arch}.decision.max_head_tokens`;
- `attention_causal`: `{arch}.attention.causal`. It is absent on causal files and `false` on Laya
  and Julia-1;
- `has_systemone_template`: `tokenizer.chat_template.systemone`.

`class_for` (`modelinfo.rs:168-180`) returns `("aux", Some(AuxKind::Decision))` when
`decision_type` is present, before the rerank test. Any value counts, including a type this lmgw
has never seen: llama.cpp decides whether it can serve it, and the model test says whether it did.
A decision header without the `systemone` template gets a plan warning that the server will refuse
to load it.

*As built (WP4):* a fifth field, `has_decision_tensors` (any `decision.*` tensor), carries Clef's
head to §3.4's head test beside `has_classifier_head`.

`hf_repo` stays filename-based (`ops/huggingface.rs:183-250`) and lists weights and projectors as
today. The header decides after the download, in the plan. The download target is `aux`, so
`hf_add … target=aux` fetches the weights and a sibling projector, and decision files live in the
aux models dir (`hf.rs:36-44`, `ops/common.rs:257-266`).

### 3.3 The row: five fields the aux row lacks

`AuxModel` (`config/models.rs:172-216`) and its table gain:

| Field | Renders | For |
|---|---|---|
| `ubatch_size`, `batch_size` | `--ubatch-size`, `--batch-size` | decisions (§3.4); embedders, whose inputs are bounded by the ubatch in the same way (Found while reading) |
| `mmproj_path` | `--mmproj /models/<path>` | OpenJev and Clef; VL embedders, whose projector moves out of `args` |
| `backend` (`cpu` or none) | `--n-gpu-layers 0`, and run args without GPU devices plus `NVIDIA_VISIBLE_DEVICES=void` (`cpu_run_args`, `lmgw-api-types/src/audio_engine.rs:66`) | CPU rows that serve under the hold |
| `threads` | `--threads N` | a CPU row's pool; without it, a CPU row uses the physical cores, as audio rows do (`runtime/audio/engine.rs:12-21`) |

Placement has two predicates, and both learn the field. `runtime/placement.rs:10-15` says the
registry entry is the truth for a running container and the row for the next start:
- **The row's:** `Snapshot::placement` (`bench/lease.rs:111-121`) answers `Cpu` for an aux row
  whose `backend` is `cpu`. Resolution and the hold's swap read it (`gpu_block_for`,
  `resolve_for_request`), and so does `vram/on_cpu.rs:39`.
- **The descriptor's:** `ModelRuntime::placement` (`runtime/descriptor.rs:139-143`) is `Cpu` today
  only through `audio_settings`. The aux descriptor carries the row's `backend`, and the method
  answers `Cpu` for it. Admission (`vram/queue.rs:72`, `:89`), the registry entry
  (`runtime/registry/acquire.rs:170`, `reconcile.rs:550`) and warm start under the hold
  (`runtime/lifecycle.rs:193`) read this one.

The footprint (`vram/plan.rs:302-322`) charges the projector and reads CPU placement from the field.
As built (WP2), it also charges a projector still named by `--mmproj` in `args` and, like the chat
footprint, still reads `--n-gpu-layers 0` there, because the argv renderer honours whichever is
present. The placement predicates read the field only, so a row that keeps the flag in `args`
stays a GPU row for the hold and the ledger until it is hoisted.

Moving the flags out of existing rows' `args` is a new aux fold. Chat's `PROMOTED_ARGS`
(`config/llama_params.rs:16-34`) covers only `mmproj` and `ubatch-size`; `batch-size`,
`n-gpu-layers` and `threads` are new. `AuxModel` has no hoist, and `hoist_promoted_args_into`
(`:95`) takes `LlamaParams`. So `AuxModel::hoist_args` is new, is idempotent like the chat one
(`store/rows.rs:151`), and moves the five flags into the fields. The list's own note
(`config/llama_params.rs:35-44`) says hoisting reorders argv. On the first boot after the
upgrade, each affected aux container fails the `Config.Cmd` match and is re-created, not adopted.
*As built (WP2b):* the fold runs at boot, not on every load. What becomes of `--n-gpu-layers 0`
depends on the image's files, a podman read the synchronous row read cannot make (below).

Existing CPU embedding rows would then serve under the hold, and their containers would stop
receiving the GPU device. That is a behaviour change, not a no-op. A CUDA build at `-ngl 0` with a
visible device still opens a CUDA context, and by default it offloads large-batch operations to
the GPU. Measured on 2026-10-12 (§6.1 step 10) with Qwen3-Embedding-4B Q4_K_M, pooling `last`,
`-c 8192 -ub 8192 -b 8192 --threads 8 -ngl 0`:

| Run | VRAM | Short input, median | Batch of 32 inputs (~200 tokens each), median |
|---|---|---|---|
| CUDA image b11549 **with** the GPU device (CDI) | ~5.7 GiB (compute buffers) | 206 ms | 2.2 s |
| CUDA image **without** a device (log: "no usable GPU found, --gpu-layers option will be ignored") | 0 | 159 ms | 50.6 s (~23× slower) |
| CPU-only `ghcr.io/ggml-org/llama.cpp:server`, no device | 0 | 161 ms | 50.6 s |

So a CUDA-image row at `-ngl 0` is not a CPU row in effect: it holds compute buffers on the card
and batches 23 times faster than one without the device. The owner refined Q4 on that basis: the
hoist moves `--n-gpu-layers 0` into `backend: cpu` **only when the row's effective image has no
GPU backend** (no libggml-cuda or libggml-vulkan in it). A row on a CUDA image keeps GPU
placement, exactly as today, and its start notice names the trade-off with the numbers above, so
switching it to `backend: cpu` is a deliberate choice. `--threads`, `--ubatch-size`,
`--batch-size` and `--mmproj` hoist as before. WP2's rule is unchanged: `backend: cpu` means no
GPU device; only the automatic hoist (WP2b) checks the image.

**As built (WP2b, 2026-10-13).** `AuxModel::hoist_args` (`config/aux_hoist.rs`) is a pure rule
over the row and what its image carries. The fold that calls it, `lifecycle::hoist_aux_args`
(`runtime/lifecycle/aux_hoist.rs`), runs first in `boot`, under the boot-settled guard and before
reconciliation. It writes each changed row back with `store::store_aux_hoist`. That is a
compare-and-set under `begin_write` over the args, the five fields, the image and the run args the
verdict was read by, so a save that lands in between wins. It then publishes the snapshot and logs
one notice per row as an info line, the channel the migration notices use. A row with nothing to
move keeps its args, so its argv stays byte-identical and its container is adopted.
- **Spellings.** The renderer's alias table (`argv::canonical_key`) matched against the five
  keys only, so `-tb`/`--threads-batch` stay. `--flag=value` moves like the spaced form.
- **What stays in `args`, as written:** a count that is not positive (the columns' CHECKs), a
  flag without a value, `-ngl` other than 0, and a projector outside `/models` or with a `..`
  (the op refuses such a path in the field). A flag whose field is already set is dropped, and
  the notice names it: the renderer claimed the key, so the copy rendered nothing.
- **Projector paths** are stored as the chat hoist stores them: `/models/` dropped, a relative
  path taken as written. One that is not a file there (e.g.
  `mradermacher/Qwen3-VL-Embedding-8B-GGUF`, a directory) moves as written, and the row's
  finding reports it ("mmproj_path '…' is not a file under the aux models dir").
- **The image check** (`backends/images/gpu.rs`) reads the row's effective image (its own, else
  the class's, as the descriptor starts it), and only for rows whose args carry `-ngl 0` with
  `backend` unset. An lmgw build's `dev.lmgw.backend` label decides (the one the Images tab
  shows), read as `GpuBackend`; a value lmgw does not know is unknown. Any other image gets one
  throwaway container per image ID (`--pull=never`, no network, `NVIDIA_VISIBLE_DEVICES=void`)
  that runs `find / -xdev -name 'libggml*'`:
  - a `libggml-<name>` other than `base`, `cpu`, `blas` or `rpc` is a GPU backend;
  - `libggml-base` with none of those is CPU-only;
  - a lone `libggml.so` (ik_llama.cpp's layout, CUDA compiled in) or no library is unknown.

  Only a whole listing (exit 0) proves CPU-only. A partial one (exit 1, some directory
  unreadable) can still show a GPU library. Any other exit is unknown, and so is a listing that
  takes over 30 s (its container is removed by name) or an image that is not on the machine.
  GPU runtime libraries prove nothing: the CPU-only `llama.cpp:server` (b11561) ships Mesa's
  `libvulkan` and a `libOpenCL`.
- **Notices.** A moved row's notice names each flag as written and each field it set.
  - **Relative projector.** One written relative says it is now read under `/models`; before,
    llama-server read it against its working directory.
  - **Moved to the CPU.** The notice says why the row is now on the CPU, and whether its
    container still gets a GPU (its own run args can pass one). It also says the row stays on
    the CPU if its image later changes, and that clearing `backend` returns it.
  - **Replaced or adopted.** The fold renders both sides of what adoption compares, the
    container command and the run-args digest. Only when either differs does the notice say the
    container is replaced rather than adopted. A dropped copy renders nothing. A
    `-cpu` shape (`--n-gpu-layers 0 --threads 16` with its own run args) renders the same argv
    from the fields on a CPU-only image. Both are adopted.
  - **Kept `-ngl 0`.** The row stays a GPU row. On a GPU image its notice names the trade-off
    with the numbers above when its run args pass a device. When they pass none, it says instead
    that the row computes on the CPU already and `backend: cpu` would let it serve under the
    hold. On an unreadable image the notice names the reason. Such a row is judged again at
    every start, because its image may change, so that notice repeats while the flag stays.

### 3.4 Planner rules

`aux_plan` (`modelinfo.rs:1934`, now `modelinfo/aux_plan.rs`) gets a decision branch. The branch
and `Probe::Decide` (§3.7) live in a new child module `modelinfo/decision.rs`, because
`modelinfo.rs` is 4,390 lines. Every value
comes from the header or is stated as lmgw's choice, with its reason. Three header shapes:

| Param | Non-causal (`attention.causal = false`; Laya, Julia-1) | Causal with a decision head (`cls.output*` or `decision.*` tensors; Kev, Clef) | Causal, label logits (no such head; lev, OpenJev) |
|---|---|---|---|
| `kind` | `decision`, naming `decision.type` | the same | the same |
| `ctx_size` | the trained `context_length` (8192) | the trained `context_length` (262144), with its KV cost in the rationale, as the chat plan states it. Kev-4B is about 8 GiB of f16 KV at full context (8 full-attention layers × 4 KV heads × 256 × 2 × 2 bytes per token). The owner may lower it, visibly | the same |
| `ubatch_size` = `batch_size` | `ctx_size`: a non-causal prompt is evaluated in one ubatch (`server-context.cpp:449-452`), and every request past 512 tokens fails without it (A.8) | 2048, **lmgw's choice**. These types run in embedding mode, which makes the engine's effective default 512 (`n_batch` forced down to `n_ubatch`, A.1). The tokens from the first option marker to the end must fit in one batch (`server-context.cpp:3588-3597`, a 400 that names the size), and 512 is short for a long option list | not set: llama.cpp's defaults (`n_batch` 2048, `n_ubatch` 512). These types keep a normal context, so the same rule already has 2048, and a larger ubatch would only grow the compute buffer |
| `pooling` | never; a decision row renders no `--pooling` | never | never |
| `mmproj_path` | none | a sibling projector when the repo has one (Clef), as the aux plan finds one today | the same (OpenJev) |
| `backend` | `cpu`: 33 ms per question; time grows with the state and with the question count (14.9 s for one question on 7,669 tokens), and the rationale names the class's 60 s request timeout (§2.3); GPU stays one field away | none (GPU) | none (GPU) |

The head test matched all four causal headers read (§1.5, Appendix B). It is a header signal, not
llama.cpp's own switch, which is the decision type (`common.cpp:1291-1305`). A type it misreads
costs a visible 400 (a head-less embedding-mode type keeps the effective 512) or a larger compute
buffer (a head-carrying label-logit type gets 2048).

These rules read the header and nothing else. The per-type facts from llama.cpp's source (which
types are whole-prompt, joint, image-capable) are recorded in §1.5 and not coded, because a table
of llama.cpp's types inside lmgw goes stale with the next model llama.cpp adds. A causal
whole-prompt model (Clef, vllm-d3) whose prompt outgrows 2048 gets llama.cpp's error, and the
rationale says to raise `ubatch_size` for it.

**Kev, checked in the source.** Kev is causal, so `can_split()` skips the non-causal branch and
returns true at "a decision task reads its outputs from the last batch"
(`server-context.cpp:447-474`). The state is split across batches and shared across questions
(`can_share_prompt`, `server-decision.h:39-53`). Only the span from `pos_first()`, the first option
marker (`server-task.h:188-200`), to the end of the prompt must fit in one batch. So a Kev state may
be far longer than the ubatch, and a long option list is what needs the 2048.

**The footprint charges the compute buffer** (owner item b, 2026-10-13). Until then
`vram/plan.rs:15-19` kept compute and graph buffers out of the estimate and in the visible
`headroom_mb` setting (default 1024), and the plan's rationale told the owner to raise the setting
when the buffer exceeded it. The aux footprint now carries a compute term: `vram/compute.rs`
derives llama.cpp's buffer from the header and the row's ubatch, batch, ctx and flash attention,
and `vram/plan/aux.rs` charges it (Appendix C has the formula and its measurements). Chat
estimates were unchanged then; since 2026-10-14 a chat row is charged its compute buffers too
(Appendix C.2).

*Measured 2026-10-13* (Kev-4B Q8_0, b11549, §6.1): at ubatch 2048, Kev's CUDA compute buffer is
3,044 MiB at ctx 262144 and 2,148 MiB at 32768. At 512 it is 761 MiB, so the buffer scales with
the ubatch and somewhat with the context. With the 201 MiB recurrent state and the CUDA context,
the card held 3.5 GiB more than the footprint at 262144, and 2.7 GiB more at 32768. That exceeds
the default `headroom_mb` of 1024. The 2048 is still needed: at 512, 255 options (1,780 tokens
from the first option marker) get llama.cpp's 400. So the 2048 rationale gives Kev's figures as
Kev's, as the CPU one gives Laya's. When `headroom_mb` was below them, the rationale said the owner
raises the setting, or the ledger counts the row short and may start another model beside it
(bb5c8bce). *Superseded 2026-10-13:* the footprint charges the compute term, so the figures above
are what it now has to reproduce (Appendix C, Kev-4B), and headroom covers the CUDA context,
allocator slack and a projector's own compute buffer.

*As built (WP4):* the branch is `modelinfo/decision.rs` (`Shape::of`, `plan`). The 2048 rationale
gives Kev's measured buffer and what to do when `vram.headroom_mb` is below it (bb5c8bce, the
paragraph above); before the Kev acceptance it only named `headroom_mb` and said to raise it on an
OOM at load. The label-logit case states its defaults in the `kind`
rationale, because no ubatch or batch is planned to attach one to. A causal ctx rationale gives the
f16 KV cost at the trained context from `gguf::kv_cache_bytes`, since an aux row renders no cache
type. A non-causal plan proposes no sibling projector and names it in a warning instead. The
configured-row diff compares `ubatch_size` and `batch_size` but never `backend`: placement stays
the owner's choice, so a Laya kept on the GPU is no difference. On a causal decision row it also
counts the tuning its own rationale asks for as in sync (`decision::within_tuning`): a `ctx_size`
at or below the trained context, a `ubatch_size` or `batch_size` at or above 2048. A re-plan then
does not offer to undo it. A non-causal row is compared as planned, since its ubatch is its whole
prompt. A header without a context length gets no ctx (and, when non-causal, no ubatch or batch),
with a warning; the shape's own values (the CPU, the 2048) are still planned. The CPU rationale
gives Laya's figures as Laya's measurement, says time grows with the model's size and the question
count, and names the thread count as physical cores or logical CPUs, per what the host read. The
non-causal ubatch rationale says its compute buffer is RAM on a CPU row (A.8: 2.22 GB), outside the
VRAM footprint only because the row has no GPU. The embed and rerank ctx rationale no longer says
over-long inputs are "truncated, not rejected": it says llama-server refuses them. Since the
slice's final review it also says that the trained context is not a non-causal encoder's ceiling
while `ubatch_size` is unset, because llama-server's default ubatch of 512 bounds every input, and
it points at `ubatch_size` and `batch_size`. The planned values were unchanged then; owner item a
(§7) changed them for embed and rerank rows the next day.

### 3.5 Argv

`LlamaArgs::Aux` (`runtime/argv.rs:137-145`) carries the new fields. `push_aux_flags`
(`:715-760`) emits no kind flag for `Decision`. It still claims `embeddings`, `reranking` and
`pooling`, so a stale `--reranking` in the freeform args (A.10: a crash at load) or a `--pooling`
cannot leak through. It then emits `--ctx-size`, `--ubatch-size`, `--batch-size`, `--mmproj`,
`--n-gpu-layers 0` and `--threads` from the fields and claims each, so `push_freeform_args` drops
duplicates.

The op's checks follow. `check_aux_flags` (`ops/aux_model.rs:101`) and `aux_config_warnings`
(`:265`) gain a `Decision` arm and the new fields. A decision row with `pooling` set, or
`--reranking`, `--embeddings` or `--pooling` in its args, is flagged. So is a projector on a
non-causal decision row, and a `backend` other than `cpu`.

### 3.6 The image check

A row's image must contain the route, and an image without it must fail visibly:
- **At plan time, never.** The plan reads the header only. The owner ruled on 2026-10-13 that no
  plan or download loads a model: the plan-time runtime probe (a throwaway container, up to 10 s)
  and its `probe` parameter and `runtime` verdict were removed from `/api/local-model-plan`,
  `/api/model-inspect` and the MCP tools. An image too old for the model shows on the row's Test
  (llama.cpp's error plus the classifier's "update the container image" hint), at start and at
  request time, below. A causal decision model on an old image may load as a plain causal LM, and
  only its answers show that.
- **At start.** The registry reads `/props` once per start and adoption
  (`runtime/registry/llama_props.rs`). For a decision row it also reads `GET /v1/models` and checks
  `architecture.output_modalities` for `decisions`. If it is missing, the entry gets a warning
  naming the image and its `build_info`, shown on Status and the Models page. As with `/props`, a
  failed read is a warning and never a failed start.
- **At request time.** A 404 from a local decision row's container on `/systemone` becomes the
  named 501 of §2.7. It says the image lacks the route and points at the Backends page, instead of
  relaying llama.cpp's bare "File Not Found".

Nothing is keyed on a build number (llama egress decision 15). ik_llama.cpp images were not probed.
The same three checks cover them.

*As built (WP4):* the start-time read is `runtime/registry/decision_route.rs`. The entry's
`LlamaEntry::decision_route` is `Listed`, `Missing` (the warning names the image and `build_info`)
or `Unread`. It is read after `/props` on a start and on an adoption, in what is left of the same
readiness budget, and `Registry::list` shows its warning with the entry's others. As with `/props`,
a later failed read does not replace an answer read before.

### 3.7 The model test

`local_model_test … target=aux` maps the decision kind to a new `Probe::Decide`
(`modelinfo.rs:2241-2248`): one `choice` question with three options about a fixed one-line state,
built with `lmgw-api-types::systemone`. It passes when the answer is 200, keyed by the question id,
names one of the options, and its probabilities sum to 1 within 1e-6. The result shows the
`input_tokens`, the latency and the answer. Like the embed and rerank probes, it does not judge the
model's choice, only that the route works. The test also refreshes the registry entry's facts
(§3.6).

*As built (WP4):* the question is `topic`, "What is this text about?", with the options
`software`, `cooking` and `sports` in that order, sent as the typed request so the order holds.
`ModelTestPassed` gains `input_tokens` and `answer` (a `systemone::Answer`), both absent for the
other probes. A 404 on the route fails the test with the image named, as the route's 501 does. The
answer is read in `modelinfo/decision.rs` (`read_answer`). The refresh reads `/props` and
`GET /v1/models` live and writes both to the entry, pass or fail, whenever the container answered
over HTTP; a transport error skips it. Each read gets what one readiness probe gets
(`registry::LIVE_FACT_READ`, the 2 s `CONTAINER_EXIT_POLL`), not the load budget.

### 3.8 Downloads

There is no new target. `hf_add target=aux` takes the weights and a sibling projector; Laya and
Julia-1 ship none, OpenJev and Clef ship one each (§1.5). `gguf_files target=aux` lists them. HF's
repo tags (`decision-model`, `pipeline_tag: text-classification` on Laya) are a hint at most; the
header decides.

*As built (final review):* `hf_add target=aux` fetches a sibling projector and no drafter, which an
aux row cannot use. The dashboard's Hugging Face wizard plans an aux download
(`local_model_plan target=aux`) and creates the row from the plan's params: the header's kind,
the pooling, the context and the five fields. It fetches the projector companion, and it shows the
plan's reasons and warnings in the aux editor's Plan card. Before the review it offered only embed
or rerank and created the row from the filename, so Laya became an `embed` row.

### 3.9 Dashboard

The Models page's aux list (`lmgw-ui/src/pages/models.rs`) shows decision rows under their own
prefix with a "decision" chip and the licence. The aux editor (`model_editors.rs`) gains the kind
option and the five fields: a CPU switch with threads, ubatch and batch, and the projector. The
editor hides `pooling` for decision rows. The Test button runs §3.7. The API reference page's
tester picks models for the route through `model_task::DECISION`.

*As built (WP6):* the aux editor (`model_editors/aux.rs`, with `aux_fields.rs`, `aux_plan.rs` and
`aux_save.rs`) edits the five fields, says the row's public name under its kind's prefix and its
route (`lmgw-api-types::public_names`, the gateway's own rule), and hides `pooling` for rerank and
decision rows. A blank context names `ctx_size` in `clear`. "Fill from plan" shows each planned
value with its rationale and the plan's warnings. A new row takes every planned value. A saved row
on its own GGUF takes only the fields the plan's comparison with it lists in
`configured_as[].change` (typed as `ConfiguredAs`), so a causal decision row keeps the tuning
`decision::within_tuning` allows and a projector its args already name. The rest show as kept, and
`backend` is never filled on a saved row. The thread count in effect has one rule,
`lmgw-api-types::aux_engine::threads_in_effect`, which the argv renderer and the editor both run. `AuxModelView` and `AuxModelDetail` carry `threads_in_effect` and `threads_source`
(`row`, `args`, `cores`, `logical_cpus`, `llama_server`). The Models page names a CPU row's thread
count in its file cell, as audio CPU rows do. A decision row's Test shows `input_tokens` and the
answer, most probable option first. *As built (owner item g, 2026-10-13):* `gguf/licence.rs` reads `general.license`,
`general.license.name` and `general.license.link` into `ModelSummary::licence` (a blank value
counts as absent), and `modelinfo/licence.rs` reads it through the shared header cache.
`licence: Option<ModelLicence {id, name, link}>` is on `LocalModelView`, `AuxModelView`,
`LocalModelDetail`, `AuxModelDetail` and `ModelInspect`, and not on `/v1/models`. The Models page
shows a plain badge after the kind badge (`pages/models/licence.rs`), labelled with the name, else
the id. It links only http(s) URLs and otherwise shows a tooltip. No licence is coloured, ranked or
filtered, and a row without one shows nothing. The chat and aux editors do not show it yet.

### 3.10 The prefix, per kind

Q3 decided `decide/` (2026-10-12); the examples and acceptance rows use it.
- **Where the field lives.** `Settings.aux_router` is a `RouterSettings`, the same type as chat's
  `router` (`config/settings_classes.rs:187`, `:234-252`). It becomes
  `AuxRouterSettings { #[serde(flatten)] base: RouterSettings, decision_prefix: String }`, with
  `decision_prefix` defaulting to `decide`. Chat's settings never carry the field, and the settings
  wire stays flat.
- **Names.** `aux_public_name` takes the row instead of its id. It returns
  `<decision_prefix>/<id>` for a decision row and `<public_prefix>/<id>` for embed and rerank rows
  (`config/snapshot.rs:472-473`). Its 18 non-test users switch to the row form, including the
  candidate-alias name collision checks (`ops/aux_model.rs:489`, `:607`; `ops/common.rs:412`) and
  the name table (`config/snapshot.rs:706`).
- **Resolution.** `resolve_aux_local` (`config/snapshot.rs:543-550`) strips the decision prefix
  and matches decision rows only, then strips `public_prefix` and matches embed and rerank rows
  only. **`embed/laya` does not resolve**: a row has one public name. If both prefixes are set to
  the same value, the kinds share one namespace, and aux model ids are already unique across the
  table. An empty `decision_prefix` gives bare ids, as the other class prefixes allow.

*As built (WP4):* both `AuxRouterSettings`, the gateway's and the api-types one, dereference to
their `RouterSettings`, so `aux_router.models_dir` reads as before. A name for a row not stored yet
comes from `Snapshot::aux_name_for(kind, id)`. Because the kind picks the prefix, an update that
changes the kind renames the row, and the candidate-alias check runs then too. The dashboard's
patch is `AuxRouterSettingsPatch` with the five class fields and `decision_prefix`, not a
flattened `RouterSettingsPatch`, because serde cannot deny unknown fields through a flatten. The
self-admin settings read reports the prefix as `aux_router.decision_prefix` (`AuxClassRead`), and
Settings → Runtimes → Aux shows it after the public prefix.

## 4. Cloud upstreams

### 4.1 How an upstream serves System One

No new protocol and no flag. `/v1/systemone` forwards to any `openai` or `llama_cpp` upstream,
the same rule `/v1/rerank` follows for cloud models ("their kind is theirs to know",
`proxy/embeddings.rs:500-504`). Both providers put the route at `{base}/systemone` under the base
an OpenAI-protocol row already has (`https://openrouter.ai/api/v1`, `https://api.typesafe.ai/v1`).
A protocol of its own would force OpenRouter into two rows with one key. A `supports_systemone`
flag beside `supports_responses` would duplicate what the catalog says per model. The catalog's raw
output modalities refuse the clear mismatches (§2.2), and an upstream's own 404 covers the rest.

### 4.2 OpenRouter

- **Row:** protocol `openai`, kind `generic`, base `https://openrouter.ai/api/v1`, a key,
  `expose_all` under a prefix such as `openrouter`. Routing `openrouter/typesafe/jev-1.13`
  works through passthrough without the catalog (`config/snapshot.rs:583-610`).
- **Catalog:** OpenRouter's default `GET /models` (459 entries on 2026-10-12) omits Jev. Only
  `?output_modalities=all` (661 entries) lists `typesafe/jev-1.13` and `~typesafe/jev-latest`, with
  `architecture.output_modalities: ["decisions"]`, context 64,000, and prompt $0.000000042 /
  completion 0 (Appendix B.3). Before slice 2, lmgw fetched `{base}/models` bare
  (`catalog::fetch_models`). Slice 1 maps `decisions` → `decision` in
  `catalog::parse_task_openai` (§2.2), which serves any catalog that lists the modality. Slice 2
  sends `output_modalities=all` so OpenRouter's list includes Jev. *As built (slice 2):* the
  catalog request of an `openai` row of kind `generic` carries the parameter while its
  `catalog_all_modalities` is on, the default (`catalog/request.rs`, §6.2).
- **Ids:** `typesafe/jev-1.13` (pinned) and `~typesafe/jev-latest` (moving). The TypeSafe SDK's bare
  `jev-1.13` is mapped by OpenRouter, so an alias `jev` → `typesafe/jev-1.13` is all a client
  needs.

### 4.3 TypeSafe direct

This is a `generic` OpenAI-protocol row with base `https://api.typesafe.ai/v1` and plain aliases
(`jev` → `jev-latest`). Its `GET /v1/models` has TypeSafe's own shape, which lmgw's OpenAI catalog
reader does not parse, so `expose_all` lists nothing and the price is a manual row ($0.042 per Mtok
in, 0 out). The alias publishes as a decision model through a capabilities override
(`{"capabilities": {"task": "decision", "output_modalities": ["decisions"]}}`), which §2.9's
vocabulary change makes valid. It is untested without an account and not part of either slice.

### 4.4 Price

Catalog sync turns OpenRouter's per-token strings into a `per_mtok` row: $0.042 in, $0 out
(`catalog::catalog_prices`). The provider's non-zero `output_tokens` are recorded and cost
nothing. A row whose upstream reported no `usage` stays unpriced, never 0. *As built (slice 2):*
the catalog listing Jev was the main missing half. The row is scoped to the alias that names
the model (`jev`) and, on an `expose_all` upstream, to the upstream scope its passthrough name
prices through, so `jev` and `openrouter/typesafe/jev-1.13` both price. Its price was checked
live (§6.2). The models whose catalog price is no per-token price are left unpriced (§6.2).

### 4.5 A local decision row falling back to Jev

`hold_fallback_mode: alias`, `hold_fallback: jev`. Under the hold a GPU decision row's requests go
to OpenRouter with `x-lmgw-fallback: jev`, and the row is priced under `jev`. A CPU decision row
keeps answering locally under the hold and its fallback goes unused (§2.3). The hold is not the only
trigger: outside-VRAM pressure swaps at admission as for any row (candidate-aliases §4.7).

## 5. Later uses inside lmgw (out of scope)

These are listed so the route's shape does not block them. None is designed here.
- **Candidate-alias complexity routing.** It was rejected earlier because it cost an LLM call. A
  3–40 ms local `score` ("how hard is this request?") changes that trade-off. The calibration
  caveat (§1.1) means thresholds have to be measured per model.
- **Realtime barge-in and end-of-turn.** A `noul` ("is this an interruption?", "is the user done?")
  on a CPU decision row, which keeps serving under the hold. Laya is English-only; Julia-1 is
  multilingual and smaller.
- **Guardrails, and "may this go to the cloud".** A request or response check is a `choice` or a
  `noul` per message. The fallbacks-always-apply rule bounds the cloud filter. It cannot be an
  exception inside a fallback, because a configured fallback is always used. It can only act
  before routing, as a refusal or as the client's choice of an alias without a fallback. The
  decision row that runs the filter must itself have no cloud fallback, or the filter would send
  the content to the cloud to decide whether it may go there.
- **KB mode decisions**: whether a turn needs retrieval and from which base.
- **Attachment classification**: what kind of document an upload is, and which pipeline reads it
  (an image-capable type such as OpenJev for screenshots).

## 6. Slices

### 6.1 Slice 1: local

| WP | Delivers | Assumes |
|---|---|---|
| WP1 | `ModelSummary` fields, `class_for` order, `AuxKind::Decision` (serde `decision`), the five aux fields with migration and CHECKs, argv, `check_aux_flags` and `aux_config_warnings` arms (§3.2–3.5) | Q2 |
| WP2 | aux placement in both predicates (`Snapshot::placement`, `ModelRuntime::placement` via the aux descriptor's `backend`), `cpu_run_args`, threads, the footprint with projector (§3.3) | Q2 |
| WP2b | `AuxModel::hoist_args`, the new aux fold, and its start notice; `--n-gpu-layers 0` becomes `backend: cpu` only on an image without a GPU backend, as refined after the measurement (§3.3, step 10). *Built 2026-10-13:* a boot fold that writes rows back, ahead of reconciliation, not an every-load fold; the image check reads the build label, else the image's `libggml*` files (§3.3, "As built (WP2b)") | Q4 |
| WP3 | the route: table row, `proxy/systemone.rs` (a `Bytes` body with `read_body`), `egress/openai_wire/systemone.rs`, `RouteCheck::Decision` returning `Refused { status: 501 }`, `RouteCheck::Count` and the reverse guard, `ClientProto::SystemOne`, errors, the 404 → 501 mapping (§2.1–2.7) | Q2 |
| WP4 | §3.10's prefix, `for_aux`'s decision arm and projector rule, `task::endpoints`, `KNOWN_TASKS`/`KNOWN_MODALITIES` and the doc comments, `parse_task_openai`'s `decisions`, notes, the planner branch and `Probe::Decide` in `modelinfo/decision.rs`, the fallback check, the start-time `/v1/models` read (§2.2, §2.4, §2.9, §3.4, §3.6, §3.7, §3.10) | Q2, Q3 |
| WP5 | `lmgw-api-types::systemone`, the `DocRoute`, `model_task::DECISION`, `GATED_ROUTES`, `OpDoc` args, drift tests, the README feature line and, under Q5, the README typed-answers paragraph, the self-admin instructions (§2.8) | Q5 |
| WP6 | Models page and aux editor (§3.9); the thread count in effect and its source on the aux detail and view, as audio rows show `threads_in_effect` (WP2 renders it, no surface shows it yet). The editor's kind select already offers `decision` and hides pooling for it (WP1/WP2 review). *Built 2026-10-13:* the rule shared through `lmgw-api-types::aux_engine`, the fields on both reads, the editor's fields and plan, the Models page's CPU cell and decision Test (§3.9, "As built (WP6)") | Q2, Q3 |

Tests follow the patterns that exist:
- `tests/it/aux_router.rs`: the kind gates both ways, as the zero-vector gate. The reverse guard on
  chat, legacy completions, embeddings and rerank. `/tokenize` and both count routes still answer
  on a decision row.
- `tests/it/runtime_argv.rs`: no kind flag, the claimed keys, a stale `--reranking` dropped.
- `tests/it/openapi_v1_wire.rs`: drift tests against wiremock bodies.
- A slice-1 wiremock upstream of protocol `openai` answering `/systemone` with OpenRouter's
  documented body (`id`, `provider`, `usage.cost`, `output_tokens: 20`). It is relayed byte for byte
  and priced from a manual `per_mtok` row (Q1).
- `tests/it/route_walk.rs`: the table row.
- `tests/it/gpu_contention.rs` with `support/gpu_world.rs`: a CPU aux row serves under the hold
  with no charge, through both placement predicates; a GPU one swaps or refuses.
- `tests/it/usage_units.rs`: the priced row.
- The 404 → 501 mapping: a fake local decision container (`support/llama_fake.rs` style) answering
  404 on `/systemone` gives the named 501.
- The fallback check at save time: `aux_model_set` on a decision row refuses a fallback whose
  catalog fixture lists `["text"]` and accepts one with `["decisions"]` or none.
- The start-time `/v1/models` read: a fake container without `decisions` leaves a warning on the
  entry and the start succeeds.
- Q4's hoist: a row with `--n-gpu-layers 0 --threads 8 --ubatch-size 4096` in `args` loads with the
  fields set and the args emptied, idempotently (`tests/it/config_hoisting.rs` style).
- §3.10: `decide/<id>` resolves a decision row, `embed/<id>` does not, and a candidate alias
  named `decide/<id>` collides.
- `gguf.rs`'s synthetic `Header` builder for a `modern-bert` decision header with `cls.*` tensors,
  for the `class_for` and planner unit tests.
- `tests/it/modelinfo_real_files.rs` for the plan on the real Laya file, gated on
  `LMGW_TEST_AUX_DIR`.

Each test module is a file of the one `tests/it` binary. Live checks run on a dev instance started
by `scripts/dev-instance.sh`, never on a real data dir.

**Acceptance, Laya on CPU:**
1. `local_model_plan target=aux` on Laya returns kind `decision`, `ctx_size 8192`,
   `ubatch_size = batch_size = 8192`, `backend: cpu`, no pooling and no rerank warning.
   *Checked 2026-10-13* on the real Laya Q8_0 by `modelinfo_real_files.rs` (`LMGW_TEST_AUX_DIR`):
   all of these, threads left unset (the test host's 16 cores).
2. `aux_model_set` creates the row, and `local_model_test target=aux` passes.
3. `POST /v1/systemone` with `model: decide/laya` and Appendix A.4's body answers with the same
   keys.
4. With the GPU hold on, the same request answers 200 with no `x-lmgw-fallback`. Status shows no
   VRAM charge, and `podman inspect` shows no GPU device on the container.
5. A 7,669-token state answers. An 8,519-token state returns llama.cpp's error as decided in Q6,
   and both write a row.
6. `decide/laya` on `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings` and `/v1/rerank` is
   refused by name, and no container starts for it. `/tokenize` on it answers token ids.
7. The request row has ingress `systemone`, class `aux`, `prompt_tokens = input_tokens` and cost 0.
8. `/v1/models` lists the row with task `decision`, endpoints `["/v1/systemone"]` and output
   modalities `["decisions"]`.
9. With the row's image set to a pre-route build, the row's Test fails with llama.cpp's error and
   the hint to update the image (the plan loads nothing, owner ruling 2026-10-13).
10. Before WP2b ships: one existing CPU embedding row is measured with the GPU device passed (as
    today) and without it (after the hoist). The record is its VRAM in use per `nvidia-smi` and its
    embedding latency on a fixed batch. Q4's hoist ships with these numbers in its notice.
    *Measured 2026-10-12* (§3.3's table): with the device, ~5.7 GiB of compute buffers and a
    32-input batch in 2.2 s; without it, 0 VRAM and 50.6 s. The hoist therefore checks the image
    (Q4, refined).

*Measured 2026-10-13* (steps 1–9, live, on a dev instance of this slice's build, Laya Q8_0 on
`ghcr.io/ggml-org/llama.cpp:server` b11561 with 16 threads; every step passed):
1. Live through the op as well: kind `decision`, ctx 8192, ubatch = batch = 8192,
   `backend: cpu`, no pooling, no warning, threads unset (16 physical cores).
2. Created as `decide/laya`; `local_model_test` passed (`probe: decide`, 41 input tokens,
   `software`, 569 ms including the start).
3. 200 with A.4's keys, `input_tokens` 299, the options in the order sent, every value within
   0.01 of A.4.
4. 200, no `x-lmgw-fallback`. The hold's sweep kept it (`kept_on_cpu: ["aux/laya"]`); `/api/vram`
   showed `resident: []` and 0 estimated. `podman inspect`: no devices,
   `NVIDIA_VISIBLE_DEVICES=void`, argv `--ctx-size 8192 --ubatch-size 8192 --batch-size 8192
   --n-gpu-layers 0 --threads 16` with no kind flag. The hold's message said "audio models … keep
   serving (aux/laya)", fixed to "models that run on the CPU" (03d656b3).
5. A 7,654-token prompt answered in 15.9 s. 8,500 tokens returned llama.cpp's 500 verbatim
   ("input (8500 tokens) is too large to process. increase the physical batch size (current
   batch size: 8192)"). Both wrote a row, the 500 with that message.
6. Chat, completions, embeddings and rerank: each a 400 `unsupported` naming the row and
   `/v1/systemone`, with the container left stopped. `/tokenize` started it and answered ids;
   `/v1/count_tokens` answered 7.
7. Every row: ingress `systemone`, class `aux`, `prompt_tokens` = `input_tokens`,
   `completion_tokens` 0, cost 0 (`free_local`).
8. `decide/laya`: task `decision`, endpoints `["/v1/systemone"]`, outputs `["decisions"]`,
   context 8192, and the type and head-budget notes.
9. The class default of a fresh dir, `server-cuda` (b11176, 2026-09-25): the plan's probe said
   `supported: false` (`check_tensor_dims: tensor 'blk.28.ffn_up.weight' has wrong shape`) with
   the warning "the running llama.cpp build cannot load this model". The model test and a request
   failed with a 502 naming that load error (exit 1). No 404 was reachable: no older image that was tried (b11176, b11226, ik_llama.cpp b4972) loads Laya or Kev without the route. So the
   404 → 501 mapping is still proved only by its fake-container test.

Latency, the median of 10 after a warm-up, client wall time. The lmgw build was a debug build;
for Laya's one question, lmgw's own `total_ms` was 32–34 ms:

| Body | Input tokens | Through lmgw | Direct to the container |
|---|---|---|---|
| Laya (CPU), one `choice` | 34 | 40.0 ms | 32.2 ms |
| Laya (CPU), `choice`, `score`, two `noul` | 210 | 187.2 ms | 181.8 ms |
| Kev-4B (GPU), one `choice` | 34 | 20.5 ms | 13.3 ms |
| Kev-4B (GPU), the same four | 162 | 72.8 ms | 65.2 ms |

On a short state Kev's four questions cost about five times one, because the questions' own
spans dominate. Step 3 below shows the sharing on a long state.

**Acceptance, Kev-4B on GPU:**
1. The plan says kind `decision` (not rerank), `ctx_size 262144` with its KV cost, and
   `ubatch_size = batch_size = 2048` on the GPU. The rationale says 2048 is lmgw's choice over the
   effective 512.
2. The model test passes. One and four questions answer. The doc records the measured GPU latency
   for both and the compute buffer at ubatch 2048 against `headroom_mb` (§3.4).
3. A state of about 4,000 tokens, longer than the ubatch, answers four questions. This proves the
   state is split and shared and only the option span must fit (§3.4); the four-question latency
   should be well under four times the one-question latency.
4. A 255-option `choice` answers, or returns llama.cpp's 400 naming the batch it needs.
5. With the hold on, the row refuses with `gpu_hold`, or answers through a configured decision
   fallback with `x-lmgw-fallback`.

*Measured 2026-10-13* (steps 1–5, live, Kev-4B Q8_0 downloaded by the dev instance's own
`hf_add target=aux`, on `localhost/lmgw-llama-server:official-master` b11549, a 24 GB GPU, 4 slots,
f16 KV; nothing else was resident and ~22 GB were free, so the row kept the planned 262144; every
step passed):
1. Kind `decision` (`has_classifier_head`, not rerank), ctx 262144 with "about 8.0 GiB" of f16 KV
   in the rationale, ubatch = batch = 2048 with no backend, "lmgw's choice over the engine's
   effective 512". The plan's probe on b11549 said supported; on `server-cuda` b11176 it said
   `wrong number of tensors; expected 428, got 426`. A row at ctx 32768 stayed `in_sync`, with
   no change offered (WP4's tuning rule).
2. `local_model_test` passed (`probe: decide`, 40 input tokens, 2.9 s including the load).
   Latency is in the table above. The VRAM:

   | ctx, ubatch | `nvidia-smi` | weights | KV | recurrent state | compute buffer | lmgw's estimate |
   |---|---|---|---|---|---|---|
   | 262144, 2048 | 16,150 MiB | 4,265.6 | 8,192 | 201 | 3,044.1 | 12,518 MiB |
   | 262144, 512 | 13,866 MiB | 4,265.6 | 8,192 | 201 | 761.0 | 12,518 MiB |
   | 32768, 2048 | 8,104 MiB | 4,265.6 | 1,024 | 201 | 2,148.1 | 5,350 MiB |

   The middle columns are the CUDA0 buffers in MiB, read from llama-server's log at `-lv 4`.
   The token embeddings (644 MiB) are mapped on the host, and the host compute buffer is 1,104,
   276 and 208 MiB. The card held 3.5, 1.3 and 2.7 GiB more than the estimate, against
   `vram.headroom_mb` 1024 (§3.4).
3. The same state of 3,959 tokens (3,981 input tokens, beyond the 2048 ubatch): one question
   in 464.5 ms, four in 520.7 ms (1.12×), so the state was split and shared. The four-question
   answer's `usage.input_tokens` is 15,910: llama.cpp counts the state once per question though
   it evaluates it once, and the request row records those 15,910.
4. 255 options answered at 2048 (1,808 input tokens, 244 ms, the probabilities summing to 1). At
   ubatch 512 the same request got llama.cpp's 400 "the question and its options (1780 tokens)
   are too large to process. increase the batch size (current batch size: 512)". 256 options got
   its 400 "too many options (256), this model supports at most 255".
5. The hold's sweep stopped the container. The request got a 503 with `lmgw_code: gpu_hold` in
   llama.cpp's envelope. Then the fallback branch: a local row is refused as a fallback ("is
   itself a local model"), so Laya's own container was registered as a `llama_cpp` upstream.
   Its catalog entry published task `decision` from `["decisions"]`, and with it as Kev's
   fallback the request answered 200 with `x-lmgw-fallback: laya-direct/laya` and
   `x-lmgw-fallback-reason: hold`. Kev's container stayed stopped, and the row recorded
   `fallback_reason: hold`.

*Upgrade check (2026-10-13).* A read-only copy of an existing install's database (prefix
`lmgw-probe`) was booted on this slice's build, with no model started:
- Migration 0079 applied.
- The hoist logged its notices. CPU rows on `llama.cpp:server` moved to `backend: cpu`,
  `threads: 16`, empty args, "adopted as it is".
- A CPU embedding row on the CUDA class image with its own run args and no device moved only
  `--threads 16`, so it is replaced, not adopted. It kept `-ngl 0`, and its notice and finding
  say to set `backend: cpu`.
- Vision projectors moved to `mmproj_path`, each told it is now read under `/models`. One that
  was a directory is reported as a finding.
- `/v1/models`, compared with the same copy on the build before this slice: the same names, the
  entries identical except that a vision embedder now publishes `["text", "image"]`
  (`gguf+config`), and a row whose projector is a directory carries a note that lmgw cannot read
  it.
- Boot reconciliation adopted 0 and removed 0; the containers on the machine were the same
  before and after.

### 6.2 Slice 2: OpenRouter

- The catalog fetch sends `output_modalities=all` to OpenAI-protocol upstreams. Before that, a
  live check that every OpenAI-protocol upstream's `/models`, and llama-server's, ignores the
  parameter; if one does not, it becomes a per-row catalog option. (`parse_task_openai` learned
  `decisions` and OpenRouter's plural `embeddings` in slice 1, WP4: the plural was one line.)
- Price sync for Jev.
- Tests: slice 1's wiremock OpenRouter, now priced from a catalog fixture built from Appendix B.3;
  the fallback from a held GPU decision row to `jev`, priced under `jev`.
- **Acceptance (live; needs an OpenRouter account and costs a few cents):**
  1. One request through the alias `jev` (→ `typesafe/jev-1.13`), and one through the
     passthrough name `openrouter/typesafe/jev-1.13`. 275 input tokens cost about $0.00001 each.
     *Not run: it needs a funded OpenRouter account.*
  2. Both rows are priced at $0.042 per Mtok, source `catalog`: the alias's row and, the
     upstream being `expose_all`, the upstream-scoped row the passthrough name prices through.
     *The price rows were checked live (below); the priced request rows wait for step 1.*
  3. `/v1/models` lists Jev with task `decision`. *Checked live 2026-10-14 (below).*
  4. The local-to-Jev fallback answers under the hold, priced under `jev`. *Not run: it
     needs a funded OpenRouter account.*

The owner approved slice 2 on 2026-10-14, the paid acceptance excepted, and the free part was
built the same day.

*The compatibility check (2026-10-14).* A dev instance on a copy of an existing install's data
dir, with its upstreams and an OpenRouter row added under a key whose account had no credits, fetched
each OpenAI-protocol catalog twice, bare and with `?output_modalities=all`, through a temporary
probe build. Gemini's OpenAI-compatible endpoint was asked with the key of a `gemini`
row. llama-server ran in a throwaway CPU container (`official-master`, b11549, embeddinggemma-300M,
no GPU device). Every request was a `GET` of a model list.

| Upstream (protocol) | Request | Bare | With `output_modalities=all` | Models the parameter adds |
|---|---|---|---|---|
| OpenAI (`openai`) | `GET https://api.openai.com/v1/models` | 200, 135 | 200, 135, the same ids | none; the list states no modalities |
| Kilo's gateway (`openai`) | `GET https://api.kilo.ai/api/gateway/models` | 200, 393 | 200, 393, the same ids | none; its default list already holds 12 image-and-text and 4 audio-and-text chat models |
| OpenRouter (`openai`) | `GET https://openrouter.ai/api/v1/models` | 200, 461 | 200, 664 | 203: 22 `decisions` (Jev in both ids among them), 37 `embeddings`, 9 `rerank`, 33 `speech`, 25 `transcription`, 47 `image`, 30 `video` |
| Gemini's OpenAI-compatible endpoint | `GET https://generativelanguage.googleapis.com/v1beta/openai/models` | 200, 63 | **400** `INVALID_ARGUMENT`: "Unknown name \"output_modalities\": Cannot bind query parameter" | n/a |
| llama-server b11549 | `GET /v1/models`, `GET /models` | 200, 1 | 200, 1, byte for byte the same | none |
| Anthropic (`anthropic`), a Gemini row (`gemini`) | not asked | | | not OpenAI-protocol: their catalog requests do not change |

An install with no OpenAI-protocol row on Gemini's host (a Gemini row speaks `gemini`) is not
affected: the 400 would hit only an install that reaches Gemini through its OpenAI-compatible
endpoint. `/v1/models` of the dev copy matched the original's entry for entry before the change,
the same capabilities, and after it too, plus OpenRouter's list.

*As built (2026-10-14, with the review's fixes of the same day):*
- **The catalog request** (`catalog/request.rs`): the `GET {base}/models` of an `openai`
  upstream of kind `generic` carries `?output_modalities=all` while the row's new
  `catalog_all_modalities` is on. An `openai` row of another kind (an audio.cpp server) was never
  asked and stays bare, and so does a `llama_cpp` row, which lists every model it serves, and
  the Anthropic and Gemini requests. The upstream Test sends the catalog's own request, so it
  fails where the catalog would. A catalog or Test that fails with a 400 or 422 while asking
  names the option in its error.
- **The option is per row, as §6.2 said it would be once one upstream refused.** Migration 0080
  adds `upstreams.catalog_all_modalities`, on for every row except an `openai` row on Gemini's
  host, the one endpoint measured to refuse. A new row starts on. It is on `upstream_set`,
  `upstream_set_full`, `GET /api/upstreams` (`UpstreamView`), `lmgw__upstream_set` and the
  dashboard's upstream form ("list every output modality", shown for the `openai` protocol).
  `NewUpstream` carries it, so a row is created or updated with it in one statement. `Upstream`
  does not: the snapshot's `catalog_bare` set names the rows where it is off.
- **The tasks of the outputs the full list adds.** Without a mapping, OpenRouter's 203 new
  entries would have published as chat models with the chat routes. `parse_task_openai` now gives
  a model whose outputs name no text the task of the first of these it names: `decisions` →
  `decision`, `rerank` → `rerank`, `speech` → `tts`, `transcription` → `asr`, `image` →
  `image_generation`, `video` → `video_generation`. Each is a task lmgw already had. A model that
  outputs text as well stays chat, and `["audio"]` and `["text", "audio"]` stay chat, as before.
  Output modalities are published as the catalog states them (`speech`, `transcription`,
  `embeddings`), which `KNOWN_MODALITIES` does not list, so an owner override on such an alias
  has to restate `output_modalities` (a pre-existing limit, unchanged here).
- **The routes each task publishes, checked against OpenRouter's documentation** (its API
  reference, read 2026-10-14; every path is under `https://openrouter.ai/api/v1`). Protocol
  fidelity: a route is published only where lmgw's route posts to a path the provider documents.

  | Task (OpenRouter output) | OpenRouter documents | lmgw posts to | Published |
  |---|---|---|---|
  | `decision` (`decisions`) | `POST /systemone`, TypeSafe's System One wire | `{base}/systemone` | `["/v1/systemone"]` |
  | `embedding` (`embeddings`) | `POST /embeddings`, OpenAI's shape | `{base}/embeddings` | `["/v1/embeddings"]` |
  | `rerank` (`rerank`) | `POST /rerank`: `model`, `query`, `documents`, `top_n` | `{base}/rerank` | `["/v1/rerank"]` |
  | `tts` (`speech`) | `POST /audio/speech`: `model`, `input`, `voice`, `response_format` | `{base}/audio/speech` | `["/v1/audio/speech"]` |
  | `asr` (`transcription`) | `POST /audio/transcriptions`, JSON or OpenAI-style multipart | `{base}/audio/transcriptions` | `["/v1/audio/transcriptions"]` |
  | `image_generation` (`image`) | `POST /images` (the Image API) | `{base}/images/generations` | none, with a note |
  | `video_generation` (`video`) | `POST /videos`, then polling | `{base}/images/generations` | none, with a note |
  | `chat` (`image` and `text`) | `POST /chat/completions` with `modalities: ["image", "text"]` | the chat routes | the chat routes; outputs `["image", "text"]` kept |

  So a cloud catalog's image or video model publishes no endpoint, and its note says that no
  lmgw route is verified to reach it and what OpenRouter documents instead
  (`capabilities::task::unverified_media_route_note`). This revises image-generation design
  §5, which gave such a model `/v1/images/generations` (and `/v1/images/edits` with an image
  input). An alias's `capabilities_override` states the routes of a provider that serves them.
  The Image lab lists only what publishes `/v1/images/generations`, and the dashboard's model
  pickers no longer offer a model that publishes no endpoint as one of their tasks
  (`CatalogEntry::routable`).
- **Price sync** (`catalog::price_updates_for_upstream`, `catalog/price_rules.rs`). Once the
  catalog lists Jev, its per-token strings become a `per_mtok` catalog row: $0.042 in and 0 out,
  under the alias that names the model and, on an `expose_all` upstream, also under the upstream
  scope (`<upstream id>:typesafe/jev-1.13`). Without the second row a request through the
  passthrough name of an aliased model would find no price. A request row is priced from it like
  any token row (§2.6). Two kinds of model are left unpriced, with no row, because a `per_mtok`
  row made of their catalog price would price them wrong downward:
  - the tasks billed in another unit: `tts` (per input character), `asr` (per second of audio:
    Scribe v2's 0.0000306 is $0.11 an hour), `image_generation` and `video_generation` (per image,
    second or output image token, in fields no row takes), and `rerank` (per search unit, at a
    catalog price of 0/0);
  - a model whose `prompt` and `completion` are both 0 beside a non-zero field no row takes
    (`image_output`, `audio_output`, `web_search`, …). A model whose every field is 0 is really
    free and keeps its 0 row.

  Over OpenRouter's full list of the same day (668 entries), 515 models are priced (456 chat,
  22 decision, 37 embedding) and 153 left unpriced (25 asr, 35 tts, 47 image, 30 video, 9 rerank
  and the 7 routers priced `-1`, such as `openrouter/auto`, unpriced before as well). No chat
  model of that list has free tokens beside a charging field. A manual row prices any of them.
- **Tests** (`tests/it/openrouter_catalog.rs`, fixture `catalogs/openrouter_all_modalities.json`,
  captured on 2026-10-14 with Jev's entry as in Appendix B.3; unit tests in
  `catalog/openrouter_tests.rs`, `catalog/price_rules.rs`, `catalog/request.rs`): a mock
  OpenRouter whose default list holds the text-output entries and whose full list is the fixture.
  The catalog request carries the parameter, and `/v1/models` lists Jev (both ids and the alias
  `jev`) with task `decision`, endpoints `["/v1/systemone"]` and outputs `["decisions"]`; every
  other output publishes the routes of the table above, the image and video models none with
  the note, and the Image lab does not list them. A Gemini-shaped upstream answering the
  parameter with a 400 names the option in its catalog error and its Test, lists bare once
  `upstream_set` turns the option off, and `upstream_set_full` turns it back on. `prices_sync`
  writes Jev's rows under `jev` and the upstream scope and leaves the fixture's speech,
  transcription, rerank, image and video models and its router unpriced; slice 1's documented
  OpenRouter answer (275 in, 20 out) is priced from the catalog, 12 micro-dollars in and 0 out,
  through `jev` and both passthrough names. A held GPU decision row whose fallback is `jev`
  answers with `x-lmgw-fallback: jev`, sends `typesafe/jev-1.13` upstream, starts no container,
  and is priced under `jev`. Migration 0080 has its own test
  (`tests/it/migrations/upstream_catalog_all_modalities.rs`).

*Live, on the dev copy with the first build (2026-10-14, free reads only):*
- Step 3: `/v1/models` listed `openrouter/typesafe/jev-1.13` and `openrouter/~typesafe/jev-latest`
  with task `decision`, endpoints `["/v1/systemone"]`, outputs `["decisions"]`, context 64000,
  source `catalog`. `typesafe/jev-router` stayed chat. Of OpenRouter's 666 entries that day, 461
  published as chat, 22 as decision, 37 as embedding, 9 as rerank, 35 as tts, 25 as asr, 47 as
  image generation and 30 as video generation. The other 658 entries matched the original's.
- Step 2, its price half: `prices_sync` wrote `typesafe/jev-1.13` and `~typesafe/jev-latest` at
  0.042 in and 0 out, source `catalog`. That build also wrote 0/0 rows for image and video models
  and token rows for speech and transcription models; the review's price rules above replaced
  that, and their counts are from the rules run over the full list. The upstream Test passed on
  OpenRouter, OpenAI and Kilo's gateway with the parameter.
- Steps 1, 4 and the priced request rows of step 2 were not run (they need a funded account). No request went to
  `/systemone` or to any generation route.

**What the paid acceptance needs from the owner.** An upstream row (Upstreams → Add upstream, or
`upstream_set action=create`): name `openrouter`, protocol `openai`, kind `generic`, base URL
`https://openrouter.ai/api/v1`, the OpenRouter key as its API key, `expose_all` on with prefix
`openrouter`, "list every output modality" (`catalog_all_modalities`) left on, native
`/v1/responses` off, timeout as the default. An alias `jev` → `typesafe/jev-1.13` on it. Then
`prices_sync`, which writes Jev's row under `jev` and under the upstream scope, and the four
steps above: step 1 through `jev` and through `openrouter/typesafe/jev-1.13`, both priced at
$0.042 per Mtok. Step 4 needs a decision row on the GPU (Kev-4B, say) with
`hold_fallback_mode: alias` and `hold_fallback: jev`.

## 7. Questions for the owner

The owner decided all six on 2026-10-12, as recommended (Decisions 5–10). The numbering stays,
because the text and source comments cite the questions by number.

1. **OpenRouter in slice 1? Recommended: no, slice 2.** *Decided 2026-10-12: as recommended.*
   Forwarding to an OpenAI-protocol upstream is slice 1's code path anyway, so OpenRouter works in
   slice 1 with a manual price row, at no extra cost. Slice 1's wiremock test with OpenRouter's
   documented answer covers it (§6.1). Slice 2 adds `?output_modalities=all` and price sync, which
   change the catalog fetch for every OpenAI-protocol upstream. Its live check needs an OpenRouter
   account and costs a few cents.
2. **Kind: `AuxKind::Decision`, a class of its own, or a chat flag? Recommended: the aux kind**
   (§3.1). *Decided 2026-10-12: as recommended.* It is the smallest change that keeps decision rows
   out of chat surfaces. It is also how llama-server runs these models, and the aux row's missing
   fields need fixing for embedders anyway.
3. **Prefix? Recommended: `decide/`**, a second prefix on the aux class, specified in §3.10 (an
   `AuxRouterSettings` wrapper, because `RouterSettings` is shared with chat; one public name per
   row, so `embed/laya` does not resolve). *Decided 2026-10-12: as recommended.* It is a verb like
   `embed/` and short. The alternatives are `embed/` (no setting and no §3.10; rerankers already
   live there, but `embed/laya` misleads) and `systemone/` (the protocol's name, longer).
4. **Hoist `--n-gpu-layers 0`, `--threads`, `--ubatch-size`, `--batch-size` and `--mmproj` from
   existing aux rows' args into the new fields automatically? Recommended: yes, with a start notice
   naming each row, after the measurement below.** *Decided 2026-10-12: as recommended.* It applies
   the owner's rule that CPU rows serve under the hold to existing CPU embedding rows, which the
   hold pauses today. Three things come with it:
   - It is a new aux fold. Three of the five flags are not in chat's `PROMOTED_ARGS` (§3.3).
   - It reorders argv, so on the first boot each affected aux container is re-created rather than
     adopted (`config/llama_params.rs:35-44`). That is a one-time restart of those rows.
   - The rows' containers stop receiving the GPU device. A CUDA build at `-ngl 0` can still use the
     GPU, so this is a behaviour change. One CPU embedding row is measured before and after (its
     VRAM in use and its embedding latency) before the hoist ships (§6.1).

   *Refined by the owner on 2026-10-12, after the measurement (§3.3, §6.1 step 10):* the hoist
   moves `--n-gpu-layers 0` into `backend: cpu` only when the row's effective image has no GPU
   backend (no libggml-cuda or libggml-vulkan). A row on a CUDA image keeps GPU placement, exactly
   as today; its start notice names the trade-off with the measured numbers (~5.7 GiB and 2.2 s per
   32-input batch with the device, 0 and 50.6 s without), so switching it to `backend: cpu` is a
   deliberate choice. `--threads`, `--ubatch-size`, `--batch-size` and `--mmproj` hoist as
   before.
5. **Protocol types in `lmgw-api-types::systemone`, with the schema generated from them?
   Recommended: yes.** *Decided 2026-10-12: as recommended.* README "Typed answers" says the `/v1`
   protocol routes have hand-written schemas and are "held to the protocol, not to an lmgw type".
   Here lmgw is itself a client of the wire: the model test now, the barge-in and guardrail checks
   later. One type set serves the schema and those callers, and the drift tests hold it to
   llama.cpp's, TypeSafe's and OpenRouter's answers. The alternative is a `json_schema!` in
   `openapi/v1/systemone.rs` plus private structs in lmgw-core, which is two descriptions of one
   wire. Because the recommendation landed, WP5 also amends the README paragraph
   (`README.md:434-437`) to name System One as the `/v1` protocol whose schema comes from
   `lmgw-api-types`.
6. **llama.cpp's 500 for a prompt over the ubatch: relay or map? Recommended: relay.** *Decided
   2026-10-12: as recommended.* On a non-causal row planned per §3.4 it only fires when a prompt
   exceeds the trained context. On a causal whole-prompt type (Clef) it fires past the planned 2048,
   and its message names the fix. The message ("increase the physical batch size") is the engine's,
   and lmgw relays the engine's answers on this route as `/tokenize` does. Mapping it to a 400 would
   make lmgw's answer differ from llama-server's for the same request.

**Open items for the owner.** Slice 1's final review (2026-10-13) left these as the owner's calls.
The owner decided all of them on 2026-10-13, and they were built the same day (Decisions 23–31).
- (a) Plan `ubatch_size` = `batch_size` = ctx for encoders llama-server cannot split. *Decided
  2026-10-13: yes.* Before, an embed or rerank plan set no ubatch, so such a row took
  llama-server's 512 tokens per input whatever its context. As built:
  - The aux plan sets both to the trained ctx for an embed or rerank model whose input llama-server
    cannot split. The rule is llama.cpp's `can_split`, as `runtime::aux::splits_input`, read from
    the header's `attention.causal` and the pooling: only a last-token embedder or a causal
    reranker splits. That covers embeddinggemma, whose header has no `attention.causal`, through
    its mean pooling. Code: `modelinfo/aux_batch.rs`.
  - A causal embedder whose header declares no pooling gets no ubatch and a warning: the row's
    pooling decides. With `last` the defaults serve; with mean, cls or none, set ubatch and batch to
    ctx, or inputs over 512 fail with a 500.
  - `configured_as` offers the ubatch and batch change only to rows that do not split by their own
    pooling (else the header's). Placement (`backend`, `threads`) stays uncompared.
  - The causal-reranker split exists from llama.cpp b11223 (4da6337, #28876) on. The ctx rationale
    and the knowledge-base limit (`knowledge/limit.rs`) name that build. The limit counts a causal
    reranker's chunk up to its ctx, and a BERT-family reranker or an unreadable header at the
    ubatch in effect. Nothing is gated on the build number.
  - An encoder read from its pooling alone (no `attention.causal` key) names, in its ubatch
    rationale, what a decoder of that shape would add.
  - An aux plan that proposes a projector llama.cpp decodes non-causally gets the chat plan's
    projector ubatch floor, through the shared `modelinfo/projector_floor.rs`. qwen3vl is causal,
    so Qwen3-VL embedders do not change.
  - Measured on b11549 and b11561: a causal Qwen3-Embedding-0.6B took a 3491-token input and a
    causal Qwen3-Reranker-0.6B a 3500-token document at the default ubatch. Non-causal
    embeddinggemma-300M (ctx 2048) refused 1492 tokens with "input (1492 tokens) is too large to
    process. increase the physical batch size (current batch size: 512)" and took them at
    ubatch = batch = 2048.
- (b) The ledger's under-count of a CUDA-image aux row at `-ngl 0`. *Decided 2026-10-13: charge the
  compute buffer.* As built:
  - `vram/compute.rs` derives llama.cpp's compute buffer from the header (vocabulary, `n_embd`, the
    largest per-layer `feed_forward_length`, `n_deepstack_layers`, the output head's bytes;
    `gguf/tensors.rs` reads tensor sizes from the data offsets) plus the row's ubatch, batch, ctx
    and flash attention.
  - `vram/plan/aux.rs` charges it on GPU aux rows. A row with `-ngl 0` and no `backend` whose run
    args pass a GPU device is charged its compute buffer plus its projector (it was 0).
    `backend: cpu`, and `-ngl 0` without a device, stay at 0.
  - llama.cpp's four false values for `-fa` (off, disabled, false, 0) count as off. A header
    declaring `attention.causal = false` is charged no KV cache (measured 0). A mean- or
    cls-pooled header without `attention.causal` keeps the encoder figure and its KV, and its
    footprint note names the uncharged decoder difference.
  - A header missing a value the formula needs gets a note ("compute buffer not sized: …") and no
    term, never a guess.
  - The whole buffer is charged, not only the part above the default ubatch. The headroom setting
    did not reliably absorb it (4 x vocab x 512 alone is 296 MiB for Qwen3 and 485 MiB for Kev,
    and each process adds a measured 0.38 to 0.44 GiB CUDA context), so headroom now covers the
    CUDA context, allocator slack and a projector's own compute buffer.
  - Chat estimates are unchanged. A follow-up would charge chat rows after chat probes (split KV
    with `parallel`, MTP drafters and the SWA mask are unvalidated). The status `vram.note` and the
    admission explainer no longer call the total a lower bound. *Done 2026-10-14 (the owner
    approved it the day before):* chat probes on real models, and the chat footprint charges
    every context's compute buffer and lays the KV cache out for slots, windows and speculative
    decoding (Appendix C.2).
- (c) Host paths in `/v1/models` notes. *Decided 2026-10-13: strip them.* As built:
  `without_host_path` (`capabilities/exposed.rs`) strips the models dir from a projector's read
  error in chat (`local_derived`) and aux (`aux_derived`) notes. An empty or "/" dir leaves the
  error unchanged. Notes name files by their container path (`/models/…`) or relative path. The
  other note builders (audio, image, catalog) carried no host path.
- (d) The plan-time image probe, which needed self-admin `full`. *Decided 2026-10-13: the gate is
  the MCP plane's. The same day the owner ruled that the Test button is the only thing that loads a
  model on request, so the probe was removed from plan and inspect.* As built:
  - `probe_runtime`, `modelinfo/agent_probe.rs` and the `probe` argument are gone, with the
    `RuntimeProbe` type and the `runtime` field of the plan and inspect answers. The plan warning
    "the running llama.cpp build cannot load this model" went with them. The self-admin question
    is moot.
  - `local_model_test`'s load-failure classifier stays: it is now the one place an unknown
    architecture shows before a request does, with its hint to update the image (Backends page).
  - The projector warning ("projector support is not verified by the plan") now shows whenever a
    plan carries an `mmproj`.
- (e) Relaying `Retry-After` on `/v1/systemone`. *Decided 2026-10-13: relay it.* As built:
  `RelayedHead` in `proxy/relay.rs` keeps `Content-Type` and `Retry-After` on `/v1/systemone` and
  `/tokenize`. No other header is relayed. lmgw's own 502 (upstream 401 or 403) and 501 (a local
  decision row's 404) carry none.
- (f) The option order in the model test's answer. *Decided 2026-10-13: keep it.* As built:
  `ModelTestPassed.answer` for a decision row is `DecideAnswer {choice, probabilities:
  [{option, probability}], confidence}` in llama.cpp's order. Before, it was a keyed map that the
  op's JSON sorted by name. The route's own wire is unchanged. The dashboard still shows the most
  probable option first.
- (g) The licence chip. *Decided 2026-10-13: show it.* §1.5 and §3.9 say a decision row shows its
  licence, and no local row read `general.license` before. Built as §3.9 describes. Follow-up: the
  chat and aux editors do not show it yet.
- (h) The pre-slice `embed_model_set` op name, which `web/api.rs` still accepted for an older SPA
  bundle. *Decided 2026-10-13: remove it, under the alpha's no-compat rule.* As built:
  `embed_model_set` is removed, with `OpArgs::AliasOf`, the `x-lmgw-alias-of` extension and the API
  page's alias chip. No caller existed in the SPA, `lmgw-client` or Kai. Two other
  pre-rename spellings were removed the same day: target `embed` (for `aux`) in the HF, container,
  model-listing and class-target tools, and protocol `openai` with kind `llama_server` (for protocol
  `llama_cpp`). Each is refused with the valid values named. No stored value could hold either: the
  `hf_models.target` CHECK has excluded `embed` since migration 0018, which rewrote it, and 0058
  migrated the upstream rows.

Two more entries came out of the same work and are not lettered items:
- **A plan that cannot fit the card says so** (`modelinfo/plan_fit.rs`, aux and chat plans).
  The plan compares footprint + `vram.headroom_mb` with the capacity admission uses
  (`vram.budget_mb`, else the devices' total). When it does not fit, it warns with the estimate and
  its terms, the capacity, and the largest `ctx_size` that fits (a binary search over the same
  estimate). The plan keeps the trained ctx. The refusal is promised only while admission is on
  (`vram.enabled`); with admission off, the warning says nothing refuses the start and llama-server
  tries anyway. CPU plans, and installs with nothing to measure against, are not checked. Example:
  a Qwen3-VL-Embedding-8B row at its trained 262144 needs about 41.3 GiB on a 24 GiB card, and the
  largest ctx that fits is 118618.
- **`gguf_files` used_by** counts an aux row's `mmproj_path` and its `--mmproj` or `-mm` args, so a
  projector an aux row uses no longer shows as unused.

## Found while reading

- **Embedding inputs and the ubatch.** llama.cpp refuses any task that cannot be split when it
  exceeds `n_ubatch` with the same 500 (`server-context.cpp:3555-3564`). `--embeddings` forces
  `n_batch = n_ubatch = 512` (A.10). Aux rows render no ubatch, and the aux plan's ctx rationale
  said inputs past the context "are truncated, not rejected" (now `ctx_rationale` in
  `modelinfo/aux_plan.rs`). Whether long inputs to existing embed rows hit the 500 was not
  checked here. §3.3's fields let the plan
  set it. *Fixed in WP4 and the final review, except the plan:* §3.3's fields render the ubatch
  and batch. The ctx rationale says llama-server refuses an over-long input, and that the 512
  default bounds a non-causal encoder while `ubatch_size` is unset. The knowledge-base chunk check
  (`knowledge/limit.rs`) reads the ubatch and batch in effect: the fields, else a copy in the
  args. What remained was that no ubatch is planned for embed or rerank rows. *Fixed 2026-10-13 (owner
  item a):* the plan sets it for an encoder llama-server cannot split (§7).
- **The aux footprint does not charge a projector** passed in `args` (`vram/plan.rs:302-322`), so
  a VL embedder's projector goes uncharged. *Fixed in WP2:* the footprint charges `mmproj_path`,
  and a `--mmproj` or `-mm` the args still carry.
- **The ledger under-counts a CUDA-image aux row at `-ngl 0`.** `vram/plan.rs` charges it nothing,
  but with the GPU device passed it holds ~5.7 GiB of compute buffers at ub 8192 (§3.3's table),
  and that scales with the ubatch and batch. The owner wanted it filed as a separate fix, outside
  this slice (owner item b). *Fixed 2026-10-13 (owner item b):* the aux footprint charges the
  compute buffer, and a row at `-ngl 0` with a GPU device is charged it plus its projector
  (`vram/compute.rs`, `vram/plan/aux.rs`; Appendix C).
- **A relayed 429 or 503 loses `Retry-After`.** `/v1/systemone` relays status, content type and
  body, and no other header (§2.1 step 6), so TypeSafe's 429 or an OpenRouter rate limit reaches the
  client without the upstream's `Retry-After`. `/tokenize` relays the same way. Relaying that one
  header is a small change once a client needs it (owner item e). *Fixed 2026-10-13 (owner item
  e):* `proxy/relay.rs` relays `Retry-After`, for `/v1/systemone` and `/tokenize`.
- **OpenRouter's catalog spells `embeddings`, plural**, where `parse_task_openai` matches
  `embedding` (`catalog.rs:866`), and its default list omits every non-text model. An OpenRouter
  embedder would publish as `chat`. *Fixed in WP4* for the plural; the default list's omission is
  slice 2's. *Fixed in slice 2:* the catalog request asks for every output modality, and the
  outputs the full list adds take their own tasks (§6.2).
- **The plan-time image check needs self-admin `full`** (live acceptance 2026-10-13). The
  runtime probe declined at `read_only` (also on the dashboard's own plan route), and a fresh
  install starts at `read_only`, so §3.6's first check stayed silent there. *Resolved 2026-10-13:*
  the probe is gone (owner item d).
- **The model test's answer loses the option order** (live acceptance). `local_model_test`
  returns `answer.probabilities` sorted by name, because the op's JSON goes through a sorting
  map. The route itself keeps the order (§2.1). It is cosmetic, and the dashboard shows it (owner
  item f). *Fixed 2026-10-13 (owner item f):* the answer carries `probabilities` as a list in
  llama.cpp's order.
- **`/v1/models` notes carry host paths** (live acceptance). An unreadable projector's note
  quotes the read error with the host path (`<models dir>/…/Qwen3-VL-Embedding-8B-GGUF: … Is a
  directory`). Chat rows did this before (`capabilities/notes.rs`), and aux rows now do too (owner
  item c). *Fixed 2026-10-13 (owner item c):* `without_host_path` strips the models dir from both.

## Appendix A: probe transcript (2026-10-12, trimmed)

### A.1 Setup

- Image: a llama.cpp master build (CUDA, built by the Backends page), `build_info`
  `b11549-aa94f20`.
- `ggml-org/Laya-GGUF` `Laya-Q8_0.gguf`, 449,397,600 bytes, repo sha 22265007.
- `podman run` with `-p 127.0.0.1:<port>:8080`, the model dir mounted `:ro,Z`, no GPU device,
  `NVIDIA_VISIBLE_DEVICES=void` and `-ngl 0`.
- The server logs "no usable GPU found" and runs 16 threads.
- Repeated on `ghcr.io/ggml-org/llama.cpp:server` (b11561, e937b17a2) with the same answers and
  latency within 5 %.

Load took 0.16 s. The decision lines of the start log:

```
I decision model reads the embeddings output, enabling embedding mode
W embeddings enabled: setting n_batch = n_ubatch = 512
I srv init: decision model type: laya
I srv load_model: initializing, n_slots = 4, n_ctx_slot = 8192, kv_unified = 'true'
```

### A.2 Discovery

`GET /v1/models` (OpenAI part; the Ollama-compat `models[0].capabilities` says `["completion"]`):

```json
{"id": "laya", "aliases": ["laya"], "object": "model", "owned_by": "llamacpp",
 "architecture": {"input_modalities": ["text"], "output_modalities": ["decisions"]},
 "meta": {"n_ctx": 8192, "n_ctx_train": 8192, "n_embd": 1024, "n_params": 421029889,
          "size": 447612932, "ftype": "Q8_0"}}
```

`GET /props` has no decision field. It reports `"build_info": "b11549-aa94f20"`,
`"total_slots": 4`, `"modalities": {"vision": false, "video": false, "audio": false}` and
`default_generation_settings.n_ctx: 8192`. Laya's trained context is 8192
(`modern-bert.context_length`).

### A.3 Request

```json
{"state": "Hi, my invoice from last month charged me twice for the Pro plan. Can you refund one of the charges? This is the second time this has happened and I am pretty annoyed.",
 "questions": {
  "department": {"type": "choice", "instructions": "Which team should handle this ticket?",
    "criteria": {"billing": "payments, invoices, refunds, plan charges",
                 "technical": "bugs, outages, errors in the product",
                 "sales": "new purchases, upgrades, quotes", "other": null}},
  "department_bare": {"type": "choice", "instructions": "Which team should handle this ticket?",
    "criteria": {"billing": null, "technical": null, "sales": null, "other": null}},
  "urgency": {"type": "score", "instructions": "How urgent is this ticket?",
    "criteria": ["not urgent: can wait a week", "normal: within two days",
                 "urgent: same day", "critical: immediately"]},
  "angry": {"type": "noul", "instructions": "The customer is upset."}}}
```

### A.4 Answer (200, 0.36 s cold; values shortened to four places)

```json
{"model": "laya",
 "answers": {
  "department": {"type": "choice", "choice": "billing",
    "probabilities": {"billing": 0.9671, "technical": 0.0097, "sales": 0.0094, "other": 0.0138},
    "confidence": 0.9561},
  "department_bare": {"type": "choice", "choice": "billing",
    "probabilities": {"billing": 0.9457, "technical": 0.0218, "sales": 0.0173, "other": 0.0151},
    "confidence": 0.9277},
  "urgency": {"type": "score", "score": 1.6808,
    "legend": {"0": "not urgent: can wait a week", "1": "normal: within two days",
               "2": "urgent: same day", "3": "critical: immediately"},
    "probabilities": {"0": 0.0749, "1": 0.2918, "2": 0.5109, "3": 0.1224},
    "confidence": 0.4360},
  "angry": {"type": "noul", "noul": 0.8319}},
 "usage": {"input_tokens": 299, "output_tokens": 0}}
```

### A.5 State shapes (one `choice`, three options)

| `state` | Result |
|---|---|
| object `{"customer": "Ana", "plan": "Pro", "message": "charged twice, want a refund"}` | 200, `input_tokens` 43, billing 0.967 |
| messages array (user, assistant, user) | 200, `input_tokens` 72, billing 0.983 |
| `null`, or absent | 400 `"state" must be provided` |

### A.6 Validation and edge cases

Every error has the envelope `{"error": {"code": N, "message": "…", "type": "…"}}`.

| Request | Status, `type`: message |
|---|---|
| question `type: "rank"` | 400 `invalid_request_error`: `questions.q: "type" must be one of: choice, score, noul` |
| `score` with 1 or 11 levels | 400: `questions.q: "criteria" must be an array of 2 to 10 levels` |
| `questions` missing, `{}` or an array | 400: `"questions" must be a non-empty object` |
| body `nope` | 400: `[json.exception.parse_error.101] parse error at line 1, column 2: …` |
| `choice` with 256 options | 400: `questions.q: too many options (256), this model supports at most 255` |
| `choice` with 255 options, default ubatch | 500 `server_error`: `input (774 tokens) is too large to process. increase the physical batch size (current batch size: 512)` |
| `images: ["data:image/png;base64,…"]` | 501 `not_supported_error`: ``This server does not support image input for decisions. For a model that supports it, start it with `--mmproj` `` |
| `files: ["data:text/plain;base64,aGVsbG8="]` | 400: `Invalid uri format: data:text/plain;base64` |
| `images: []` | 200 |
| `choice` with 1 option | 200, probability 1.0, confidence 1.0 |
| `stream: true` | ignored: 200 JSON |
| `model: "nonexistent"` | ignored by a single-model server: 200, `"model": "laya"` |
| `noul` with `criteria: {"true": …, "false": …}` | 200 |

### A.7 Other routes against the decision model

| Route | Answer |
|---|---|
| `POST /v1/chat/completions`, `POST /completion` | 500 `server_error`: `the current context does not support logits computation. skipping` |
| `POST /v1/embeddings` | 400: `Pooling type 'none' is not OAI compatible. Please use a different pooling type` |
| `POST /v1/rerank` | 501: ``This server does not support reranking. Start it with `--reranking` `` |
| `POST /tokenize` | 200 `{"tokens": [25521, 1533]}` |
| `GET /v1/systemone`, `POST /systemone` | 404 `{"error": {"message": "File Not Found", "type": "not_found_error", "code": 404}}` |

### A.8 The ubatch

| State | Default (512) | `--ubatch-size 8192 -b 8192` |
|---|---|---|
| 699 tokens | 500, batch size 512 | 200, 0.68 s median of 5 |
| 7,669 tokens | 500 | 200, 14.9 s |
| 8,519 tokens (over the trained 8192) | 500 | 500, `current batch size: 8192` |

The container's memory was 2.22 GB after the 7,669-token request. The weights are mapped and not
counted. Small requests were unaffected by the raised ubatch: 33.2 ms against 32.8 ms.

### A.9 Latency (CPU, 16 threads, one warm-up, then 10 runs)

| Body | Input tokens | Median | Min | Max |
|---|---|---|---|---|
| one `choice` | 36 | 32.8 ms | 32.2 | 39.7 |
| `choice`, `score`, two `noul` | 183 | 156.4 ms | 156.0 | 168.3 |
| one `choice`, ghcr b11561 | 36 | 34.4 ms | 33.9 | 36.3 |
| four questions, ghcr b11561 | 183 | 162.4 ms | 158.8 | 194.2 |

Four questions cost four passes, because Laya shares no state across questions.

### A.10 Today's aux argv on Laya

- `--reranking --ctx-size 8192 --n-gpu-layers 0`, what today's plan would produce: the server logs
  "setting n_batch = n_ubatch = 512", then aborts with
  `GGML_ASSERT(ggml_can_mul_mat(a, b)) failed` in `llm_graph_context::build_pooling`. The container
  exits with 139.
- `--embeddings --ctx-size 8192 --n-gpu-layers 0`: it loads. The server enables embedding mode
  itself, `/v1/systemone` answers as in A.4, and `/v1/embeddings` answers the 400 of A.7.

### A.11 An image from before the route

A master build at b11226 (0c6a6a7):

```
E llama_model_load: error loading model: check_tensor_dims: tensor 'blk.28.ffn_up.weight' has wrong shape; expected 1024, 5248, got 1024, 4096, 1, 1
E srv llama_server: exiting due to model loading error
```

The container exits with 1, so there is no 404 to see. The per-layer `feed_forward_length` array
of Laya's header is newer than that loader.

## Appendix B: GGUF headers

### B.1 Decision keys

| File | Keys under `{arch}.decision.` and related |
|---|---|
| Laya Q8_0 | `type: laya`, `block_count: 2`, `max_head_tokens: 192`; `temperature.choice 1.637`, `.score 1.251`, `.noul 1.983`, `.choice.2 1.906`, `.choice.3_5 1.760`, `.choice.6_10 1.000`, `.choice.11 0.101`, `.score.3_5 1.251`, `.noul.2 1.983`; `attention.causal: false`; `classifier.pooling_type: 1` (not `pooling_type`); tags `system-one`, `calibrated-decisions`, `guardrails`, `routing` … |
| Julia-1 Q8_0 | `type: laya`, `block_count: 2`, `max_head_tokens: 256`; no temperature keys; `attention.causal: false`; `classifier.pooling_type: 1`; 256k-token multilingual vocabulary |
| Kev-4B Q4_K_M | `type: kev`; `temperature.choice`, `.score`, `.noul` all 2.406; `embedding_length_out: 512`; `general.languages: [en]`; 13 training datasets listed |
| lev Q4_K_M | `type: lev`; `temperature.noul 2.333`, `.score 2.803`, `.choice 1.767`, `.choice.small 1.790`, `.choice.mid 1.609`, `.choice.large 1.664` |
| OpenJev Q4_K_M | `type: openjev`; `temperature.choice 0.85`, `.score 0.85`, `.noul 1.555`; `general.languages: [en, de, fr, hi, zh, ja]` |
| Clef Q4_K_M | `type: clef`, `block_count: 4`, `routing_block_count: 2`, `head_count: 16` |
| mmproj-OpenJev Q8_0, mmproj-Clef Q8_0 | `general.type: mmproj`, `clip.projector_type: qwen3vl_merger`, `clip.has_vision_encoder: true`, `clip.vision.block_count: 27`, 461M |

### B.2 What lmgw would make of each today

`class_for` and the aux plan (`modelinfo.rs:168-180`, `:1934`) without this design:

| File | `pooling_type` | Classifier head | Encoder arch | Class, kind | Plan |
|---|---|---|---|---|---|
| Laya, Julia-1 | absent (`classifier.pooling_type` is another key) | yes (`cls.output.weight`) | yes (`modern-bert`) | aux, **rerank** | `--reranking`, ctx 8192, ubatch default: crashes (A.10) |
| Kev-4B | absent | yes (`cls.output.weight [2560, 512]`) | no | aux, **rerank** | `--reranking`, ctx 262144 |
| lev | absent | no | no | **chat** | a chat row with the Qwen chat template; no route serves it |
| OpenJev | absent | no | no | **chat** | a chat row with the projector found beside it |
| Clef | absent | no | no (`clef`) | **chat** | the same |

### B.3 OpenRouter's catalog entry (2026-10-12)

`GET https://openrouter.ai/api/v1/models?output_modalities=all`:

```json
{"id": "typesafe/jev-1.13", "context_length": 64000,
 "architecture": {"modality": "text->decisions", "input_modalities": ["text"],
                  "output_modalities": ["decisions"], "tokenizer": "Other", "instruct_type": null},
 "pricing": {"prompt": "0.000000042", "completion": "0"},
 "top_provider": {"context_length": 64000, "max_completion_tokens": 57600, "is_moderated": false},
 "links": {"details": "/api/v1/models/typesafe/jev-1.13-20260917/endpoints"}}
```

`~typesafe/jev-latest` is the same. `typesafe/jev-router` is a separate chat-shaped router model
(output `text`, pricing `-1`) and is not a decision model. The output modalities across the full
list are `audio`, `decisions`, `embeddings`, `image`, `rerank`, `speech`, `text`, `transcription`
and `video`.

## Appendix C: compute-buffer measurements (2026-10-13)

Image b11549. "Measured" is llama.cpp's own `CUDA0 compute buffer size` line, in MiB; "formula" is
`vram/compute.rs`. The raw probe logs are not in the repo.

| model | placement | ubatch | ctx | measured | formula |
|---|---|---|---|---|---|
| Qwen3-Emb-0.6B | GPU | 512/2048/8192 | 32768 | 336.2/1345.0/5379.9 | 336.2/1344.9/5379.7 |
| Qwen3-Emb-4B | GPU | 512/2048/8192 | 40960 | 356.2/1425.0/5699.8 | 356.2/1424.9/5699.5 |
| Qwen3-Reranker-0.6B | GPU | 512/2048/8192 | 40960 | 344.2/1377.0/5507.9 | 344.2/1376.9/5507.7 |
| Qwen3-VL-Emb-2B | GPU | 512/2048 | 32768 | 400.8/1603.1 | 392.8/1571.0 |
| embeddinggemma-300M | GPU | 512/2048 | 2048 | 13.8/67.1 | 13.8/67.0 |
| Laya | GPU | 512/2048/8192 | 8192 | 25.0/120.0/630.1 | 33/144/768 |
| Kev-4B | GPU | 512/8192 | 32768 | 537.0/8592.3 | 537.0/8592.0 |
| Qwen3-Emb-0.6B | -ngl 0 + device | 512/2048/8192 | 32768 | 463.6/1393.5/5092.3 | 461.6/1374.3/5025.0 |
| Qwen3-Emb-4B | -ngl 0 + device | 512/8192 | 40960 | 620.0/5363.3 | 620.0/5363.3 |
| Kev-4B | -ngl 0 + device | 2048 | 32768 | 2664.2 | 2664.1 |
| Qwen3-Emb-0.6B, fa off | GPU | 2048 | 32768 | 4432.1 | 4384.0 |

The formula, in bytes:
- `ub` = min(ubatch, batch, ctx); `inp` = `n_embd` x (1 + deepstack layers).
- **Decoder:** `attention.causal` is not false and the pooling is not mean or cls. The mask (f16)
  is (1 + has_swa) x n_kv x ub x 2, where n_kv = ctx for a decoder and ub for an encoder.
- **Decoder logits:** ub x (4 x vocab + 16 x inp).
- **Encoder layers:** 4 x ub x (4 x inp + 3 x ffn_max).
- **GPU:** mask + max(logits, layers).
- **Flash attention off:** the mask is f32, and the scores add 4 x heads x n_kv x ub + 16 x inp x
  ub.
- **-ngl 0 with the device:**
  - decoder: max(logits + output-head bytes, layers), with no mask;
  - encoder: as on the GPU.
- Laya's 20 to 30 % over-estimate comes from using the largest per-layer FFN width. It is accepted.

### C.2 Chat rows (2026-10-14)

Image b11549, a 24 GB GPU, `-lv 4`. Each probe was a throwaway container running a chat row's
argv and image, with its own name and port and no lmgw labels; nothing else was resident. 13
models ran with their full argv; 8 variants changed one thing (a context, the cache type, or the
weights file, for a ladder rung), because the full-size configuration would not fit with a 2 GiB
margin. The 30B DFlash variant was added after the review.
"Measured" is llama.cpp's own `CUDA0 compute buffer size` per context, in MiB; "formula" is
`vram/compute/chat.rs`; "process" is the container's whole-process VRAM from `nvidia-smi`.

| probe | context | ctx | ub | slots | measured | formula | error | process |
|---|---|---|---|---|---|---|---|---|
| gemma4-e4b, ctx 16384 | main / assistant | 16384 | 512 | 1 | 157.3 / 96.3 | 154.0 / 99.0 | -2 / +3 % | 3392 |
| gemma4-e4b, ctx 131072 | main / assistant | 131072 | 512 | 1 | 717.3 / 656.3 | 714.0 / 659.0 | -0.5 / +0.4 % | 5464 |
| gemma4-e4b, ctx 131072, f16 | main / assistant | 131072 | 512 | 1 | 256.0 / 149.0 | 251.0 / 147.0 | -2 / -1 % | 5474 |
| gemma4-e4b, ctx 16384, f16 | main / assistant | 16384 | 512 | 1 | 144.0 / 37.0 | 139.0 / 35.0 | -4 / -5 % | 3458 |
| gemma4-e4b, 4 slots (and with a projector) | main / assistant | 512000 | 512 | 4 | 2202.3 / 2141.3 | 2199.0 / 2144.0 | -0.1 / +0.1 % | 11660 (12814) |
| gemma4-12b | main / assistant | 512000 | 1280 | 2 | 1787.0 / 1749.4 | 1865.6 / 1810.6 | +4 / +4 % | 15960 |
| gemma4-26b-a4b, ctx 32768 | main / assistant | 32768 | 1280 | 4 | 384.5 / 262.4 | 383.1 / 333.6 | -0.3 / +27 % | 17482 |
| gemma4-26b-a4b, ctx 16384 | main / assistant | 16384 | 1280 | 4 | 374.5 / 188.4 | 373.1 / 267.6 | -0.4 / +42 % | 17228 |
| qwen3.5-4b | main | 262144 | 512 | 1 | 1340.3 | 1332.0 | -0.6 % | 9822 |
| qwen3.5-4b, ctx 16384, f16 | main | 16384 | 512 | 1 | 90.0 | 90.0 | 0.0 % | 4732 |
| qwen3.5-0.8b | main | 262144 | 512 | 1 | 796.3 | 792.0 | -0.5 % | 3770 |
| qwen3.8, MTP, ctx 65000 | main / MTP | 65000 | 2048 | 1 | 908.3 / 526.1 | 860.0 / 606.0 | -5 / +15 % | 16042 |
| qwen3.8 on IQ4_XS (a ladder rung), ctx 30000 | main / MTP | 30000 | 2048 | 1 | 686.3 / 510.1 | 686.0 / 568.0 | 0.0 / +11 % | 17554 |
| qwen3.6-27b, ctx 16384 | main / MTP | 16384 | 256 | 1 | 122.3 / 57.0 | 116.0 / 71.0 | -5 / +25 % | 16258 |
| readerlm-v2 | main | 512768 | 512 | 4 unified | 1016.6 | 1025.5 | +0.9 % | 10472 |
| readerlm-v2, ctx 16384, f16 | main | 16384 | 512 | 4 unified | 80.5 | 80.5 | 0.0 % | 2532 |
| lfm2.5-8b (f16 cache) | main | 128000 | 512 | 1 | 183.0 | 215.0 | +18 % | 10706 |
| ling-3.0-tiny | main | 131072 | 512 | 4 unified | 351.0 | 356.0 | +1.4 % | 5942 |
| medgemma-27b, ctx 8192 | main | 8192 | 512 | 4 unified | 180.7 | 176.0 | -2.6 % | 18004 |
| 30B with a DFlash drafter, ctx 4096 | main / DFlash drafter | 4096 | 512 | 1 | 175.7 / 803.0 | 175.5 / 791.6 | -0.1 / -1.4 % | 19074 |

Projectors reserve a buffer of their own (13.5 + 58.6 MiB for Gemma 4 12B's two, 150.6 for 26B,
101 + 101 for E4B, 161 to 248 for the Qwen3.5/3.8 ones). llama.cpp sizes it from per-projector
image limits no header carries, so it stays the headroom's.

**What a chat context is not.** llama-server reserves a chat context's graph for
`min(batch, slots x (1 + draft window))` outputs (`server_output_limits`), not one per token, so
the aux formula's logits term, which dominates an embedder, is a few MiB here. That is why the
first guesses made with the aux formula (1975 MiB for gemma4-26b-a4b, 761 MiB for qwen3.5-4b)
were wrong in both directions.

**The formula, per context, in bytes.** `ub` = min(ubatch, batch, ctx); a context of `ctx` cells
(padded to 256) has one stream per slot, or one stream when the cache is unified.
- **Masks** (f16; f32 with flash attention off): 2 x one stream's cells x ub, once over the
  full-attention cells and once over the window's: `window + ub` per stream, `window x slots + ub`
  unified, padded to 256.
- **The f16 copy.** With flash attention on and a cache type other than f16, CUDA's tile and MMA
  kernels read K and V through an f16 copy allocated with the attention op, inside this buffer:
  2 x heads x (dk + dv) x cells, all streams, for the widest layer (K only under MLA, where V is a
  view of K). It is the largest term at long contexts: 1000 MiB for Qwen3.5-4B at 262144, 2000
  MiB for Gemma 4 E4B at 512000, and it was most of the gap behind an OOM at an MTP context.
- **Activations**, the widest phase: attention 16 x ub x (n_embd + heads x dk), plus 4 x ub per
  per-layer-input width (Gemma 4 E-models), with the copy added; FFN 4 x ub x (4 x n_embd + 3 x
  ffn), or for a mixture-of-experts layer 4 x ub x (4 x n_embd + used x (3 x ffn_exp + 2 x
  n_embd) + 3 x ffn_shared_exp) when wider, plus 8 x ub per per-layer-input width; logits 4 x
  vocab x outputs.
- **Buffer** = masks + max(copy + attention, FFN, logits). A one-layer MTP context frees its mask
  before its FFN: max(masks + copy + attention, FFN, logits).
- The masks and the copy are exact. The activation widths are a fit across architectures
  (Qwen3.5's gated attention runs about 1.8 times a plain one's), so a context they dominate is
  within -5 % to +42 %, at most 80 MiB, over-estimating more often than not.

**Contexts.** `draft-mtp` without a draft model runs the weights' MTP layers
(`nextn_predict_layers`) in a context of their own: one attention layer of the main model's shape,
with its own cache at the draft cache types (f16 by default, `--cache-type-k-draft`). A draft model
runs its own layers in a second context with its own cache, unless every layer shares the target's
(`attention.shared_kv_layers`, a Gemma 4 MTP assistant), in which case it reads the target's cache
at the target's types, f16 copy included. A drafter's header counts all of its layers as MTP
layers, so all of them run. A hybrid drafter keeps a recurrent state per slot in its own context
(no rollback snapshots there). `draft-dspark` reserves `n_max + 1` outputs per slot; an EAGLE3 or
DFlash drafter is an encoder to llama.cpp (`llama_model_has_encoder`), so its context reserves an
output per batch token. Measured on a DFlash drafter at batch 1024: 803 MiB,
nearly all logits, where `n_max + 1` would have charged 13. Its own window cache (five windowed
layers, 2560 cells, f16: 50 MiB) matched to the MiB.

**Outputs of the main context.** `min(batch, slots x (1 + n_max))`, `n_max` being the largest
window of the row's speculative types (`common_speculative_n_max`): the draft window for the
draft types, 48 for the n-gram maps (`--spec-ngram-*-size-m`), 64 for `ngram-mod`
(`--spec-ngram-mod-n-max`, and `--spec-default`), 8 for `ngram-cache`, 0 for `none`. Four slots of
`ngram-mod` on a Gemma vocabulary reserve 260 outputs, 260 MiB of logits.

**KV cache, as llama.cpp lays it out** (each checked to the MiB against the `KV buffer size`
lines):
- full-attention layers hold `ctx` cells (padded to 256, and each slot's share padded to 256
  again); windowed layers hold `window + ub` per slot (`window x slots + ub` unified), or the
  whole stream with `--swa-full`. The header read before this charged `min(ctx, window)`:
  Gemma 4 26B's window cache at four slots and ubatch 1280 is 956 MiB, not 100. A window array
  without `attention.sliding_window` leaves every layer full;
- layers that reuse another's cache keep none (Gemma 4 E4B: 4 + 20 of 42 layers own one);
- under MLA V is a view of K and has no cache (Ling: 459 MiB of K, 0 of V). llama.cpp runs MLA
  when the header states `attention.key_length_mla` and `value_length_mla`
  (`llama_hparams::is_mla`), not on `kv_lora_rank` alone: a DeepSeek or Kimi file converted
  before those keys keeps a real V cache;
- zero-head layers keep a recurrent state only beside recurrent keys (`ssm.*`, `shortconv.*`,
  `kda.*`, `wkv.*`); without them they are attention-free blocks that keep nothing;
- q8_0 is 8.5 bits (34 bytes per 32 values), q5_1 6, q4_0 4.5;
- a header without `attention.key_length` has heads of `n_embd / head_count`, llama.cpp's own
  default (ReaderLM-v2's 7449 MiB cache was charged nothing before);
- a recurrent layer keeps a state per slot, and a speculative row of a draft type keeps one more
  per draft position for rollback (`n_rs_seq`): a qwen3.8 row's 48 layers hold 598.5 MiB
  at a window of 3, not 149.6;
- the MTP layers are not part of the main cache (Qwen3.6/3.8 count 65 blocks; 64 run in the main
  graph).

**Weights.** llama.cpp keeps the input embeddings in host memory: `token_embd` beside an untied
`output.weight` (a tied head is copied to the card for the logits), and Gemma 4's
`per_layer_token_embd`. Checked against `CUDA0 model buffer size` on ten files; the E4B rows were
charged 1.5 GiB of weights that never reach the card. llama.cpp applies `--override-tensor` to
these tensors too, so a row whose args override any tensor to a device buffer keeps them charged,
and its note says why.

**Estimate against the process**, for the rows probed with their real argv (MiB):

| row | estimate before | estimate now | process | process − now |
|---|---|---|---|---|
| gemma4-e4b, ctx 16384 | 4319 | 2976 | 3392 | 416 |
| gemma4-e4b, 4 slots | 11095 | 11243 | 11660 | 417 |
| gemma4-e4b, 4 slots, with a projector | 12040 | 12189 | 12814 | 625 |
| gemma4-12b | 10974 | 15505 | 15960 | 455 |
| qwen3.5-4b | 7568 | 9156 | 9822 | 666 |
| qwen3.5-0.8b | 2286 | 3174 | 3770 | 596 |
| readerlm-v2 | 1807 | 10044 | 10472 | 428 |
| lfm2.5-8b | 8593 | 10308 | 10706 | 398 |
| ling-3.0-tiny | 5388 | 5431 | 5942 | 511 |

What remains is the CUDA context (about 400 to 450 MiB per process), a projector's buffer and,
for Ling and LFM2, a recurrent state the header has no `ssm.*` keys to size (77 MiB and 0.3 MiB,
named in the note). All of it is inside the default 1 GiB headroom.

The 30B DFlash variant at ctx 4096 is charged 18409 MiB against a 19074 MiB process (its projector's
buffer is 229 MiB of the rest).

`model_inspect` reports a chat model's KV the same way, for one slot at the default ubatch, and
its weights less the host-resident input embeddings.

**Not probed.** Models too large for the card with a 2 GiB margin. Their shapes were covered by
the variants above. The auto-unified rows (four slots) and the rung variant cover the unified
and ladder shapes. A gemma3 header states no window pattern, which llama.cpp sets in code, so its
cache is still charged as all full-attention layers (medgemma-27b: 8234 MiB charged at 32000
where llama.cpp keeps about 2320).
