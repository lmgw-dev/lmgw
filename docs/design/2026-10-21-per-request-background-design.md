# Per-request background traffic — design (2026-10-21)

**Status:** approved 2026-10-21 (the header and its semantics, decisions A
and B below); implemented the same day. Choices made while building are in
§11, for review; the review's fix round (2026-10-22) changed §11.9 and added
§11.12–§11.19; its follow-up round the same day changed §11.9, §11.11,
§11.14 and §11.16 again, added §11.20–§11.21 and a line to §12.

Builds on [2026-09-27-candidate-aliases-unified-kv-design.md](2026-09-27-candidate-aliases-unified-kv-design.md)
("aliases §n" below): background traffic, the guest's start, draining for the
owner, the deferral. Until now those semantics existed only on a candidate
alias with `background` set.

## 1. Summary

A client doing speculative work — pre-generating, prefetching, warming a
cache — wants lmgw's local models only when nobody else needs them, and an
answer it can act on when they are busy. The first such client is a gamebook
that pre-generates the pages behind the offered choices while the player
reads. lmgw stays generic: it knows nothing about the game, and the client
should not need an alias created for it.

So the background semantics go on a single request:

- **`x-lmgw-background: on|off`** on any inference route that admits a local
  model (§3).
- **`on` on a plain local model id** makes the request a guest on the GPU, as
  if it had come through a background candidate alias whose only candidate
  is that model, with the model row's own fallback in place of an alias
  fallback (§4).
- **`on` on a candidate alias** makes the walk a guest walk whatever the
  alias row says (§5).
- **A cloud model** is not affected: there is no GPU to defer (§6).
- **A deferral answers `503 gpu_deferred`**, aliases included: "retry later",
  which is not the hold's "do not retry" (§8, decision A).
- **Nothing is preempted.** A client cancels its own stale work by closing the
  request (§9, decision B).

## The owner's decisions (2026-10-21)

- **Approved:** the request header `x-lmgw-background: on|off` (§3), and its
  semantics: on a plain local model, a background alias with that model as its
  only candidate and the row's fallback (§4); on a candidate alias, a guest
  walk (§5); on a cloud model, nothing (§6); every model class through the
  gate (§7); the owner drain covering per-request guests (§10).
- **A. A deferral answers its own wire code, `gpu_deferred`, everywhere,**
  alias deferrals included (§8). Clients must not retry `gpu_hold`; a deferral
  is "retry later", which is why it gets a code of its own.
- **B. Never preempt.** A foreground request never cancels or interrupts
  running background work; clients cancel their own stale background work by
  closing the request (§9).

## 2. Facts this rests on (checked 2026-10-21, `main` 127e0401)

1. **The guest walk** is `Walk::guest` (`gate/candidate/walk.rs`): join the
   primary if loaded and not draining; else `vram::start_background`; else a
   loaded alternate; else the alias fallback (reason `background`); else
   `GpuDeferred`. A primary that cannot be brought up, or that no card could
   hold, goes on to the alternates and the fallback (aliases §12 entry 74).
2. **`vram::start_background`** (`vram/background.rs`) is class-generic: it
   classifies the route (`classify` → `Target`), sizes by
   `footprint(class, id)`, and starts with `acquire_as(.., Origin::Background)`.
   It joins a loaded model first, waits for another guest's start of the same
   model (its turn, aliases §12 entry 91), try-locks the admission gate, evicts
   only idle `Background`-owned models, and answers `Blocked(why)` for
   everything else. A model larger than the card is `vram_too_large`; the hold
   is `gpu_hold`; a card it cannot measure is `Blocked` (only loaded models
   serve then).
3. **Draining** (`vram/background.rs::owner_drain`) was armed only while
   `Snapshot::any_background_alias()` held. An owner admission waiting for
   room then evicted and waited without marking residents draining.
4. **The wire code.** `GatewayError::code()` answered `gpu_hold` for
   `GpuDeferred` and `kind()` everywhere else (aliases §12 entry 67); the
   request log has always recorded `gpu_deferred`.
5. **A hold's origin** (`LocalHold::origin`) is the one place the background
   fact lives for the life of a request. Climbs (`vram::climb`), the restart
   rule (`Restart::Background`), the dead-container stop and the re-pick
   triggers (`gate::candidate::repicks`) read it.
