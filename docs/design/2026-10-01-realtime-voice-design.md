# Realtime voice API (lmgw) — Design

**Date:** 2026-10-01
**Status:** **Draft v2 plus the WP0 spike results (2026-10-01), for
review.** Building has started on branch `feat/realtime` (§18).
- v1 was written from four research passes: the protocol, lmgw's internals,
  an earlier prototype, and VAD/turn models.
- v2 folds in an adversarial review of v1 that checked its claims against the
  code and the SDK sources. That review found one blocker (§3.2) and nine
  major gaps. §21 lists what changed.
- §22 records the WP0 spike. Its results are already written into the body.
- §20 holds the decisions, including the owner's answers of 2026-10-01.

> Companion to [2026-08-30-per-model-containers-design.md](2026-08-30-per-model-containers-design.md)
> ("containers §n"), [2026-09-04-gpu-hold-design.md](2026-09-04-gpu-hold-design.md)
> ("hold §n"), [2026-09-17-model-capabilities-design.md](2026-09-17-model-capabilities-design.md),
> [2026-09-18-usage-analytics-cost-policy-design.md](2026-09-18-usage-analytics-cost-policy-design.md)
> ("policy §n") and [2026-09-22-principals-origins-mounts-design.md](2026-09-22-principals-origins-mounts-design.md)
> ("principals §n"). This spec adds one route, `GET /v1/realtime`, that
> speaks OpenAI's Realtime protocol over a WebSocket and answers it with a
> **cascade of aliases lmgw already serves**: speech in → turn detection →
> an ASR alias → a chat alias → a TTS alias → speech out.

## 1. Summary

lmgw fronts local and cloud models for text, embeddings, images and audio
(TTS, ASR and a dozen more audio.cpp tasks). What a client still cannot do
through it is **hold a spoken conversation**: stream microphone audio in,
have the gateway decide when the user has finished, and stream a spoken
answer back that stops when the user talks over it. OpenAI's Realtime API is
the de-facto client shape for exactly that. Official SDKs (`openai`
Python/JS, `@openai/agents`) and third-party clients (Pipecat, LiveKit
Agents, Hugging Face's `speech-to-speech` `talk` client) all accept a base
URL.

Decision in one paragraph: **lmgw serves `GET /v1/realtime` as a WebSocket
speaking the GA Realtime event protocol, and implements the session itself
as a cascade.** Each user turn runs:
1. voice-activity detection and end-of-turn detection **in-process**
   (Silero VAD and Smart Turn, on ONNX Runtime);
2. the committed utterance to an **ASR alias**, through the same gate every
   `/v1/audio/transcriptions` request takes;
3. the conversation to a **chat alias**, streamed token by token through the
   gateway's in-process send path, with client-side function tools;
4. the streamed reply, cut into clauses, to a **TTS alias** clause by clause;
5. PCM16 deltas back to the client, **paced to real time** so the server
   knows what the listener has heard.

Interruption (barge-in) cancels the response and trims the stored assistant
turn to what the client actually played. The chat model can be any alias,
local or cloud, so "cloud brain, local voice" and "all local" are the same
code path. Nothing here needs a new model class, a new container or a new
upstream kind.

Latency, measured through the gateway on the RTX 4090 dev box (§3.1): ASR
and TTS cost **tens of milliseconds** per utterance and per clause. The
end-of-turn wait dominates, which is why turn detection is where this
design spends its care (§6).

Voice assistants are **clients** of this route and stay out of it; this
spec does not design them.

## 2. The protocol — facts

Read from OpenAI's GA docs and API reference, `openai-python`'s
`types/realtime/*`, and `@openai/agents`' `openaiRealtimeBase.ts` /
`openaiRealtimeWebsocket.ts` / `realtimeSession.ts` (2026-10-01).

### 2.1 Transport and auth

- **URL.** `wss://<base>/v1/realtime?model=<model>`. The Python SDK derives
  it from the HTTP base URL (`base_url.path + "/realtime"`) and appends
  `model`, and sometimes `call_id`. A client pointed at
  `http://127.0.0.1:8001/v1` therefore lands on
  `ws://127.0.0.1:8001/v1/realtime?model=…` with no extra config.
  `@openai/agents` given an explicit `url` sends **no** `?model=` at all, and
  names its model in the first `session.update` instead (§5.1).
- **Server-side SDKs, captured (WP0):** `openai-python` 3.22.1 and
  `@openai/agents` 0.18.0 on Node offer **no subprotocol**, no `Origin` and
  no `OpenAI-Beta` header. The upgrade must therefore succeed with no
  subprotocol selected.
- **Credentials.** Server clients authenticate with `Authorization: Bearer
  <key>`. Browsers cannot set headers on a WebSocket, so they pass the key as
  a subprotocol: `Sec-WebSocket-Protocol: realtime,
  openai-insecure-api-key.<key>, <sdk-meta>`. **A server must select one
  offered subprotocol** (`realtime`) in the 101, or the browser aborts. In a
  browser, `@openai/agents` refuses any key that is not an ephemeral `ek_…`
  key unless the app sets `useInsecureApiKey: true`.
- **GA versus beta.** GA sends **no** `OpenAI-Beta` header. `OpenAI-Beta:
  realtime=v1` marks a beta client (the legacy
  `client.beta.realtime.connect()`), whose shapes are not wire-compatible
  (§2.4).
- **Not covered here.** Ephemeral keys, WebRTC, SIP and sideband connections
  are separate surfaces (§19).

### 2.2 The session object

`session.type` is `realtime`, or `transcription` (§19). The fields that
matter to a cascade:

| Field | Meaning here |
|---|---|
| `model` | the chat model (§5.1) |
| `instructions` | the system message |
| `output_modalities` | `["audio"]` (default: audio + transcript) or `["text"]` |
| `audio.input.format` | `{type: "audio/pcm", rate: 24000}`; `audio/pcmu`/`audio/pcma` exist (§19) |
| `audio.input.transcription` | `{model, language, prompt}`: whether input transcription *events* are sent, and with which ASR model |
| `audio.input.turn_detection` | `server_vad` {threshold, prefix_padding_ms, silence_duration_ms, create_response, interrupt_response, idle_timeout_ms} · `semantic_vad` {eagerness, create_response, interrupt_response} · `null` = manual |
| `audio.output.format`, `.voice`, `.speed` | output PCM; voice name (built-ins `alloy … marin cedar`) or `{id}`; speed 0.25–1.5 |
| `tools`, `tool_choice`, `parallel_tool_calls` | flat function tools `{type:"function", name, description, parameters}` |
| `max_output_tokens` | 1–4096 or `"inf"` |
| `reasoning` | for reasoning realtime models |
| `tracing`, `truncation`, `prompt`, `include`, `noise_reduction` | accepted; see §7.5 for `truncation`, the rest are ignored |

`session.update` is a partial update that must carry `type`; the server
answers `session.updated` with the full session.

**Stock `@openai/agents` defaults** (0.18.0, captured in WP0). Its first
full `session.update` sends:
- `model: "gpt-realtime-2.1"`;
- `instructions` and `output_modalities: ["audio"]`;
- `audio.input`: `format {audio/pcm, 24000}`, `noise_reduction: null`,
  `transcription {model: "gpt-4o-mini-transcribe"}`, `turn_detection {type:
  "semantic_vad"}` (no eagerness);
- `audio.output`: `format` and `speed: 1`, with **no voice**;
- flat function `tools` whose `parameters` carry a `$schema` key, with
  optional parameters spelled `anyOf: [<type>, {type: "null"}]` and listed
  as required.

A second `session.update` carrying only `tracing: "auto"` follows at once,
and each update gets its own `session.updated`. When its tool list becomes
empty it omits `tools`, so clearing tools needs an explicit `tools: []`.

**The Hugging Face `talk` client** is built on `openai-python` and asks for
`model: "local"`.
- It sends `session.extensions` as a session-level array.
- At its default 16 kHz it **omits `audio.input.format`**, which it means as
  "the server's native rate". Against lmgw it has to be run at 24 kHz.
- It correlates its follow-up responses by `event_id` and
  `response.metadata`.

Unknown session-level fields are therefore ignored, never refused. Only the
`session.lmgw` object is strict (§5.4).

### 2.3 Events

**Client → server.** The whole GA union is eleven events:
- `session.update`
- `input_audio_buffer.append {audio: base64}`, `.commit`, `.clear`
- `conversation.item.create {item, previous_item_id?}`, `.retrieve`,
  `.truncate {item_id, content_index, audio_end_ms}`, `.delete`
- `response.create {response?}`, `response.cancel`
- `output_audio_buffer.clear` (WebRTC only)

**Server → client.**
- Session: `session.created` (immediately on connect), `.updated`, and
  `error {error: {type, code, message, param, event_id}}`.
- Input buffer: `input_audio_buffer.speech_started {audio_start_ms,
  item_id}`, `.speech_stopped {audio_end_ms, item_id}`, `.committed`,
  `.cleared`, `.timeout_triggered`.
- Items: `conversation.item.added`, `.done`, `.retrieved`, `.truncated`,
  `.deleted`.
- Input transcription:
  `conversation.item.input_audio_transcription.delta`, `.completed`,
  `.failed`.
- Response, in order: `response.created`, `response.output_item.added`,
  `response.content_part.added`, `response.output_audio.delta`/`.done`,
  `response.output_audio_transcript.delta`/`.done`,
  `response.output_text.delta`/`.done`,
  `response.function_call_arguments.delta`/`.done`,
  `response.content_part.done`, `response.output_item.done`,
  `response.done {response: {status, status_details, output, usage}}`.
- Optionally `rate_limits.updated`.

Shape rules the SDK's schemas enforce:
- **Every server event carries an `event_id`.**
- **Delta events carry `response_id`, `item_id`, `output_index` and
  `content_index`.**
- An event that fails the schema is treated as a generic event, and its
  audio is silently dropped.
- `speech_started.item_id` names the user item that *will* be created when
  speech stops.
- `audio_start_ms` includes `prefix_padding_ms`.

**One voice turn under `server_vad`:**
1. `speech_started`
2. `speech_stopped`
3. `committed`
4. user item `added`
5. input transcription `completed`, which may arrive after the response has
   started
6. user item `done`
7. `response.created`
8. `output_item.added` (assistant message, `status: in_progress`)
9. `conversation.item.added`
10. `content_part.added`
11. interleaved `output_audio_transcript.delta` and `output_audio.delta`
12. `output_audio.done`
13. `output_audio_transcript.done`
14. `content_part.done`
15. `output_item.done` (`status: completed`)
16. `item.done`
17. `response.done`

**A function call** replaces the message item with a `function_call` item
(`call_id`, `name`, `arguments`):
- It streams `function_call_arguments.delta` and `.done`, and ends the
  response.
- The client answers with `conversation.item.create {type:
  "function_call_output", call_id, output}` and a new `response.create`.
- `@openai/agents` fires its tool call on **any** `output_item.added` or
  `.done` whose item has `status: "completed"`. A second such event with the
  same call re-runs the tool and sends a second `response.create`. It also
  rejects a `call_id` reused across invocations.

**Barge-in:**
1. `speech_started` arrives during an active response with
   `interrupt_response: true`.
2. The server cancels: `response.done` with `status: "cancelled"` and
   `status_details.reason: "turn_detected"`.
3. The client stops playback and sends `conversation.item.truncate` with
   `audio_end_ms`. The SDK computes this as the wall-clock time since the
   first audio delta arrived, capped at the audio received.
4. The server cuts the stored item's audio and transcript at that point and
   answers `.truncated`. OpenAI errors when `audio_end_ms` exceeds the item
   length.

`@openai/agents` stops playback **on `speech_started`**, and only while it
still considers the item playing. It resets that state on
`response.output_audio.done`, after which `interrupt()` does nothing. This
is why output is paced (§8.2).

Further client behaviour seen in the WP0 captures:
- **Barge-in.** After a barge-in the client sends `truncate` and then
  `conversation.item.retrieve`. It sends `response.cancel` **only when the
  echoed session says `interrupt_response` is not true**, so every echoed
  session carries `interrupt_response` and `create_response` explicitly.
- **Early follow-up.** After a function call, the client's
  `function_call_output` and follow-up `response.create` arrive ~13 ms after
  `output_item.done`, which is *before* the server's `response.done` (§4.3).
- **Optional ids.** `openai-python` sends no `event_id`, and
  `conversation.item.create` comes without an item id.

### 2.4 GA versus beta

The GA API renamed most of the surface:
- `response.audio.delta` → `response.output_audio.delta`
- `response.text.delta` → `response.output_text.delta`
- `conversation.item.created` → `.added` / `.done`
- `modalities` → `output_modalities`
- `input_audio_format: "pcm16"` → `audio.input.format`
- flat `voice` → `audio.output.voice`

It also made `session.type` required. Current `openai-python`
(`client.realtime.connect`) and `@openai/agents` are GA throughout. This
design is **GA only** (§20).

### 2.5 The reference cascade

Hugging Face's `speech-to-speech` (Apache-2.0) is the same cascade in
Python: Silero VAD v5 → Smart Turn v3.2 → STT → LLM → TTS behind a
Realtime-compatible WebSocket (`src/speech_to_speech/api/openai_realtime/`).
It is tested in CI against stock `@openai/agents`, though it controls that
client's session config.

Choices worth copying:
- Smart Turn gates Silero's end-of-speech.
- A turn Smart Turn calls complete is processed at once, but its output is
  held for a short "speculative reopen" window. Speech that resumes reopens
  the same user item as a new revision, whose stale output is dropped.
- Barge-in is a generation counter, checked per token and in the send loop.

Its deviations:
- it emits the beta-style `conversation.item.created`;
- it accepts `conversation.item.truncate` without editing history.

The other open implementations are less complete:
- LocalAI has a similar cascade that mixes GA and beta.
- Speaches is beta, with no cancel and no truncate.
- vLLM's `/v1/realtime` is ASR-only.

## 3. What lmgw already has — facts

### 3.1 Measured through the gateway (WP0, 2026-10-01, RTX 4090, warm, p50)

| Call | Time |
|---|---|
| ASR `audio/nemotron-asr-q8-0`, 1 / 2 / 3 / 5 s of speech (16 kHz WAV) | 14–23 / 14–26 / 16–29 / 31–38 ms |
| ASR `audio/qwen3-asr-1-7b-q8-0`, the same segments | 82–84 / 88–102 / 94–118 / 122–171 ms |
| ASR, the same segment uploaded at 24 kHz instead of 16 kHz | +5–20 ms, identical text |
| TTS `audio/pocket-tts-german-q8-0`, 22 / 44 / 113 chars | 25 / 35 / 69 ms |
| Chat `gemma4-e4b` (current row), time to first token, ~200-token system prompt | 202 ms, 240 tok/s |
| Cold start + first request: nemotron / pocket-tts / gemma4-e4b | 1.35 s / 0.90 s / 4.25 s |

The time to first audio for one voice turn, excluding end-of-turn
detection, is about **380 ms** with the general chat row. With the
voice-sized row below it is about **200 ms**: ASR ~30, time to first token
~20, first clause ~100, TTS ~35, overhead ~20.

The measurement turned up several facts:
- **Pocket TTS needs a voice.** The row answers **500 "requires a session
  voice"** when no `voice` is sent, so a session must always send one (§5.3).
- **TTS output is always a WAV.** It is 24 kHz mono 16-bit, even when
  `response_format: "pcm"` is asked for, so the client parses the header.
- **No TTS family installed here can stream.** Pocket is offline only. Fish
  needs `reference_text` for library voices and is unusable for voice as
  configured.
- **The general chat row is slow on prompt tokens, not missing its cache.**
  It reuses the system-prompt prefix (302 of 307 tokens cached), but
  evaluates each *new* prompt token at ~41 ms. Its configuration is 512k
  context over 4 slots, flash attention off and f16 KV; the V cache is
  padded because flash attention is off; and the ledger estimate is 19 GB.
  A voice-sized row on the same weights (`gemma4-e4b-voice`: 16k context, 1
  slot, flash attention on, q8_0 KV) answers in **20 ms** warm time to first
  token at the same ~200 tok/s, with a 4.5 GB estimate (§22). An earlier
  probe's "no cache reuse" reading was wrong.
- **The VRAM ledger is wrong for audio.** Measured per process: nemotron
  1.48 GB, qwen3-asr 3.28 GB, pocket-tts 0.97 GB, against ledger estimates of
  0.93 / 2.47 / 0.13 GB. Pocket is under-counted about 7×, and audio overall
  by 1.5× or more (§9.4).

An earlier prototype of the same cascade, with an lmgw chat alias behind
it, measured mouth-to-ear **620–920 ms**:
- **430–730 ms** was the end-of-turn wait;
- ~82 ms was warm LLM time-to-first-token;
- ~80 ms was time to first TTS audio.

### 3.2 Internal paths to reuse — and the one that does not fit

- **Chat, streamed: `proxy::stream_once_on`** (`proxy/in_process.rs`).
  - It takes the caller's `LocalHold` and runs the per-send gate half: fit,
    ladder climb, candidate reroute.
  - It streams every `ir::StreamDelta` (`TextDelta`, `ReasoningDelta`,
    `ToolCallStart`, `ToolCallArgsDelta`, `Usage`, `Stop`) to the caller's
    `DeltaSink` **as it arrives**.
  - It writes one `request_logs` row per call and returns the assembled
    `Completion`, tool calls included.
- **Not `agent::run`.** Its turn runner collects deltas into a local vec and
  emits `LoopEvent::Text` only after each model turn returns
  (`agent.rs:728-780`). A clause splitter behind it would get the whole
  answer at once, so the first spoken word would wait for the last generated
  one. The Chat's `ChatSink` and `/v1/responses` share this buffering. v1
  has only client-side tools, where every tool call ends the response anyway,
  so it calls `stream_once_on` directly (§7.4). `agent::run` becomes relevant
  with server-side tools (§19), and needs live relay of `TextDelta` first.
- **ASR: `proxy::transcribe`** (`proxy/transcribe.rs`). Today it opens its
  own route per call (`multipart_call` → `gate::open(…, RouteCheck::Audio)`),
  logs with `RequestCtx::default()`, and runs no policy check.
- **TTS:** there is no in-process helper. `handle_audio_speech` returns an
  axum `Response`. `audio_json_call` opens its own route per call too.
- **Residency.** `gate::open` returns `Opened { route, hold:
  Option<LocalHold>, headers }`. `LocalHold` wraps the registry's
  `AcquireGuard`. While it lives, the model counts as in flight and cannot be
  evicted; dropping it releases the claim. It is a counter, never a
  serializing lock.
  `VramManager::check_background_start` (`runtime/lifecycle.rs:190-208`) is
  the non-evicting admission that warm starts use: when the GPU is full it
  skips rather than evicts.
- **GPU hold.** A hold never kills work. A model with a live `LocalHold`
  stays *draining* until the hold is dropped (hold §1, §4). §9 depends on
  this.
- **Policy.** `policy::check_alias` (behind `policy_or_refuse`) enforces a
  key's scope and budget on public routes. `state.policy.admit(key)` hands
  out the key's concurrency slot. Two gaps matter here:
  - `stream_once_on` does **not** run a policy check, and
    `policy::check_internal` resolves identities from the ingress label, not
    from a client key.
  - A public handler's concurrency guard is released at the 101, because
    hyper drops the response body on upgrade (`server.rs:1011-1017`).
- **WebSocket.** The only existing WebSocket code is `web/agent_proxy.rs`, a
  raw hyper byte pipe that never parses a frame. axum's `ws` feature is off,
  and `tokio-tungstenite` is not in the lock.

### 3.3 audio.cpp constraints

- **No streaming VAD.** Its VAD (`vad` task: Silero, MarbleNet, PulseVAD) is
  reachable only as a whole-clip task, and `/v1/tasks/stream` answers with
  one buffered document. There is **no per-frame VAD over HTTP** and no
  end-of-turn model.
- **Live ASR partials** exist on `/v1/audio/transcriptions/live` (chunked
  PCM in, SSE `transcript.text.delta` out) for rows with `mode: "streaming"`
  (nemotron_asr, qwen3_asr, voxtral_realtime, …). lmgw does not proxy that
  route; doing so needs a duplex client (§19).
- **Streaming TTS** exists for rows with `mode: "streaming"` (`stream_format:
  "sse"` or `"audio"`, `response_format: "pcm"`). Which families really
  stream must be read from the live catalog. lmgw can already configure a
  row's `mode`.
- **Conversational speech-to-speech.** PersonaPlex (7B, English only, no
  tool calling) sits behind `/v1/audio/speech/live`. It is a different
  product and is not used here (§19).

## 4. Architecture

### 4.1 One session = one task tree

```
WebSocket ──► reader ──► control events ──────────────► session core
   ▲             │                                          │   ▲
   │             └─► input_audio_buffer.append ─► audio-in  │   │
   │                                              (decode,  │   │
   │                                               resample,│   │
   │                                               VAD/turn)┘   │
   │                                                            │
   └──── writer ◄── server events ◄── responder (ASR → chat → clauses → TTS)
                     (paced audio)
```

- **reader:** parses frames into `ClientEvent`s (serde, tagged on `type`).
  An unparseable or unknown event gets an `error` echoing its `event_id`,
  and the session continues, as with OpenAI.
- **audio-in:**
  - decodes base64 PCM16 and keeps the session's input ring buffer at 24 kHz;
  - resamples to 16 kHz for the detector;
  - runs the turn detector (§6): Silero inline, at ~0.1 ms of CPU per
    32 ms frame, and for `semantic_vad` Smart Turn on the blocking pool,
    once per pause, at ~18 ms on 4 threads.
- **session core:** owns the session config, the conversation, the response
  state machine and the output queue. It is the only writer of conversation
  state.
- **responder:** runs one response at a time (ASR if pending, chat stream,
  clause splitter, TTS per clause), under a **response generation id**.
- **writer:** serializes `ServerEvent`s. Output audio is paced (§8.2). Every
  queued audio or delta event carries its response's generation id. A cancel
  bumps the id and **purges queued events of the old id** before
  `response.done {cancelled}` goes out, so no stale audio follows a cancel.

Everything is tokio tasks except the detector worker. The heavy stages (ASR,
TTS) are HTTP calls, and the only in-process compute is the small detector.

### 4.2 A turn, end to end

1. **Speech.** Audio frames arrive and the detector sees speech, which sends
   `speech_started` (subject to §6.4). The core fires a **non-evicting
   background warm** of the session's ASR, chat and TTS models (§9.1). A cold
   container starts while the user is still talking; nothing is evicted for
   it, and nothing is pinned.
2. **Commit.** End of turn (§6) sends `speech_stopped` and `committed`, then
   user item `added`. The segment, pre-roll included, is sent to ASR as a
   16 kHz mono WAV, under an ASR hold taken for that call.
3. **Transcript.** The transcript sends `input_audio_transcription.completed`
   (if requested) and user item `done`. **An empty transcript** (a detector
   false positive on noise) keeps the user item with an empty transcript and
   **starts no response**, even with `create_response` on. If
   `create_response` is on and the transcript is non-empty, a response
   starts.
4. **Response.** `response.created` snapshots the session config for this
   response, so a mid-response `session.update` applies to the next one. The
   chat and TTS routes are opened and held (§9.1). The chat stream feeds the
   clause splitter (§8.1). Each clause is synthesized, resampled, and queued
   with its transcript delta ahead of its audio.
5. **Done.** Paced playback of the last clause ends, and the done events and
   `response.done` (with usage) go out. The holds are dropped.

### 4.3 State rules

- **Phases of a response.** Pending (committed, ASR running, no
  `response.created` yet), Generating, Closing (generation finished, paced
  audio still being sent) and Playing (the writer drains the last audio, and
  the client plays it).
  - The writer reports to the core when a generation is **drained**: its
    output has left, and the client has played its audio — the playing
    window's end (§6.4, owner's decision Q1).
  - Only then does the core emit the closing events and `response.done`.
  - A barge-in during Playing — to the last moment of playback — therefore
    still has a response to cancel, and a finished response's audio is never
    purged by a later cancel. Purges name exact generations, and a cancelled
    generation's drained marker is due at once.
- **Cancel.** It is cooperative: the stream's sink stops it, it is never
  aborted.
  - The model call's usage row is still written, with status `cancelled` and
    the partial usage, so cancelling does not evade budgets or vanish from
    Usage.
  - A cancelled response closes its open items with `status: "incomplete"`
    (`output_item.done`, `conversation.item.done`) before `response.done`,
    because `@openai/agents` updates its history only from item events.
  - Only queued *audio* is purged; text already produced stays in the item.
  - A cancel that lands after generation finished, in text mode, finishes the
    response as `completed`, because the client may already have run its
    tool.
  - `response.cancel` with nothing active gets `error {code:
    "response_cancel_not_active"}`, as with OpenAI.
- **One response at a time.** `response.create` while a response is
  **generating** gets `error {code: "conversation_already_has_active_response"}`,
  echoing the client's `event_id`. `@openai/agents` matches errors to its own
  `response.create` ids.
  - **Closing phase.** A `response.create` that arrives while the active
    response is only **closing** is **queued** and starts right after that
    response's `response.done`. Closing means generation is finished and only
    paced audio or the closing events are left. The stock client sends its
    post-tool follow-up exactly then (§2.3).
  - **Metadata.** `response.metadata` is echoed on the response object.
- **No response starts while the user's turn is open** (owner's decision
  Q3, a deliberate departure from OpenAI, which would start it). A
  `response.create` that arrives while the user speaks, or one queued behind
  a response that ends then — a barge-in's cancel — is **held** with the
  owed debt and starts once, after the turn, answering it too.
  - `@openai/agents` sends its tool follow-up right after the cancel's
    `response.done`, while the user still talks: started at once it would
    talk over the user; refused, the client would report an error on every
    barge-in after a tool preamble and never retry.
  - A second one meanwhile is refused as
    `conversation_already_has_active_response` — until the held one has
    started, so also after the turn's commit while its transcript is still
    being made: it answers that turn, and a second would be a second
    response (B3 review 6).
  - A held create is answered even when the turn has no words: the client
    asked. It is checked when it arrives, so one that can never start gets
    its error at once.
- **Speech during the ASR or pre-response phase** (committed, no
  `response.created` yet) cancels the pending automatic response. The new
  speech becomes the next user item. The two adjacent user items are merged
  when rendered (§7.2).
- **Speech during a response** that stays below the barge-in threshold
  (§6.4) is treated as backchannel ("mhm") and **dropped**. It is neither
  committed nor answered. Speech that crosses the threshold is a barge-in,
  and its audio from the back-dated onset becomes the next user item.
  Every other turn that starts during a response is announced and
  committed too; none is dropped silently.
- **A barge-in** cuts the response in a fixed order: the owed response is
  deferred first, so nothing the cancel ends can start another response
  while the user talks; then its queued audio is purged, which says whether
  the client **heard** any of it; then, if it did not, what it owes is
  settled again; then the cancel, `turn_detected`, by the same rule as
  `response.cancel` (§6.4).
  - **Heard** means audio samples or a transcript delta left the writer,
    or a text delta went out — not that an item was announced (B3 review
    1). A function call says nothing to the listener.
  - A response cut before it was heard answered nothing yet: its turns are
    owed again, ahead of anything owed since, and if a client created it,
    its `response.create` — or the one queued behind it, which renders the
    same conversation — is held and starts again after the turn, answered
    even if the turn has no words. `@openai/agents` sends its tool
    follow-up while nothing plays, so the normal onset applies there: a
    cough during its generation no longer strands the tool's result.
  - An answer the client was already hearing is **not resumed** when the
    turn that interrupted it has no words (owner's decision Q2, OpenAI's
    behaviour; the evidence gate already filters coughs). No response
    follows, and an INFO line says why.
  - A cough that cuts a response nobody heard yet does not strand the
    question before it, nor a client's follow-up: the question is owed
    again and answered, the follow-up starts again.
- **`input_audio_buffer.clear`** drops uncommitted audio and resets the
  detector.
- **A truncate of the item still being produced** stops further audio for it
  and cancels the rest of the response, as a client-side stop.

## 5. Model selection

### 5.1 The chat model

What the client sends is resolved in this order, later wins:
1. `?model=` on the URL, if present;
2. `session.model` in any `session.update`.

A handshake **without** a model is allowed (`@openai/agents` with `url`);
the session then starts on the setting `realtime.default_model`.

Each name is resolved in turn:
- **a name in the setting `realtime.model_map`**: the mapped alias. An
  explicit mapping is the owner's intent, so it wins.
- **an OpenAI Realtime model name** (`gpt-realtime*`, `gpt-4o*-realtime*`):
  `realtime.default_model`. These names are never resolved as chat aliases,
  because a catch-all passthrough upstream would otherwise route them to a
  chat endpoint that cannot serve them.
- **a lmgw chat alias** (local, cloud, ladder, candidate): used as is.
- **anything else**: the handshake fails with 404, or a `session.update`
  gets an `error`. The Hugging Face client's `local` is in this case unless
  it is mapped.

If no default model is configured, a model-less handshake opens with
`lmgw.resolved.chat: null`, and the first `session.update` must name a
model.

Every substitution is visible:
- the session object echoes `lmgw.resolved.chat` (and `.asr`, `.tts`,
  `.voice`);
- the log line names the substitution.

That is the rule for all three models and the voice: **a fallback is
allowed only when it is echoed and logged**. Stock SDK sessions work
unchanged, and nothing is silent.

### 5.2 ASR alias

The ASR alias comes from `audio.input.transcription.model`, resolved the
same way as the chat model (§5.1), against ASR-capable aliases:
1. `realtime.model_map`;
2. an OpenAI transcription name (`gpt-4o*-transcribe`, `whisper-1`) maps to
   the setting `realtime.asr_alias`;
3. a real ASR alias — a cloud one included: an OpenAI-protocol model whose
   catalog says nothing has its name's task (`openai/gpt-4o-mini-transcribe`
   is `asr`, R3 D1);
4. with no transcription model at all, `realtime.asr_alias`, then the Chat's
   `chat_stt_alias`.

A name that is an alias of another task is refused `not_an_asr_alias`,
naming its task (a `model_map` target too); `unknown_alias` is for a name
that is no model at all (R3 D1).

A *broken* `realtime.asr_alias` (one that names no ASR model) is reported
in the `asr_not_configured` message. It does not fall through to
`chat_stt_alias`, so a model the owner did not pick for voice never answers
silently.

With nothing configured, an audio session fails at its first commit with
`error {code: "asr_not_configured"}`. Text sessions still work. A detected
turn that ends this way never commits, so what its speech deferred — an
owed response, a held `response.create` (§4.3) — decides at once, as after
a clear (B3 review 7).

The cascade always transcribes, because the chat model needs text.
`audio.input.transcription` only decides whether the transcription events
are sent, which keeps the protocol's meaning intact.

### 5.3 TTS alias and voice

The protocol has a voice field but no TTS-model field.

**TTS alias:** `session.lmgw.tts_model` (§5.4), else the setting
`realtime.tts_alias`. Unset means audio output fails with
`tts_not_configured`; text output works. Its capability task is `tts`, or
`vdes` — a voice designed from the session's speech instructions (WP10,
§5.5) — one list, `lmgw_api_types::realtime::SPEECH_TASKS`, for the
session, the setting's save check, the dashboard's picker and `/v1/models`.
A cloud TTS needs no override for it: an OpenAI-protocol model whose catalog
says nothing has its name's task (`openai/gpt-4o-mini-tts`, `tts-1`; R3 D1).
A `session.lmgw.tts_model` that is an alias of another task is refused
`not_a_tts_alias`, naming its task; `unknown_alias` is for no model at all.

**How it speaks** — the speech instructions, the seed of a designed voice,
the delivery cues (WP9b) and the hint about its sounds or cues — is §5.5's
"Expressive speech in a session" (WP10). Named design presets (a style or a description plus a seed, under
one name a voice can be asked for) are not built; they are §19.

**Voice**, in order:
1. `audio.output.voice`, if the TTS model knows the name (its
   `/v1/audio/voices` list, its presets, or — for an lmgw audio row — a
   voice its package ships, matched without case, §5.5). An OpenAI TTS —
   the OpenAI protocol on a generic upstream, a cloud TTS — knows OpenAI's
   built-in names (`alloy`, `marin`, …), sent lowercase as asked, as for an
   OpenAI fallback (R3 D2): neither `default_voice` nor a `voice_map` entry
   stands in for them there;
2. a name mapped by the setting `realtime.voice_map`;
3. an OpenAI built-in name (`alloy`, `marin`, …): `realtime.default_voice`,
   then the TTS row's `default_voice_preset`, echoed in `lmgw.resolved.voice`;
   with neither, a TTS that designs its voice from the speech instructions
   (`voice_design`, §5.5) is sent no voice at all — its engine reads no
   speaker (Qwen3-TTS VoiceDesign, MOSS-VoiceGen) — echoed as `designed`
   (R2). Such a TTS passes over a `default_voice` it is not known to have
   (not a preset, a library clip or in its list): the owner's setting
   names another model's voice (R3 N2). The same holds for a TTS whose
   engine speaks with a fixed voice of its own when none is named
   (MagpieTTS, Kokoro, Supertonic — `audio::families::unvoiced`, read off
   audio.cpp's source): sent no voice, echoed as `engine_default` (R3).
   OmniVoice is not one of them (R5 F1): named no voice, it draws a new
   speaker per request, and a session's clauses are requests — the
   session's seed fixes each clause's draw, but the clause's text still
   picks the speaker (live run 3c: 222, 118 and 222 Hz within one answer).
   It takes a `default_voice` only when it knows it (a preset, a library
   clip, its list), and with no voice configured — none from the client,
   no such `default_voice`, no default preset — its voice is missing, the
   message saying why (it draws a new speaker per request; configure a
   voice clip or preset). With a voice its clauses still carry the
   session's seed (§5.5, R4 M1); `/v1/audio/speech`, one request one
   speaker, still speaks it unnamed. Its drift is the model's own
   behaviour, and lmgw adds no switch to another model to hold it (the
   owner's decision, R7 in §20). A TTS that clones from
   reference audio (CosyVoice3, Chatterbox, IndexTTS2, MioTTS, Qwen3-TTS
   Base) takes `default_voice` only when it is one of its clips — a
   voice-library clip, a preset of its row with a `voice_ref`, or a name
   its list shows — and skips any other name too (R4 D5). Both rules —
   this one and OmniVoice's — need lmgw to see the engine's voices: the
   class's `voice_dir` is the voice library, or empty (then the engine
   answers to no clip at all, R6), or a response of the session has read
   the model's list. A voice dir mounted for the engine alone
   (`extra_run_args`) is a supported setup that lmgw cannot list, so
   there, with the list unread, `default_voice` is sent provisionally as
   to any model (a preset of the row that loads no clip excepted) and the
   first clause's list decides (R5, R4 review). That list stays seen for
   the session (R6): forgetting it for a fresh read (below) does not make
   the voices unseen again, so a `default_voice` it ruled out is refused
   before the next `response.created`, nothing started. Only another TTS
   alias drops it, or the owner changing what it was read under —
   `default_voice`, the class's `voice_dir`, the row's container
   configuration — not any new settings snapshot (one is published for a
   learned residency, too). A clip added to a mounted dir meanwhile is not
   seen by an open session until then, as the library is re-read only by
   a `session.update`;
4. `{id: "…"}` names a voice-library clip;
5. any other name gets `error {code: "voice_not_found"}`.

If the chain resolves to no voice at all, the session reports it rather
than calling the engine without one, because at least one engine (§3.1)
refuses to speak without a voice (a voice-design TTS, or one whose engine
has a fixed voice of its own, which need none, never gets here; one that
clones from reference audio — CosyVoice3 — does when its row's inline
default preset loads no clip, R3 N4, or when `default_voice` is no clip of
it and its row has no default preset, R4 D5; and one that draws its
speaker per request — OmniVoice — does with no voice configured, R5 F1):
- `resolved.voice` is echoed as `null`;
- audio responses fail with `voice_not_configured` before `response.created`;
- text responses keep working.

**Nothing is asked of the model to resolve a voice.** The handshake and
every `session.update` resolve against what is known without a request: the
row's presets, the voices its package ships (§5.5), the voice library
(re-read when the alias or the voice changes), and a voice list an earlier
response read. Asking the model meant
an admission on the session core — a cold start or an eviction that froze
the session, under the GPU hold a refusal read as `voice_not_found` — and a
container that is up but does not answer hung the handshake or the core.
- A name the known facts show is verified; an unknown name the facts can
  rule out refuses the `session.update` with `voice_not_found`.
- A name only the model's list could confirm (rule 1 with the list unread,
  or a `voice_map` / `default_voice` target the facts do not show) is
  accepted **provisionally**: echoed, and logged as unverified.
- It is checked at the response's **first clause**, on the TTS route that
  response holds (§9.1), raced against the response's stop (which the end
  of the session raises). An audio.cpp model's list is read there — for an
  lmgw audio row lmgw's own list (§5.5), with no request, the engine asked
  as well only when the class's `voice_dir` is not the voice library; a
  voice it lacks fails that response with `voice_not_found` — or
  `voice_not_configured` for one the owner's settings named — and the
  session stays open. A list that cannot be read, or a cloud TTS that has
  none, leaves the engine to judge.
- **The list read there decides** (package B review 5). It is kept for the
  session, and the session's voice is resolved again against it at once:
  verified if the list shows it, and otherwise
  - the session's own name is **not found**: the next audio response is
    refused with `voice_not_found` (param `session.audio.output.voice`),
    and a `session.update` naming a voice the list rules out is refused at
    once;
  - a target the **owner's settings** named — a `voice_map` target,
    `default_voice`, the row's `default_voice_preset` — is **missing**
    (B2 review 4): the owner's misconfiguration is `voice_not_configured`,
    said by audio responses only. It never refuses the client's
    `session.update` (a text session never trips over it), so a built-in
    name such as `marin` is never `voice_not_found`.
- **Neither is for good** (B2 review 5). Before each response is created
  or started, the session resolves its voice again when the settings
  snapshot changed since it last did — the owner fixed `voice_map`,
  `default_voice` or the TTS row (its presets and default preset are read
  again; the voice library is a directory listing, read again only by a
  `session.update`). And while the voice is not found or missing, the list
  read before is forgotten, so the next audio response's first clause
  reads the model's list again: a voice added to the model is seen. The
  price: while the voice stays wrong, each audio response opens its TTS
  route and fails at its first clause rather than before
  `response.created` — except where rule 3's clip or known-voice check
  decides by the list seen (R6), which refuses before.
- **A fallback speaks its own voice.** When the GPU hold or admission
  answers with the TTS alias's fallback, the voice is resolved once more
  against it, by what its upstream is — its protocol and kind, not merely
  "not audio.cpp" (package B review 7):
  - an **OpenAI** one (the OpenAI protocol on a generic upstream, a cloud
    TTS) speaks the requested OpenAI name, or what `voice_map` maps it to;
  - an **audio.cpp** one runs the chain over its own list, and each rule
    falls through to the next when the fallback lacks its voice: the name
    asked for, its `voice_map` target, and for an OpenAI name
    `realtime.default_voice`, then the fallback row's
    `default_voice_preset` (an inline preset last, sent as no voice). The
    first voice the fallback has speaks. A list that cannot be read does
    not refuse (B2 review 8): the row's presets and the voice library still
    answer, then its inline preset, and otherwise the voice the primary's
    chain would send unseen — the `voice_map` target or the name itself,
    for an OpenAI name `default_voice` or the default preset — goes
    unverified, and the engine judges it, as for the primary;
  - anything else (a llama.cpp or sd.cpp upstream) fails with
    `voice_not_configured` naming the fallback.

  The substitution is logged.
- `response.create`'s `audio.output.voice` (and `speed`) apply to that
  response through the same chain; a `format` other than the session's
  PCM16 at 24 kHz is an `unsupported` error. An error about that voice —
  at the create, or at the first clause — names
  `response.audio.output.voice` (package B review 6).
- **`speed`** is checked against the GA range, 0.25 to 1.5: outside it a
  `session.update` or `response.create` gets an `invalid_value`
  `invalid_request_error` naming `session.audio.output.speed` or
  `response.audio.output.speed` (package B review 10).

Voice-library clips count as names the model knows. `response.audio.output.voice`
always echoes a string, because `@openai/agents`' schema requires one there,
while the session echo keeps an `{id}` object as sent.

### 5.4 The `lmgw` extension object

`session.lmgw` carries the knobs the protocol has no field for:
- `tts_model`;
- the turn-detection extensions (`barge_in_min_ms`, `barge_in_guard_ms`,
  `post_interrupt_silence_ms`, `half_duplex`, `echo_tail_ms`,
  `barge_in_check`, `barge_in_check_timeout_ms`);
- `output_lead_ms`, `synthesis_ahead_s` and `longest_pause_ms` (§8.2);
- the expressive knobs (WP10, §5.5): `speech_instructions` (string; `""` =
  none for this session, absent or `null` = the owner's
  `realtime.speech_instructions`), `speech_seed` (u32, pins the TTS seed)
  and `tag_hint` (bool, the hint in an audio response's prompt about what
  square brackets do: the sounds the TTS makes, or since WP9b the delivery
  cues it takes).

`speech_instructions` and `speech_seed` are echoed only as the client set
them, as `tts_model` is; `tag_hint` echoes the setting when the client sent
none, as `half_duplex` does. `response.create`'s `response.lmgw` is a strict
object of its own with `speech_instructions`, for that one response.

The server writes `session.lmgw.resolved`, and unknown keys are an `error`.
Since WP10 it holds `speech` too — what the TTS alias speaks with, `null`
without one:

```json
{"instructions": "style", "text": "calm, warm", "source": "session",
 "dropped": false, "tags": "none", "cues": true, "tag_hint": "Your words are …",
 "seed": null}
```

`instructions` and `tags` are the primary's `capabilities.speech` words (a
cloud alias the owner did not describe: `passthrough`, `none` — reachable
since R3, when OpenAI's TTS became a TTS by its name); `source` is
`session`, `setting` or `row`; `dropped` says the TTS reads none. `cues`
(WP9b) says the primary takes delivery cues (§5.5) — a `style` or
`passthrough` TTS that renders no tags, but not a cloud alias nobody
described, whose `passthrough` is only assumed; `tag_hint` holds the
paragraph the prompt gets, about its sounds or its cues (§7.2).
Standard clients never send the object, and the echoed session always
includes it, so the defaults in effect are visible. Hugging Face uses the
same pattern (`session.extensions`).

### 5.5 Voices, languages and speech shaping (audio-class gaps, packages AC1 and AC2, built)

What a TTS row's engine accepts by name comes from its package, not from
the engine's own voice list, and `POST /v1/audio/speech` and every realtime
clause send it the way the engine reads it. Package AC1 built the profile,
voices and languages; AC2 added instructions, inline tags, the published
`capabilities.speech`, the voice-design task, incomplete packages, clip
transcripts and streaming rows (the end of this section).

**The speech profile** (`audio/profile.rs`), per local audio row:
- **Spec**: the row's `model_spec_override`, else the catalog snapshot's
  family (never fetched for this), else the spec the package GGUF embeds.
- **Package GGUF** (`gguf/embedded.rs`): audio.cpp packs the spec and every
  small file of the model directory into the header, as one `uint8` array
  (57 MB for Supertonic). lmgw keeps the file table and seeks past the
  array; one named file is one seek and one read, refused over 1 MiB with
  an error naming it. The GGUF is the row's weights file by the residency's
  rule (`audio/files.rs`, §9.4).
- **Native voices**: the spec's `voice_id` request enum (MagpieTTS:
  `Aria`, `Jason`, `John`, `Leo`, `Sofia`; read from `options.voice_id`),
  `ui.builtin_voices` (Kokoro, Pocket TTS), the embedded spec's
  `voice_style_<name>` sources (Supertonic: `F1`…`M5`), the embedded
  `config.json` `talker_config.spk_id` of a Qwen3-TTS CustomVoice package
  (nine lowercase speakers; VoiceDesign and Base have none). A built-in
  voice a spec package keeps as `embeddings/<name>.safetensors` is present
  only when the row's root has that file (Pocket's `alba` ships with the
  English package only); otherwise it is **missing**.
- **Default voice**: `ui.default_voice`, else the `voice_id` option's
  default.
- **Language vocabulary**: the package's table (Qwen3 `codec_language_id`
  names plus `auto`), else the spec's `language` enum, else its `languages`
  when they are names, else its `languages` for a family the table lists
  (Kokoro). ISO codes in `languages` describe the model, not its request,
  and are no vocabulary otherwise.
- Cached in `AppState` per row, keyed by model id, family, path, task,
  weight id, spec override, the catalog's fetch time and the GGUF's path,
  length and mtime; computed on the blocking pool. A fact that cannot be
  read is logged and left out.