6. **Request headers** are parsed once in `server/gate.rs::principal_mw` into
   `RequestCtx` and judged by the routes that consume them; the reasoning
   headers set the pattern (`reasoning_or_400`).
7. **Every gated route** reaches `gate::resolve` / `gate::open` /
   `gate::open_pinned` (`openapi/headers.rs` `GATED_ROUTES`), most of them from
   a proxy helper that in-process callers share (the Chat, MCP media tools,
   realtime, quickdoc, agent runs).

## 3. The header

- **Name and values:** `x-lmgw-background`, `on` or `off`, trimmed and
  case-insensitive like `x-lmgw-reasoning`. Absent or `off` is today's
  behaviour; `off` never turns a background alias into foreground traffic.
- **Any other value is a `400`** naming the header, in the route's own error
  shape: OpenAI on the OpenAI-shaped routes, Anthropic on `/v1/messages` and
  `/v1/messages/count_tokens`, llama.cpp's on `/tokenize` and
  `/v1/systemone`. The middleware parses it on the routes below only, and
  they judge it (§11.12); anywhere else it is not read at all, so a typo on
  `GET /v1/models` refuses nothing, and realtime, the Chat and `/mcp` are the
  owner's traffic whatever a client sends.
- **Routes:** every route the gate answers for a client — chat completions,
  messages, responses, legacy completions, embeddings, rerank, systemone,
  both token counters and `/tokenize`, audio speech, transcriptions (plain
  and details), alignments, voices, image generations and edits, and the two
  task routes. The table row is `LMGW_HEADERS`' `x-lmgw-background`
  (`Direction::Request`, `Audience::Client`).
- **Carried as** `RequestCtx::background` (parsed, not judged) and handed to
  the gate as an `Origin` (`Owner` | `Background`), the registry's own word for
  whose claim a request is. `gate::resolve`, `gate::open` and
  `gate::open_pinned` take it; every in-process caller passes `Origin::Owner`
  except those acting for a request, which pass that request's origin (§7).

## 4. A plain local model

`on` on a plain local model id behaves as a background candidate alias whose
only candidate is that model, with the row's fallback instead of an alias
fallback (`gate/guest.rs`):

1. **The GPU hold or a benchmark's lease first, unchanged:** the row's
   fallback with reason `hold` / `benchmark`, else `503 gpu_hold` /
   `gpu_benchmark`. The swap happens at resolve time, as for any request;
   a hold that comes on while the guest is being admitted is answered the
   same way (§11.9).
2. **Loaded** (ready, starting, climbing) and not draining for the owner:
   joined as `Origin::Background`. It queues in the model's own slot FIFO or
   pool ledger like any request, with no priority either way.
3. **Not loaded:** `vram::start_background` — only into free VRAM or VRAM held
   by idle `Background`-owned models. It never evicts an `Owner`-owned model
   and never waits in the admission queue. A card lmgw cannot measure serves
   only loaded models. A model it starts is `Background`-owned until a
   foreground request claims it (aliases §4.4).
4. **Otherwise** the row's fallback, stamped `x-lmgw-fallback` and
   `x-lmgw-fallback-reason: background` (a configured fallback always
   applies). With none, `503 gpu_deferred`: "'m' deferred: GPU in use by … —
   background traffic never disturbs the owner's models; retry later, or give
   the model a fallback".
5. **`vram_too_large` stays the configuration error it is** (aliases §4.7),
   never a deferral and never the fallback — judged wherever a busy card
   would defer the guest before its start reads the card itself, so a busy
   card never turns it into one (§11.14). So does a start that fails. With
   VRAM admission off no estimate is trusted and the size is not judged:
   the guest goes on to step 4.
6. **Climbs** follow the guest's rules (aliases §9, §12 entry 66): never an
   `Owner`-owned model, only into free VRAM. A denied climb is answered like
   step 4 — the row's fallback, else the deferral — since nothing was sent yet.
7. **A send-time `context_length_exceeded`** is the plain `400`: there are no
   alternates to pick instead.

A pinned caller (`open_pinned`) never falls back, so step 4 is the deferral
for it.

## 5. A candidate alias

`on` makes the walk a guest walk (`Walk::guest`) even when the alias row is not
`background`. On a background alias it changes nothing. The re-pick a send
makes (aliases §12 entry 45) keeps the request a guest: the context carries
the origin the client asked for (`CandidateCtx::origin`).