**The voice list** (`audio/voices.rs`) is the row's presets, the library's
clips (when the class's `voice_dir` is the library), the present natives
and the root's `embeddings/` stems — one entry per name, in audio.cpp's own
precedence for `voice` (preset, then library clip, then voice id).
`GET /v1/audio/voices` answers a local row with it from lmgw, starting
nothing (no admission, no claim, no request row):

```json
{"voices": ["Aria", "Jason", "…"],
 "lmgw": {"source": "config", "engine_asked": false,
          "entries": [{"id": "Jason", "kind": "native", "send": "options.voice_id"},
                      {"id": "me", "kind": "library", "send": "voice", "transcript": true}],
          "default": "Aria", "missing": []}}
```

with `x-lmgw-voices-source: config`. `?probe=engine` is the admitted read of
the model's own server (it starts the container), `x-lmgw-voices-source:
engine`; the audio lab's "load voices" (`start=1`) is that probe. A remote
route is asked as before. A row whose `extra_run_args` mount another voice
directory needs the probe to see its clips. Under the GPU hold (or a
benchmark's lease) a local row without a fallback is still answered from
config, with `lmgw.held: true` — the hold is a zero VRAM allowance, not a
missing model, so a voice picker keeps working; with a fallback the list is
the fallback's (AC2).

**Shaping** (`audio/shape.rs`, `proxy/audio/speech.rs` and
`Synthesis::speak`): `/v1/audio/speech` is resolve → preflight → admit →
shape on the final route → send. An lmgw audio row is shaped in full; any
other route (a fallback included) gets the expressive half only — its
instructions and inline tags (AC2). `input` is touched only for its inline
tags.
- A native voice named in any case is sent in the package's spelling;
  for a family that reads `options.voice_id` (Magpie) it moves there and
  `voice` is dropped. A preset's, or with no `voice` the row's default
  preset's, `voice_id` naming such a voice is copied there too. A preset or
  library clip of the name is the engine's (its precedence); a client's own
  `options.voice_id`, and for the default preset the row's
  `default_request_options.voice_id`, win.
- `language` (and `options.language`) go in the vocabulary: exact entries in
  its spelling, an ISO 639-1 code or BCP-47 primary by its English name
  (`de`, `de-DE` → `german`), a region form or the family's preferred region
  (`en` → `en-us` for Kokoro), the one entry with that primary; anything
  else as it came.
- `x-lmgw-speech` says what changed: `voice=options.voice_id`,
  `voice=preset->options.voice_id`, `voice=<spelling>`,
  `language=de->german`. Absent when nothing did.

**Instructions** (AC2, gap 9a). audio.cpp hands a request's `instructions`
to the engine as `options.instruction`. The profile says what the row does
with them: a spec declaring an `instruction` option passes them
(`passthrough`); one declaring only `instruct` (Auk, MOSS) gets them moved
to `options.instruct`; Qwen3-TTS by its GGUF's variant — CustomVoice a
`style`, VoiceDesign the `voice_design` description (required: without one
in the request or the row's default request options the request is 400
`instructions_required`), Base none. Two engines read `instruction`
although their spec declares no option for it, so a family table
(`audio::families::undeclared_instructions`, read off audio.cpp's source)
says so and wins over the spec: OmniVoice `passthrough`, MOSS-VoiceGen
`voice_design` (required, as above). Every other family drops them
(Kokoro refuses an unknown option) and says `instructions=dropped`. A
remote route passes them, unless the owner's override on the alias says
`none`. audio.cpp merges a row's `default_request_options` under the
request's options key by key, and the engines that read both synonyms
(Qwen3-TTS, MOSS-VoiceGen, MOSS-TTS v1.5, MOSS-TTSD, dots) refuse an
`instruction` and an `instruct` that differ. So a text the request sends
under one key, on a row whose defaults say something else under the other,
goes under that key too (R2): the request's replaces the row's, as any
request option does, and `x-lmgw-speech` says
`instructions=also:options.instruct` (or `options.instruction`). A key the
client sets itself is left as it came.

**Inline tags** (AC2, gap 9b, `audio/tags.rs`). The canonical client syntax
is `[tag]` (a lowercase letter, then up to 30 of lowercase letters, space,
`_`, `'`, `-`); `(…)`, `<…>`, `*…*` and a capitalised `[…]` count only when
the words are a stage direction of a closed list (`laughs`, `sighs`,
`gasps`, `clears throat`, …); `<|…|>` markup and `[S1]` labels are never
tags. A `fixed` family keeps the tags its tokenizer renders and maps stage
directions onto them — OmniVoice and CosyVoice3 (`[laughs]` →
`[laughter]`); a `free` family keeps every tag (none yet: Fish waits for
the owner's live probe, `audio_probes_live`); every other family, and a
remote route without an override saying otherwise, has them stripped and
the text rejoined. A tag is never read out. An input of nothing but tags is
400 `empty_input`.

**Preflight** (`audio/preflight.rs`, AC2): before admission, on the
resolved route — nothing is started for a request the engine would refuse:
`empty_input` (any route); on an lmgw row `task_mismatch` (its task is not
the one its package's variant runs), `streaming_unsupported` (any
`stream_format` or `stream: true` on an offline row), `instructions_required`,
and (R3 N4) `reference_required`: a family that clones from reference audio
(CosyVoice3, Chatterbox, IndexTTS2, MioTTS, Qwen3-TTS Base) asked to speak
with no `voice`, no `voice_ref` and no default preset that loads a clip —
the engine answered 500 after its container started. A `voice` the request
names is the engine's to judge. All are `400 invalid_request_error` with
that code.

**What an engine does with no voice** (R3, `audio::families::unvoiced`,
the profile's `unvoiced`), only where audio.cpp's source shows it:
OmniVoice draws a speaker (`omnivoice/session.cpp`), MagpieTTS takes its
first baked one (`magpie_tts/request.cpp`, `types.h`), Supertonic speaks
`M1` (`supertonic/session.cpp`, `session.h`), Kokoro `af_heart`
(`kokoro_tts/frontend.cpp`) — those two only when the package ships it.
CosyVoice3, Chatterbox, IndexTTS2, MioTTS and Qwen3-TTS Base refuse without
reference audio. Every other family is unknown: Pocket TTS refuses without
a voice, Qwen3-TTS CustomVoice without a speaker, and realtime keeps naming
one. Realtime names one for OmniVoice too (§5.3, R5 F1): its drawn speaker
holds for one request, and a session's answer is many.

**`capabilities.speech`** (AC2, `capabilities/speech.rs`) publishes it per
speaking row (`tts`, `vdes`): `instructions` (`none` | `style` |
`voice_design` | `passthrough`), `instructions_required`, `inline_tags`
(`none` | `fixed` | `free`) and `tags`, `languages` (ISO 639-1),
`streaming`, `sample_rate`, with a notes line. A cloud alias has one only
through the owner's `capabilities_override` — which is also what shaping
of that alias goes by.

**Realtime.** `VoiceFacts.native` holds the row's present natives, matched
without case, so `Ryan` resolves verified against Qwen3's `ryan`. A
response's clauses carry the session's `audio.input.transcription.language`
as a hint: sent only to an lmgw row whose vocabulary comes from its package
or spec enum or names, or whose spec has a `language` option and lists the
code (Magpie `de`); never to Kokoro (it derives the language from the voice
and refuses a mismatch) and never to another route. Elsewhere the engine's
default stands, as before.

Since AC2 each clause also gets the expressive half and the preflight:
inline tags are mapped or stripped in what is sent. **Since WP10 the
transcript has no tags** — it is what was said, and the listener heard a
laugh, not a word; the model's history keeps them (§7.3). AC2's rule was
the other way round. A clause of nothing but tags is not sent at all (§8.1).
The response's TTS route is refused when it opens (before admission) for a
row whose task its package does not run, and (R1) for a voice-design row
with nothing to design from — judged on the speech instructions the
response's clauses carry, or the row's own default. The session gets the
`error` and a failed `response.done`, and goes on. The session's warm skips
such a row the same way (§9.1). `Synthesis::speak` returns the
`ShapeReport`; the first clause's losses are logged once per response.
Per-clause streaming is still §19.

**Expressive speech in a session** (WP10, built). GA has no speech-style
field, and `session.instructions` is the chat model's prompt — long, often
about tools, and on a voice-design row it would design a voice from it — so
the style is lmgw's: `session.lmgw.speech_instructions`, a response's
`response.lmgw.speech_instructions` and the owner's
`realtime.speech_instructions` (`realtime/expressive.rs`).
- **Precedence:** the response's, the session's, the setting's, then the
  TTS row's own description (its default request options' `instruction` or
  `instruct`, which audio.cpp merges in). `""` at a level is "none": lmgw
  sends nothing, and a row's own description still holds. On a voice-design
  row that has a description the setting stands back — a style is not a
  voice. A text that is sent replaces the row's: shaping writes it under
  the row's key too (R2), since audio.cpp would merge the row's in beside
  it and an engine that reads both refuses two that differ.
- **Per mode** of the primary TTS: `style` and `passthrough` get the text
  with every clause; `voice_design` designs its voice from it — with no
  description from any source an audio response is refused before
  `response.created` with `instructions_required`, param
  `session.lmgw.speech_instructions`, nothing admitted, text responses
  untouched; `none` has it shaped away (`dropped: true`), said in one INFO
  line per resolution, never per clause.
- **Seed:** without one Qwen3-TTS draws a random seed per request, and a
  designed voice changes from clause to clause; OmniVoice, sent no voice,
  samples its speaker from an RNG seeded once at random, so its voice
  changed clause by clause too (R4 M1) — and with the seed it still does,
  the clause's text picking the speaker (live run 3c), so a session no
  longer sends it no voice (§5.3, R5 F1). A session draws one at random, or
  takes the client's `speech_seed`, and a clause carries it only to an
  lmgw row that reads one (its spec declares `seed`, or the family table
  lists `qwen3_tts` or `omnivoice`) — when the client pinned it, or when
  the row's voice comes from the seed (it designs its voice, or it draws a
  speaker when named none: `Unvoiced::DrawsSpeaker`) and pins no `seed` of
  its own (the owner's per-row pin wins). It is decided on the final route, so a remote route never gets
  it. A seed fixes the RNG, not the timbre: different text samples another
  path, and the owner heard the voice drift (R7 in §20) — between the
  clauses of one answer, and between sessions with the same seed. That
  drift is the model's own behaviour; lmgw adds no switch to another model
  to hold it. A voice that must hold is a clip for a cloning row (§19's
  voice-sample generator).
- **The hint** (§7.2) and the echo come from the primary. A fallback is
  shaped per clause by its own rules: its tags never read out, its
  instructions passed or dropped, a delivery cue taken when it takes cues.
  A designed voice's description reaches a cloud fallback as its style.
- **Delivery cues** (WP9b, built; `audio/cues.rs`). The tags that open a
  sentence (§8.1) are a cue on a TTS that takes cues: a `style` or
  `passthrough` one that renders no tags (`none`, or `fixed` with an empty
  list — the hint's own condition), decided by one predicate,
  `lmgw_api_types::realtime::takes_cues`, which the dashboard calls too.
  That is Qwen3 CustomVoice and the local passthrough rows without tags —
  Auk, MOSS-TTS v1.5/TTSD, Qwen3 of unknown variant. A cloud alias nobody
  described takes none: its `passthrough` is lmgw's assumption, not a
  declaration (`capabilities.speech.instructions` absent), and
  gpt-4o-mini-tts ignored every cue phrasing tried (2026-10-02: "Say this
  line while laughing.", "Laugh while you say this line…", "Whisper this
  line." — no laugh, no whisper), so the hint would only have the chat
  model write brackets that are stripped. Its style instructions still
  pass. The alias override `capabilities.speech.instructions: "style"`
  turns cues on for one cloud alias. OmniVoice and CosyVoice3
  render tags and keep them: on a TTS that could do both, tags win (a real
  sound beats a style, square brackets keep one meaning in the prompt, and
  OmniVoice's instruction describes its speaker, so per-clause text would
  move the voice). A `voice_design` row never takes one — its description
  *is* the voice, a cue appended to it would make a new voice every clause,
  and the owner ruled voice switches out (R7) — nor does a TTS that reads
  no instructions. A cued clause is sent `"{base}, but {cue} right now"`,
  or the cue alone with no base: appended, never a replacement, so the
  persona stays, and last as the most specific. The contrast is measured:
  with Qwen3 CustomVoice `"{style}; {cue}"` muted the laugh, while
  `"{style}, but {cue} right now"` laughed in 4 of 8 renders against 1 of
  8 (listening tests, 2026-10-02). The base's closing punctuation goes
  first, so a style written as prose ("Speak calmly.") is sent "Speak
  calmly, but laughing right now", not "Speak calmly., but …". The base is the style in effect — the
  clause's instructions, else the answering row's own description (its
  `instruction` or `instruct` default) — since a text that is sent replaces
  the row's under both keys (R2), and a bare cue would wipe a CustomVoice
  row's own style for that sentence. The echo's `cues` (§5.4) is the
  primary's; which clause gets its cue is decided on the route that
  answers (§8.2). No request is added: on a cloud alias declared a style a
  cue is a few input tokens, and the audio may run longer (+0.8 s for
  probe 4e's laugh). `POST /v1/audio/speech` is untouched.
  **Cues are best effort.** Qwen3 CustomVoice treats one as a nudge; the
  text's content dominates. A funny line laughs easily, with or without a
  cue (unprompted, even); a neutral one rarely does — "laughing" alone
  made 0 of 6 neutral renders laugh, and the best phrasing 2 of 6, and a
  whisper was sometimes only half one. lmgw sends the cue and promises
  nothing about how it sounds.

The facts behind it (`SpeechFacts`: mode, tag vocabulary, whether the row
reads a seed, its own description and seed) are gathered with the voice
facts, without a request — a local row's speech profile, a remote alias's
owner override — and the row's defaults and the override are read again
when the settings change.

**Builds.** audio.cpp images install eSpeak NG (`runtime-espeak`, optional
edit of the three audio profiles) so Kokoro loads; see
[backends.md](../backends.md#16-known-limits).

**AC2 as built** (gaps 9, 2, 4, 5, 8, and the held voice list):
- *Instructions* (gap 9a, fixed in R1): a family reads `instructions` when
  its spec declares an `instruction`/`instruct` option **or** the family
  table says its engine reads `instruction` anyway — OmniVoice
  (`passthrough`) and MOSS-VoiceGen (`voice_design`, required). As first
  built only the spec counted, and both had their instructions dropped.
- *Voice design* (gap 2): the catalog suggests `vdes` for a package whose
  id or name says voice design, of a family that lists `design`, and `tts`
  for its siblings. Saving a row whose task its package's variant does not
  run is refused with the fix spelled out — never corrected — and a row
  saved before is refused per request (`task_mismatch`) and noted in
  `/v1/models`. A `vdes` row publishes `/v1/audio/speech`.
- *Incomplete packages* (gap 4): the catalog names a package's
  `missing_files` (no finished download, or the file gone from disk) and
  marks it `incomplete` when it was downloaded once and lacks files now;
  its download queues only those ("Complete install"). A built-in voice
  whose file is missing is noted on the row.
- *Clip transcripts* (gap 5): `proxy::transcribe_local_only` — the hold's
  fallback refused rather than taken, only lmgw's own container for an
  `asr` row, pinned admission — writes a voice-library clip's
  `prompt_text`: per clip (`POST /audio-lab/api/refs/{name}/transcribe`),
  for every clip without one (op `voice_transcribe`,
  `lmgw__voice_transcribe`, which answer lengths, never the words), and on
  upload when `audio.voice_transcribe_alias` names a model (empty by
  default: nothing automatic). A cloning model's notes name the clips that
  lack one.
- *Streaming rows* (gap 8): the catalog suggests `streaming` wherever the
  family lists it (a streaming row answers a plain request with one WAV
  too). `stream_format: audio`'s chunked PCM counts as streamed, its model
  as having run once the relay ends whole. A streamed answer states
  `x-lmgw-sample-rate`, learned in memory from the row's last WAV through
  lmgw (absent until it has answered one since lmgw started).
- *Owner's live probes* (`tests/it/audio_probes_live.rs`, ignored,
  `LMGW_LIVE_AUDIO_PROBES=1`): whether Fish renders free-form tags (fails
  when lmgw's table disagrees), and Supertonic streaming against offline.
  With the variable set, a missing precondition fails the probe instead of
  skipping it (R1). Its VRAM need is worked out per probe from the models
  it holds at once (Fish, then the ASR model; two Supertonic rows), each at
  the larger of lmgw's admission charge plus headroom and the residency
  §9.4 measured, where one is recorded; a shortfall lists the driver's
  per-process holders by container or command (`support/live_vram.rs`).

## 6. Turn detection

Three modes, chosen by the client's `turn_detection`:
- **`server_vad`** (§6.2): Silero decides speech versus silence, and a
  fixed silence window ends the turn.
- **`semantic_vad`** (§6.3): the same detector, with Smart Turn deciding
  when a pause ends the turn.
- **Manual turns** (§6.6).

Barge-in and the turn after it (§6.4, §6.5) work the same under both
automatic modes.

### 6.1 Frames and features

- **Input.** PCM16 24 kHz mono.
- **Silero** works at 16 kHz in 32 ms frames (512 samples).
- **Smart Turn** needs up to the last 8 s of the turn, which is its real
  input size. It reads them at 16 kHz from the stream Silero already
  gets, before the level normalizer (§6.3).
- **Level normalizer.** Before Silero, a boost-only level normalizer lifts
  quiet microphones. It comes from the earlier prototype, with floor 0.008,
  target 0.4 and max gain 32. It never attenuates, and it does not touch the
  audio sent to ASR.
- **Silero context is mandatory (WP0).** The current Silero (v6.2.3) needs
  the previous 64 samples prepended to each 512-sample chunk: input
  `[1, 576]`, state `[2,1,128]`, scalar `sr`. The context resets to zeros
  together with the state.
  - Without the context the model is dead: p < 0.06 on every speech clip.
  - With it, speech scores a mean of 0.77–0.94 and noise stays ≤ 0.12.
  - The earlier prototype's bare-512 path only worked on an older graph.
- **The model file** is `silero_vad_16k_op15.onnx` from the same release:
  1.3 MB, 16 kHz only, and bit-identical in output to the 2.3 MB stock file.
- **Cost.** About 0.1 ms per frame on one thread, so the VAD runs inline on
  the session's task.

### 6.2 `server_vad`

- **Onset.** Silero probability ≥ `threshold` (default 0.5) counts as speech,
  and onset needs a few consecutive voiced frames.
- **End.** The turn ends after `silence_duration_ms` (default 500) of
  unvoiced frames.
- **Blip guard.** A voiced blip resets the silence count only after **~96 ms
  of sustained voice**. The earlier prototype's live bug was a single blip
  holding a turn open forever.
- **Pre-roll.** The committed segment starts `prefix_padding_ms` (default
  300) before the detected onset, so the first word survives detector lag.

**Parity with OpenAI.** The *documented* fields keep OpenAI's defaults and
meanings. The two lmgw extensions (§6.4, §6.5) change timing only around a
response. Setting them to 0 restores plain behaviour.

### 6.3 `semantic_vad` — Smart Turn (WP6, built)

`semantic_vad` is the `server_vad` detector of §6.2 with one thing
changed: Smart Turn v3.2 decides when a pause ends the turn. The decision
is the owner's, taken on their own recordings (scripted rounds 1 and 2: 161
pauses inside unfinished sentences, 109 true ends of a turn, German and
English).

**The rule.**
- **Speech versus silence** stays Silero's, with `server_vad`'s onset, blip
  guard, pre-roll and retention.
- **One score per pause.** Once a pause has lasted 200 ms, Smart Turn
  scores the turn's last 8 s **including those 200 ms of silence**
  ("variant B"). On the frame grid the score is asked at 224 ms.
- **p ≥ threshold:** the turn commits now.
- **floor ≤ p < threshold:** the turn commits when the silence reaches
  `realtime.semantic_floor_window_ms` (500 ms). The model is unsure there,
  and a plain `server_vad` would have committed at that point too.
- **p < floor:** keep waiting. Speech that resumes continues the turn, and
  its next pause gets a fresh score. Otherwise the turn commits at the
  eagerness's maximum wait.
- **No score.** If the model did not load, the run failed, or the answer
  was not a number, the pause commits at the row's `silence_duration_ms`.
  That is `semantic_vad` exactly as it was before Smart Turn (high 300,
  medium/auto 500, low 800 ms). The session says so at WARN once, and at
  DEBUG after that. **Too little audio is not scored** (fix package B6):
  a span under 400 ms (`MIN_SCORED_MS`, the 200 ms probe and as much
  again, an algorithm constant) would reach the model left-padded with
  zeros to its 8 s, and near-silence scores as a finished turn. Such a
  pause takes the same fallback, said at DEBUG — it is no model failure.
- **A pending score is awaited**, bounded by the maximum wait, and no
  other window races it. **Stale answers are dropped:** a request carries
  an id, and the answer for a pause that has ended (speech resumed, the
  turn committed, discarded or cleared) changes nothing. The ids are the
  detector's, so a switch to `server_vad` and back does not start them
  over (fix package B6). A request that waited for the model behind a
  running score while a newer one was issued is answered without running
  it: its pause is over.
- **An answer never commits inside a word.** When it comes after voice
  resumed for less than `resume_ms`, it ends nothing yet: the next
  unvoiced frame commits, or enough voice clears the pause and its answer
  (WP6 review M2). Ended there, the commit split the word, and its second
  half barged into the answer to the first.
- **After a barge-in** the post-interrupt window (§6.5) is a floor under
  every one of these: nothing commits before `post_interrupt_silence_ms`,
  and a longer maximum wait is not shortened by it.
- **Inside the playing window** the barge-in gate and the word check judge
  as in §6.4. A turn the word check keeps open uses the plain window and
  asks for no score. Smart Turn applies once it is a normal turn: promoted
  past the window, announced as a reply, or cut. A carried turn (§6.4) is
  a normal turn from its start.

Variant B against variant A (the audio up to the pause start): on the
first round both separate equally well (AUC DE 0.89 / 0.89, EN
0.91 / 0.90), and B leaves fewer true ends with a low score (9 of 70 below
0.5, against 14). The six unscripted clips had suggested A; that did not
hold on the larger set.

**The defaults per eagerness**, all of them settings (§12). They are
data-driven, not OpenAI's 2 / 4 / 8 s waits. The scores of unfinished
sentences are bimodal, so the threshold barely matters; the floor and the
maximum wait do.

| Eagerness | Threshold | Floor | Max wait | No score |
|---|---|---|---|---|
| high | 0.5 | 0.1 | 2 s | 300 ms |
| medium / auto | 0.5 | 0.2 | 4 s | 500 ms |
| low | 0.95 | none (= threshold) | 3 s | 800 ms |

What they do on the owner's recordings, combined rounds 1 and 2. A false
commit is an unfinished sentence whose pause lasted long enough to commit;
latency is measured at the true ends, from the end of speech. The Python
column is the spike's report. The Rust columns are
`tests/it/realtime_turn_corpus.rs`, a local-only replay that reads the
recordings from `LMGW_TURN_CORPUS`, is ignored by default, and prints
numbers only.

| Eagerness | Python: false commits, latency mean / p90 / max | Rust rule, Python scores | Rust rule, Rust scores | True ends below the floor (Python / Rust) |
|---|---|---|---|---|
| high | 65/161 (40 %), 0.38 / 0.50 / 2.0 s | 65/161, 0.41 / 0.51 / 2.0 s | 63/161 (39 %), 0.39 / 0.51 / 2.0 s | 9 / 8 of 109 |
| medium / auto | 48/161 (30 %), 0.71 / 4.0 / 4.0 s | 48/161, 0.73 / 4.0 / 4.0 s | 49/161 (30 %), 0.70 / 4.0 / 4.0 s | 14 / 13 |
| low | 20/161 (12 %), 1.79 / 3.0 / 3.0 s | 20/161, 1.81 / 3.0 / 3.0 s | 22/161 (14 %), 1.78 / 3.0 / 3.0 s | 62 / 61 |

For comparison, plain `server_vad` commits 78 % of those pauses at
500 ms, 40 % at 1000 ms and 9 % at 2000 ms, with the window itself as its
latency. Low eagerness had been computed ad hoc. The replay confirms it
within two pauses, so the decided grid point stands.

Where the columns differ, and why:
- **The Rust rule on the Python scores** commits exactly the same pauses.
  The corpus pauses are whole 32 ms frames, and the rule's windows round
  up to frames exactly where the Python `≥` tests fall (224 / 512 / 2016 /
  3008 / 4000 ms against 0.2 / 0.5 / 2 / 3 / 4 s). The latency is 20–30 ms
  more, which is that rounding.
- **The Rust scores** move 10–11 of the 265 scored pauses into another
  band at high and medium, and 5 at low. That nets out to ±2 false
  commits. Pause for pause, the Rust p differs from Python's by a mean of
  0.016 and at most 0.27, and 181 of 265 lie within 0.01. The cause is the
  runtime and nothing else:
  - on the six pauses that differ most, the Rust features are
    bit-identical to the Python extractor's;
  - ONNX Runtime 1.30 fed those Rust features returns exactly the Python
    score;
  - the pinned 1.28 (§13) executes the int8 graph differently, and a few
    pauses near a quantization boundary swing far;
  - the intra-op thread count changes nothing (1 and 4 are identical), and
    a lower graph optimization level is far worse (max 0.90).

  An `ort` update will move individual pauses, so the replay is the check
  to re-run with it.
- **Not in the replay:** the score's own time (below), which adds to every
  commit that waits for an answer.

The weakest true ends are one-word answers, short refusals and letter
closers, which score 0.01–0.04. Below the floor, they wait for the maximum
wait. Dictation that goes on after a
full stop (11 of 12 such pauses in round 2) commits under every rule. A
client that dictates wants manual turns or a long `server_vad` window.

**Surfaces.**
- **Escape hatch.** `realtime.semantic_vad_engine: "server_vad"` brings
  back the old mapping: plain `server_vad` on the row's
  `silence_duration_ms`, with the substitution logged and echoed as
  `lmgw.resolved.turn_detection: "server_vad"`. The default is
  `"smart_turn"`.
- **Echo.** A `semantic_vad` session echoes
  `lmgw.resolved.turn_detection: "semantic_vad"` and
  `lmgw.resolved.semantic_vad: {threshold, floor, floor_window_ms,
  max_wait_ms, silence_duration_ms}`, its eagerness's row, which is `null`
  otherwise. It is read-only: the rule comes from the settings, and a
  session cannot override it. **A stored row that cannot run** — a
  threshold or floor outside 0..1, the floor above the threshold,
  `semantic_floor_window_ms` past the row's `max_wait_ms` — is the
  owner's, not the client's (fix package B6): it used to fail every
  client's `session.update` to `semantic_vad` with `invalid_value`. A
  session says so at WARN when it starts and runs the built-in row and
  floor window in its place, and its echo shows them; a settings save is
  to refuse such a row (§12). OpenAI's other `semantic_vad` fields
  (`create_response`, `interrupt_response`) keep their meaning.
- **Logs.**
  - At INFO, the rule in effect whenever a session's turn detection
    becomes `semantic_vad`.
  - At DEBUG, one line per score: p, where the pause began, the score's
    time and what it decided.
  - The §11 timing line names the part of the rule that ended the turn,
    e.g. `end of turn → commit 260 ms (semantic_vad threshold, p 0.98)`,
    or `max wait`, `floor window`, `silence fallback`, and `held by
    post_interrupt_silence_ms`.

**Running it.**
- **Input:** the last 8 s, **left**-padded with zeros, then Whisper
  features with `do_normalize` (zero mean and unit variance over the padded
  window).
- **Source:** a 16 kHz ring in the session's input, fed by the stream
  resampler that already feeds Silero. It is taken before the level
  normalizer, so a score needs no 24 → 16 kHz resample of its excerpt,
  which would cost 13 ms. The ring keeps the 8 s before the open pause plus
  the pause, so a late request still finds its audio. Between pauses it
  keeps the last 8 s (512 KB), and a plain `server_vad` session keeps none.
- **Cost:** 18 ms per score in release with 4 intra-op threads (48 ms on
  one, 27 on two; the features are 1.6 ms of it), once per pause, on the
  blocking pool and never on the session's task. One ONNX Runtime session
  per realtime session, loaded on its first pause (~25 ms), sharing the
  process-wide environment with Silero. The threads do not spin after a
  run, since they would only burn a core between pauses.
- **Measured end to end** (it suite, debug build, real Silero and Smart
  Turn, a live 20 ms microphone): a finished sentence commits 305–342 ms
  after its speech ends, against 534 ms on the plain 500 ms window (the
  same question with no score). A turn that stops mid-sentence commits at
  the 2 s maximum wait (2024 ms). The suite asserts the part of the rule
  from the scores it records (the threshold) and a bound well under the
  maximum wait, not 500 ms, which a slower build's score could pass (fix
  package B6).
- **Parity:** the Rust front end matches the Python extractor to one f32
  ulp on the committed fixtures, and bit for bit on the six of the owner's
  pauses that were compared. The
  model's output is int8-quantized: logits lie on a 0.039 grid, so p moves
  in steps of about 0.01 near 0.5. A threshold finer than that means
  nothing.

**Earlier evidence**, superseded by the recordings above:
- WP0's TTS sets separated poorly (AUC 0.66–0.78).
- The first real-speech set (95 s of reading) gave English AUC 0.88 and
  German 54–64 % false commits. That was read speech, with one real
  mid-sentence pause.
- "200 ms of appended zeros pushes nearly every cut to ≥ 0.9" did not hold
  on the fixtures (0.0075 → 0.0075) or on real silence. Variant B scores
  the real 200 ms, not zeros.

### 6.4 Barge-in

With `interrupt_response: true`, user speech **while the client is playing**
cancels the response.
- "Playing" is the window from the first audio delta until the end of
  playback. The writer keeps it per response, from when each chunk really
  left: each chunk plays from when it left, or from the end of the one
  before it if that is later (the client model of §7.3 and §8.2). At the
  steady state of pacing that end is the paced send's end plus
  `output_lead_ms`; it is earlier for an answer shorter than the lead, or
  after an underrun, and later when the last chunk is longer than the lead
  (a lead of 0). The earlier draft's "send's end plus the lead" overstated
  a short answer by up to the whole lead — with a long lead, by minutes.
- It is not the server-side generation window: generation finishes long
  before playback does. While a response still produces audio the window
  is open-ended, so a pause while its next clause is synthesized is still
  the middle of the answer. So it is while any of its audio still **waits
  in the writer** (B3 review 8): the end counts only what has left, and
  with a lead of 0, timer jitter or a writer stalled behind its socket it
  can pass with seconds of the answer to come. The synthesis bound reads
  the same two facts the other way round: an end that passed while audio
  waits is a stall (§8.2).
- **The end used for membership is pushed back by a margin** (B3 review 2):
  the ping round trip, once one was measured, plus `echo_tail_ms`
  (default 250, a setting and `session.lmgw` knob). The round trip is the
  **least of the last five** (B3 review E3): a longer one is queueing — a
  WiFi spike, a slow first pong — not the network's transit, and would
  push the window's end back for a whole interval. A "round trip" as long
  as the ping interval answered no ping of its own (an unsolicited pong
  after one that got none) and is dropped. The ping is stamped before it is
  sent, so a fast pong read while the send is still flushing finds it. Each
  ping carries a number of its own as its payload, and only the pong that
  echoes the outstanding one measures (B4 review): a ping sent while the
  one before was unanswered used to overwrite its stamp, and that one's
  late pong measured almost nothing. A DEBUG line names the margin whenever
  it changes — the round trip in tenths of a millisecond, or "not measured
  yet". The window is in
  release time, and the client plays the audio its transit and its output
  buffer later; the capture estimate below is arrival-based, so the
  client's input transit and buffer make it later still. The delays add —
  an earlier draft claimed they cancel, which was wrong — so without the
  margin the window ended ~50–300 ms (Bluetooth: more) too early: under
  half duplex the echo of the answer's last words reached the detector and
  became a turn, and a late "mhm" became one too. The round trip covers
  the network; the tail covers what it cannot see — the client's audio
  buffers (~20–100 ms each way, wired, through a browser or PipeWire) and
  the room's reverberation (~100 ms above Silero's threshold). The default
  fits that, not Bluetooth output, which wants more. The session pings its
  client on its first frame for an early measurement, and with every
  liveness ping after that; with pings off (`ping_interval_s` 0) the tail
  is the whole margin. So it is for a client that answers pings without
  echoing their payload: once three pings went out with no round trip
  measured (`SILENT_PINGS`, an algorithm constant: the first-frame ping and
  two whole intervals), the session says so once, at INFO (fix package
  B6). Both the gate and half duplex judge by this end; the cut does not
  (below).
- The window outlives `response.done` until the next spoken response
  paces: input captured inside it is judged by it however late it is
  processed.
- A cancel ends the window at once: the client was told to stop.

Clients stop playback **on the `speech_started` event**, so the event itself
must not fire on echo or a cough. Inside the playing window, the detector
uses the earlier prototype's evidence accumulator:
- voiced time accumulates;
- a gap of 0.8 s resets it — the prototype's algorithm constant, not a setting,
  so it is not echoed in `session.lmgw` (it is named here and in the code
  instead);
- nothing counts in the first `barge_in_guard_ms` (default 500) of playback;
- `speech_started` fires when the evidence reaches `barge_in_min_ms`
  (default 200 since B4 — it was 300; see the word check below), with
  `audio_start_ms` back-dated to the onset.

Outside the window, `speech_started` fires at normal onset.

**Placing input on the playback clock.** The window is wall-clock time,
and the input is samples; the client never says when it recorded them. Each
append is stamped with when it came off the socket — by the socket's own
reader task, not when the session core got round to it, so a busy core
neither bunches the stamps nor makes their gaps look like a muted client
(B3 review 10) — and a sample's capture is estimated from it, assuming the
client sends audio as it records it: the last sample of an append was
captured about when the append arrived, the ones before it that much
earlier. The estimate is late by the client's input transit and buffer,
which the window's margin above accounts for.
- A client faster than real time (a burst after other appends) is not
  spread over a past it did not send in: no sample is placed before the
  previous append arrived, so such a burst is judged at its arrival —
  audio uploaded during playback must pass the gate. An isolated append —
  the session's first, or one after a pause longer than itself — has no
  such floor and is placed back over its own duration, as if recorded
  right before it was sent (push-to-talk style).
- A stall then a burst is judged late, and may fall after the window: a
  normal turn, the safe side.
- A muted client sends no frames, so no silence resets the evidence; a
  wall-clock gap of more than 0.8 s between two frames does.
- `audio_start_ms`, `audio_end_ms` and every back-dating stay pure sample
  math. The estimate decides window membership and the guard only.
- **Where the window began on the input timeline** — what the guard counts
  from — is estimated from every frame inside it, not fixed by the first
  (B3 review 9). A capture estimate is never early (a client cannot send
  audio before recording it, and a burst is floored at the previous
  arrival), only late, so the latest start any frame gives is kept: one
  stalled frame no longer shortens the guard for the whole window. The
  gate keys its window on the response, so a refined start is the same
  window. The guard itself counts from the release, so the client hears
  about a round trip less of it than `barge_in_guard_ms` says.

**Who judges a frame.** Inside the window, with no turn open, the gate
does: the detector only keeps its timeline moving, so the gate's frames
never add up to a normal onset, and it holds the audio of pending evidence.
- **A trigger** starts the turn back-dated to where the evidence began,
  pre-roll included — unconfirmed, while the word check below asks what
  was said.
- **Speech below `barge_in_min_ms`** is a backchannel ("mhm"): never
  announced, never committed; its held audio goes when the evidence resets,
  and a DEBUG line says so.
- **A turn already open stays the detector's**, gate or no gate (the user
  was talking before playback began, or `interrupt_response` is off). The
  gate forgets its window meanwhile, so once that turn ends inside the same
  window it listens again, with no guard.
- **Evidence the window's end cuts short** carries its onset: the next
  normal onset within 0.8 s is back-dated to it, so the utterance's start
  inside the window survives. The voice before the end counts towards that
  onset (B3 review E1): a short reply that straddles the end — "Nein",
  with less than the onset's 96 ms after it — used to make no onset of its
  own and was dropped; it is a turn now, starting at the first voiced
  frame after the end. The answer has played out by then, so a carried
  turn is a normal one, on the plain silence window: a reply, not an
  interruption.

**The word check** (owner's decision 2026-10-01, `barge_in_check:
"words"`, the default). Duration cannot tell an interruption from a
backchannel: on the owner's own recordings "Stopp" has 320 ms of voiced
evidence and "Stop" 384, while "Mhm" has 544 and "Okay" 608. At
`barge_in_min_ms` 300 all 11 interruptions pass the gate — and so do 8 of
9 backchannels, a cough (a 352 ms run) and a laugh (416). Words can tell
them apart, so the gate's trigger no longer cuts by itself.

**The gate opens at 200 ms with the word check** (B3 review E7). The
owner's second round of recordings had a quick "Stopp" of 256 ms and a
"Stop" of 288: at 300 the gate never opened for exactly the word that must
work. Replayed with qwen3-asr as the checker, 200 ms caught 36 of 36
interruptions for 7 of 46 backchannels and 2 of 21 noises cut, against 32
of 36, 5 of 46 and 2 of 21 at 300. The default assumes `barge_in_check:
"words"`: with `"duration"` the gate alone decides, backchannels then cut
far more often at 200, and an owner who chooses it may want 300 or more.

How the check runs:
- **The turn opens unconfirmed.** When the evidence reaches
  `barge_in_min_ms` the turn starts in the detector, back-dated as above,
  but no `speech_started` goes out and the answer keeps playing. Its audio
  so far, pre-roll included, goes at once to `barge_in_check_alias` — a
  setting and `session.lmgw` knob; empty, the default, is the session's
  ASR alias — as a direct call, not queued behind the turn transcriber,
  which runs one call at a time in commit order, and with the session's
  `audio.input.transcription.language` when it has one (B3 review E4; the
  turns' own calls send it too since B5). audio.cpp takes the field: its
  server's usage text lists the transcription fields as "file, model,
  language, prompt, stream". **Only a two-letter ISO 639-1 code goes up**
  (fix package B6): the lowercased primary subtag ("de" for "de-DE",
  "pt" for "pt_BR"), the parse the scripts warning uses; "german" or
  "deu" name no such code and are not sent, with a DEBUG note — never
  refused, since a model that refuses a value would fail every turn of the
  session. A client's own check alias is checked against its key's scope
  at the `session.update`, like a new model, and then **must be an ASR
  alias** (fix package B6): a chat alias failed every check, silently, so
  the update is refused, `invalid_value` on
  `session.lmgw.barge_in_check_alias`. An owner's
  `realtime.barge_in_check_alias` that is no ASR alias is a WARN when a
  session starts, and that session checks with its own ASR alias (its echo
  says `""`).
- **Which model checks** (live run 2, W1). The check hears 200 ms of voice
  or a little more, and not every ASR model hears that much. With the
  owner's clips and a long German answer, nemotron-asr (the session's ASR
  alias) returned nothing at the first check in 7 of 7 "I am tired"
  barge-ins — the cut then waited for the second check, 564–644 ms after
  the onset against 275–400 ms when the first heard words — and returned
  nothing twice for the owner's quick 256 ms "Stopp", which then never cut.
  qwen3-asr heard all of them and cut ~250 ms earlier (72–95 ms per call
  against nemotron's 30–71). **Recommendation:** a qwen3-class model as
  `barge_in_check_alias` when the turns are transcribed with nemotron.
- **Words cut**, exactly as without the check: the withheld
  `speech_started` first, then the cut, in the order below; the turn ends
  on `post_interrupt_silence_ms` from there. The INFO line names the words.
- **Nothing, or only backchannel words, while the answer plays**
  (`realtime.backchannel_words`, a visible list; default German and
  English: mhm, hm, hmm, mm, mm-hmm, ja, jo, jap, okay, ok, genau, richtig,
  ach so, aha, stimmt, gut, yeah, yes, yep, right, uh-huh, sure, alright,
  i see, haha — and, from the offline replay of the owner's recordings
  through both local ASR models (B3 review E4), uh huh, mhmm, mmh, hm hm,
  hm-hm, mm hm, mmhm, hmhm, so, jaja, achso, okey, o.k., klar, alles klar,
  super) **do not cut**: no `speech_started`, a DEBUG line per check. A
  transcript is a backchannel when it is a sequence of list entries —
  lowercased, punctuation dropped, a hyphen or an apostrophe joining; an
  entry may be several words ("ach so") and entries may repeat ("ja ja").
  "O.K." is the two one-letter words of the entry "o.k.". Round 2 of the
  replay added okee, uh, huh, uh hmm, a so, ah so and ah (E6). **A hum is
  a backchannel by rule**, whatever its length: a word of only the letters
  h and m ("hmm", "mhmmm", "hmhm") counts as an entry — qwen3-asr wrote a
  yawn as "Hmm." and then a degenerate "Hmmmm…" repetition, which no list
  can hold. "Hm, stopp" still cuts.
- **Only words in the session's scripts count**
  (`realtime.barge_in_check_scripts`, default `["Latin"]`, a setting and
  `session.lmgw` knob; empty = every script). ASR models write noise in
  scripts nobody spoke: in the replay qwen3-asr heard a hum, a throat-clear
  and a sigh as "嗯。" and a cough as "咳。" (which would have cut), and
  nemotron heard "Mhm" as "Угу." A word whose letters are all outside those
  scripts is no word, so a transcript of only such is empty. A token with
  no letter at all — a number: "2.", "15:30", the "5" of "5 mm" — is a word
  (B4 review M3): a "Zwei!" during playback written as a digit used to be
  empty and lost, and "5 mm" was a hum. An owner who speaks a language in
  another script must add it; a session whose transcription language is
  written in a script the list leaves out (Russian with `["Latin"]`) warns
  once, at WARN — the check would count none of its words, so nothing the
  user says during an answer would cut it. The table knows the scripts an
  ASR model writes (Latin, Greek, Cyrillic, Armenian, Hebrew, Arabic,
  Devanagari, Bengali, Thai, Georgian, Hangul, Hiragana, Katakana, Han, by
  name or ISO 15924 code); a client naming another is refused, and one in
  the owner's setting is a WARN at session start (it matches nothing).
- **Backchannel words when nothing plays any more are a reply** (B3 review
  E1): no response is active, or a turn starting now would not cut it (it
  played out). "Ja" or "Okay" right after "Soll ich das so machen?" — said
  within the window's margin, so the gate judged it — is the user's
  answer: the turn is announced as a **normal** turn (the plain silence
  window, no post-interrupt), ended at once if its silence window passed
  while the words were checked, committed and answered. An INFO line says
  so.
- **It keeps listening — while the answer plays.** The unconfirmed turn
  does not end by itself: the detector keeps counting its silence, and the
  words decide. It is checked again once its voiced evidence has grown by
  another `barge_in_min_ms`, and once more when its silence window passes
  with speech in it the last check did not hear — so "Mhm, aber warte mal"
  still cuts. A re-check uploads only the audio since the last check's,
  with at least the pre-roll's length of overlap (B3 review E2), so "aber
  warte mal" is still heard whole and the upload does not grow with the
  turn. It starts at a pause: the last unvoiced frame at or before that
  overlap's start (B4 review M2) — looked for within one more pre-roll
  before it only, and without one there the re-check starts at the
  overlap's start (fix package B6). Cut mid-word, "alles klar" came back
  as "Les klar." and "ja genau" as "Nau.", neither on the list, and cut
  the answer; falling back to the turn's start instead (B5) let hum, music
  or a foreign radio grow every upload until the check timed out and the
  duration rule cut. Each re-check is the new audio and at most two
  pre-rolls. When the
  window has passed and nothing unchecked is left, the turn is discarded:
  never announced, never committed, its audio gone. At most one check is in
  flight per turn; one that falls due meanwhile is made when the verdict is
  back, and a verdict for a turn that is gone (a commit, a clear, a
  promotion, a discard) changes nothing and says so at DEBUG.
- **The playing window bounds it** (B3 review E1, E2, B4 review M1). Past
  the window's end the answer has played out and there is nothing left to
  cut:
  - a voiced frame, or speech no check has heard yet, **promotes** the turn
    to a normal turn — its `speech_started` goes out then, on the plain
    silence window — and the normal endpointer and the turn's own
    transcript take over; one whose silence window had already passed ends
    in the same frame;
  - a check still in flight decides: backchannel words are then the user's
    reply, as above;
  - a backchannel whose words were all checked, silent since, is
    **discarded**. A "Mhm" the check had judged while the answer played
    used to be promoted when its silence ran past the answer's end, and was
    committed and answered.

  Humming or music that Silero calls voice therefore cannot hold a turn
  unconfirmed, re-checked again and again, for longer than the answer
  plays.
- **An answer can end while a check is in flight** (B3 review L5). The
  turn is not the user's yet, so a `response.create` then is not held
  (Q3). When the verdict, or the window's end, makes the speech a turn
  whose automatic response starts first, a create sent after it — the
  `@openai/agents` tool follow-up — is refused as
  `conversation_already_has_active_response`, as any create during a
  generating response is. Nothing is lost: the response that runs renders
  the whole conversation, the tool's output included, and a cut re-carries
  a client's create nobody heard yet (§4.3). The client sees one `error`
  event. Holding creates for a turn nobody announced would put every
  follow-up behind a "mhm"'s check.
- **A mute ends it.** A wall-clock gap of more than 0.8 s between two
  frames — a muted client sends nothing — discards an unconfirmed turn:
  what its checks heard did not interrupt, and what follows the mute is a
  new utterance, not glued onto it (B3 review E5).
- **No answer is the duration rule.** An ASR error, no answer within
  `barge_in_check_timeout_ms` (default 500, a setting and `session.lmgw`
  knob, 0 = no bound — about five times what the slower measured local ASR
  takes for a second of speech, §3.1, and at most that much more before an
  interruption cuts), or a session with no ASR alias cuts, and the INFO
  line says why. A timed-out call still runs to its end and writes its row.
- **No hidden work.** Every check is an ASR call with its own usage row,
  class `audio`, label `realtime` (§11).
- **An empty turn the check heard words in is transcribed again** (live
  runs 3 and 3b, N3; R7). nemotron, the session's ASR, answered "" for
  turns qwen3's check had cut on, and no response followed. When the turn
  a check let through — a cut on words, or backchannel words that became a
  reply — gets an empty transcript, and the check's alias is not the one
  that transcribed the turn, the turn's whole audio goes once more to the
  check's alias and that transcript is the turn's: its transcription
  event, its item, its automatic response. The check's own words are not
  reused: they are only the start of the turn. The second call runs in
  the turn's own job, so the turns after it still wait in commit order;
  it is one more ASR call with its own usage row, and an INFO line names
  both aliases and what was heard. A second call that fails leaves the
  turn empty, and the line says so. The audio takes the check's own path
  (`transcribe::caught`): the key's scope and budget per call, the gate,
  the alias's own routing. The realtime ASR path has no local-only rule of
  its own (`transcribe_turn` sends with `local_only: false`, the check
  too), so the turn's audio goes nowhere the check's audio of the same
  turn could not. A turn no check heard words in (a promotion, the
  duration rule, a check that failed) and a check on the session's own
  ASR alias get no second call.
- **Where it applies.** Only to the gate's turns: half duplex hears the
  window as silence, manual turns are the client's, and a turn outside the
  window keeps the normal onset (a noise turn there already dies on the
  empty-transcript rule, §4.3). `barge_in_check: "duration"` is the
  evidence alone; with `barge_in_min_ms` and `barge_in_guard_ms` 0 too it is
  OpenAI's plain behaviour.
- The cut is judged at the instant the gate triggered, as below — not when
  the words came back.
- Until its words cut, the turn is not the user's turn to the rest of the
  session: nothing was announced, so a `response.create` meanwhile is not
  held (Q3), and a client's `input_audio_buffer.commit` commits its audio as
  an item of its own.

**What a turn's start does** is judged at the moment its deciding frame was
*captured*, and in this order:
1. `speech_started` goes out first. `@openai/agents` interrupts only until
   the cancelled item's `output_audio.done`, which the cancel sends right
   behind it (§2.3).
2. The owed response is deferred (§4.3), so nothing the cancel ends can
   start another response while the user talks.
3. The response is cut — if `interrupt_response` is on, and only if a
   cancel still changes something:
   - a response whose items are all closed after the end of generation (a
     text response whose tool call the client may have run) finishes as
     generated, by `response.cancel`'s own rule, so a completed call is
     never marked cancelled;
   - a spoken response whose audio had played out when the deciding frame
     was captured — by the modelled playback end, not the margin, and only
     once none of it waits in the writer — is finishing, not interrupted;
   - anything else is cancelled with `status_details.reason:
     "turn_detected"`: its queued audio is purged synchronously, so no audio
     of it follows `speech_started`, and its item keeps what was sent.
     The client's `truncate` then cuts that to what it played (§7.3).
4. Every turn that starts during a response, or that the gate started,
   writes one INFO line: where it began, how far into the playback, and
   what it did.

A frame captured inside the window but judged after `response.done`
(the **residual race**, at most one append) finds no response to cut: it
is a normal turn.

`interrupt_response` and `create_response` combine as with OpenAI:

| | `speech_started` | server cancel | the turn | afterwards |
|---|---|---|---|---|
| interrupt **on** | through the gate in the window | yes, `turn_detected` | committed | per `create_response`; a held `response.create` starts after the turn (§4.3) |
| interrupt **off** | the same — clients stop playback on it, and `@openai/agents` sends `response.cancel` itself (`client_cancelled`) | no | committed | answered after the response it ran beside |
| create **off** | unchanged | unchanged | committed | nothing automatic; a held `response.create` starts after the turn |

**Echo is not solved by this gate.** The earlier prototype's barge-in signal
was *ASR tokens*, not VAD, and it saw residual echo leaking past the
operating system's echo cancellation. Silero scores echoed speech as speech,
so voiced time alone cannot tell echo from a user. v1 relies on the client's
echo cancellation. Browsers do it (`getUserMedia` with `echoCancellation`);
a native client does it itself, for example with PipeWire's echo-cancel
module, or uses a headset. The evidence gate only filters short noises.

The server does hold the exact output PCM and, because it paces, its
playback schedule. So a cheap **echo-reference check** was measured offline
(2026-10-01).
- **Method.** Real answer audio was played through 22 real room impulse
  responses (12 rooms, tune and test split). The echo return loss was 0–30
  dB, the delay 20–250 ms, and users spoke at −6 to +12 dB against the echo,
  with noise. The real Silero model and the evidence gate ran on the result.
- **Without client echo cancellation, barge-in is unusable.** The gate fired
  about **111 false barge-ins per minute** of playback. Silero fires on echo
  down to ~10 dB above the noise floor.
- **No cheap check rescues it.** Energy ratio against a tracked echo return
  loss, envelope correlation, waveform cross-correlation, and a logistic
  combination of them were all tried:
  - at ≤ 0.1 false barge-ins per minute, they lose nearly every real
    barge-in;
  - at low-miss operating points, 5–10 false barge-ins per minute remain.
- **Delay estimation is not the bottleneck.** GCC-PHAT lands within 10 ms in
  ≥ 85 % of trials.
- **What would work.** Only a server-side adaptive filter, i.e. a real echo
  canceller, could do it, which is out of scope.

**Decision: no server-side echo check.** Barge-in **requires** echo
cancellation on the client: a browser's `getUserMedia` with
`echoCancellation`, PipeWire's echo-cancel module, or a headset. For
clients without it, `session.lmgw.half_duplex: true` ignores input during
the playing window: nothing is committed and there is no barge-in.
- Inside the window every frame counts as silence: no turn starts, and a
  turn already open when the window began ends at the last frame before it
  (B3 review 5) — what was said before the answer still commits, and the
  answer's echo neither holds the turn open for another silence window nor
  is transcribed with it.
- **Not the turn that cancels that answer** (live run 2, H1). The user's
  next sentence began ~100 ms before an answer's first audio: one append
  held the turn's onset, before the window, and frames inside it. The
  frames of an append are judged against the window the core saw when it
  arrived, and the core acts on a turn's start after the whole append — so
  the turn ended at the window before the answer was even cancelled, "I've"
  was committed and answered, and the next answer was cut the same way. The
  core now tells the input which response a turn starting now would cancel
  (one a cancel still changes, with `interrupt_response` on), and that
  response's window is no window for that turn — neither for the rest of
  the append nor for later frames inside the cancelled window's margin.
  The client is told to stop at `speech_started`; the echo of what it had
  played by then goes into the turn. A turn that does not cut the answer
  (`interrupt_response` off) still ends at the window.
- It is a setting too (`realtime.half_duplex`, default off), echoed in
  `session.lmgw`. Manual turns are unaffected: the client commits.

### 6.5 After an interruption

A user who interrupts usually pauses to rephrase, and a tight silence window
turned the interjection into the whole prompt. The turn that interrupts
therefore ends on `post_interrupt_silence_ms` (default 1500) instead of
`silence_duration_ms`, under `server_vad`. That is every turn the barge-in
gate starts and its words cut, and every turn that starts while a response
the client has heard some of (§4.3) is in progress — with
`interrupt_response` off too, since the user still talks over the answer.
A turn that starts after the answer played out — carried over the
window's end, a gate turn promoted past it, or one whose backchannel words
came back when nothing played — is a reply and keeps the plain window
(B3 review E1). A response nobody heard yet — awaiting its
transcripts, or generating before its first audio — interrupts nothing the
user listened to: a turn then is most often the user going on after a
pause, and keeps the plain window, rather than waiting a second longer
for an answer whose timing hung on the ASR's speed (B3 review 4).
It is armed in the frame that starts the turn, so even a turn that ends
within the same append gets it, and it ends with that turn, a clear, or a
commit. Under `semantic_vad` it is a floor under Smart Turn's rule (§6.3).
Nothing commits before it, whatever the score, and a longer maximum wait
stays longer.

### 6.6 Manual mode

With `turn_detection: null`, nothing is automatic:
- `input_audio_buffer.commit` ends the turn. Committing an empty buffer is
  `input_audio_buffer_commit_empty`.
- `response.create` starts the response.
- **Switching to it mid-turn** closes the turn the detector had open:
  `speech_stopped` names its item, at the timeline's end, and its audio
  becomes the uncommitted buffer, for the client to commit (as a new item)
  or clear. What the turn deferred decides at once — a held
  `response.create` starts — rather than waiting for a commit that manual
  mode never makes by itself (B3 review 3).

### 6.7 Speculative responses (later)

A later version could start the chat call at the first pause and hold only
the audio until the turn is confirmed. That hides the LLM latency inside the
silence wait. Hugging Face calls it "speculative reopen"; an earlier
prototype called it "decode-ahead / speak-on-confirm". It is §19 until the
plain cascade is measured.

## 7. Conversation and history

### 7.1 Items

The session keeps an ordered item list, following OpenAI's model:
- user `message` items with `input_audio` (the committed segment plus its
  transcript) or `input_text`;
- assistant `message` items with `output_audio` (plus transcript) or
  `output_text`;
- `system` messages;
- `function_call` and `function_call_output` items.

`previous_item_id` controls insertion. Ids (`item_…`, `resp_…`, `event_…`,
`call_…`) are unique for the session. The conversation lives in memory for
the session and is **not persisted**, as with OpenAI.

### 7.2 Rendering to the chat model

Before each response, the items are rendered into an `ir::ChatRequest`:
- `instructions` becomes the system message — for a spoken response with
  the **tag hint** after it (WP10, WP9b), a paragraph of its own;
- user audio becomes its transcript;
- assistant audio becomes **what the model wrote, up to what was heard**
  (§7.3): its own text — fenced code and markdown the voice left out
  included — not the spoken transcript (B2 review 2);
- function calls and outputs become assistant tool calls and tool results.

Strict chat templates (Gemma-class through llama-server, and OpenAI-style
upstreams) refuse some sequences the conversation can legitimately contain.
The OpenAI egress does not repair them, so the renderer does:
- **consecutive user items are merged** into one user message. These come
  from `create_response: false` sessions and speech during the pre-response
  phase.
- **assistant items with no heard content are dropped**, for example a
  response cancelled before its first audio.
- **a tool result always directly follows its tool call.** A user item that
  arrived while a client tool was pending is placed after the result. If the
  result has not arrived when a response must be rendered, a synthetic
  result `"(no result yet)"` fills the slot.
- **a `function_call_output` rendered for a provider that needs the tool
  name** (Gemini) looks the name up by `call_id`.

**The tag hint** (WP10) tells the model which sounds its voice can make, so
it can write them. Audio responses only, when the session's primary TTS
renders tags (`fixed` with at least one listed, or `free`) and `tag_hint` is
on — the default, since it only acts on a voice picked because it makes
sounds (or, since WP9b, one that takes delivery cues: the cue hint below). A fixed vocabulary is named by its non-verbal sounds (the
`STAGE_DIRECTIONS` targets but `pause`: laughter, sigh, breath,
quick_breath, cough, lipsmack), else whole — OmniVoice's `question-en` is
nothing a model could place, though it still renders if written:

> Your words are spoken by a voice that can also make these sounds:
> [laughter] [sigh]. To make one, write it exactly so, before the words it
> goes with, rarely and only where it fits. Never put other words in square
> brackets.

A free vocabulary gets the same with "performs short stage directions in
square brackets, such as [laughs] or [whispers]".

**The cue hint** (WP9b C6) is the same switch's other text: a TTS takes
tags or cues, never both (§5.5), so one switch — "tell the model what
square brackets do" — covers both, and there is no new setting, patch or
field. For a primary that takes cues the paragraph is:

> Your words are spoken by a voice that can change how it speaks. To ask
> for a delivery, begin a sentence with a short cue of one or two
> lowercase English words in square brackets, such as [laughing],
> [whispering] or [excited]; it lasts until that sentence ends. Use cues
> rarely and only where they fit. Never put other words in square brackets.

It asks for one or two words, not three, because a tag is at most 31 bytes
(§8.1): three words such as "whispering very conspiratorially" are more,
and the hint should not invite a bracket that is read out.

The text is `lmgw_api_types::realtime::speech_hint_text(instructions,
inline_tags, tags)`: `tag_hint_text` when tags render, else
`cue_hint_text` when the TTS takes cues, else none. The gateway's hint and
the dashboard's preview both call it. Cues apply with the hint off too,
just as tags render with it off: the switch only says what the model is
told. It never goes into `session.instructions` — clients send that back,
so it would double — and is echoed verbatim in `resolved.speech.tag_hint`.

`response.create` may override `instructions`, `tools`, `tool_choice` and
`output_modalities` for one response, and its `lmgw.speech_instructions`
the speech style (§5.5). `conversation: "none"` and `input`
(out-of-band responses) are §19: they imply concurrent responses, which the
one-response rule of §4.3 does not allow.

### 7.3 Heard, not generated

The history must match what the user heard, which is why OpenAI's
`conversation.item.truncate` exists. The server keeps, per
assistant audio item, a table of `(clause text, first sample, last sample)`:
- samples are counted **cumulatively per item, at the output rate, after the
  resampler's delay**;
- `truncate {audio_end_ms}` converts the time to a sample index;
- clauses before it are kept whole, and the clause it falls into is cut by
  character, interpolated linearly, and then back to the end of the last
  word heard whole (*changed in the chat-voice WP9 fixes, review B*: a live
  barge-in stored "… bestimmte Lichtw"). Words are UAX #29 word segments
  (`unicode-segmentation`), so Han and Hiragana still cut per character and
  "21,5" stays one word; a hyphenated compound ("Rayleigh-Streuung") counts
  as one word; the punctuation written right after the last whole word stays
  with it. Back, never forward: the character share is only good to a word
  (a voice does not speak at a uniform rate), and a word the model repeats
  costs nothing where one it wrongly believes was said does. A cut inside a
  clause's first word leaves nothing of it, and what was written before it
  goes with it, as for a clause none of which was heard. Only the text moves:
  the clause's audio still ends at the cut, and `.truncated` echoes
  `audio_end_ms` unchanged. One rule for every user of the table — the item's
  transcript, what the model is sent, a cancel's cut, and a bound session's
  stored reply (`realtime/heard/words.rs`);
- audio after it is dropped, and the server answers `.truncated`;
- `audio_end_ms` beyond the item's length is an error, as with OpenAI.

A batch's audio (§8.2) is shared out among its clauses by the characters
each said, so a clause's row spans its share of the batch's samples, and a
cut inside it is interpolated over that share, not over the whole batch.

The SDK's `audio_end_ms` is wall-clock time since the first delta arrived,
so it slightly overstates what was heard by the client's buffering. Pacing
(§8.2) keeps that error to roughly `output_lead_ms`.

A cancelled response that is never truncated keeps what was **sent**. With
pacing, that is what was played, plus at most the lead.

**The transcript is what was said; the history is what was written** (B2
review 2). The spoken transcript has no code (§8.1), but the model must
see what it wrote — "run the second command again" needs the commands. So
each clause in the table also keeps what the model wrote for it: the text
the voice left out since the clause before (fenced code, list markers, rule
lines, a clause with nothing to say) and the clause as written. Text after
the last clause (a code block that ends the answer) is kept as the item's
tail. The model is sent, for an assistant audio item:
- every clause heard whole as written, each with the text left out before
  it;
- for the clause the cut falls in, the text left out before it, then the
  part of the clause that was heard, as it was said, to the last word heard
  whole: its words after the cut were not heard, and the model must not
  believe they were;
- the tail, unless audio was cut away.

**Inline tags** (WP10) follow the same rule: the transcript — the delta,
the item, the heard table — is the clause without its tags, and the history
keeps them as written, so the model stays in its own style. A tag the
stream ends with is not voiced and is kept as unspoken text. **Delivery
cues** (WP9b) are tags in that sense too: the transcript never shows one,
the history keeps it, and a barge-in inside a cued clause gives the
history the heard part of what was said, so that clause's cue drops out
as a tag does. Inside a
clause the transcript's characters are spread over audio that includes the
sound (a laugh adds about a second), so a cut there is coarser; the clause
spans stay exact.

What was written before a clause nobody heard a word of goes with that
clause: a code block between the last heard clause and the cut-off one is
not in the history. An answer with nothing said at all (only code) opens
no item, so nothing of it is in the history, and an INFO line says so.
The client-visible transcript, `truncate` and `retrieve` are unchanged.

### 7.4 Tools

**Client-side function tools** are the whole v1 story:
- Session or response `tools` of type `function` go to the chat model as
  tools.
- A tool call in the stream ends the response. The server emits the
  `function_call` item: `output_item.added` with **`status:
  "in_progress"`** and empty arguments, the argument deltas and `.done`,
  then `output_item.done` with **`status: "completed"`**. Exactly one event
  per call carries `completed`, because the SDK fires the tool on every
  completed event (§2.3).
- The client runs the tool and sends `function_call_output` and
  `response.create`.
- `call_id`s are minted **session-unique** when the upstream gives none or
  repeats one; some local models number calls per request. The mapping to
  the upstream id is kept for rendering.

There is **no spoken filler** while a tool runs; the client knows its own
tool latency. **Server-side tools** (`type: "mcp"` session tools, or the
gateway's own MCP toolsets as the Chat runs them) are §19, and would bring
`agent::run` back (§3.2).

### 7.5 Context length

v1 does **not** auto-truncate. Every model call goes through
`stream_once_on`'s per-send fit, which measures the request against the
route's real context length (the top rung for a ladder, counted by the
running server). An overflow comes back as `context_length_exceeded`, and
the session reports it as an `error` on that response while staying open.
The client can then delete items.

`truncation: "auto"` (OpenAI's default) is accepted but not honoured in v1,
and the session echoes `lmgw.resolved.truncation: "disabled"`. Dropping the
oldest turns, with `conversation.item.deleted` events so the loss is
visible, is §19.

### 7.6 Reasoning

Voice answers want no hidden thinking time; an earlier prototype measured a
~400 ms gap with reasoning on. Unless the session's `reasoning` field says otherwise,
each response asks for **reasoning off** through lmgw's existing reasoning
control, which knows the per-engine spelling (including the llama-server
`enable_thinking` kwarg). Reasoning deltas are never spoken.

## 8. Output

### 8.1 Clauses

The chat stream goes through a clause splitter taken over from an earlier
prototype (pure `std`, 6 tests) and changed since:
- **A clause ends where a sentence ends, or at a line end — nowhere else**
  (2026-10-05, TTS batches). A `.`, `!` or `?` followed by whitespace cuts,
  under the abbreviation, ordinal, decimal and inline-list rules below,
  unchanged; never a comma, never a word count. A TTS speaks each request
  as an utterance of its own, with the engine's silence at both ends, so a
  cut inside a sentence ("In Berlin," | "it's currently around 14 degrees
  Celsius.", "…a mix of" | "sun and clouds.") was heard as two, and the
  owner's A/B listening preferred the sentence whole. The prototype's
  first-comma chunk (+9 ms median against a 6-word cap) and the 24-word cap
  for later chunks are gone; an engine that needs shorter input chunks it
  itself (audio.cpp does, 300 characters for Supertonic).
- **Other scripts' sentence ends** (2026-10-05, `clauses/stops.rs`). With
  no word cap, a script whose sentences end with another mark got no cut
  until a line end, so its first audio waited for a whole paragraph. `।` `॥`
  (Devanagari and Bengali danda), `։` (Armenian), `؟` (Arabic question mark)
  and `።` (Ethiopic) end a sentence like `!` and `?`: whitespace after them,
  and none of the `.`-only abbreviation, ordinal and decimal logic. The CJK
  `。` `！` `？` and the half-width `｡` need no whitespace after them, but a
  closing quote or bracket right after one (`」` `』` `）` `"` `”` `’` `】`
  `〉` `》`, and a further stop of the run, "えっ？！") belongs to its
  sentence: the cut is after the last of them, so at the buffer end the
  splitter waits for the next character, as for a `.`. "彼は「行く。」と言った。"
  is cut "彼は「行く。」" | "と言った。". As for a `.`, a stop cuts only once
  the clause has a word (a Han run is one), and a flushed clause ending in
  one of these marks is closed already.
- **Decimals:** kept whole (`3.14`, and the German `3,14`).
- **Sentence end at the buffer end:** deferred until more text arrives.
- **Line ends** are boundaries too, so an answer that opens with a heading
  or a list speaks at once (§23 L7).
- **Closed clauses.** A clause cut at a line end, and the stream's last
  one (also the text before a tool call), gets a full stop when it has no
  closing punctuation of its own — after any closing quote, bracket or
  emphasis marker. A heading or a list item is then spoken as finished
  rather than trailing off, and the transcript says it as spoken (package
  B review 4).
- **List markers** at a line start are not text, and are left out of the
  clause: the next item's "2." is no sentence end of the item before. A
  numbered `N.` is a marker only when it can start a list (`1.`) or
  continues the numbering at its own depth (`3.` after `2.`; the line start
  and indented lines count apart, so the outer "2." after "  2. b" is one,
  B2 review B), any other number only as an indented item right inside a
  list (a nested list not starting at one) — and never when a month name
  follows: "1. Mai ist Feiertag" is a date (B2 review C). Otherwise —
  "3. Oktober", "2024.", and after a list, blank line or not, "42. Minute:
  Tor" or "100. Geburtstag" (B3 review L3) — the number is text and kept
  (package B review 3); its dot before a capital is a sentence end as
  anywhere. The cost: a top-level gap ("1.", "2.", "4.") keeps "4." as
  text. `N)` is always a marker.
- **A bare number after a sentence end opens the next clause** (R3 D4,
  R4 M2). A bare `N.` right after a sentence end (`.`, `!`, `?`), before a
  capital and no month, was a clause of its own by the ordinal rule — "…
  weiter. 1. Wasser holen. 2. Brot kaufen." was spoken "Eins." with a
  clause gap before each item. When it starts or continues the numbering
  (the same rule as at a line start; inline and line-start items share
  it), its dot is no sentence end: the number is the first text of the
  clause it opens, said with it and in its transcript — "1. Wasser
  holen." / "2. Brot kaufen.", no bare-number clause, no gap. It is never
  dropped: German nouns are capitalised, so the same shape is a real
  sentence's number ("Das ist klar. 1. FC Köln ist abgestiegen.", "1.
  Bundesliga", "1. Advent", "Wie viele? 1. Das reicht."), and R3's rule,
  which left it out as a marker, deleted it. Such a clause begins
  mid-line (`Placed::line_start`), so the speakable pass judges no
  numbered marker first in it either. **The TTS gets the number without
  its dot** (R5 F2): Pocket TTS read "2." as a sentence end and paused
  450–650 ms between "zwei" and "Brot kaufen", longer than the gap between
  clauses (live run 3c). So the clause (`Placed::inline_item`) is sent "2
  Brot kaufen." — the number said straight on as the same word, as a
  line-start item's dropped marker is no stop either; a colon or a comma
  would put a pause back into "1. FC Köln", which still says "eins" with
  none — and its transcript keeps "2. Brot kaufen." (the clause's two
  texts, below). The cost: "Wie viele? 1. Das reicht." runs "eins das
  reicht" on. Anything else stays as before — "42." after a sentence, "3.
  Oktober" and "1. Mai" (dates), "Das kostet 30.", "der 1. und 2. Platz".
- **German ordinals and dates are no sentence end** (B3 item 15): cut
  after "am 3.", a voice pauses and reads "drei" for "dritten". A dot after
  a one-to-three-digit number goes on when a month name (German or
  English) or a lowercase word follows, or when the word before the number
  is one German puts before an ordinal (am, vom, zum, zur, bis, ab, seit,
  der, die, das, den, dem, des, im, beim). Anything else stays a sentence
  end: "Das kostet 30. Dann …", and four digits are a year.
- **Known abbreviations** (a small, visible table: e.g., i.e., etc., vs.,
  z. B., d. h., usw., bzw., ca.) are no sentence end — except that `etc.`
  and `usw.`, which can stand last, end the sentence when a capital
  follows ("… Birnen usw. Dann …", package B review 2). `bzw.` and `ca.`
  never do: they introduce what follows, and German capitalizes nouns
  ("bzw. Birnen", "ca. Mitte Mai"). **Titles** (Dr., Mr., Mrs., Prof.,
  Nr., St.) are never a sentence end, and are not written out.
- **Fenced code is not spoken.** The lines between an opening fence (three
  or more backticks or tildes) and its closing fence, and the fence lines,
  are no clause; a fence the stream never closes takes the rest with it,
  as CommonMark has it, and an INFO line with the session's id says how
  many bytes of it — from its opening line on — went unspoken. A
  backtick fence's info string may not hold a backtick (CommonMark), so a
  line like "```npm i``` installiert …" is inline code and spoken —
  taken as a fence, it silenced the rest of the answer (B2 review A). Such
  a line is judged once it is whole.
  It is skipped silently, with no "code omitted" placeholder — no other
  part of the voice says what it leaves out — and the spoken transcript,
  which is what was said, has no code either. The text the client sees is
  the transcript, so a client that needs the code must ask for text
  output (§8.3). The model's own history keeps the code (§7.3, B2 review
  2).
- The scan is incremental: each delta is judged once.
- **An inline tag is no words** (WP10, `clauses/tagged.rs`). A tag's words
  do not count — a `[` may open a tag: a lowercase letter, then up to 31
  bytes of `[a-z _'-]` on the same line, then `]`, the canonical grammar of
  `audio/tags.rs` — so "[laughs]\nOkay." is one clause. A candidate that
  turns out to be no tag gives its words back.
- **Every clause ends a sentence or a line** (2026-10-05): it was cut at a
  `.`, `!` or `?`, or is closed — at a line end, or the stream's. A delivery
  cue (below) therefore covers exactly the clause it opens, and
  `Placed::ends_sentence` is gone.

A **speakable-text pass** runs before TTS. It removes markdown syntax
(emphasis markers, heading hashes — a closing run too, "## Titel ##" —
code fences and the code between them,
link targets but not link text), bullet and number markers (with the
numbering rule above, within the clause), setext underlines (`===`) and
rule lines (`---`, `***`, `___`). It writes the known abbreviations out
("for example", "zum Beispiel") — one that ends a sentence keeps its full
stop — and turns parentheses into commas. **Inline tags are none of it**
(WP10): after the link pass (`[see here](url)` stays a link) every tag the
grammar finds is hidden behind a private-use character and put back at the
end, canonical, so emphasis no longer makes `*laughs*` the word "laughs" nor
the parenthesis rule `(laughs)` ", laughs,"; `(laughs)` comes out
`[laughs]`, which shaping maps to the TTS's `[laughter]`. The text's own
private-use characters are dropped. The cleaned text is what the TTS gets;
the spoken transcript is it without its tags.

**Two texts per clause, and the carry** (WP10). A clause goes to the
speaker as `tts` (tags kept) and `said` (`tts` with its tags stripped as for
a TTS that renders none: "Hello [laughs]." is "Hello."); `said` is the
transcript. An inline list's number opening the clause is the one other
difference: `tts` has it without its dot, `said` with it (R5 F2, above). A clause whose `said` has no letter or digit is not sent: its
tags go before the next clause's words ("[laughs], ja." is sent "[laughs]
ja.") and its text joins that clause's written-before. One at the end of the
stream is not voiced — an engine needs words around a sound, and the hint
says "before" — and stays in the history. The `empty_input` arm stays as a
backstop.

**A leading tag is a cue** (WP9b, `responder/speech/cue.rs`,
`audio/cues.rs`). A cue is a WP10 tag in a new position, not a second
grammar, so the splitter, the speakable pass and the transcript strip
already handle it:
- **What counts:** every tag at the very start of a clause's `tts`, after
  the carry — the canonical `[a-z][a-z _'-]{1,30}` (a single letter is no tag since chat-voice
  WP7 review m12: `[x]`, `[a]` are a checkbox and an enumeration), and the stage
  directions, which come out `[laughs]` already. Free text, no closed list:
  both target families read natural language, and a list would be lmgw
  guessing what each model renders. Several join with ", " ("excited,
  laughing"), `_` and `-` become spaces, and what punctuated the cue goes
  with it, as in `said`: "[laughing], ja." sends "ja.". A link's text
  (`[see](url)`) is no cue.
- **Cue or sound tag:** the route decides, not the spelling (§8.2). On a
  cue route no tag renders, so a leading tag is a cue and any other tag is
  stripped as before.
- **Literal brackets are unchanged:** digits, capitals, non-ASCII and
  anything over 31 bytes are no tag, so no cue either (`[1]`, `[S1]`,
  `[Note]`, `[flüsternd]`). The 31 bytes are the grammar's (and the
  splitter's no-cut window); the hint asks for one or two words, which
  stay inside them, and a longer bracket is read out and shows in the
  transcript — visible, never a silent cut.
- **Where:** at a clause start only. A tag inside a clause is stripped as
  before; the splitter does not cut at tags. A cue alone on a line is no
  clause of its own — a line end or a sentence end cuts only after a word —
  so it opens the next line's clause. Only a stage direction written as a
  word ("(laughs)", "*laughs*", which the speakable pass makes a tag) and
  the flush make a cue-only clause: the stage direction's rides the carry
  into the next clause, where it is that clause's cue; the flush's, at the
  stream's end or before a tool call, is not voiced and stays in the
  history.
- **Scope:** a cue covers the clause it opens — the sentence it opens, since
  every clause ends a sentence or a line (§8.1) — and no other. A clause
  with no leading tag has no cue, so nothing outlives the sentence, a tool
  call's flush or the response. "Until the next cue" would be too long: the
  laugh would run through the weather report. (Before 2026-10-05 the
  first-comma cut made "[laughing] Oh no," a clause of its own, and the
  scope ran on to the clause that ended the sentence.)
- **Language:** English. Qwen3 reads English and Chinese instructions, so
  German speech with English cues is its normal case; a German cue
  (`[lachend]`) is sent as written.

**German dates are written out** (live run 2, E2; narrowed in fix package
B6). Kept in one clause, "3. Oktober ist ein Feiertag." still went to
Pocket TTS with the dot: it said "drei" and then ~2.7 s of nothing (5 of
8 probes failing), while "3 Oktober" and "der dritte Oktober" were clean
8 of 8. The pass writes a number 1 to 31 with its dot out **only when a
German month name follows it** — a date, whose noun is always a masculine
month, so the ending is known:
- after "der": "-te" ("der dritte Oktober");
- after den, dem, am, vom, zum, im, beim, des, or a bare preposition
  (bis, ab, seit, vor, nach, für, gegen): "-ten" ("am dritten Oktober",
  "bis dritten Oktober");
- at the clause start: "-ter", capitalised ("Dritter Oktober ist …");
- after und, oder, bis, a comma or a dash that follows another date: that
  date's ending ("am 3. und 4. Oktober" → "am dritten und vierten
  Oktober", "der 1. bis 3. Mai" → "der erste bis dritte Mai"); a day so
  joined to a date is a date too, its month named once at the end, and one
  with its own word ("vom 1. bis zum 24. Dezember") takes that word's
  ending;
- after any other word ("Heute ist 3. Oktober", "Montag, 3. Oktober"): as
  written.

**Every other "N." keeps its number and dot exactly as written**, as
before B5: "in der 3. Klasse", "der 1. und 2. Platz", "die 3. Version",
"Seite 3. Dann", "ab 18.", "bis 17. Danach", "Ende der 1. Woche". B5 wrote
any recognised ordinal out and guessed its ending from the word before:
a bare "ab"/"bis" made cardinals ordinals ("ab achtzehnten.", "bis
siebzehnten Danach" with the sentence's dot lost), and "der"/"die" ignored
case ("in der dritte Klasse"). An ordinal's ending follows its noun's
gender and case, which no text pass knows. English month names never count
("Step 3. May I …"), and a number at the clause end is never a date. The
splitter's no-split decisions are unchanged (the dot after "ab 18" before
a capital still does not end the clause). Numbers with a digit after the
dot ("3.14", "3.10.2026", "15.30") are untouched. The transcript says what
was spoken.

### 8.2 Synthesis, format and pacing

- **Synthesis.** Each batch — the clauses gathered into one request, below;
  a clause that goes alone is a batch of one — is synthesized with a
  non-streaming `/v1/audio/speech` call (`response_format: wav`) on the
  **response's held TTS route** (§9.1). At 21–50 ms per clause (§3.1),
  streaming TTS buys little for v1; rows with `mode: "streaming"` are §19.
- **Batches** (2026-10-05, `responder/speech/batch.rs`). The speaker joins
  the sentences of one paragraph into one TTS request where it can wait for
  them, and the engine reads them as one text. **Why:** an A/B
  listening test (2026-10-05) preferred one request clearly. A TTS
  speaks each request as an utterance of its own: Supertonic pads each
  request with 0.3–0.55 s of silence at either end, so a join between two
  requests was a hole of 0.7–1.0 s, where the same engine pauses 0.25–0.4 s
  between the sentences inside one request; and cuts inside a sentence
  sounded like stalls (§8.1). The rules:
  - **The first clause of a response goes alone and at once**: nothing
    plays yet, so the first audio waits for nothing more.
  - **A later clause joins while the listener still has audio to hear — the
    deadline rule.** One that is queued, or that comes while the speaker
    waits, joins when the batch with it is still predicted to be synthesized
    before the listener runs dry; one that would not starts the next batch.
    The listener runs dry at the first audio's departure plus the audio made
    so far, counted as if it played from then without a break: playback
    cannot have started earlier, and a break only moves the real end later,
    so the speaker never waits past it. The prediction is the response's own
    and errs long — the slowest request so far, or the slowest rate per
    character (the short first request's fixed costs included) times the
    batch's characters, whichever is longer — and the deadline leaves twice
    that. Nothing is capped: a batch is as long as the listener's audio
    leaves time for, and an engine that chunks long input (audio.cpp) does
    that itself, its joins shortened like ours (Longest pause, below). The
    Chat's read-aloud has no writer, so no back-pressure holds its
    synthesis, but it gathers by the same estimate.
  - **The text stopping ends a batch at once** (`Work::Break`, which
    `Splitter::finish` sends: a tool call, a flush, the stream's end). Once
    the stream has ended nothing is waited for, so the TTS route is held no
    longer than before.
  - **A batch is one paragraph's at most.** A clause the model began on a
    new line — a list item, a heading, the next paragraph, the text after a
    skipped block — starts a batch of its own, and so do an announcement and
    a clause with a delivery cue (its cue is that sentence's alone). A clause
    cut at a line end owns it, so the next one starts anew.
  - **Each clause keeps its own row.** The batch's audio is shared out among
    its clauses by the characters each said, contiguous and the last to the
    end, so the core gets one `Msg::Clause` per clause as before: one
    transcript delta, one heard-table row (§7.3), one `speech` frame in the
    Chat. The audio is the same; only the rows are the clauses'.
- **What a clause is sent** (WP10). The responder hands `Synthesis::speak`
  semantic fields — `ClauseSpeech {input, voice, speed, language,
  instructions, cue, seed}` — and shaping on the route that answers decides
  what each takes (`audio/shape.rs`), as for `POST /v1/audio/speech`.
  The seed goes only to an lmgw row that reads one
  (`synthesize::sends_seed`, §5.5); a remote route never gets it. The open
  checks the row against the instructions the clauses will carry (R1's
  `refuse_route`).
- **The cue is decided on the route that answers** (WP9b C5). The splitter
  works the cue out without knowing the route, and `ClauseSpeech.cue`
  carries it. `clause_body` reads the answering route's `Expressive` — the
  row's profile, or `remote_rules` of the alias that answers
  (`proxy::audio::rules_on`, shared with `shape_on`) — and only on a route
  that takes cues (`audio::cues::fold`) strips the leading tags from
  `input` and folds the cue into `instructions` (§5.5's `"{base}, but {cue}
  right now"`),
  before the preflight and shaping. Elsewhere shaping works as before: a
  fixed vocabulary maps `[laughing]` to `[laughter]` (an OmniVoice or
  CosyVoice3 fallback), a voice-design or `none` route strips the tag
  (D12's one line). The route is opened once per response, so the decision
  holds for every clause: under the GPU hold a cloud fallback declared a
  style keeps the cues (one nobody described gets the style and the tag
  stripped), and a voice-design route never gets a cue in its
  description. One DEBUG line per cued clause names the session, the
  route, the cue and the instructions sent; there is no INFO line — a cue
  is no loss, and D12 stays losses-only.
- **Format.** audio.cpp answers with a WAV even when `pcm` is asked for, so
  the WAV header is always parsed and gives the model's native rate. Pocket
  TTS returns 24 kHz, which already matches. Other rates are resampled to the
  session's output format (PCM16 24 kHz). Each clause fades in and out
  over 5 ms (raised cosine), so clauses join without a click (§23 L11); the
  sample count is unchanged. The clause is then cut into
  `response.output_audio.delta` events of ~100 ms.
- **Longest pause** (`longest_pause_ms`, default 400 ms, a visible setting
  and `session.lmgw` knob; 0 = off, the engine's silences kept). A TTS pads
  each request's audio with silence before the first word and after the
  last, so every join between two requests was a hole of 0.7–1.0 s, where
  Supertonic pauses 0.25–0.4 s between the sentences of one request
  (measured 2026-10-05). `realtime/audio/pauses.rs` cuts every silence
  longer than the setting down to it, in each synthesized request's audio
  (the batch's, above) before the fade: the silence before the first sound
  and the one after the last keep half the setting each, so two requests
  join with the longest pause at most, and a longer silence inside (an
  engine that joined chunks of its own, as audio.cpp does) is shortened,
  keeping half at either end of the cut. Shorter pauses are left as they
  are.
  - **Silence is judged against the clip's own level:** a 10 ms window whose
    loudest sample stays under 1 % (−40 dB) of the clip's level is silent,
    the level being the 95th percentile of the windows' peaks — so one click
    louder than the voice does not make the voice around it silence, and a
    quiet voice is not taken for silence. A clip with no sound at all is left
    as it is, and a noise floor or DC offset above −40 dB is no silence, so
    that engine's padding stays.
  - **It knows no reason for a pause.** One the engine made on purpose (a
    `[pause]` tag, a dramatic gap) is capped like any other, and below
    ~150 ms the setting reaches the gaps inside words (stop closures) and
    makes speech choppy. That is not refused: it is the owner's call.
- **Pacing.** The writer sends output audio **paced to real time, plus a
  lead** (`output_lead_ms`, default 500 ms, a visible setting and
  `session.lmgw` knob).
  - The first `lead` of audio goes out at once, and after that audio leaves
    as fast as it plays.
  - `output_audio.done`, the closing events and `response.done` are sent
    when the client has played the audio — at the playing window's end
    (§6.4), which at the steady state is the paced send's end plus the lead
    — not when synthesis ends, nor when the last chunk leaves (owner's
    decision Q1). A barge-in in the last lead of playback therefore still
    has a response to cancel. The price: whatever waits for `response.done`
    (a queued tool follow-up, an owed response) starts up to
    `output_lead_ms` later than at the send's end.
  - This keeps the stock SDK's playback state alive until playback really
    ends (§2.3). It also gives the barge-in gate (§6.4) and the heard table
    (§7.3) a true clock.
  - It costs no first-audio latency.
  - **Function-call events are the one exception to pacing.** Those of a
    spoken response are not held behind its paced audio, so the client's
    tool runs while the text before it is still playing. The message item
    closes at the drained ack. They do wait for the **synthesis** of the
    clauses before them, though (the speaker keeps stream order, §7.4), and
    that synthesis waits on the back-pressure below: a call after a spoken
    preamble longer than `synthesis_ahead_s` goes out only once all but
    that much (and its last clause) of the preamble has been sent — up to
    the preamble's length minus `synthesis_ahead_s` later than without the
    bound — unless the bound is lifted (below). Package B review 9: a
    30-second preamble before a tool call is rare in a voice answer, so the
    bound stays.
  - A chunk that leaves late, behind a stalled socket, re-bases the
    schedule: what follows is due from when it really left, not all at once.
  - Paced audio waits in the writer as PCM and is base64-encoded as it
    leaves. When the session closes, paced audio not yet sent is dropped:
    its client is going away.
- **Back-pressure.** Synthesis runs ~30× real time, so a speaking response
  stops synthesizing its next clause while more than `synthesis_ahead_s`
  (default 30 s, a visible setting and `session.lmgw` knob; 0 = no bound) of
  its audio is queued and not yet sent, and goes on as the writer sends it.
  Nothing is dropped. A looping model or a huge answer cannot grow a
  session's memory with its whole length — except for the one answer whose
  bound is lifted (below), which then holds the rest of its own audio. The
  price: the TTS hold of a long answer spans all but that much of its
  playback (§9.1).
  - **What lifts the bound** (package B review 1). For a response whose TTS
    route holds a local model, the bound is dropped for the rest of the
    response — the rest is synthesized at full speed and the hold let go
    after its last clause — as soon as one of these is true:
    - the **GPU hold** is on, or a benchmark has the GPU: the hold means
      lmgw uses no VRAM, so its work must end soon, not after minutes of
      playback;
    - an **admission waits for room** (the registry's draining mark, which
      every owner admission's make-room wait sets): a claimed model is never
      evicted, so the TTS model would stand in that request's way — the
      session's own ASR included — for as long as the answer plays. The
      mark is not specific to the TTS model (the VRAM manager has no cheap
      "this model is wanted" signal), so a wait that another eviction would
      satisfy lifts it too; that costs only memory;

    The hold and the mark are atomic reads, judged whenever the writer
    releases a delta and at the stall deadline. A route without a local
    claim (a cloud TTS, a fallback answering) keeps the bound: it holds
    nothing on the GPU, and lifting it would only cost memory. What a
    lifted bound costs is the rest of that one answer's audio in memory,
    48 KB a second of PCM — bounded by the answer, that is by the chat
    model's own output maximum. An INFO line names the reason.
  - **A client that stops reading keeps the bound** (B2 review 3). The
    writer stalls: the client's playing window ended, by more than one
    output delta (100 ms), while the answer's audio is still waiting to
    leave — the same facts the window reads (B3 review 8). The pacer never
    lets a reading client run dry while audio waits, so this is a client
    that stopped reading (or cannot keep up), not an underrun. B2 lifted
    the bound here too, which brought back the unbounded memory the bound
    exists for: a stalled client, `ping_interval_s` 0 and a looping model
    synthesized the rest of the generation, 48 KB a second, into a queue
    nobody reads. Now the bound stays, and the response **lets go of its
    TTS model** while the client does not read — so the model is neither
    pinned against eviction nor draining under the GPU hold for as long as
    the session lives. When the writer has room again (the client reads),
    the next clause **admits the model again on the same route**: through
    normal admission, which may queue it or start it again if it was
    evicted, and which refuses it under the GPU hold or a benchmark — the
    response then fails with that error rather than switching to a
    fallback voice mid-answer (§9.1). INFO lines say when it lets go and
    when it is admitted again.

### 8.3 Text modality

`output_modalities: ["text"]` skips the splitter, TTS and pacing, and
streams `response.output_text.delta` directly. A text-only session needs no
TTS alias.

## 9. Residency, VRAM and the GPU hold

### 9.1 Warm early, hold at the point of use

A session must neither pin its models while quiet nor leave them to be
evicted mid-answer. So it does not hold its models for the whole session:
- a quiet session left open would pin three models against every other
  request's admission;
- under a GPU hold it would keep them *draining* forever, because the hold
  never kills work (§3.2).

The rules:
- **Warm, non-evicting.** At session start (`warm_on_connect`, default on)
  and again at every `speech_started`, the session asks
  `check_background_start` for the ASR, chat and TTS models — and for the
  barge-in word check's model when `barge_in_check` is `words` and it is
  not the session's ASR model (R3 D3). The same path
  lifecycle warm starts use starts a cold container if it fits, **skips
  rather than evicts** when the GPU is full, and takes no hold. A false
  `speech_started` from noise therefore costs nothing. A TTS row that can
  never speak the session's answers is not warmed (WP10): judged as a
  response's open judges it (`synthesize::refuse_route` — a task its
  package does not run, a voice-design row with no description in the
  session's speech instructions or its own defaults), skipped with the
  reason in the log; the response that tries to speak reports it.
- **Warm means loaded** (R4 D3'). audio.cpp loads a lazy row's weights on
  its first request (`audio.lazy_load`, on by default), and its port
  answers before that. So once an audio row's container is up — started
  by the warm, or already there — a row that has not loaded its model
  (`VramScheduler::audio_model_loaded`: it answered nothing yet, or
  audio.cpp unloaded it for idling) is sent the smallest real request: half
  a second of 16 kHz silence to a speech-to-text row (the ASR, the word
  check's), one word ("Hello.") in the session's voice, seed, language and
  speech instructions to the TTS — nothing to a TTS whose voice resolves
  to none. audio.cpp's own `POST /v1/models/load` is not usable: it is
  refused without `--ui-management`, which lmgw keeps off. The request
  goes on an owner's join of the running container (`vram::join`,
  `Restart::No`): it never starts, queues for or evicts anything, it is
  refused under the GPU hold or a benchmark, and a container gone
  meanwhile is not brought back. It is accounted like any request — the
  model is busy while it loads, the pending load stops being charged once
  it answers, the residency is read — but it writes no `request_logs` row
  and is one INFO line: nobody asked for it, and the owner's Usage page
  and costs stay what clients sent. A row that loads at start
  (`lazy: false`) and one already loaded are sent nothing.
- **ASR hold.** Taken at commit, around the transcription call, and dropped
  with the transcript.
- **Chat and TTS holds.** The chat hold is taken at `response.created`. The
  TTS route is opened at the **first clause**, so a response that only calls
  a tool never holds, or fails on, a voice it does not use. Warming has
  already started the model. Each is dropped as
  soon as **its last call returns**: the chat hold when the stream ends, and
  the TTS hold when the last clause is synthesized — for an answer longer
  than `synthesis_ahead_s`, that is that long before its playback ends
  (§8.2). This is not
  `response.done`, which with paced audio comes seconds later and would pin
  the models for the whole playback. A follow-up `response.create` after a
  function call takes them again.
- **A long answer lets go when asked to.** The synthesis bound keeps the TTS
  hold for most of a long answer's playback. The GPU hold switched on or an
  admission waiting for room lifts the bound for the rest of the response
  (§8.2): the rest is synthesized at full speed and the hold dropped after
  it, so the reaper's hold sweep can stop the model and an admission can
  evict it. A client that stopped reading keeps the bound, and the hold is
  let go while it does not read and taken again when it does (B2 review
  3), so a silent client does not pin the model either.
- **One route per response.** Each held route is **the** route for its
  response. The TTS helper sends every clause of the response over the one
  `Opened` it was given, rather than calling `gate::open` per clause, so a
  GPU hold engaged mid-answer cannot switch the voice to a fallback
  mid-sentence. `transcribe` gains the same `Opened`-taking form.

If a model was evicted between turns, the turn pays its cold start, and the
log line says so.

### 9.2 The GPU hold

Each response resolves its routes afresh, so a hold switched on between
turns takes effect at the next turn:
- a local audio or chat alias with a fallback uses the fallback, echoed in
  `lmgw.resolved`;
- one without a fallback fails that turn with `error {code: "gpu_hold"}`
  (and `…transcription.failed` for ASR);
- the session stays open.

An audio row on the CPU (`backend: cpu`, the per-row CPU switch of
2026-10-02) is not held at all: it takes no VRAM, so its turns and word
checks keep running on it under the hold, `warm_on_connect` still warms and
loads it (its cold load, 2.8–3.7 s for Parakeet, lands at connect), and a
speaking response on a CPU TTS keeps its playback bound under the hold. A
benchmark's lease does cover it, so the lease lifts that bound, and a client
that stops reading lets its claim go as on the GPU: the benchmark's drain
waits for that claim. `realtime_budget`
shows such a stage as `cpu`, 0 bytes, with its thread count; host RAM is not
measured. The 500 ms check timeout stands: Parakeet's check measured 89/136
ms (p50/p95) on 8 CPU threads, and about 390 ms queued behind a short turn
on the same row.

A hold switched on mid-response lets that response finish on its held
routes, which is what the hold design promises every request. A speaking
one finishes its synthesis at full speed rather than at the pace of its
playback (§8.2, "What lifts the bound"), so its TTS claim ends with its
last clause instead of near the end of the playback. That last clause
comes only when the chat stream ends, though, and the chat model's own
claim lasts until then too: the hold drains work, it does not cut it. So
both models are let go — and stopped by the reaper's hold sweep — as soon
as the generation ends: seconds after the hold for an answer that was
nearly written, and as long as the rest of the generation takes for one
that was not, up to the chat model's output maximum (B2 review 7).

### 9.3 Admission

The holds go through normal admission. A turn that cannot get VRAM queues
exactly as a request would. A refusal becomes an `error` on that response
rather than a hang.

### 9.4 Audio footprint (WP7, built)

The audio class used to charge its on-disk size to the VRAM ledger, but
measured residency is about twice that (§3.1). That is more than the
default 1 GB headroom absorbs once three audio models are up, and a voice
session that routinely keeps ASR, TTS and an LLM resident makes it matter.
The ratios (1.3× nemotron, 1.6× qwen3-asr, 7.5× pocket) rule out a
multiplier, so lmgw learns the figure per row, as the image class learns
its peak (image §9).

**Lazy loading is the bigger gap.** audio.cpp loads a model on its first
request, and its readiness route answers before that. A container that is
up but has not been asked anything — warmed by a session (§9.1), a boot
warm start, the operator's Start — holds only its CUDA context. Its
reservation is gone and the driver does not show the weights it is about
to load, so the ledger used to offer that memory to the next start.

**What is learned** (`vram/residency.rs`):
- When an audio container answers an inference request with a 2xx
  (speech, tasks, ASR uploads, each realtime TTS clause; never the voice
  list), its generation is marked **loaded**, and one per-process reading
  of the container is taken: the driver's figures for its init and every
  process under it. At most one reading per generation is in flight. A
  buffered answer counts at its headers, a streamed (SSE) one only once its
  relay ends whole: at its headers audio.cpp may still be loading.
- **While a request runs** the container is also sampled, at the image
  sampler's 50 ms (`SAMPLE_INTERVAL`), from the send until nothing is in
  flight on it: audio.cpp frees part of its compute buffers once it has
  answered (the live gate read pocket-tts 0.921 GB after its answer and
  0.981 GB at most while it worked). The figure is the larger of the
  samples and the reading after the answer, and only a stretch with an
  answer counts. One sampler per generation, only for a container running
  the row's current configuration, and none once `SETTLE_AFTER` (3)
  sampled stretches in a row did not raise the figure — the count is in
  memory, and the models page says where it stands. A stretch is one
  request, or a realtime response's clauses under one hold.
- The row keeps the **largest** figure (`resident_bytes`, migration 0053).
  A smaller one never lowers it; nothing is stored as 0.
- No per-process figures (no NVML or amdgpu process list), no learning. No
  device-wide figure stands in for them.
- A reading that finds no figure says why in the notes, and the next
  answered request reads again (a failed PID read is asked again too). The
  owner's reset bumps an epoch under the store's lock, so a reading or a
  sampler that began before it cannot write its figure back.

**Keyed, not reset.** The figure is stored with a 12-hex key of what
changes what audio.cpp loads: family, path, task, mode, config and weight
ids, spec override, load and session options, the effective image and run
args, the class backend and device. Voice presets, request defaults, busy
timeout, lazy loading, idle unload, threads, warm start and the hold
fallback are not in it. A row whose key no longer matches keeps its figure
but is charged at its on-disk size again, and its next request teaches the
new configuration. The key a container was started with is recorded on its
registry entry, so a container still running a previous configuration (an
edit while it was busy, a class image changed without a restart) teaches
the new one nothing — and is charged the figure stored for the
configuration it runs, when that is the one stored. A rebuilt image under
the same tag is not detected: the running maximum corrects such a figure
upward, the owner's reset downward.

**What is charged.**
- An audio row's footprint is its learned figure when the key matches,
  otherwise its on-disk size. This feeds admission, reservations, a
  `starting` entry, budget-only planning and the outside-VRAM verdict.
  Before anything is learned the charge stays the on-disk size, never a
  guessed multiple of it.
- A **ready audio container that has not loaded** is charged what it is
  still to take as **pending**: subtracted from the measured free figure,
  next to the starts the driver cannot see yet and the image peaks.
- **Read at rest.** Every path that makes an audio container ready (a
  start, a warm start, the operator's Start, boot's adoption) takes one
  per-process reading of it while it has answered nothing and has nothing
  in flight. Nothing is stored. Pending is the expected residency less that
  reading, so the CUDA context the driver already shows is not counted
  twice. A lazy row's container holding more than `BARE_CONTEXT_CEILING`
  (512 MiB) plus half its on-disk size at rest is **taken as loaded** — a
  container a previous lmgw left running, which had answered requests
  then. An eager row's reading is its weights and context.
- Where nothing could be read (no per-process figures, a failed reading, a
  request that came first): all of the expected residency for a lazy row,
  and what comes on top of its selected weights file for an eager one —
  the GGUF matching its `weight_id`, else the smallest, never the
  directory, which may hold several variants. The note says why.
- A container holds its model while it answered within `idle_unload_ms`.
  After that it is pending again — its expected residency less its bare
  context when that was read — and a request in flight does not count as
  holding: an unloaded model's next request is a reload. An adopted
  container's idle clock starts at its adoption.
- Budget-only planning already counts the whole estimate, so nothing is
  added there.
- Eviction is unchanged: an idle audio container that has not loaded is a
  candidate like any other, and its pending charge goes with it.

**Surfaces.**
- `GET /api/vram` (and the `vram` frame, `lmgw__status`): a ready audio
  resident's `pending_bytes`, and a note that says what is charged and why
  — "resident X learned <date>" with where sampling stands, "not learned,
  charged at the on-disk X, which audio.cpp has been measured to exceed",
  "learned for a previous configuration", "cannot learn: <probe's reason>",
  "not loaded yet: X kept free until its first request (it holds Y at rest
  already)" or "— all of it, because <why nothing was read>", "taken as
  loaded", "unloaded after idling", and why the last reading found no
  figure.
- An admission refusal names it: `audio/tts (2.0 GiB, 2.0 GiB of it until
  loaded)`.
- `GET /api/models/full`: each audio row's `model.residency` (`bytes`,
  `learned_at`, `key`), `residency_charged_bytes` (the figure admission
  charges, `null` when none applies) and `residency_note`. The models page
  shows "resident X" on the row, the dashboard's chips "X of it to load".
- The owner's reset: `audio_model_set` (or `lmgw__audio_model_set`) with
  `action: "update"` and `clear: "residency"`.

**Image peak fix.** The image sampler abandons a window whose shape
changes, and an audio load inside a container that stays `ready` changed
nothing it compared — so a lazy load during a generation was learned as the
image's peak. The shape now carries which ready audio containers hold their
model, and a request in flight on one that has not loaded spoils every
window it overlaps. A window opens against an earlier tick's baseline only
when that tick saw the same shape with no audio load in progress, so a load
between two ticks is not learned either.

**Verified live** (`tests/it/audio_residency_live.rs`, ignored and
env-gated, the owner's run). The first run (at e6d6fbf) failed (a): the
reading after the answer missed pocket-tts's transient by 6 %, which is
why requests are sampled now. The run checks (a) again, against the
learned figure; (b) that an eager row's readiness route answers only after
its weights are loaded (else, without a reading at rest, eager rows take
the lazy rule); (c) that the three voice models learn about 1.48 / 0.97 /
3.28 GB; and (d) that each bare container reads below the at-rest line and
each learned figure above it. Every failure is listed at the end of the
run.

**Measured 2026-10-01, RTX 4090** (the live gate, passed; whole test
11.5 s):

| row | learned | = | at rest | on disk |
|---|---|---|---|---|
| pocket-tts-german | 0.981 GB | 20 Hz in-flight max (100 %) | 0.000 GB | 0.128 GB |
| nemotron-asr | 1.455 GB | max (100 %) | 0.000 GB | 0.931 GB |
| qwen3-asr-1.7b | 3.343 GB | max (100 %) | 0.000 GB | 2.473 GB |

qwen3-asr eager (`lazy: false`): 2.875 GB on the card at ready. A lazy
audio.cpp container holds no GPU memory at all before its first request
(no CUDA context yet), so its pending figure is the full expected
residency.

## 10. Auth, policy, limits

### 10.1 Credentials

The route sits in the `/v1` group, so `Cap::Inference` applies unchanged.
Credentials:
- `Authorization: Bearer` or `x-api-key`, as everywhere;
- the dashboard's same-origin session cookie;
- for browsers, the `openai-insecure-api-key.<key>` subprotocol, **for this
  route only**. `principal_mw` reads it only on `/v1/realtime`, because
  widening `token::presented` would change every route.

The 101 selects `realtime` whenever the client offered it.

With gateway auth off, anonymous requests are allowed and CORS is
permissive, which would let any web page the owner visits open a voice
session against local models. **An anonymous upgrade whose `Origin` header
is present and differs from `Host` is refused.** SDK clients send no
`Origin` and are unaffected.

What this rule does not cover:
- **DNS rebinding.** A rebinding page makes `Origin` and `Host` both the
  attacker's name, so the rule does not stop it. With auth off, every other
  `/v1` route is equally reachable that way, so this is not a new exposure.
- **Exposure in general.** Turning gateway auth on is the real protection,
  as it is for the rest of the gateway.

### 10.2 Before the upgrade

Everything that can fail cheaply fails **before the 101**, as an ordinary
HTTP error that clients surface:
- auth and capability;
- `policy::check_alias` with the session's key for the **chat, ASR and TTS
  aliases** the session starts with (scope and budget);
- an unknown alias;
- an `OpenAI-Beta: realtime=v1` header, which gets 400
  `beta_protocol_unsupported`, "lmgw speaks the GA Realtime protocol";
- an anonymous cross-origin upgrade (§10.1).

A plain `GET` without upgrade headers answers **426**. The handler takes
`Result<WebSocketUpgrade, _>` and maps the rejection, because axum's default
is 400.

### 10.3 Policy during the session

- **Concurrency.** The session takes the key's concurrency slot
  (`state.policy.admit`) itself and holds it for its life. The generic
  guard does not survive the upgrade (§3.2), so one open session is one
  concurrent request for `concurrency_limit`.
- **Scope and budget.** These are re-checked with the session's key
  **before every model call**:
  - each chat stream;
  - each ASR call;
  - the first TTS call of a response.

  A refusal becomes an `error` on that response, and the session stays open.
- **Rate limits.** rpm and tpm count those model calls, not the session.
- **`session.update`.** An update that changes an alias re-runs
  `check_alias` for it.

### 10.4 Limits

None are invented, and the ones that exist are visible:
- **Session length.** A session lasts as long as the socket.
- **Input buffer.** It grows until a commit. The ASR engine's own body
  limit (`max_request_body_mb`, in Audio settings) is where a runaway manual
  buffer surfaces as an error.
- **Smart Turn's 8 s** is the model's input size, not a cap on the turn.
- **WebSocket message and frame size.** tungstenite has built-in defaults
  (64 MiB per message, 16 MiB per frame). They are set **explicitly** from
  the settings `realtime.max_message_mb` / `max_frame_mb`, shown in the
  Realtime settings. An overrun closes the socket with a close reason that
  names the setting.
- **Read-ahead.** The socket is read by a task of its own, which stamps
  each frame as it comes off (§6.4). It reads ahead only while less than
  `max_frame_mb` of frames waits for the session core — never less than one
  frame, and no number of its own — so a core that waits for its writer
  still pushes back on the client through TCP, as before. Every frame
  counts its payload plus the slot it waits in (B3 review L1): an empty
  ping or pong used to count nothing, so a flood of them grew the queue,
  and a liveness check's backlog, without bound.
- **Concurrent sessions** are not capped beyond the key's concurrency
  limit. They are visible in the log.
- **Liveness.** The server pings every `realtime.ping_interval_s` (default
  20 s) and closes with a reason naming the setting when no pong arrives
  within one more interval. When closing, the writer gets one interval to
  drain. `0` turns pinging off, and then there is no bound at all: a live
  peer that stops reading answers TCP's probes, so the session, and the
  key's concurrency slot, can last forever. The setting's text says so.
  Before a no-pong verdict the frames the reader already passed on are
  looked at: a pong lifts it, and a read side that ended instead ends the session at
  once, closed as the reader loop would have closed it for that frame — a
  frame over the size limit with 1009 and the reason naming the setting
  (A2 review 1), a close or a FIN with nothing. The log line says which
  happened rather than "the client closed".

## 11. Usage and telemetry

- **Chat:** one `request_logs` row per model call, as in every tool loop
  (`stream_once_on`).
- **ASR:** one audio row per committed segment, and one per barge-in word
  check (§6.4).
- **TTS:** **one audio row per response**, not per clause. The TTS helper
  runs a response's clauses on one route and records once. Per-clause rows
  would add five or more rows per answer and drown the Usage page.
- **Labels.** All rows carry the session's client key, so per-key budgets
  and Usage filters apply. They also carry a `realtime` label, which needs a
  new `ClientProto` variant: today's enum has only `openai` and
  `anthropic`.
- **The key is its id, at record time** (package A review 2, A2 review
  3–4). A row is recorded against the key's `api_keys.id` and its current
  name, so a session — or a `/v1/responses` tool loop, which takes the same
  key reference — that outlives a rename of its key keeps charging that
  key, never a new one that took the old name. A key deleted while its
  request ran gets no tokens/minute window folded in: nothing would read
  it again.
- **A stopped call bills the prompt only if one is being worked on.** A
  chat call stopped before the upstream answered is a `canceled` row with
  an estimated prompt once the request went out (§4.3). An attempt the gate
  takes back clears that, and a stop while the gate retries bills nothing:
  - a guest candidate's context refusal held back, or a candidate lost or
    re-picked — the call that replaces it keeps its own record (A2 review 2);
  - on a ladder, an attempt its count says does not fit, or that
    llama-server refused as above its context (the backstop): it is dropped
    and the model climbs (B2 review 1);
  - a container found dead (a transport failure on the send or the count),
    on a ladder or not: it is recovered, and a dead container works on
    nothing (B2 review 1).

  A wait for response headers that timed out is none of these: that prompt
  went out to a container that is still there, so a stop then bills it.
- **No new columns in v1.** `request_logs` has no field for characters or
  audio seconds, so those go into the response's TTS log line only
  ("realtime {session}: TTS '{alias}': … characters sent, … s of audio"),
  the characters as sent — after the cue and shaping took theirs. Audio rows carry no
  tokens today, and cloud audio is unpriced, so audio budgets bite only
  through per-call counts. That limitation is stated here rather than
  hidden.
- **`response.done.usage`.** It reports the chat model's `input_tokens` and
  `output_tokens` as text tokens. The audio token details are 0, because a
  cascade has no audio tokens.
- **Smart Turn: no row.** It runs in process, with no alias, no gate and
  no hold, so there is nothing to record against a model or a key. Its
  cost is CPU, one score per pause, visible at DEBUG (§6.3).
- **Timing line.** Each response logs one line: end of turn → commit, ASR,
  LLM time to first token, first clause, TTS to first audio, and total.
  For a `semantic_vad` turn, end of turn →
  commit also names the part of the rule that ended it (§6.3). A response is timed from its own turn
  only: a turn no response answers — noise or a failed transcription,
  whether an automatic response was owed to it or not (`create_response`
  off, a client's commit; A2 review 5) — is dropped, and a later response
  is timed from its own `response.created`.

## 12. Settings and surfaces

- **`RealtimeSettings`** in `config/settings_classes.rs`:
  - models: `default_model`, `model_map`, `asr_alias`, `tts_alias`;
  - voices: `default_voice`, `voice_map`;
  - expressive speech (WP10, §5.5): `speech_instructions` — the style, or a
    voice-design row's description, while a session sends none; empty by
    default, with no built-in text (a default is not the owner's taste, and
    the TTS's own delivery is neutral) — and `tag_hint` (on): the prompt is
    told what square brackets do — the sounds the TTS makes, or since WP9b
    the delivery cues it takes (§7.2); tags and cues apply with it off too.
    Types only: no save check, no length cap;
  - `default_instructions`: unset means the built-in short voice-assistant
    prompt (plain spoken sentences, no markdown, the user's language, §23
    L8; and since B5 that it does not know the date or time unless the
    conversation says it — with the earlier prompt gemma4-e4b-voice made
    the time up in 5 of 6 German answers, live run 2), empty means none
    (package B review 8). It is an `Option`, not a
    string defaulting to the prompt, because the settings blob is saved
    whole: a stored copy of the text would pin an old prompt across
    releases. Used while the client's own are absent or empty, and **only
    in a session with audio output**: a text-only session gets no
    instructions of its own, since the prompt asks for answers made to be
    heard. The echo shows what is in effect; a session that switches to
    text output drops the default, and one that switches back gets it
    again. **Instructions equal to the default in effect count as the
    default, wherever they came from** (B2 review 6) — the session's own
    echo, or a client sending back the session it was given with text
    output: either way text output drops them. The session cannot tell
    where a text came from, so after the owner changes the default
    mid-session the old text is the session's own until the client changes
    it. The default is judged on the session's `output_modalities`, not per
    response: a `response.create` that asks for text in an audio session
    keeps the session's instructions, and one that asks for **audio in a
    text session gets no voice prompt** — that session has none.
    - **What an older data dir holds** (B2 review M5). An earlier
      `feat/realtime` build kept `default_instructions` as a plain string
      defaulting to empty, and the settings blob is saved whole, so a data
      dir it wrote holds `"default_instructions": ""` — which now means
      *off*: such a dir speaks without the voice prompt until the owner
      clears the field (unsets it, rather than emptying it). There is no
      migration: the branch was never released, and only development
      copies, since deleted, wrote one. The B2 commit message called this
      case fixed; it is not, and this is the behaviour.
  - turn detection: `threshold`, `prefix_padding_ms`, `silence_duration_ms`;
    for `semantic_vad` (§6.3) `semantic_vad_engine` (`smart_turn`, or
    `server_vad` as the escape hatch), `semantic_vad`, a table with rows
    `high`, `medium` and `low` (auto is medium), each holding `threshold`,
    `floor`, `max_wait_ms` and `silence_duration_ms` (the window for no
    score), and `semantic_floor_window_ms` (500). A row or field missing
    from the stored blob takes its own row's default. Then
    `barge_in_min_ms` (200, assuming
    `barge_in_check` `words`; with `duration` an owner may want 300 or
    more, §6.4), `barge_in_guard_ms`
    (500), `post_interrupt_silence_ms` (1500), `half_duplex` (off, §6.4),
    `echo_tail_ms` (250, §6.4), `barge_in_check` (`words`),
    `backchannel_words` (the German and English list of §6.4),
    `barge_in_check_scripts` (`["Latin"]`, §6.4: an owner who speaks a
    language in another script must add it), `barge_in_check_timeout_ms`
    (500) and `barge_in_check_alias` (empty = the session's ASR alias;
    §6.4 recommends a qwen3-class model when the turns use nemotron). The
    barge-in gate's 800 ms evidence gap is an algorithm constant, named in
    §6.4, not a setting;
  - **what a save refuses** (fix package B6; WP8, built), since no session
    should discover it, each refusal naming the field: a `semantic_vad`
    row that cannot run with `semantic_floor_window_ms`
    (`SemanticVadTable::problems`: threshold or floor outside 0..1, floor
    above threshold, floor window past the row's `max_wait_ms`), judged
    when the save touches the table or the floor window; a
    `barge_in_check_alias` that is no ASR alias (`asr::is_asr_alias`); an
    `asr_alias` whose capability task is not `asr`, or a `tts_alias` whose
    task is neither `tts` nor `vdes` (WP10, `SPEECH_TASKS`);
    a `default_model` that is no chat alias in the session's sense
    (`resolve::is_chat_alias`), and a `model_map` target that is neither a
    chat nor an ASR alias; a `barge_in_check_scripts` name the word check
    does not know; a `threshold` outside 0..1; and `max_message_mb` and
    `max_frame_mb` both 0. An empty alias is never refused — each one's
    "none" has its meaning above. The session-start fallbacks stay as the
    second line for a blob written another way (a hand edit, an older
    build): a WARN, and the built-in row (§6.3) or the session's ASR alias
    (§6.4) in its place, never the client's error;
  - output: `output_lead_ms`, `synthesis_ahead_s`, `longest_pause_ms`
    (default 400; `0` keeps the engine's own silences);
  - `warm_on_connect`;
  - WebSocket: `max_message_mb`, `max_frame_mb`, `ping_interval_s` (§10.4).

  **As built (WP8):** `RealtimeSettingsPatch` (`ops/realtime_settings.rs`)
  is the one patch both saves take — `settings_set_full`'s `realtime` and
  `settings_set`'s, whose flat MCP tool takes it as a JSON-encoded string
  (`realtime`), hoisted like `ladder`. Every field above is in it; the two
  maps and the two lists are replaced whole; `default_instructions`
  follows the Chat prompt's convention: the built-in text saves as unset,
  `""` as none. The section reads back on `/api/settings-full` and
  `lmgw__settings` as `realtime`, `default_instructions` spelled as the
  text in force, `default_instructions_builtin` and
  `default_instructions_is_builtin`.
- **Dashboard (as built).** A Realtime category in Settings, rows of the
  page's own table: alias pickers filtered by task (chat, `asr`, `tts` or
  `vdes`;
  the word-check model is an ASR picker whose empty choice is "the
  session's speech to text"), the voice — a free box offered the voices of
  the chosen TTS model from `GET /v1/audio/voices`, asked only for a local
  row (lmgw answers it from its catalog without starting it; a cloud
  model's list would be a call to the provider) — the maps as
  `name = value` lines, the word lists as comma-separated boxes, the voice
  prompt with Reset, the turn defaults, the Smart Turn table, barge-in,
  output and the connection limits. Cross-field refusals are caught at the
  field before Save, by the server's rule (R7, WP8 review): only what the
  save judges — a changed field, the whole Smart Turn table with its floor
  window once any of it moves, both WebSocket limits once either moves. A
  value a hand edit broke in a field the save does not touch is a dashed
  amber warning at that field ("as stored: … — other changes still save")
  and blocks nothing; editing it, or what it is judged with, makes it an
  error again. Since WP10 (`settings/realtime/style.rs`): **Speech
  style**, a prose box under the voice whose note says what the drafted
  TTS does with it — a speaking style, the voice's description (required),
  or nothing (dropped) — from its `capabilities.speech` on
  `GET /v1/models/{alias}`, which lmgw answers from its catalog without a
  provider call; and **Sound tags and delivery cues in the prompt** (the
  label since WP9b), a checkbox under the voice prompt with the paragraph
  the prompt would get, live, in the gateway's own words
  (`speech_hint_text`: the sounds, the cue text, or a note that the model
  does neither, so the prompt says nothing about square brackets). A
  **GPU memory** card shows what the drafted cascade holds (below). There is no new page in v1; a live-mic test page is §19.
- **The cascade's VRAM budget (WP8, built).** The `realtime_budget` op
  (read-only, `/api/op`, no self-admin tool) sizes the cascade stage by
  stage: turn detection (CPU, 0), the chat model, ASR, the word check when
  its alias differs from the ASR's, TTS. Each figure is the one admission
  charges (`VramScheduler::footprint`): a chat row's GGUF weights + KV at
  its context, an audio row's learned residency (§9.4) or its on-disk size
  before it has one; a candidate alias its primary, a ladder its base
  rung. A cloud stage is 0, a server lmgw forwards to but does not run is
  0 and labelled so, a model two stages share counts once, and a stage
  with no figure is listed unknown — the total is then a lower bound and
  the verdict says so. Against it: `vram.headroom_mb`, what lmgw may use
  (`budget_mb`, else the GPU's total) and what other programs hold now
  (the per-process share, when measured). Verdicts: `fits`, `tight` (fits
  what lmgw may use, not beside what others hold now), `too_large`,
  `unknown`. Arguments override the saved aliases, so the dashboard sizes
  its draft before it is saved.
- **`/v1/models` (as built).** When the session's ASR alias
  (`realtime.asr_alias`, else the Chat's, as §5.2 resolves it) and
  `realtime.tts_alias` are valid, every chat model lists `/v1/realtime` in
  its `capabilities.endpoints` (list and single model). The route is in
  the list-level `lmgw.endpoints.openai` either way: it exists. Since R3 a
  cloud model whose catalog states nothing is a chat model only when its
  name is no OpenAI speech or Realtime name (§20, R3 D1): OpenAI's TTS and
  transcription models list their own routes and never `/v1/realtime`, and
  `gpt-realtime*` lists none.
- **API docs (as built).** The `DocRoute` for `GET /v1/realtime` and the
  registry's WebSocket response kind came with WP1; WP8 rewrote its
  description to state the implemented subset (§19) and kept the browser
  note of §2.1. `realtime_budget` is documented as an op.

## 13. Dependencies and build

- **WebSocket.** axum's `ws` feature, which pulls in `tokio-tungstenite`.
  The byte-pipe proxy's `Cargo.toml` comment gets updated; the proxy itself
  stays as it is. `tokio-tungstenite` also serves as the test client.
- **ONNX Runtime.** `ort`, pinned exactly (`=2.0.0-rc.13`, ONNX Runtime
  1.28), CPU provider only. Silero gets `with_intra_threads(1)` per session.
  Smart Turn gets 4 threads (owner's decision: 18 ms a score
  against 48) with intra-op spinning off. Both use `ort`'s process-wide
  default environment. The pin moves Smart Turn's int8 output against 1.30
  by up to 0.27 on single pauses; re-run the corpus replay of §6.3 with any
  update.
  - **Linking:** `ort`'s build-time download of a prebuilt static library
    for now (owner's decision, §20), cached in CI. A **vendored** build via
    `ORT_LIB_PATH` is the long-term target.
  - **Packages:** the RPM and the AppImage ship ONNX Runtime's
    `ThirdPartyNotices`, its `LICENSE`, and the licences of Silero and Smart
    Turn under `/usr/share/licenses/lmgw/` (`onnxruntime/`, `silero-vad/`,
    `smart-turn/`), via `tauri.conf.json`. Next to Smart Turn's sits
    OpenAI Whisper's MIT licence (`smart-turn/LICENSE-whisper`, fix package
    B6): the model card names a Whisper Tiny encoder as Smart Turn's
    backbone. Its text is the standard MIT licence with Whisper's copyright
    line ("Copyright (c) 2022 OpenAI"), written rather than fetched.
    **Known leftover, not fixed:** the RPM that Tauri's bundler writes
    (rpm-rs) owns the files but not the three directories, so an uninstall
    leaves `/usr/share/licenses/lmgw/onnxruntime`, `…/silero-vad` and
    `…/smart-turn` behind, empty (A2 review 6, WP6 review). Harmless, and
    fixing it means a bundler change; the first real RPM build is still to
    be checked.
  - **Measured (WP0):**
    - **Download:** the prebuilt is 10 MB from `cdn.pyke.io` and unpacks to
      a 105 MB `.a` in `~/.cache/ort.pyke.io`. It is not fetched again on a
      clean rebuild, so CI caches that directory.
    - **Size:** each binary that links ORT grows by **+25 MB stripped / +30
      MB unstripped**.
    - **Build time:** +3.5 s wall on a clean build.
    - **Runtime libraries:** only `libstdc++`, `libgcc_s`, `libm` and `libc`;
      no `libonnxruntime.so`.
    - **glibc floor:** a hard 2.38 from the prebuilt. A host build on
      Fedora 44 binds 2.43, the same constraint today's AppImage already has.
  - **No feature gate.** The size is acceptable, because rust-cache stores
    dependencies only, not test binaries. ORT goes behind a cargo feature
    only if CI disk complains.
  - **API notes:**
    - `Session::run` takes `&mut self`, so there is one session per VAD
      stream.
    - Builder errors are not `Send` and need a `map_err`.
    - `ndarray` is not needed: `Tensor::from_array((shape, vec))` works.
- **Resampling.** `rubato` 5.0 (pure Rust; a new `audioadapter` API since
  0.16), used for 24↔16 kHz and the TTS models' native rates. WP0 proved it
  on 24, 44.1 and 48 kHz input with an async sinc resampler (length 256,
  BlackmanHarris2 window).
- **Model files, in git** (owner's decision), under
  `crates/lmgw-core/assets/realtime/` with their licence files, loaded with
  `include_bytes!`:

  | File | Licence | Size | sha256 |
  |---|---|---|---|
  | `silero_vad_16k_op15.onnx` (Silero VAD v6.2.3, commit `1e261b0`) | MIT | 1.3 MB | `7ed98ddb…1b2c49` |
  | `smart-turn-v3.2-cpu.onnx` (`pipecat-ai/smart-turn-v3`, int8) | BSD-2-Clause (Daily), `LICENSE-smart-turn`; its Whisper Tiny encoder MIT (OpenAI), `LICENSE-whisper` | 8.7 MB | `2bb02631…967e4f` |

  Provenance and full hashes: `assets/realtime/README.md`.
- **Smart Turn features.** Whisper's log-mel front end, reproduced in Rust
  (`realtime/turn/mel.rs`) on `realfft` 3, which is pure Rust and pulls in
  `rustfft` 6, given opt-level 3 in dev builds like `rubato`. A parity test
  holds it to the Python extractor's output on the committed clips, to one
  f32 ulp.

## 14. What was taken over from an earlier prototype

lmgw took a few small pieces from an earlier voice prototype and changed them
for this route: a VAD framer and level normalizer (loaded from bytes, with
the context prefix of WP0, and now tested); an endpointer with a blip-resume
guard, which here follows Realtime's `threshold`, `prefix_padding_ms` and
`silence_duration_ms` and keeps a pre-roll ring buffer; the barge-in evidence
accumulator, which here counts VAD evidence only, keyed to the playback
window, and gates `speech_started`; the trailing window after a barge-in
(`post_interrupt_silence_ms`); the clause splitter, verbatim at first, plus
the speakable pass; the rule that the history holds what was heard, now a
per-clause sample table instead of dispatch counting; and the fields of the
timing line (§11). The prototype's other features are client concerns or are
replaced by audio.cpp.

## 16. Testing

- **Protocol.** serde round-trips for every client and server event,
  against fixtures derived from the SDK type files. Every server event is
  checked for `event_id` and the delta id fields.
- **Session state machine, no audio.** A text session over a
  `tokio-tungstenite` client, against a streaming chat fake (the
  `support/llama_fake.rs` pattern). It covers:
  - `session.update` merging and resolution echoes;
  - the model-less handshake;
  - item ordering and rendering normalization (§7.2);
  - the active-response error;
  - cancel purging queued deltas;
  - a function-call round trip with the `in_progress`/`completed` status
    rule and session-unique `call_id`s;
  - the context-overflow error.
- **Golden sequences.** The exact event order of §2.3 for a voice turn, a
  function call and a barge-in, with ASR and TTS fakes returning fixed text
  and a fixed WAV.
- **Turn detection, offline.** Onset and offset times, the blip guard,
  backchannel dropping, barge-in evidence, Smart Turn complete/incomplete
  on labelled clips, the mel and probability parity tests, and the
  `semantic_vad` rule with injected scores (`turn/semantic/tests.rs`).
  `realtime_semantic` runs the rule end to end with a live microphone and
  the real models. A test seam (`AppState::set_turn_score_for_tests`) makes
  the scorer fail or answer a chosen p.
  - **Local only:** `realtime_turn_corpus` replays the owner's labelled
    pauses through the Rust rule (§6.3). It is ignored by default and
    gated on `LMGW_TURN_CORPUS`, and it prints numbers only. The
    recordings and anything derived from them never enter the repository.
  - Committed fixtures are synthetic: Piper's `en_US-ljspeech-medium` voice
    (public-domain dataset, trained from scratch) plus seeded noise, with
    the licence evidence beside them, in about 1 MB.
  - They are never recordings of people, and never voices with a
    restrictive derivation chain. The German Thorsten voice was rejected
    because it is fine-tuned from a Blizzard-licensed voice.
  - Silence and noisy mixes are synthesized in the test.
- **Pacing and heard audio.** The paced clock, and the `audio_end_ms` →
  character cut on a known clause table.
- **Route plumbing.** `route_walk` (plain GET → 426), `openapi_coverage`,
  `principal_gate` (subprotocol key only on this path, cross-origin
  anonymous refused), `key_policy` (refusal before the 101, per-call
  re-check).
- **Expressive speech** (WP10): `realtime/expressive/tests.rs` (precedence,
  the voice-design rule, `dropped`, the seed with a row pin, a client pin
  and a remote route, the hint, its sounds against `STAGE_DIRECTIONS`),
  `clauses/tests/tags.rs` (the cap never cuts a tag, a tag alone on a line,
  the speakable pass), the splitter's two texts and carry
  (`responder/speech/tests.rs`), the settings round trip; end to end
  `realtime_expressive` on the fake GPU world — a style on every clause from
  the level that sets it, a TTS that reads none, a designed voice's
  description and seed across clauses, responses and sessions, the refusal
  with no description, the hint and the tag-free transcript, a cloud TTS,
  a cloud fallback under the GPU hold — and `realtime_warm` (a voice that
  cannot speak is not warmed).
- **Delivery cues** (WP9b): `audio/cues/tests.rs` (the leading cue, joined
  and normalised, what is no cue — a tag inside, `[1]`, a link, over 31
  bytes — the cue after the style, the row's description as the base, the
  routes that take cues, the fold), `takes_cues`, `speech_hint_text` and
  `resolved().cues` per row kind (`realtime/expressive/tests.rs`),
  every clause ending in a stop (`clauses/tests.rs`), the scope through the
  splitter — a cue covering its own clause only, none for a tag inside, a
  clause after it with none, `!`, `?`, a closed clause and a tool call's
  flush, the carry (`responder/speech/tests.rs`), the dashboard's preview
  (`settings/realtime/style.rs`); end to end `realtime_cues` — a cue after
  the style for the sentence it opens, the transcript without it and the
  history with it; a row's own description as the base under both keys
  (R2); the cue hint, the hint off, OmniVoice's tags and a voice-design
  row's unchanged description; a cloud TTS nobody described and one whose
  override reads no instructions (no cue, the tag stripped), and one
  declared a style (the cue as its instructions); a local passthrough row
  without tags (Auk, the cue under `options.instruct`); a cloud fallback
  declared a style under the GPU hold, after a CustomVoice row's and a voice-design row's
  sent style, and the bare cue when those rows' own descriptions are not
  sent; a cue-only clause (a stage direction written as a word) riding the
  carry into the next clause, and a cue at the end of the answer.
- **Live, opt-in** (`LMGW_LIVE_REALTIME=1`, ignored by default):
  - the `openai` Python SDK's `client.realtime.connect`;
  - stock `@openai/agents` (`RealtimeSession` with only the URL changed);
  - Hugging Face's `talk` client, by hand.

  All three run from Podman containers against a running gateway.

## 17. Touch map

| Layer | Existing | New |
|---|---|---|
| deps | `Cargo.toml` axum features | `ws`; `ort` (pinned), `ndarray`, `rubato`; dev: `tokio-tungstenite` |
| route | `server.rs` `/v1` group, `CAPABILITY_TABLE` | `.route("/realtime", get(realtime::upgrade))`; table row |
| auth | `principal.rs` `resolve` | subprotocol key, path-scoped; anonymous cross-origin refusal |
| policy | `policy.rs` `check_alias`, `admit` | pre-101 checks for three aliases; per-call re-check; session-held slot |
| session | — | `realtime/{mod,protocol,session,conversation,render,responder,heard,clauses,pacing,resolve}.rs` |
| audio | — | `realtime/audio/{pcm,resample,vad}.rs`, `realtime/turn/{server_vad,semantic,barge_in,smart_turn,mel}.rs`, `realtime/scorer.rs`, `realtime/audio_in/ring16.rs`, `assets/realtime/` |
| chat | `proxy::stream_once_on` | `realtime/chat.rs` delta sink → clause splitter |
| ASR | `proxy/transcribe.rs` | an `Opened`-taking variant with the session's `RequestCtx` |
| TTS | `proxy/audio.rs` `audio_send` | `proxy/synthesize.rs`: clauses over one `Opened`, one row per response |
| vram | `check_background_start`, `peak.rs` | reused for warming; `vram/residency.rs` (learned audio footprint, pending loads, WP7); the image window's audio shape |
| telemetry | `ClientProto`, `RequestClass` | `realtime` variant |
| settings | `settings_classes.rs`, `api_settings.rs`, `ops/settings_patch.rs` | `RealtimeSettings` + DTO + patch |
| capabilities | `lmgw.endpoints` | `/v1/realtime` on chat models when configured |
| expressive (WP10) | `audio/tags.rs` (`spans`), `audio/profile.rs` + `families.rs` (`reads_seed`), `proxy/synthesize.rs` (`ClauseSpeech`, `sends_seed`, `refuse_route`) | `realtime/expressive.rs`, `realtime/clauses/tagged.rs`, `response.lmgw`, `resolved.speech`, `settings/realtime/style.rs` |
| delivery cues (WP9b) | `responder/speech.rs` (the splitter's cue, `Work::Clause.cue`), `proxy/synthesize.rs` (`ClauseSpeech.cue`, `clause_body`), `proxy/audio/speech.rs` (`rules_on`), `realtime/expressive.rs` (`takes_cues`, `hint`, `resolved.speech.cues`), api-types `realtime.rs` (`takes_cues`, `cue_hint_text`, `speech_hint_text`), `settings/realtime/style.rs` + `realtime.rs` (preview, label) | `audio/cues.rs`, `realtime/responder/speech/cue.rs`, `tests/it/realtime_cues.rs` |
| openapi | `planes/inference.rs`, `registry.rs` | `DocRoute`, WebSocket response kind |
| ui | Settings page | Realtime section |
| tests | `tests/it/*` | `realtime_protocol`, `realtime_session`, `realtime_turn`, `realtime_live`; `support/realtime_fakes.rs` |

New logic goes into child modules of `realtime/`, each under ~400 lines.

## 18. Work packages

| WP | Content | Verifies |
|---|---|---|
| **0 — spike** | `ort` pinned (sizes, cache); Silero with and without context; Smart Turn reference + parity fixtures; ASR/TTS/chat latency; TTS formats and streaming; SDK client captures; residency — **done 2026-10-01, §22** | §6, §8, §13 assumptions |
| **1 — transport + text sessions** | upgrade, auth (header, cookie, subprotocol, cross-origin rule), pre-101 policy, 426, beta refusal; protocol types; model resolution + echoes; session core and state rules; items and rendering normalization; streamed text responses via `stream_once_on`; client function tools; cancel with purge; per-call policy | `realtime_protocol`, `realtime_session`, route/openapi/policy tests |
| **1c — review fixes** | cooperative cancel with usage rows, closing events on cancel, early hold release, ping/write-stall close, `response_cancel_not_active`, Playing phase + writer drained-ack (prerequisite for WP3) | lifecycle tests, usage-row tests |
| **2 — audio in** | PCM decode, resampling, Silero, `server_vad` (+ `semantic_vad` mapped to it until WP6), manual commit, ASR per segment, empty-transcript rule, transcription events, background warm | `realtime_turn`, golden voice-turn sequence (fakes) |
| **3 — audio out** | clause splitter, speakable pass, `proxy/synthesize.rs`, voice resolution, pacing, heard table | golden sequence incl. audio; first live turn |
| **4 — barge-in** | playback-window gate on `speech_started`, cancel, truncate → heard cut, post-interrupt window; echo-reference experiment | barge-in sequence; live; echo measurements |
| **5 — residency + ops** | holds at point of use, one route per response, GPU-hold behaviour, usage rows + `realtime` label, timing line | hold tests with the GPU-world fake |
| **6 — `semantic_vad`** | Smart Turn, mel front end, eagerness table — **done 2026-10-01** (`ac6533b`, `3913cf7`): the hybrid rule, variant B, the 16 kHz ring, the engine switch | parity + labelled clips, `realtime_semantic`, the local corpus replay (§6.3); live A/B against `server_vad` still to run |
| **7 — audio footprint** | learned residency for audio rows, pending loads, the image window fix (§9.4) — **built 2026-10-01** (`be68d61`, `7e6dd2f`, `a8e1cba`); the live gate still to run | `vram_admission::audio_residency`, `realtime_tts_residency`, `audio_residency_live` |
| **8 — surfaces + docs** | settings DTO/patch/UI, `/v1/models` endpoints, `DocRoute`, README | `web_pages`, `openapi_coverage` |
| **9 — acceptance** | the three live clients of §16 against the dev box | recorded in this spec |
| **10 — expressive TTS** | speech instructions (session, response, setting, the row's own) by the TTS's mode, the voice-design refusal before `response.created`, a session seed for designed voices, the tag hint, two texts per clause and the tag carry, tags kept whole by the splitter and the speakable pass, `resolved.speech`, the settings and their dashboard rows, a `vdes` row as a session's TTS, no warm for a voice that cannot speak — **built 2026-10-02** | `realtime_expressive`, `realtime_warm`, `audio_expressive`, `audio_voicedesign`; live listening still the owner's (§20) |
| **10b — delivery cues** | a leading tag as the clause's delivery cue on a `style` or `passthrough` TTS that renders no tags, scoped to its sentence, appended to the style in effect, decided on the route that answers; the cue hint on `tag_hint`; `resolved.speech.cues`; the dashboard's preview and label — "WP9b" in the notes, since WP 9 here is acceptance — **built 2026-10-02** | `realtime_cues`, `audio/cues` and the splitter's scope tests; live listening still the owner's (§20) |

WP1 → WP2 → WP3 is the critical path to a spoken exchange. WP4 and WP5
follow it; WP6 and WP7 are independent. Work happens on branch
`feat/realtime`, merged when WP5 passes live.

## 19. Out of scope / follow-ups

- **WebRTC** (`/v1/realtime/calls`, `oai-events`, Opus), **SIP**, sideband
  and **ephemeral client secrets**. Browsers use the subprotocol key or the
  dashboard cookie.
- **The beta protocol.**
- **G.711** input and output (`audio/pcmu`, `audio/pcma`).
- **`transcription` sessions** (transcription-only, with live partials) and
  **live partial transcripts** in conversation sessions. Both need the
  duplex client for audio.cpp's `/live` route.
- **Out-of-band responses** (`conversation: "none"`, `input`) and
  concurrent responses.
- **`truncation: "auto"`** with visible `conversation.item.deleted`.
- **Speculative responses** (§6.7).
- **Streaming TTS** for rows in `mode: "streaming"`.
- **Language-aware TTS**, choosing the TTS alias or voice from the detected
  language.
- **Audio-native chat models** (`input_audio` straight to a model that
  hears; Hugging Face's `--stt none`).
- **Server-side tools**: `mcp` session tools, or the gateway's MCP toolsets
  via a live-relaying `agent::run`.
- **Spoken filler** while a client tool runs.
- **Usage columns** for characters and audio seconds, and **a session
  parent row** in `request_logs`.
- **A realtime block** in `lmgw__status`, and **a live-mic test page** in the
  dashboard (the Tauri webview's media-permission path needs checking
  first).
- `noise_reduction`, `idle_timeout_ms`, `rate_limits.updated`,
  `output_audio_buffer.*`, `input_image` content.
- **Passthrough** of `/v1/realtime` to a cloud Realtime upstream, and
  audio.cpp's conversational s2s model as an alternative backend.
- **Audio warm-up request** (after WP7): a session's warm could send half
  a second of silence to its ASR and one word to its TTS, so the models
  load before the first turn instead of during it (the 2.2 s cold first
  turn). The owner's call, together with `lazy: false` on voice rows.
- **Expressive speech, phase 2** (after WP10):
  - **Design presets**: `audio.speech_presets` (the audio class, so
    `/v1/audio/speech` gets them too), name → `{instructions, seed?,
    alias?}`, a name in the voice chain after rule 1 (§5.3). For a
    one-prompt TTS a preset is its style prompt.
  - **A voice-sample generator**: an interface (the Audio lab, for
    instance) that renders a sample with a model that can design or speak
    a voice and saves it to the voice library as a clip with its exact
    transcript, so a cloning row (Qwen3 Base, Fish, CosyVoice3) can speak
    it as a fixed voice. It makes the clip once, outside any session; it
    is not a per-session "freeze" — the owner ruled out a mid-request
    switch to another model (R7 in §20).
  - **A one-prompt TTS (Gemini style)**: realtime hands over semantic
    fields, and composition stays per route in `Expressive.field`; a
    `Prompt` variant would compose `"{instructions}\n\n{input}"` at the
    egress, declared by the alias override as
    `capabilities.speech.instructions: "prompt"`. Realtime never folds the
    style into `input` — it would reach the transcript, defeat tag
    stripping and double on routes that read instructions. Such a model
    reads bracketed directions as text, so its override says tags `free`
    and the hint follows. No protocol work.
  - ~~**Clause granularity per row**: sentences rather than the first
    comma for LLM-TTS prosody; measure first.~~ Measured and decided
    2026-10-05: a clause is a sentence for every row, no first comma and no
    word cap (§8.1).
- **Audio footprint follow-ups**: a measured prior from this box if
  first-use OOMs appear before a row has learned; charging nothing for a
  `backend: "cpu"` audio row; the image peak sampler on per-process
  figures.

## 20. Decisions

**Taken in this draft** (overturn any of them in review):

- **Where the cascade runs.** Inside lmgw, with no new model class or
  container. VAD and end-of-turn detection run in-process, because audio.cpp
  has no per-frame VAD and no turn model.
- **Protocol.** GA only, over WebSocket only.
- **Chat.** Streamed through `stream_once_on`, not `agent::run` (§3.2). v1
  tools are client-side only.
- **Model resolution.** Any alias; OpenAI model names map through
  `default_model` / `model_map` / `asr_alias`. Every fallback is echoed in
  `session.lmgw.resolved` and logged, never silent.
- **Output pacing.** Output is paced to real time plus a visible lead, and
  the done events wait for the end of playback (§8.2).
- **Holds.** Non-evicting background warm at connect and at
  `speech_started`. Real holds at the point of use (ASR per call, chat and
  TTS per response), with one route per response.
- **Per-call work.** Non-streaming TTS per clause (21–50 ms measured). ASR
  on committed segments, with no live partials in v1.
- **Policy.** Before the upgrade for all three aliases, again before every
  model call, and one concurrency slot per session.
- **Usage.** One TTS row per response, a `realtime` label, and no new
  columns in v1.
- **Turn detection.** `server_vad` by default when a client names none,
  as with OpenAI, and `semantic_vad` with Smart Turn when a client asks for
  it (§6.3, WP6). Barge-in is gated by an evidence accumulator over the
  *playback* window. Echo rejection is not claimed: measured in WP4, no
  cheap server-side check works, so barge-in needs the client's echo
  cancellation, and `half_duplex` serves clients without it (§6.4).
- **Reasoning.** Off by default for voice responses.
- **Context overflow.** An error, not auto-truncation, in v1.

**Decided by the owner (2026-10-01):**

1. **Model files in git.** Silero (~2 MB) and Smart Turn (8 MB) are
   committed with their licence files and loaded with `include_bytes!`.
2. **ONNX Runtime.** `ort`'s build-time download for now, pinned and cached
   in CI. The long-term goal is a **vendored** ONNX Runtime build
   (`ORT_LIB_PATH`).
3. **Voice profiles later.** The pipeline has to prove itself first. v1
   uses the settings defaults plus `session.lmgw`.
4. **Default turn detection** is decided with WP6's measurements. WP6 has
   them (§6.3). The session default when a client names none is still
   `server_vad`, OpenAI's own. Whether lmgw's becomes `semantic_vad` is
   open until the live A/B. The stock `@openai/agents` asks for
   `semantic_vad` itself, so it already gets Smart Turn.
5. **WP7** (learned audio footprint) is part of this feature. The
   adversarial review suggested splitting it out; the owner's call stands.

**Decided by the owner for barge-in (WP4, 2026-10-01):**

- **Q1 — `response.done` at the end of playback.** Yes: the drained
  acknowledgement, and with it the closing events and `response.done`,
  waits for the playing window's end, and the window's record outlives
  `response.done` (§4.3, §8.2). Every barge-in during playback has a
  response to cancel, and `@openai/agents` keeps its playback state to the
  real end. Cost: up to `output_lead_ms` before a follow-up starts.
- **Q2 — resume an interrupted answer when the barge-in turn has no
  words?** No, as with OpenAI: no response follows, and the log says why
  (§4.3). A response cut before the client heard any of it is not an
  interrupted answer: its turns are owed again.
- **Q3 — hold a client's `response.create` while the user speaks?** Yes, a
  deliberate departure from OpenAI: it starts once after the turn (§4.3).
- **Q4 — the echo-reference check.** Measured offline only, and not shipped
  (§6.4). `session.lmgw.half_duplex` (default off, also a setting) serves
  clients without echo cancellation instead.

**Decided by the owner: the barge-in word check (2026-10-01).** Yes. The
scripted round on the owner's own recordings measured voiced evidence of 320 ms
for "Stopp" and 384 ms for "Stop", against 544 ms for "Mhm" and 608 ms for
"Okay": at `barge_in_min_ms` 300 all 11 interruptions passed the gate, and
so did 8 of 9 backchannels, a cough (352 ms) and a laugh (416 ms). The
gate's evidence is now transcribed before the cut, and a backchannel or
nothing keeps the answer playing (§6.4); `barge_in_check: "duration"`
keeps the old rule.

**Taken while building WP4** (overturn any of them in review):

- **The window ends at the modelled playback end**, not at the send's end
  plus a full lead: the two agree at the steady state of pacing, and the
  latter overstated an answer shorter than the lead by up to the whole lead
  (§6.4).
- **The turn that interrupts** — started by the gate, carried over the
  window's end, or started at all while a response is in progress, with
  `interrupt_response` off too — ends on `post_interrupt_silence_ms` (§6.5).
  (Narrowed in B3: a response the client has heard some of. B4: not a
  carried turn, which starts after the answer played out.)
- **Half duplex** hears the window as silence: a turn open when it began
  runs out and commits what was said before (§6.4). (B3: it ends at the
  last frame before the window instead.)
- **What "reached the client" means** for Q2: an item was announced
  (superseded in fix package B3: what was *heard*, §4.3). A
  spoken message opens with its first clause, whose audio leaves at once
  with the lead.

**Taken in fix package B2** (overturn any of them in review):

- **The synthesis bound is lifted, not the answer cut**, when the GPU hold
  comes on, an admission waits for room or the client stops reading
  (§8.2, §9.1): the rest is synthesized at full speed and the TTS hold let
  go. (B4: not for a client that stops reading — the bound stays and the
  hold is let go until it reads.) "Eviction wanted" is the registry's draining mark — any owner
  admission waiting for room, not one that wants the TTS model in
  particular, which the VRAM manager cannot say cheaply. A stall is the
  playing window ending more than one delta (100 ms) before the next
  audio leaves; a cloud or fallback route keeps its bound.
- **Only `etc.` and `usw.` end a sentence before a capital**, not `bzw.`
  and `ca.` as the review suggested: German capitalizes the nouns that
  follow those two, so the rule would cut "Äpfel bzw. Birnen" in half
  (§8.1).
- **Fenced code is skipped in speech and in the spoken transcript**, with
  no placeholder (§8.1): the transcript is what was said. (B4: not in the
  model's history, which keeps it, §7.3.)
- **The stream's last clause is closed too**, not only one cut at a line
  end: it ends the answer, or the text before a tool call (§8.1).
- **Default instructions only for sessions that speak**, judged on the
  session's `output_modalities`, not per response (§12). A session's
  instructions that equal the default in effect count as the default — a
  client that sent exactly that text and then switches to text output loses
  it. (B4: wherever the text came from, the update that carries it
  included; a stored `""` from an older branch build is *off*, §12.)
- **A read voice list rules out every rule's voice**, not only the name
  the client sent: a `voice_map` or `default_voice` target the list does
  not show is `voice_not_found` too (§5.3). (B4: `voice_not_configured`,
  and said only by audio responses — it is the owner's setting.)
- **An audio.cpp fallback's chain keeps OpenAI-name-only rules for OpenAI
  names**: `default_voice` and the default preset stand in for a built-in
  name the fallback lacks, as in the primary chain, not for a custom name
  the client chose — that one, if the fallback lacks it and no mapping
  helps, is `voice_not_configured` (§5.3).

**Taken in fix package B3** (overturn any of them in review):

- **Heard is what left** (§4.3): audio or a transcript delta the writer
  released, or a text delta. A response cut before it was heard owes its
  turns again, and a client's create — the one queued behind it if there
  is one, else its own — is held to start again; an automatic response's
  own create is not, its turns are.
- **The window's margin is the ping round trip plus `echo_tail_ms`**
  (default 250 ms: wired or browser audio buffers both ways plus a room's
  reverberation; Bluetooth needs more), for membership only — the gate and
  half duplex. Played-out is still judged by the modelled end, where
  `response.done` goes. The session pings on the client's first frame, so
  the first answer already has a round trip (§6.4).
- **The socket has a reader task** that stamps frames as they come off;
  it reads ahead at most `max_frame_mb` of frames, so TCP flow control
  stays (§10.4).
- **The window stays open while audio waits in the writer**; the stall
  rule of the synthesis bound reads the same facts, unchanged (§6.4, §8.2).
- **The guard's start is the latest estimate any frame of the window
  gives**, since estimates are only ever late (§6.4).
- **A switch to manual turns closes the open turn** with `speech_stopped`;
  its audio is the manual buffer and commits as a new item (§6.6).
- **The word check's turn is held open by the detector** until its words
  are known; its "end of speech" is the turn's own silence window passing,
  not the gate's 0.8 s gap. Timeout 500 ms (§6.4). The older barge-in suites
  run on `barge_in_check: "duration"`; the word check has its own suite.
- **`N.` after a list item is a marker whatever its number** (nested
  lists), and a date never is; an unclosed code fence stays silent but is
  logged (§8.1).

**Taken in fix package B4** (overturn any of them in review):

- **The GPU hold's lift is tested end to end on a local TTS model** (B2
  review 9): the fake GPU world's containers answer as audio.cpp too
  (health, voice list, a second of speech per clause), so a session's TTS
  claim is the registry's. The stall's let-go and re-admission are tested
  on the real writer with a fake claim: stalling a real socket takes
  megabytes of unread audio.
- **No migration for a stored `"default_instructions": ""`** (B2 review
  M5): it means off, as an owner's empty setting does (§12).
- **`barge_in_min_ms` defaults to 200** (E7, the owner's measurements in
  §6.4); the unit suites keep their frame counts at an explicit 300.
- **A word of only h and m is a backchannel** (E6), by rule rather than a
  list of lengths.
- **The voice is resolved again before each response** when the settings
  snapshot changed (pointer identity of the snapshot, no version counter
  needed), and the read list is forgotten while it is not found or
  missing (B2 review 5, §5.3): an audio response then opens its route and
  fails at the first clause instead of before `response.created`.
- **A stalled client keeps the synthesis bound; its TTS model is let go and
  admitted again** (B2 review 3, §8.2), rather than the response cancelled
  after a stall time: `Synthesis` owns its claim, so dropping it and
  re-admitting the same route before the next clause is a small change,
  and it keeps a slow client's answer whole. Re-admission may queue or
  reload; under the GPU hold it is refused and the response fails with
  `gpu_hold`, with no fallback voice mid-answer.
- **A turn that starts after the answer played out is a reply** (B3 review
  E1): backchannel words when nothing plays, a gate turn still unconfirmed
  at its first frame past the window, and evidence carried over the
  window's end all make normal turns on the plain silence window. The last
  overturns WP4's "carried over the window's end ends on
  `post_interrupt_silence_ms`" (§6.5): by the time the carried turn
  starts, the client has played the whole answer.
- **Re-checks upload what is new**, with the pre-roll's length of overlap
  (E2), and the window bounds how long a turn stays unconfirmed.
- **The margin's round trip is the least of the last five** (`KEPT`, an
  algorithm constant named in §6.4 and the code), samples as long as the
  ping interval dropped (E3) — the ping interval, not a new cap.
- **Words in other scripts are dropped word by word** (E4), not only a
  transcript with no letter at all: "Mhm 嗯" is a backchannel too, and
  "Stopp 嗯" still cuts. Names are accepted as Unicode script names or
  ISO 15924 codes. The `language` is passed to the word check's ASR call
  only, as asked; committed turns still do not send it.
- **A mute discards a turn being checked** (E5), even with a check in
  flight — at the default 500 ms timeout its verdict is back long before
  the 0.8 s gap shows.
- **Not done (E5):** a cut response's own `response.create` is still
  dropped when a create is queued behind it. The queued one starts, with
  its own per-response parameters; restarting the original would re-run
  a tool call the client already ran, and keeping both needs a second
  held create in `pending` — a design change, not a nit.
- **The model's history is what it wrote, the transcript what was said**
  (§7.2, §7.3). After a cut, the clause the cut falls in is rendered as far
  as it was heard, cut by character as the transcript is, not whole — so
  the model is never told the user heard words they did not — with the
  code before it; the code before a clause nobody heard is left out with
  that clause.

**Taken in fix package B5** (overturn any of them in review):

- **A turn that cancels a response ignores that response's window** (H1,
  §6.4), rather than re-judging an append's frames after the turn's start:
  the window has to be left out after the cut as well, up to its margin,
  so the exemption had to outlive the append anyway.
- **Ordinals are written out only for 1 to 31**, the days of a date and
  the ordinals a voice answer uses; other recognised ones lose their dot
  (E2, §8.1). The sentence-start case reads "dritten Oktober ist ein
  Feiertag" — not the nominative, but never silent. *Narrowed in B6, below:
  dates only, every other "N." as written.*
- **`barge_in_check_alias` is a setting of its own** (W1, §6.4, §12),
  empty by default so nothing changes until the owner names one; the
  recommendation is qwen3 when the turns use nemotron.
- **The language goes up with every ASR call** of the session, the turns'
  too (B4 review), verified against audio.cpp's own usage text rather
  than a live call. A model that refuses a value fails visibly: a failed
  transcription, or the duration rule for a check. *Since B6 only a
  two-letter code goes up, below.*
- **Past the window a checked backchannel is discarded at its first
  silent frame** (M1), not at the end of its silence window; speech after
  it is a turn of its own, from the normal onset.
- **Re-checks start at a pause, else at the turn's start** (M2): with no
  pause in the overlap the upload grows again, from a word's start.
  *Bounded in B6, below.*
- **Numbers are words for the check** (M3), also under `["Latin"]`.
- **A top-level numbering gap is text** (B3 review L3): "1.", "2.", "4."
  speaks "4." — the price of keeping "42. Minute" and "100. Geburtstag".
- **Pings carry ids** (round trip), and a pong without the outstanding
  id measures nothing — a client that does not echo the payload never
  gets a round trip, and the margin is `echo_tail_ms` alone.
- **A TTS row deleted during a stall fails the response** (`NotFound`)
  instead of speaking on with no claim; a writer that is gone ends the
  speaker's wait as a stop. Neither has an end-to-end test: the first
  needs a stalled real socket, and the second is unreachable as wired —
  the speaker's own view keeps the writer's release channel open, so the
  session's stop is what ends it in practice.
- **Not held (L5):** an `@openai/agents` follow-up that meets the
  automatic response of a turn the word check let through gets
  `conversation_already_has_active_response` (§6.4); holding creates for
  unannounced turns would delay every follow-up behind a check.

**Decided by the owner for `semantic_vad` (WP6, 2026-10-01)**, from their
recordings (§6.3):

- **The hybrid rule.** Score once after 200 ms of silence. The threshold
  commits at once, the floor at the plain 500 ms window, and anything
  lower waits for the maximum wait. No score falls back to the plain
  window.
- **Variant B**: the score includes the 200 ms of silence.
- **The defaults:** high 0.5 / 0.1 / 2 s, medium and auto 0.5 / 0.2 / 4 s,
  low 0.95 / none / 3 s. These are not OpenAI's waits, and all of them
  are settings.
- **4 intra-op threads.** Scores come from the 16 kHz ring the Silero
  stream fills, with no second resample.
- **The model is committed** with its licence.

**Taken while building WP6** (overturn any of them in review):

- **One floor window for every eagerness** (`semantic_floor_window_ms`,
  500): the rule was measured at 500 for each row, and low has no middle
  band.
- **The fallback window is a column of the table** (`silence_duration_ms`,
  300 / 500 / 800, the old mapping). The same values serve the
  `server_vad` engine, so a pause with no score is `semantic_vad` as it
  was, and no constant stays hidden.
- **The echo is read-only** (`lmgw.resolved.semantic_vad`): no
  per-session override of the rule. `auto` is the `medium` row, as OpenAI
  defines it.
- **A turn the word check keeps open** is judged on the plain window
  without a score, and is scored once it is a normal turn. That leaves
  §6.4 exactly as it was.
- **Smart Turn's threads do not spin** after a run: one score per pause
  would otherwise keep a core busy between pauses, and energy counts.
- **A model that fails to load is not retried** within the session (its
  bytes are compiled in). Every pause then falls back, warned once.
- **A test seam** (`set_turn_score_for_tests`, per gateway) instead of a
  global: suites run in one process.

**Taken in fix package B6** (B5 and WP6 reviews; overturn any of them in
review):

- **Only a German date is written out** (the owner's decision, §8.1).
  Taken within it: the bare prepositions that take "-ten" are bis, ab,
  seit, vor, nach, für and gegen; a comma or a dash joins two dates as
  und, oder and bis do; after any other word — "Heute ist 3. Oktober",
  "Montag, 3. Oktober" — the date stays as written, so the pre-B5 risk of
  a stalled voice remains for that case rather than a guessed case.
- **The ASR is told a two-letter code or nothing** (the owner's decision,
  §6.4): "deu" and "german" are not mapped to "de" — no table of language
  names to keep.
- **A re-check's pause lookback is one pre-roll** (§6.4), the overlap's
  own length, rather than `barge_in_min_ms`: one setting bounds the upload
  (new audio and at most two pre-rolls).
- **A word-check alias must be an ASR alias** (§6.4): the key's scope is
  checked first, so an alias out of scope is not described; the owner's
  bad setting falls back to the session's ASR alias, and the echo shows
  `""`.
- **An unmeasured round trip is said once**, at INFO, after three pings
  (`SILENT_PINGS`, §6.4).
- **Smart Turn** (§6.3): request ids live on the detector; a span under
  400 ms (`MIN_SCORED_MS`) is not scored and falls back at DEBUG; a request
  a newer one overtook while it waited is answered without running; an
  answer during resumed voice commits at the next unvoiced frame (WP6
  review M2).
- **A stored `semantic_vad` row that cannot run** runs the built-in row
  *and* the built-in floor window: the floor window is one setting for all
  rows, so the row it breaks cannot keep it. The check a save must make is
  a function today; the save path is WP8 (§12).
- **Whisper's licence** is the standard MIT text with OpenAI's 2022
  copyright line, written, not fetched (§13).
- **The semantic suite's bound** is 200–1000 ms after the speech for both
  live-microphone tests, with the threshold read off the scores it records
  (§6.3).

**Taken in fix package B7** (WP7 review and its live gate; overturn any of
them in review):

- **A streamed audio answer counts at the end of its relay** (§9.4, M1),
  and only when the relay ended whole; a buffered one at its headers.
- **The image window's baseline must have seen the same shape** (M2):
  same containers, same audio models holding, no audio load in progress.
- **Holding is "answered within `idle_unload_ms`"** (M3), with no in-flight
  override: a reload is pending until it answers.
- **One reading at rest per ready container** (M4), on every path that
  makes one ready and regardless of the outside-VRAM trigger; pending is
  the expected residency less it. **`BARE_CONTEXT_CEILING` is 512 MiB**,
  plus half the row's on-disk size: a lazy container above that line at
  rest is taken as loaded. The figure is chosen from the review's double
  count (0.25–0.5 GB a context) and the smallest loaded container measured
  (768 MiB); the live test's check (d) pins it on the real containers.
- **Requests are sampled while they run** (the live gate), at the image
  sampler's 50 ms, until **`SETTLE_AFTER` = 3** sampled stretches in a row
  leave the figure where it was. The count lives in memory: a restart, a
  configuration change or a reset starts it again. The reading after each
  answer goes on after that, so a larger job still raises the figure.
- **The lows:** a reading's failure is in the notes and retried by the
  next answer (`Fill::Verdict`); the reset bumps an epoch under the store
  lock; readings settle tombstones; an eager row with no reading subtracts
  its selected GGUF (by `weight_id`, else the smallest); a container on a
  previous configuration is charged that configuration's stored figure.
- **Not taken:** a measured prior in place of the on-disk size before the
  first answer, and a warm-up request at warm time — both still the
  owner's call (WP7 decisions 1 and 2).
- **Beside it:** the self-admin plane gained `lmgw__audio_catalog` (list,
  refresh, download) and `lmgw__audio_model_set`, and `lmgw__local_model_get`
  reads audio rows — so the owner's new voice models can be added by an
  agent. `lmgw__audio_catalog` is one write-tier tool, as
  `lmgw__container` is, rather than a read tool plus a write tool. And a
  field named in `clear` is unset on create too, for every class.

**Taken in WP8** (the settings surface, §12; overturn any of them in
review):

- **One patch, two planes.** `RealtimeSettingsPatch` serves
  `settings_set_full` and `settings_set`. The self-admin tool's schema
  stays flat, so it takes `realtime` as a JSON-encoded string, hoisted
  before the patch is parsed as `ladder` is; `""` leaves the section
  alone.
- **Judged when touched.** A save checks the fields it changes: the
  `semantic_vad` table when the table or the floor window moves, an alias
  when that alias moves, the limits when either moves. A value a hand edit
  broke is the session start's to fall back from, and does not block an
  unrelated save.
- **More refusals than B6 named:** the ASR and TTS aliases by capability
  task, `default_model` by the session's own chat rule, a `model_map`
  target that is neither a chat nor an ASR alias, an unknown word-check
  script (as `session.update` refuses it), a `threshold` outside 0..1.
  **Not checked:** `default_voice` and `voice_map` — a voice belongs to
  whichever TTS model a session ends up on (`session.lmgw.tts_model` may
  differ), and a cloud model's voices cannot be listed.
- **`default_instructions` takes the Chat prompt's convention**: the
  built-in text saves as unset, `""` as none. No JSON `null` of its own.
- **The budget is an op, not a route**: `realtime_budget`, read-only, with
  no self-admin tool. Its arguments override the saved aliases, so the
  dashboard sizes its draft.
- **How a stage is placed.** A managed container is local. A route to an
  upstream lmgw does not run is `cloud`, or `external` when its kind is
  llama-server / audio.cpp or its base URL is loopback — 0 either way,
  since such memory is outside lmgw's plan and shows up as other programs'.
  A candidate alias is sized by its primary, a ladder by its base rung,
  and a model two stages name counts once. The word check is a stage only
  with `barge_in_check: words` and an alias other than the ASR's.
- **One headroom on the sum**, as the last of the cascade's models to
  start needs it free; `tight` compares with what other programs hold now,
  when the per-process share is measured.
- **`/v1/models`:** the route goes into chat models'
  `capabilities.endpoints` — there is no per-model `lmgw` block — once the
  two aliases are valid ASR and TTS aliases, not merely set; it is in the
  list-level `lmgw.endpoints.openai` either way.
- **Dashboard:** GiB in the GPU memory card, as the server's notes; the
  voice list is asked only for a local TTS row; the two word lists are
  comma-separated boxes rather than one line per word.
- **Beside it:** `cargo clippy --workspace --all-targets -- -D warnings`
  fails on the current toolchain (rust 1.98) in code WP8 does not touch
  (`quickdoc-core/src/vector.rs`, `result_large_err` in several core
  files); `ci/check.sh` runs clippy as advisory for that reason. WP8's
  files are clean.

**Taken in WP10 (expressive TTS)** (its design's decisions D1–D14,
accepted as written; §5.3, §5.4, §5.5, §7.2, §7.3, §8.1, §8.2, §12;
overturn any of them in review):

- **D1 The style is lmgw's own knob**, never derived from
  `session.instructions` (the chat prompt, which on a voice-design row would
  design a voice from it).
- **D2 Names:** `session.lmgw.speech_instructions` (`""` none, absent or
  `null` the owner's), `session.lmgw.speech_seed`, `session.lmgw.tag_hint`,
  and `response.lmgw {speech_instructions}`, a new strict object — named
  after `/v1/audio/speech` and `capabilities.speech`, not "voice
  instructions" (the chat voice prompt).
- **D3 Echo:** `speech_instructions` and `speech_seed` only as the client
  set them, `tag_hint` the setting in effect; what is in effect in
  `resolved.speech`. No "equal to the default" trick.
- **D4 Precedence:** response, session, setting, the row's description;
  the setting stands back on a voice-design row that describes itself.
- **D5 Per mode:** `none` dropped and said once per resolution; `style`
  and `passthrough` sent with every clause; `voice_design` designs from it,
  and with nothing from any source an audio response is refused before
  `response.created` (`instructions_required`). R1's open-time check takes
  the instructions the clauses carry.
- **D6 Seed:** a random u32 per session, or the client's, sent where the
  final row reads one — when pinned by the client, or on a voice-design row
  without a `seed` of its own; decided in `Synthesis::speak`.
- **D7 Hint:** after the instructions, audio responses only, for a TTS that
  renders tags, on by default; the non-verbal sounds of a fixed
  vocabulary, else the whole vocabulary; English.
- **D8 One tag grammar** (`audio/tags.rs` `spans`): the splitter counts no
  tag words (and never cut a tag at the word cap, until the cap went,
  2026-10-05); the speakable pass hides tags after the link pass and
  restores them canonical.
- **D9 Two texts:** `tts` keeps tags, `said` (the transcript) does not; the
  history keeps them.
- **D10 Tag-only clauses** are carried to the next clause's words; one at
  the end is not voiced and stays in the history.
- **D11** Heard maths unchanged. **D12** Hint and echo from the primary, a
  fallback shaped by its own rules, the first clause's losses logged once
  per response. **D13** `ClauseSpeech`, instructions per clause. **D14**
  `realtime.speech_instructions` (no built-in text) and `realtime.tag_hint`
  (on), types only, no length cap.
- **The reversible calls of the design, accepted:** `tag_hint` on by
  default; the hint lists non-verbal sounds only; the owner's style stands
  back on a voice-design row with its own description; a random seed per
  session on voice-design rows (the voice holds within a session and differs
  across sessions unless pinned). **Delivery cues (WP9b) are not in WP10**:
  decided after the owner's probe and listening check — built as WP10b
  (below).
- **Beside the design (WP10's own calls):**
  - *One task list.* `vdes` rows are a session's TTS like `tts` rows:
    `lmgw_api_types::realtime::SPEECH_TASKS`, read by
    `capabilities::speech::speaks`, the session, the setting's save check,
    `/v1/models`' advertising and the dashboard's picker.
  - *One placeholder.* Every hidden tag shares U+E000 and they are restored
    in order — the design's U+E000+n would have capped a clause at the
    6,400 code points of the private-use area.
  - *`pause` is no sound:* the hint's list (`HINT_SOUNDS`) is the
    `STAGE_DIRECTIONS` targets but `pause`, and a test keeps them so.
  - *A dropped style still travels.* On a `none` primary the text is passed
    to `Synthesis::speak` and shaped away on that route — so a fallback
    that reads instructions still gets it; the first-clause log leaves out
    the drop the resolution already said.
  - *`""` stops the chain,* and what is in effect is then the row's own
    description (`source: row`), which its engine merges whatever lmgw
    sends. Texts are trimmed; a blank one is none. The setting is trimmed
    on save.
  - *The transcript after a leading tag* loses the punctuation that went
    with it ("[laughs], ja." is said "ja.").
  - *The free-vocabulary hint* ends "Never put anything else in square
    brackets."
  - *The warm* judges the TTS on the session's own style, else the
    setting's — a response's own is not known yet.
  - *R1's open-time refusal stays* as the backstop for facts the session
    read before the row changed; R1's realtime test now sees D5's refusal
    before `response.created`, so no TTS row is written for it (R2 adds a
    test that reaches the backstop).
  - *Dashboard:* the style box sits in the cascade card under the voice,
    the hint checkbox in the prompt card under the voice prompt.
- **For the owner, live:** whether a style is heard on Qwen3 CustomVoice,
  whether the session seed holds a designed voice (it fixes the RNG, not
  the timbre), and whether tags placed by the hint are laughed rather than
  read — the design's probes (a) to (e).
- **R2 (review fixes):**
  - *A text over a row's own description* failed every clause: audio.cpp
    makes `instructions` the engine's `options.instruction`, merges the
    row's `instruct` default beside it, and Qwen3 (both variants), the MOSS
    families and dots throw "conflicting option values" — so a session's
    description on a voice-design row described under `instruct` (the key
    lmgw's own `instructions_required` message recommends), or the owner's
    style on a CustomVoice row with one, broke after admission. WP10's
    premise that Qwen3 reads `instruction` before `instruct` was wrong.
    Shaping (`audio::shape`, both `/v1/audio/speech` and realtime) now
    writes the text under the row's key too and says so in
    `x-lmgw-speech`. The fake audio.cpp containers of the test worlds
    (`support/audiocpp_options.rs`) read each model's mounted
    `server.json` and refuse such a request as the engine does, so the
    class cannot hide behind a fake that speaks anything.
  - *A voice-design TTS needs no voice.* On default settings (no
    `realtime.default_voice`, no default preset on the row, no voice from
    the client) the session's `marin` resolved to nothing, and every audio
    response of a described voice-design TTS was refused
    `voice_not_configured` — the voice check runs before the description's.
    Rule 3 of the chain now ends, for a TTS whose primary designs its
    voice, in no voice sent, echoed as `resolved.voice: "designed"`
    (`VoiceVia::Designed`, its own knob in the log line rather than
    "the TTS row's default_voice_preset"). A voice the client names, or
    the owner's `default_voice` or default preset, is still sent; a fallback
    resolves its own voice as before.
  - *The open-time backstop has a realtime test again*
    (`audio_voicedesign.rs`): a voice-design row that describes itself
    passes the response's snapshot, the owner clears its default while the
    chat model writes, and the route the first clause opens is refused
    `instructions_required` before admission — nothing started, a failed
    response, its TTS row. That window is the only realtime path to it: a
    fallback is never a local row (`Snapshot::usable_fallback`), so it never
    has a description to lack, and between responses the refreshed facts
    refuse before `response.created`. The backstop stays for that window.

**R3 (live run 3)** — the fixes from the third live acceptance
(2026-10-02, `target/realtime-live/RUN3-RESULTS.md`), one commit each:

- *D1 + N6, cloud audio aliases are stages.* OpenAI's catalog states no
  modalities, so every entry was `chat`: `openai/gpt-4o-mini-tts` was
  refused as a session's TTS (`unknown_alias`), `…-transcribe` as its ASR,
  and both advertised the chat routes and `/v1/realtime`. An
  OpenAI-protocol entry with no stated output modalities now takes its
  task from OpenAI's naming — `tts`, `asr`, or `realtime` for
  `gpt-realtime*`, which lists no routes and is never a chat model — in one
  place (`capabilities::task`) that the session's TTS and ASR checks, the
  settings' save checks and `/v1/models` all read. An owner's override
  task still wins, and now brings that task's routes unless it names its
  own (N6). An alias of the wrong task is refused `not_a_tts_alias` /
  `not_an_asr_alias`, naming its task. *Left:* `realtime::resolve`'s chat
  check is still the gate's (any non-media route), so a session can still
  name a cloud TTS as its chat model and fail at its first response.
- *D2, a cloud OpenAI TTS speaks OpenAI's names.* A remote alias has no
  voice facts, so `alloy` on the primary went to rule 3: with no
  `default_voice` the session's audio was refused `voice_not_configured`,
  with the owner's local `alba` that name would have gone to OpenAI. The
  fallback path already spoke the requested OpenAI name; the primary now
  does too: an OpenAI TTS's facts know the built-in names (rule 1), so the
  name is sent as asked (lowercase), verified. Other names are sent
  unverified as before, and the engine judges them.
- *D3, the word check's model is warmed.* The session warmed chat, ASR and
  TTS only; the first barge of the run found the check's container still
  loading, its check timed out, and the duration rule decided — a
  backchannel then would have cut the answer. With `barge_in_check: words`
  and a check alias other than the session's ASR alias, the check's model
  is warmed with the others, under the same never-evict rule (as the
  budget already counted it).
- *D4, inline enumerations.* "… weiter. 1. Wasser holen. 2. Brot kaufen."
  made "1.", "2.", "3." clauses of their own (spoken "Eins." with a gap):
  a bare number after a sentence end, before a capital, is a sentence end
  by the ordinal rule. Such a number that starts or continues the
  numbering is now the next item's marker, left out of it as a
  line-start marker is (§8.1); dates and the B6 rules are untouched.
- *N2, a designing TTS passes over the owner's `default_voice`.* The
  owner's `default_voice` (Pocket's `alba`) broke every session that
  switched to a voice-design TTS: rule 3 sent `alba`, the row's list did
  not have it, and the response was refused `voice_not_configured`. A TTS
  that designs its voice now skips a `default_voice` it is not known to
  have — not a preset, not a library clip, not in its list — and goes on
  to its default preset, then `designed`. One it has is still sent.
- *Voice-optional and reference-only families (4d blocker, N4).* OmniVoice
  speaks without any voice on `/v1/audio/speech`, but realtime refused a
  non-design TTS with nothing configured (`voice_not_configured`); and
  CosyVoice3 without a voice passed preflight, started its container and
  answered 500 "requires reference audio". The profile now records what
  each engine does with no voice, read off audio.cpp's source
  (`audio::families::unvoiced`): OmniVoice, MagpieTTS, Supertonic and
  Kokoro speak with a voice of their own (the last two only when the
  package ships their default), CosyVoice3, Chatterbox, IndexTTS2, MioTTS
  and Qwen3-TTS Base need reference audio. Realtime resolves the first
  kind, with nothing configured or a `default_voice` they lack, to no
  voice, echoed `engine_default`, like `designed`; preflight refuses the
  second with no voice or clip (`reference_required`) on the speech route
  and per clause, and realtime says `voice_not_configured` before
  `response.created` when its only voice is an inline preset without a
  clip. *Not confirmed, left unknown:* Qwen3-TTS CustomVoice has no
  default speaker (`prompt_tts_custom_voice.cpp` refuses an empty one).

**R4 (review of R3, live run 3b)** — the fixes from R3's review and the
re-check (2026-10-02, `target/realtime-live/RUN3B-RESULTS.md`), one commit
each:

- *M1, OmniVoice's own voice is held by the session's seed.* R3 sent
  OmniVoice no voice, and realtime sends one request per clause: with no
  reference the engine samples its speaker from a sampler seeded by
  `std::random_device` when the model loads (`omnivoice/generator.cpp`
  `Impl::rng`), so every clause could speak another speaker. A request's
  `seed` reaches that sampler — audio.cpp maps the body's `seed` for every
  family (`app/server/runtime.cpp` `build_speech_request`), OmniVoice
  parses it (`omnivoice/session.cpp` `generation_options_from_options`)
  and reseeds the generator with it at the start of each request (`run()`,
  `initialize_streaming_request`: `seed_rng`), and that sampler is the
  engine's only randomness. So OmniVoice joins the family table's
  `reads_seed` (its spec declares no options), and its no-voice behaviour
  is its own kind, `Unvoiced::DrawsSpeaker` — still `engine_default`, but
  its voice comes from the seed like a designed one: every clause carries
  the session's seed unless the row pins its own, echoed in
  `resolved.speech.seed`. MagpieTTS, Kokoro and Supertonic keep their
  fixed fallback speaker and get no seed. As for a designed voice, a seed
  fixes the RNG, not the timbre — whether one speaker holds across
  different clause texts is the owner's live probe; "freeze" (§19) is the
  cure if it does not (superseded by R7: it does not hold, and no freeze
  is planned). `/v1/audio/speech` is unchanged.
- *M2, a bare number after a sentence end is never deleted.* R3's D4 left
  an inline "N." out of the item it opened when it started or continued
  the numbering. But the ordinal rule hands it on for any capitalised
  non-month word, every German noun is one, and "1." always starts a
  numbering — so "Das ist klar. 1. FC Köln ist abgestiegen." lost its
  "1." in audio and transcript, as would "1. Bundesliga", "1. Advent" and
  "Wie viele? 1. Das reicht.". The number now goes with the clause it
  opens (no skip, no cut): kept, said, in the transcript, and run 3b's
  inline list still makes no bare-number clause. The speakable pass,
  which re-judges a clause's first line as a line start, would have
  dropped it there too: a clause now knows whether it begins at a line
  start of the stream (`Placed::line_start`), and one that begins
  mid-line has no numbered marker first in it (`speakable_at`). Whether
  "1. Wasser holen." reads well is the owner's ear.
- *M3, an owner `{task}` over unreadable capabilities.* Over a model
  whose capabilities could not be derived (`None`), R3's N6 inserted the
  task's `endpoints`, but `ModelCapabilities.source` has no serde default:
  the merge failed "missing field `source`", the failure became a note,
  the capabilities stayed `None`, and realtime and its settings refused
  the alias as no TTS/ASR. `source` is filled in when the merged object
  lacks it (it is set to `owner` on success anyway), so a task alone is a
  whole object, as N6 meant.
- *D3', the warm loads, not only starts.* With `audio.lazy_load` on (the
  owner's setting; no row overrides it), audio.cpp loads a row's weights
  on its first request, and R3's warm returned once the container
  answered its port: ~5 s after a warm, nemotron and qwen3-asr held no GPU
  memory, the first barge's word check loaded the 3 GB model (~2.5 s) and
  missed its 500 ms, and the first turn's transcript took 1.2 s (0.1 s
  warm). audio.cpp has a load route, but `POST /v1/models/load` answers
  403 without `--ui-management` (`app/server/runtime.cpp`
  `handle_model_load`), and lmgw keeps that off. So the warm now sends a
  lazy audio row whose container has not loaded its model the smallest
  real request (§9.1 "Warm means loaded"), on an owner's join of the
  running container — never a start or an eviction — through the audio
  routes' own sends, so the claim, the pending charge and residency
  learning work as for any request, and with no request row. The TTS's
  clause is built by the same function as a response's
  (`synthesize::clause_body`). A container left up by an earlier session
  but never asked anything is loaded by the next session's warm too.
- *D5, a reference-only TTS passes over a `default_voice` that is no
  clip.* With the owner's `default_voice` (Pocket's `alba`) and a session
  switched to CosyVoice3 with no voice, rule 3 handed `alba` on
  provisionally ("not verified yet"): `response.created` went out, the
  container started (1.8 s), and only then was the response failed
  `voice_not_configured`. R3 skipped an unknown `default_voice` for
  designing and engine-default rows, not for reference-only ones — yet
  their only voices that can work are known without the container: a
  voice-library clip, or a preset with a `voice_ref`
  (`VoiceFacts::clip_presets`). Any other `default_voice` is now skipped
  for such a row: the chain goes on to the row's default preset, and with
  none the session's voice is missing — refused before `response.created`
  with nothing started, the message naming `realtime.default_voice` and
  what it would take.

**R5 (live run 3c)** — the fixes from the R4 re-check (2026-10-02,
`target/realtime-live/RUN3C-RESULTS.md`) and R4's review, one commit each:

- *F1, OmniVoice needs a voice in a session.* R4 M1's seed reached the
  engine — the same seed and clause text gave bit-identical audio across
  sessions — but it fixes each clause's draw, not the speaker: the
  clause's text picks it, and one answer under seed 777 spoke at a median
  F0 of 222, 118 and 222 Hz, as wide as R3's unseeded clauses. A voice
  that changes mid-answer is worse than a clear refusal. So in a session
  `Unvoiced::DrawsSpeaker` no longer resolves to `engine_default`: it
  takes a `default_voice` only when it knows it, and with no voice
  configured — the client's, such a `default_voice`, the row's default
  preset — the voice is missing, refused `voice_not_configured` before
  `response.created` with nothing started, the message saying the engine
  draws a new speaker per request and to configure a voice clip or
  preset. MagpieTTS, Kokoro and Supertonic keep `engine_default`: their
  speaker is fixed. With a voice, OmniVoice's clauses still carry the
  session's seed (harmless, echoed). `/v1/audio/speech` is unchanged: one
  request, one speaker. The cure that would let it speak unnamed is a
  per-session "freeze" (§19), not built (superseded by R7: not planned).
- *F2, an inline list's number is sent without its dot.* R4 M2 kept the
  bare "N." with the clause it opens, said and in the transcript, and the
  cuts held: no bare-number clause, 390–420 ms between clauses. But Pocket
  read the dot in " 2. Brot kaufen." and " 3. Nach Hause gehen." as a
  sentence end and paused about 450 and 650 ms after "zwei"/"drei" (about
  150 ms after "eins"). The splitter now marks such a clause
  (`Placed::inline_item`, set where it decides the number is an inline
  marker, so a date, an ordinal or "der 1. und 2. Platz" never is), and
  only its TTS text loses the dot: "2 Brot kaufen.", "1 FC Köln ist
  abgestiegen.". A bare number is the one rendering with no pause for
  either reading; the transcript, the item and the history keep "2.".
  Whether it reads well is the owner's ear.
- *R4 review (MEDIUM), D5 refused a working default voice when
  `voice_dir` is not lmgw's library.* D5's clip check looked only at the
  library and the row's clip presets. The library is read only when the
  class's `voice_dir` is `/models/voices`; with a voice dir mounted for
  the engine alone it is `None`, so `default_voice = "anna"` with
  `anna.wav` in that dir was skipped, every audio response was refused,
  and the message said it was no clip. Now the clip rule — and F1's
  known-voice rule for OmniVoice — applies only while lmgw sees the
  engine's voices (the library, or the model's list once a response has
  read it), and a name the list shows counts as a clip. Otherwise
  `default_voice` is sent provisionally, as to any model: the first
  clause reads the engine's list, which shows the mounted clips. A preset
  of the row that loads no clip is never taken. The refusal names only
  what lmgw looked at.

**R6 (R5 review)** — one commit:

- *MEDIUM, the list never decided where lmgw cannot list the voices.* R5
  made D5's clip rule and F1's known-voice rule wait until lmgw sees the
  engine's voices: the library, or a list a response read. But before each
  response `refresh_voice` forgets the list while the voice is missing, so
  it is read afresh (B2 review 5), and the voices were unseen again. On a
  CosyVoice3 or OmniVoice row with its voice dir mounted for the engine,
  `default_voice = "alba"` went out provisionally on every response:
  `response.created`, the TTS admitted (a cold start, maybe an eviction),
  refused at the first clause. `VoiceFacts::seen` now keeps the list when
  `listed` is forgotten, and the two rules decide by it: the first
  response fails after admission, the next is refused before
  `response.created` with nothing started. It is dropped with another TTS
  alias, or when `voice::list_key` changes (`default_voice`, the class's
  `voice_dir`, the row's `resident_key`). Not with every new snapshot:
  one is published for a learned residency right after a TTS container's
  first answer, an image peak or a benchmark lease, and each would have
  reopened the hole for the next response. The trade-off: a clip added to
  a mounted dir is not seen by an open session until one of those changes
  or the session reconnects, as the library is re-read only by a
  `session.update`.
- *No `voice_dir` was a blind spot.* lmgw's own list is then the model's
  whole one (`Synthesis::voice_names` asks the engine nothing), yet the
  facts said "library unread", so the same loop ran there. They now carry
  an empty library (also for a library `voice_dir` without a models dir):
  the engine answers to no clip, so a `default_voice` it does not have is
  refused before anything starts, the message saying the library has
  none or the `voice_dir` is empty.
- *Tests.* R5's unit test asserted "missing before anything starts" on a
  state a session never reaches at a response start (list read, library
  unread). It now walks the read and the forget. Two session tests on the
  GPU world: with a mounted dir, the first response fails after admission
  and the second (after an unrelated snapshot) before `response.created`,
  one container start; the owner's `default_voice` change is read afresh,
  and a listed clip is spoken. With no `voice_dir`, CosyVoice3 and
  OmniVoice are refused at the first response.

**R7 (owner review, 2026-10-02)** — the owner's listening results, the
decision on them, and the package that closed the branch's review notes:

- *Voice-design rows drift.* Within one answer the clauses spoke
  different speakers, and two sessions with the same seed spoke different
  speakers again: every one fits the description, none is the same voice.
  The per-session seed (R4 M1, §5.5) does not hold a designed voice.
- *OmniVoice without a voice switches speaker* (female to male), which
  confirms R5 F1: a session refuses it with no voice configured, as
  built.
- *Pocket TTS in German* misreads numbers and list bullets and sounds poor
  in German. For German the owner recommends Supertonic.
- *Decided (the owner, 2026-10-02):* per-clause speaker drift — a
  voice-design row, OmniVoice without a voice — is the model's own
  behaviour. A model that needs a switch to another model to hold its
  voice is not suitable for per-clause realtime speech, and lmgw will not
  add a mid-request model switch. The "freeze" that R4 M1 and R5 F1 named
  as the cure is not planned; §19 has a voice-sample generator instead,
  which makes a fixed clip for cloning rows outside any session.
  Voice-design rows behave as before.
- *Built, one commit each:* the dashboard's Settings save judges only what
  it changes, as the server does — a value a hand edit broke is a warning
  at its field and blocks no other change (§12, WP8 review); an agent's
  service container and a benchmark's container retry a host port taken
  before podman bound it, once, as a model's start does
  (`registry::PortRetry`); an empty turn the word check heard words in is
  transcribed again with the check's alias (§6.4, N3).

**Taken in WP10b (delivery cues; "WP9b" in the notes)** — approved by the
owner after probe 4e, where Qwen3 CustomVoice `ryan` with the instructions
"laughing while speaking" laughed (4.32 s against 3.52 s neutral). The
design's decisions C1–C8, accepted as written (§5.4, §5.5, §7.2, §7.3,
§8.1, §8.2, §12):

- **C1 Which rows:** one predicate in lmgw-api-types,
  `takes_cues(instructions, inline_tags, tags)`, so core and dashboard
  agree: a `style` or `passthrough` TTS that renders no tags (`none`, or
  `fixed` with an empty list, the hint's own condition). Cues: Qwen3
  CustomVoice, local passthrough rows without tags (Auk, MOSS-TTS
  v1.5/TTSD, Qwen3 of unknown variant). *Changed 2026-10-02:* a cloud
  alias nobody described no longer takes them — `instructions` is
  `Option`, `None` when nothing declares it — since gpt-4o-mini-tts
  ignored every cue phrasing tried; its style still passes, and the
  override `capabilities.speech.instructions: "style"` turns cues on for
  one alias. Tags, unchanged: OmniVoice and CosyVoice3 — on a TTS that does
  both, tags win. None on `voice_design` (a cue in the description would
  make a new voice every clause, and R7 ruled switches out), none on
  `none`.
- **C2 Grammar:** a cue is a WP10 tag in a new position — every tag at the
  very start of a clause's `tts`, after the carry; free text, no closed
  list; several join with ", ", `_` and `-` are spaces, the punctuation
  after the cue goes with it. The route decides cue or sound, not the
  spelling. Literal brackets are unchanged; the 31-byte limit is the
  grammar's, and a longer bracket is read out, visibly. At a clause start
  only. English.
- **C3 Scope:** a cue covers the clause it opens, which is its sentence;
  nothing outlives the sentence, a tool call's flush or the response.
  *Changed 2026-10-05* from "until the sentence ends"
  (`Placed::ends_sentence`, a new cue replacing the one in effect): with
  no first-comma cut and no word cap every clause ends a sentence or a
  line (§8.1), so the scope is the clause.
- **C4 With the style:** `"{base}, but {cue} right now"`, or the cue alone
  with no base — appended, never a replacement, last as the most specific
  (every clause is its own request); the base's closing punctuation goes
  first. *Changed 2026-10-02* from `"{base}; {cue}"`: with Qwen3
  CustomVoice `"{style}; {cue}"` muted the laugh, `"{style}, but {cue}
  right now"` laughed in 4 of 8 renders against 1 of 8. The base is the style in effect: the clause's
  instructions, else the answering row's own description, which a sent
  text would otherwise replace under both keys (R2).
- **C5 Decided on the route that answers:** the splitter works out the
  cue, `ClauseSpeech.cue` carries it, and `clause_body` folds it only on a
  route that takes cues (`rules_on`, shared with `shape_on`), before the
  preflight and shaping. Per response, primary or fallback: a cloud
  fallback declared a style keeps the cues under the GPU hold; one nobody
  described gets the style alone (2026-10-02); a voice-design route never
  gets one. No extra request. `capabilities.speech.instructions: "style"`
  on a cloud alias turns cues on, `"none"` keeps them and the style off.
  `POST /v1/audio/speech` is untouched.
- **C6 Hint and switch:** `tag_hint` is reused — a TTS takes tags or cues,
  never both, so one switch covers both; no new setting, patch, DTO or MCP
  field. `speech_hint_text` gives the tag text when tags render, else the
  cue text, else none; core and the dashboard's preview call it. Cues
  apply with the hint off.
- **C7 Echo and log:** `resolved.speech.cues` says whether the primary
  takes cues; the text stays in `resolved.speech.tag_hint`. One DEBUG line
  per cued clause (session, route, cue, instructions sent); no INFO line — a cue is
  no loss, D12 stays losses-only.
- **C8 Transcript, history, heard:** WP10's two texts, unchanged — `said`
  has no tag, `Written.own` keeps the cue, a cue alone on a line opens the
  next line's clause, a cue-only clause (a stage direction written as a
  word) rides the carry, a cue at the stream's end is not voiced and stays in
  the history,
  and a barge-in inside a cued clause drops its cue from the history as a
  tag's.
- **Beside the design (WP10b's own calls):**
  - *`Cut::Stop`.* The splitter's cut kinds gained a sentence end of their
    own, which replaced the `stop` flag `cut` took for the inline-list
    rule; `ends_sentence` ("not cut mid-sentence") and `Cut::Open` went
    with the first-comma cut and the word cap (2026-10-05).
  - *A link's text is no cue* (`[see](url)` at a clause start): the link
    pass removes links before the TTS text exists, so this only guards
    `lead` itself.
- **Review (2026-10-02, seven LOW, all fixed):** the end-to-end carry test
  now has a real cue-only clause (L1; a cue alone on a line never needed
  the carry); the base's closing punctuation goes before `"; {cue}"` (L2);
  the cue DEBUG line and the TTS summary line name the session (L3); the
  summary counts the characters sent (L4); the hint asks for one or two
  words, which fit the 31 bytes (L5); tests for a row's own description
  under the hold, the `instructions: "none"` override and Auk (L6); the
  MCP, OpenAPI and README wording (L7).
- **For the owner** (three reversible calls): the cue hint is on by
  default, since it shares `tag_hint` (cloud aliases without an override
  no longer take cues, since 2026-10-02); local passthrough rows without tags
  (Auk, MOSS-TTS) take cues untested — the live check covers only Qwen3 and
  OpenAI; cues are English and last one sentence.

## 21. Changes from v1 (adversarial review, 2026-10-01)

- **Blocker, fixed:** `agent::run` buffers each model turn, so v1's cascade
  could not speak before generation ended. Chat now streams through
  `stream_once_on` (§3.2, §7.4).
- **Output pacing added** (§8.2). The stock SDK ends its playback state on
  `output_audio.done`, and unpaced output made barge-in dead after about
  0.5 s.
- **Stock SDK defaults handled** (§5.1, §6.3). These are a model-less
  handshake, `gpt-realtime*` / `gpt-4o-mini-transcribe` names, and a
  `semantic_vad` default. v1 would have refused or silently changed them.
- **Policy per model call** (§10.3). v1 wrongly claimed `stream_once` checks
  policy, never checked the ASR and TTS aliases for the key, and lost the
  concurrency slot at the 101.
- **Holds at the point of use** (§9.1). v1's holds from `speech_started` to
  `response.done` leaked on every turn without a response, and did not pin
  the route used, because the audio helpers re-opened the gate per call.
- **Echo** is stated as unsolved by VAD evidence, with a measured
  echo-reference experiment in WP4 (§6.4).
- **Function-call status and `call_id` rules** (§7.4), rendering
  normalization for strict templates (§7.2), and the state rules for
  cancel, active responses and speech between commit and response (§4.3).
- **Facts corrected** (§2.3): when the user item is added, `event_id` and
  the delta id fields, and the SDK's `audio_end_ms` meaning.
- **Transport:** 426 needs a mapped rejection, WebSocket size limits are
  made explicit settings, and anonymous cross-origin upgrades are refused
  (§10).
- **Telemetry:** the `realtime` label needs a `ClientProto` variant, and
  there are no audio-seconds columns in v1 (§11).
- **Silero:** the stock graph takes dynamic lengths. v1's inference that
  an earlier prototype's export differs was wrong, and WP0 measures context
  on and off. ORT also brings `ThirdPartyNotices`, intra-threads 1, and size gating
  (§13).
- **Cut from v1:** G.711, out-of-band responses, `truncation: auto`, the
  status block (§19).
- **Added:** the empty-transcript rule, a WAV for the ASR upload, and
  per-response config snapshots (§4.2).

## 22. Spike results (WP0, measured 2026-10-01)

Five parallel probes. Each ran in its own scratch directory and touched no
gateway configuration, apart from the voice-row probe noted below. Their
numbers are folded into §2.2, §2.3, §3.1, §6.1, §6.3, §8.2 and §13. What
stays only here:

- **Silero graph variants.** The stock graph and the 16k "op15" graph accept
  only up to 576 samples per call, because an internal `If` breaks above
  that. Only the "op18 ifless" graph is truly length-dynamic. All three agree
  to within 4e-7. The op15 file is chosen because it is the smallest. If
  `ort` is ever replaced by a pure-Rust runtime, the ifless file is the
  candidate, because `If` nodes are what such runtimes trip on.
- **Silero timing, Rust and Python.** The Rust `ort` path and the Python
  reference give the same per-frame picture. Both found context mandatory,
  and both measured ~0.1 ms per frame.
- **Smart Turn parity fixtures.** These are Whisper features plus the
  model's probability, produced with `transformers.WhisperFeatureExtractor`
  (5.18.0, numpy only) and onnxruntime 1.30. The spike's first set used
  two recordings that cannot be published. The committed set
  (`smartturn_en_*`) is generated from the Piper LJSpeech clips instead,
  and the recordings never entered the repository.
- **Client captures.** The captures of `openai-python` 3.22.1 and
  `@openai/agents` 0.18.0 contain no private data and become the protocol
  test fixtures (WP1): a normal turn, a function call, and a barge-in with
  `interrupt_response` true and false.
- **Model rows (recommendations; the probes changed nothing).**
  - Pocket TTS needs a default voice.
  - Nemotron is the voice ASR. qwen3-asr is ~4× slower and ~1.8 GB larger,
    for cleaner punctuation.
  - Fish TTS is unusable for voice without `reference_text`.
  - A voice-sized chat row was created: `gemma4-e4b-voice` (the only
    configuration change the probes made). It uses the same GGUF and MTP
    draft as `gemma4-e4b`, with `ctx_size` 16384, `parallel` 1, flash
    attention on, q8_0 K/V, reasoning off and `idle_seconds` 300. Warm time
    to first token fell from ~205 ms to ~20 ms. The other `gemma4-*` rows
    share the slow setting (flash attention off, very large per-slot
    context); changing them is the owner's call.

## 23. First live acceptance (2026-10-01, HEAD b7b2d81)

The setup:
- A dev copy of the owner's gateway, with real models on the RTX 4090:
  `gemma4-e4b-voice`, `audio/nemotron-asr-q8-0`, and
  `audio/pocket-tts-english-q8-0` or `-german-q8-0`.
- The stock clients unmodified: `openai-python` 3.22.1 and `@openai/agents`
  0.18.0.
- LJSpeech and German clips streamed in real time in 100 ms appends.

Results, as p50 in ms after the clip's true end of speech:

| Run | `speech_stopped` | transcript | `response.created` | first audio (text) | Result |
|---|---|---|---|---|---|
| `server_vad`, audio out (3 warm) | 565 | 632 | 633 | **818** | pass |
| `semantic_vad` (then mapped to `server_vad`; Smart Turn since WP6) | 565 | 630 | 630 | 847 | pass |
| text out | 565 | 620 | 621 | (646) | pass |
| `@openai/agents`, audio, own default session | 566 | 638 | 642 | 866 | pass |
| German | 585 | 643 | 643 | 928 | pass |
| cold first session | 567 | 2259 | 2263 | 3016 | pass, slow |

What the numbers show:
- **The end-of-turn wait dominates.** 565 ms is the 500 ms silence window
  plus the client's 100 ms chunks. After it come ASR (~60 ms), the first
  token (~25 ms) and the first clause plus TTS (~185 ms).
- **Protocol fidelity holds.** `@openai/agents` failed no schema check
  across ~460 events, ran the tool round trip, and spoke its answer. There
  were zero error events, and pacing had zero underruns.
- **Usage rows are exact:** one per chat call, one per ASR turn, one per TTS
  response, under the client's key.
- **VRAM measured:** chat 3.4 GB, nemotron 1.55 GB, pocket-tts ~1 GB. The
  whole cascade is ~5.9 GB.
- **Found broken:**
  - Speech during an answer is dropped (WP4).
  - Warm-up runs sequentially and audio.cpp loads its models lazily, which
    makes the cold first turn slow.
  - `transcription.completed` lacks `usage`.
  - Rows are not labelled `realtime`, and the §11 timing line is missing.
  - Gaps in the speakable pass: rules, abbreviations, list numbers,
    parentheses.
  - There is no voice-oriented default instruction. These are fixed in the
    follow-up packages.