## 6. Cloud models and CPU rows

- **A cloud model id** (a plain alias, a passthrough id) is not affected: the
  flag has nothing to defer. The header row says so.
- **A row on the CPU** (an aux or audio row with `backend: cpu`) is admitted as
  today: it takes no VRAM, so there is no owner to disturb.
- **A fallback** that answers (the hold's, the guest's) is a cloud route, and
  the flag does not reach it.

## 7. Classes and routes

The gate is class-generic, and every class reaches the guest branch through
`gate::Routed::admit` / `admit_pinned`:

| Class | Routes | Path to the gate |
| --- | --- | --- |
| chat | chat completions, messages, legacy completions, responses | `proxy::handle_chat`, `handle_legacy_completions`, `responses::handle_responses` |
| chat | `/v1/count_tokens`, `/v1/messages/count_tokens`, `/tokenize` | `count::count_alias`, `count_messages::open_for_count`, `tokenize` (`count_admit`) |
| aux | embeddings, rerank, systemone | `embed_in_process`, `rerank_in_process`, `systemone::relay` |
| image | generations, edits | `image_json_call`, `handle_image_edit` |
| audio | speech, transcriptions, alignments, voices, tasks | `speech_call`, `audio_json_call`, `multipart_call`, `handle_audio_voices`, `task_call` |

**`/v1/responses`** holds one admission for the run (aliases §12 entry 75), so
the inbound flag holds for every turn. A run's claim that is let go and lost
meanwhile (`agent::claim::reroute`) goes back to the gate with the hold's own
origin, so a guest's run stays a guest. The image and audio tools a run calls
read the run's request context, so they are guests too (`image_call::run`,
`image_call::upscale`, `synthesize::open`, `transcribe`), and so are the
docs and knowledge tools' embedder, reranker and token counts (§11.15). MCP
sampling a server asks for during a guest's run is still the owner's (§12).

The tests (`tests/it/background_header/`) drive chat, embeddings, image
generation, speech, the voice list's engine probe, both counters,
`/tokenize`, `/v1/systemone` and `/v1/responses` over HTTP through the guest
branch; the other routes reach it through the same `Routed::admit` with the
same `ctx.origin()`.

## 8. The wire code of a deferral (decision A)

A deferral answers its own code everywhere: the OpenAI body's `code` is
`gpu_deferred`, alias deferrals included, and `GatewayError::code()` is gone —
`kind()` is the wire code again for every variant. Anthropic bodies carry no
code (unchanged: `api_error`, never `overloaded_error`). The reason: clients
must not retry `gpu_hold` — the hold can last an hour — while a deferral is
"the GPU is busy with foreground work, retry later".

## 9. Never preempt; clients cancel (decision B)

A foreground request never cancels or interrupts running background work; a
request that has started always finishes (aliases §4.5). A client cancels its
own stale background work by closing the request: hyper drops the handler
future, the relay drops the upstream stream (the container sees its connection
close and stops generating), and the hold goes with it, so the next request is
not blocked by it (`tests/it/background_header/cancel.rs`). A guest's
streamed `/v1/responses` run, whose loop runs in a task of its own, takes
the closed stream as its cancel and ends at once, a tool call in flight
included (§11.16); the owner's streamed runs stop their model call at once
and let a tool call in flight finish (*changed 2026-10-24*, §11.16).

## 10. Owner drain and visibility

- **Draining** (aliases §4.5, §12 entry 47) is armed by every foreground
  admission's make-room wait, on every install: any client may send the
  header, so a guest can always exist. `Snapshot::any_background_alias` is
  gone. The frame shows `draining_for_owner` on every resident during such a
  wait whether or not a guest is running — one global mark, "the owner is
  waiting for room; background work is not taken", and worded so (§11.19);
  the realtime speech bound, which reads the same mark (realtime design §8.2),
  now lifts on every such wait, as its design says.
- **Deferrals per model.** A plain-id deferral is logged `gpu_deferred` with
  the model's public name as `requested_alias`, so the 24 h count that
  candidate aliases show is shown for local rows too: `deferrals_24h` on the
  local, aux, audio and image rows of `GET /api/models/full` and of
  `lmgw__models`, and a "N deferred/24h" badge in the Models page's State
  cell when it is above zero.

## 11. Implementation decisions (for review)

Taken while building (not yet reviewed), under "a desktop app for one user".

1. **The origin is a parameter of the gate,** not a field of `RouteCheck` or a
   setter on `Routed`: `resolve(state, alias, check, origin)`, and `open` /
   `open_pinned` the same. Every call site says whose traffic it is, and the
   compiler found each of them. In-process callers pass `Origin::Owner`
   unless they act for a request (§7); the proxy helpers both kinds share
   (`embed_in_process`, `multipart_call`, `count_alias`, …) take it as a
   parameter.
2. **The 400 is the handler's,** as for the reasoning headers: no request-log
   row, the route's own error shape. `/tokenize` and `/v1/systemone` refuse
   in their handlers too, and write the row those handlers write for every
   outcome.
3. **Parsing:** `on` / `off`, trimmed and in any case; anything else,
   an empty value included, is the 400.
4. **A deferral's error carries what it was named for**
   (`GatewayError::GpuDeferred { on: Deferred::Alias | Deferred::Model }`), so
   the message says "give the alias a fallback" or "give the model a
   fallback". `GatewayError::code()` is removed; `kind()` is the OpenAI
   `code` and llama.cpp's `lmgw_code`.
5. **The plain-model guest is its own module** (`gate/guest.rs`), not a
   one-candidate alias synthesized for the walk: the walk is chat-only
   (`chat_local_route`), re-picks a guest over a candidate's context (which
   §4 step 7 rules out here), and stamps `x-lmgw-candidate`. The guest
   module calls the same `vram::start_background` the walk does.
6. **What applies to it:** a local model with a row, placed on the GPU. A
   cloud route, a CPU row and an id no row describes are admitted as today. A
   pinned caller (`open_pinned`) gets the deferral, never the fallback.
7. **A deferral at admission names the model's route** on the request row
   (`upstream_*`), as any admission refusal about a model does; an alias
   deferral names none, since no candidate was settled on.
8. **After admission:** a guest's climb that is denied, and a model gone
   under the request that the guest may not bring back (`candidate_lost`:
   its dead container cannot restart without disturbing the owner, or it is
   the owner's model, which a guest never stops), are answered like step 4 —
   the row's fallback, else the deferral (`guest::lost`) — on the chat sends
   (`gate::send`), which can hand the request elsewhere. The legacy
   completions route keeps the client's body for that, as it does for a
   candidate alias (`candidate::reroutable`).
9. **The GPU hold switched on between resolve and admission** — or a
   benchmark's lease — is answered as the resolve-time swap would have
   answered it, for every request that may fall back (`gate::open::held`):
   the row's fallback with the block's reason (`hold` / `benchmark`) when it
   can take the request, else `gpu_hold` / `gpu_benchmark` saying why a
   configured fallback did not answer. A configured fallback always applies.
   A guest meets the block at a join parked on a start in flight or at its
   start's own looks; the owner's plain request in its admission, the queue
   wait included (`admit_route`). A pinned caller gets the refusal, as
   before; a candidate alias's primary is left to its walk, which answers
   with the alias's fallback (`Walk::held`). *Changed 2026-10-22*, twice: the
   fix round answered so for a guest only and left the owner's plain
   request with the refusal; the follow-up round gave the owner's the same
   answer.
10. **Draining on every install** (§10), with `Snapshot::any_background_alias`
    removed rather than kept unused.
11. **The header's routes** are `GATED_ROUTES` without the Chat's dictation, a
    dashboard route the Chat calls itself. `GET /v1/audio/voices` reads the
    header because `?probe=engine` admits a model, and so does a `HEAD` of it
    (§11.20).

Added by the review's fix round (2026-10-22):

12. **The header is read on its own routes only.** `principal_mw` parses it
    only when the request's method and path are a `BACKGROUND_ROUTES` row
    (`openapi::takes_background` — the table the API docs print), rather
    than everywhere and clearing it at the realtime session, `Caller` and
    MCP-plane boundaries: a context those paths hand on in-process (TTS via
    `synthesize::open`, ASR `transcribe_turn`, the media tools, a device's
    Chat dictation and read-aloud) cannot carry it, because it was never
    parsed for them. `/v1/realtime`, its WebRTC calls, the Chat and `/mcp`
    are wholly the owner's traffic; a malformed value there refuses nothing.
13. **The TTS regain takes the claim back as whoever held it**
    (`synthesize/claim.rs`): the owner's through admission, a guest's by the
    guest's start rule (`Restart::Background` semantics — joined if loaded,
    started only into room that disturbs nobody, else `gpu_deferred`). No
    guest reaches it yet: only a realtime response lets its claim go, and
    realtime is the owner's (item 12).
14. **`vram_too_large` where a busy card would defer.** Wherever
    `start_background` would answer `Blocked` before its start reads the
    card itself — the owner waiting for room, another guest's turn timing
    out, the gate's try-lock failing — it first judges the footprint against
    the card's last known total (a declared `vram.budget_mb`, else the
    devices' total the last ledger read measured,
    `VramScheduler::known_total`) and answers `507` when the model does not
    fit it. A start that holds the gate is judged on its own reading, never
    on the cached total. No device read of its own up front: the one a
    guest's start makes stays under the gate, where a benchmark's drain
    waits for it (benchmark design §13 decision 44). *Changed 2026-10-22*
    (follow-up round), from "before every reason to defer":
    - **Admission off** (`vram.enabled` false) judges no size: no estimate
      is trusted there, and the owner's start of the same model would not be
      checked, so a guest is not refused with a `507` and no fallback — it
      defers or takes the row's fallback, as before the fix round.
    - **The cached total's staleness.** The measured total is as old as the
      last ledger read that saw a device (a failed read keeps the previous
      figure), and a guest refused from it made no read of its own. It is
      read only where something else is reading the card: the owner's wait
      reads it on every pass, the other guest and the gate's holder under
      the gate. So a total that went stale — smaller than the card is now —
      is corrected by the very contention it answers for, and once the card
      is quiet the guest's start reaches the gate and reads it itself
      (`a_stale_smaller_card_total_does_not_refuse_a_start_that_reads`). A
      total not known yet (no budget, no ledger read since lmgw started)
      judges nothing before the gate.
15. **The docs and knowledge tools are the run's traffic.** The in-process
    embedder and reranker carry an explicit `Origin`
    (`InProcessEmbedder::as_traffic_of`, `InProcessReranker::as_traffic_of`),
    and so do the counts behind a search's per-input limits
    (`proxy::count_tokens_inner`, `knowledge::limit::calibrate`,
    `cached_counter`, `fit::prime_ratio`, the bounded embedder and
    reranker); `retrieve::Options::origin` and `docs::call` / `kb::call`
    carry it there. `DocsExecutor` and `KbExecutor` pass their request's
    `ctx.origin()`; the `/mcp` plane, the Chat, the knowledge playground and
    every ingest, re-embed and probe pass `Origin::Owner`. A guest's
    `docs__query` on a cold embedder therefore defers — pinned, it never
    takes a fallback's vectors (decision 6) — and the owner's models stay.
    The origin is separate from charging (`charged_to`), which stays a
    device key's only.
16. **A guest's streamed `/v1/responses` run ends when its client closes
    the stream.** Its loop runs in a task of its own, which saw the hang-up
    only when the next frame failed to send — a tool call could put that
    minutes away, holding the run's claim. For a guest's run (the header, or
    a hold admitted as `Background` through a background candidate alias)
    the run's `Cancel` is now a stop raised by `tx.closed()`: every await in
    the loop is raced against it, so the loop claim, a tool's own image or
    audio claim and the upstream call go at once, while what the run made is
    still stored (`persist`). **The trade-off is the guest's:** a tool call
    in flight that does not take the call stop — a self-admin op,
    `docs__*`, `kb__*`, an MCP server's — is dropped mid-way: it writes no
    request-log row, and the stored response has it abandoned rather than
    its result; a mutating self-admin op can be cut between its write and
    the snapshot reload. Media calls take the stop and write their
    `canceled` row. *Changed 2026-10-22* (follow-up round): the owner's
    runs are not cancelled, exactly as before this branch — such a call
    finishes with its row and result, and the run ends at the next frame it
    fails to send
    (`closing_the_owner_s_stream_mid_tool_call_lets_the_call_finish`). Both
    the streamed and the unary run now let their claim go before `persist`
    writes the stored response. A unary run is ended by hyper dropping its
    handler, as before.
    *Changed 2026-10-24* (owner decision 2026-10-24): the owner's streamed
    runs now stop the model call at hang-up. Ending at the next failed write
    let a model that wrote the client nothing (a long prefill, frames that
    carry nothing, the 2026-10-22 runaway class) generate for as long as it
    liked after the client left. The owner's run takes the same `tx.closed()`
    stop as a guest's, as `agent::Cancel::sparing_calls`: the model call in
    flight is stopped at once and writes its row (`canceled`, the hang-up
    rule of the chat routes), a tool call in flight is not raced and
    finishes with its rows and result, and the run then starts no other
    call and no other turn; what it made is stored (`persist`)
    (`client_hangup::responses`,
    `closing_the_owner_s_stream_mid_tool_call_lets_the_call_finish`). In a
    sequential batch (`parallel_tool_calls: false`) the calls after the one
    in flight are never started; the stored response closes them as never
    made
    (`closing_the_owner_s_stream_mid_sequential_batch_starts_no_further_call`).
    Either way the run's status is `canceled`, as when the hang-up stops a
    model call, not `client_disconnected`. A spared tool call that itself
    runs a model — an image or audio tool, an agent run, an MCP server's
    sampling request — finishes fully, and its model keeps the GPU until it
    is done.
17. **Deferral visibility.** The review's case of a guest deferred through
    a plain alias onto a local model does not occur: a plain alias references
    a stored upstream (`aliases.upstream_id`, a foreign key), never the
    synthetic local upstreams `vram::classify` recognises, so a plain alias
    always routes to a provider or to a server lmgw does not start, and the
    header admits it as today. Nothing is counted on plain alias rows; the
    UI comment says why instead of "no GPU". `lmgw__models` reads the
    deferral counts as `GET /api/models/full` does: a failed query is zero
    with a warning, not a failed list.
18. **`gpu_deferred` follow-through.** `GET /v1/models`'s notes explain it as
    "deferred because the GPU is busy with other work; retry later", beside
    `gpu_hold`'s and `gpu_benchmark`'s "do not retry". The Chat's voice
    surfaces (dictation, read-aloud, a turn's refusal) say a deferral in its
    own words as a warning — passing, unlike the hold's amber chip — since a
    background candidate alias can be a voice stage's model.
19. **Draining is worded as the global mark it is.** `lmgw__status` says
    every running entry carries `draining_for_owner` while any ordinary
    request waits for room; the sidebar says once "The owner is waiting for
    room: background work is not taken." instead of a tag on every resident,
    and the Models and Overview chips read "owner waiting for room".

Added by the review's follow-up round (2026-10-22):

20. **A `HEAD` is read as its `GET`.** axum answers `HEAD` with a route's
    `GET` handler, so `openapi::takes_background` reads a `HEAD` as the
    `GET` it runs: `HEAD /v1/audio/voices?probe=engine` with the header is a
    guest as the `GET` is, rather than an owner's probe that evicts for its
    model.
21. **The Chat's voice deferral is one sentence** ("dictation deferred: the
    GPU is busy with other work, try it again shortly"), without the
    gateway's message after it, which said the same in its own words.

## 12. Not yet

- **`/v1/realtime` and WebRTC calls** read no background flag yet — the
  header is not even parsed there (§11.12); their warm loads, turns, TTS,
  ASR and media tools are admitted as the owner's.
- **MCP sampling, agent runs and the dashboard's Chat** have no way to ask for
  background yet; they are in-process callers and pass `Origin::Owner`.
  Sampling also fires inside a guest's run: an MCP server a guest's
  `/v1/responses` run calls that asks for sampling is answered as the
  owner's traffic (`mcp/handler.rs`).
- **Self-admin tools inside a guest's run** are operator actions and run as
  the owner's traffic: `lmgw__voice_transcribe`, `lmgw__voice_generate`,
  `lmgw__local_model_test` and `lmgw__container` start / apply admit their
  model with `Origin::Owner` even when a guest's `/v1/responses` run calls
  them (`selfadmin::call_capped` carries no origin yet), so such a call may
  evict the owner's idle models as the owner's own call would.
- **Kai's voice client** (`voice/events.rs`) special-cases only `gpu_hold`
  among the GPU refusals; a `gpu_deferred` there reads as an ordinary error
  until Kai learns it at its next pin (not edited in this round).
- **A guest's model lost on the image, audio and aux sends**
  (`vram::send_local`, which cannot hand a request elsewhere) is still a
  `503 candidate_lost` rather than the row's fallback or a deferral.
- **`lmgw__status`** shows no per-model deferral count yet: its per-model
  section is the runtime list, which is built without a database read.
