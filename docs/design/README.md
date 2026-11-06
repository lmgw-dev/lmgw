# Design records

Each major lmgw feature started as a design document. They record the
intent, the alternatives that were considered and the reasoning behind each
decision, as it stood when the feature was built. Later changes are not
always written back, so where a record and the code or the user
documentation disagree, the code and the user documentation win. Source
comments cite these documents by section number (`§4.2`), so the numbering
is kept stable.

| Date | Record |
| --- | --- |
| 2026-06-09 | [LLM API gateway](2026-06-09-llm-api-gateway-design.md): the original architecture, canonical request representation, protocol adapters |
| 2026-06-29 | [MCP gateway](2026-06-29-mcp-gateway-design.md): aggregating MCP servers behind `/mcp`, Podman-isolated stdio servers |
| 2026-08-29 | [quickdoc](2026-08-29-quickdoc-design.md): versioned documentation corpora with hybrid retrieval |
| 2026-08-30 | [Per-model containers](2026-08-30-per-model-containers-design.md): one Podman container per model, VRAM admission |
| 2026-09-04 | [GPU hold](2026-09-04-gpu-hold-design.md): pausing local models, cloud fallbacks |
| 2026-09-17 | [Model capabilities](2026-09-17-model-capabilities-design.md): capabilities on `/v1/models`, reasoning control |
| 2026-09-18 | [Agent catalog](2026-09-18-agent-catalog-design.md): agent manifests, batch pipelines |
| 2026-09-18 | [Usage analytics, cost and policy](2026-09-18-usage-analytics-cost-policy-design.md): pricing, rollups, per-key budgets and limits |
| 2026-09-19 | [Agent container runtime](2026-09-19-agent-container-runtime-design.md): container agents, agent tokens, the run ledger |
| 2026-09-21 | [Image generation](2026-09-21-image-generation-design.md): stable-diffusion.cpp as a model class |
| 2026-09-22 | [Principals, origins and mounts](2026-09-22-principals-origins-mounts-design.md): credentials, capabilities, agent origins, host mounts |
| 2026-09-24 | [folder-chat vision pages](2026-09-24-folder-chat-vision-pages-design.md): reading scanned and table-heavy PDF pages with a vision model |
| 2026-09-25 | [Chat archive, pinning, attachments](2026-09-25-chat-archive-pin-attachments-design.md) |
| 2026-09-26 | [Container builds from git](2026-09-26-container-builds-design.md): the Backends page |
| 2026-09-27 | [Candidate aliases and unified KV](2026-09-27-candidate-aliases-unified-kv-design.md): background traffic, alternates, shared KV pools |
| 2026-09-27 | [Ladder models](2026-09-27-ladder-models-design.md): one model id over several configured rungs, climbing when a request needs more |
| 2026-09-28 | [API reference page](2026-09-28-api-docs-page-design.md): the OpenAPI document, client-compatible token counters |
| 2026-09-29 | [Benchmarks](2026-09-29-benchmark-design.md): measuring models and engine builds |
| 2026-09-30 | [Chat, made complete](2026-09-30-chat-complete-design.md): rendering, parameters, folders, search, knowledge bases |
| 2026-10-01 | [Realtime voice API](2026-10-01-realtime-voice-design.md): OpenAI Realtime over WebSocket, served as a cascade of ASR, chat and TTS aliases |
| 2026-10-02 | [Audio catalog revisions](2026-10-02-audio-catalog-revisions-design.md): downloads at the commit an audio.cpp spec pins, and the commit every download came from |
| 2026-10-05 | [Server-side MCP tools on `/v1/realtime`](2026-10-05-realtime-server-tools-design.md): `mcp` session tools resolved against lmgw's own servers, and the `/v1/mcp/servers` discovery routes |
| 2026-10-06 | [The registry owns the start](2026-10-06-registry-owns-start.md): a request that goes away mid-load no longer leaves its model's container running unowned; reconciliation after boot; `499` rows |
| 2026-10-06 | [llama.cpp egress](2026-10-06-llama-egress-design.md) (draft): llama-server and ik_llama.cpp as their own `llama_cpp` protocol, `/props` facts, tool-result images |
| 2026-10-06 | [Client apps](2026-10-06-client-apps-design.md) (draft): device keys and a Chat capability, the Chat change feed, ongoing-conversation folders, device-hosted MCP, approvals, MCP resources |
| 2026-10-07 | [Billable units](2026-10-07-billable-units-design.md) (draft): prices per minute of audio, per character, per image and per request next to tokens, measured quantities on the request row |
| 2026-10-09 | [Personality profiles](2026-10-09-personality-profiles-design.md) (draft): how a Chat thread's model talks in text and voice: persona, length rule, examples, voice block, reasoning, TTS voice — and the shared UI crate for the profile editor |
| 2026-10-09 | [MCP Tasks and late tool results](2026-10-09-mcp-tasks-design.md) (draft): task-augmented calls to hosted servers (MCP 2025-11-25), results that arrive after the turn and enter the thread, bound-session continuations |
| 2026-10-12 | [Decision models](2026-10-12-decision-models-design.md) (draft): `POST /v1/systemone` (TypeSafe's System One wire as llama.cpp serves it), decision GGUFs as a third aux kind, CPU aux rows that serve under the hold, OpenRouter's Jev as a cloud upstream |
| 2026-10-14 | [WebRTC realtime calls](2026-10-14-realtime-calls-design.md) (approved): `POST /v1/realtime/calls` beside the WebSocket for the Android client over WireGuard: str0m in ICE-lite on UDP sockets grouped per listener and read by one driver, Opus in and out on the RTP clock, `output_audio_buffer.*`, lmgw's own events on bound calls (the stop, the close reason, parts, playout reports, `takeover=ask`, `chat_folder`), a transport seam under the session |
| 2026-10-14 | [Device link](2026-10-14-device-link-design.md) (approved): paired devices on a TLS listener of their own with mutual TLS (lmgw's private CA pinned by the pairing link, keys enrolled with one-time pairing tokens and confirmed on the dashboard, loopback until the first remote device), and the Chat feed as their push channel |
| 2026-10-16 | [The lab store and the Decision lab](2026-10-16-labs-store-decision-lab-design.md) (approved): one store of groups, runs and content-addressed artifact files for the Audio, Image and Decision labs (history, size in view, delete and Clear, JSONL, zip and CSV export, no retention rule), and the Decision lab on it: in-process `/v1/systemone` cells, Compare, Perturb, a token gauge per header shape; question sets, set runs and batch agents through a set in later slices |
| 2026-10-21 | [Per-request background traffic](2026-10-21-per-request-background-design.md) (approved, built): `x-lmgw-background: on` makes one request a guest on the GPU (a plain model through its row's fallback, a candidate alias as a guest walk, every model class), a deferral answers `gpu_deferred`, foreground work never preempts it |
| 2026-10-21 | [Image and audio tools for the Chat](2026-10-21-chat-media-tools-design.md) (draft; slice 1 built): `image__*` and `audio__*` as a built-in toolset on `/mcp` and by label in the Chat, in-process calls over sd-server's native job API (previews, step progress, cancel), produced files as reply attachments, the image card; default models per tool in Settings, a folder and a thread (§3.8) |
| 2026-10-21 | [Device-to-device transfers](2026-10-21-device-transfers-design.md) (approved): a paired device offers a file to another device of the same app (an app id declared at pairing), relayed through lmgw's disk: a resumable ranged upload checked by sha256, the offer on the Chat feed (`transfer.updated`), a ranged download, expiry, the Devices card's Transfers section |
| 2026-10-22 | [Structured output, translated everywhere](2026-10-22-structured-output-design.md) (approved): `response_format`, `text.format` and `output_config.format` as one IR field every egress translates (Anthropic `output_config.format`, Gemini `responseJsonSchema`), an early `response_format_unsupported` where a model's flag says `false`, upstream refusals classified as `invalid_json_schema` / `response_format_unsupported` with `param` set |
| 2026-10-24 | [The OpenAI platform gets its own egress](2026-10-24-openai-egress-design.md) (decided 2026-10-24): `openai` becomes api.openai.com only, every other OpenAI-shaped provider moves to `openai_compatible` (migration 0090), both composing `openai_wire`; service-tier pricing (the tier OpenAI reports, a tier dimension on price rows, unpriced with a note when missing), `/v1/responses` without the flag and priced, `max_completion_tokens`, OpenAI's error codes; chat over the Responses wire and encrypted reasoning as later packages |
| 2026-11-03 | [The machine check](2026-11-03-machine-check-design.md) (draft; CDI ruling decided 2026-11-03): on first launch lmgw checks the graphics card, its driver, NVIDIA's container toolkit, Podman and free space, says what is missing in plain words, and installs the toolkit through a fixed root helper behind one polkit prompt (started only from the app's own window, run as a system task); lmgw writes and refreshes its own CDI spec without root and passes it with `--cdi-spec-dir`; a configured fallback answers `gpu_setup`; the engine download happens before admission, shows its bytes, and comes with a model's download by default |
| 2026-11-03 | [The Chat on the Chat plane](2026-11-03-chat-plane-design.md) (built; draft calls not yet confirmed): every route the dashboard's Chat reads is on `/chat/api/*` or `/v1`, scoped to the caller, so a paired device can host the same Chat — the settings it uses, the toolsets a thread may attach, the knowledge bases and the citation viewer as Chat-plane reads (a device's original download not yet), the thread rows on `Cap::Chat`, and live updates from the source the host hands the Chat: the dashboard's shell stream, else the change feed (with a benchmark's lease in its hold) |
| 2026-11-03 | [Find a model](2026-11-03-find-a-model-design.md) (draft; no-catalog ruling decided 2026-11-03; WP1 built 2026-11-04: the header read over a Range, one projector rule, downloads that go on from their `.part` after a restart): a name someone said ("qwen 3.6") is searched on Hugging Face, ranked only by what the hub answers live (original before uploads, `conversational`, likes) with no list of models, uploaders or aliases, and one file is picked from a header-only plan (a Range read of the GGUF header, 11 to 16 MB) against the whole card; one card in plain words, "Get it" sets the model up server-side without starting it, "Say hello" opens a thread; the Hugging Face file list gets fit marks and a recommended file; "Send to a friend" shares the public Hugging Face address |
| 2026-11-04 | [API compatibility](2026-11-04-api-compat-design.md) (live check built; level 1 planned): providers' pinned schemas validating every egress body and ingress answer in the gate (planned), and `scripts/compat-live.sh`, a manual live check that drives multi-turn scenarios (tool calls then turn two, parallel calls, reasoning across a tool call, structured output, errors) through `/v1/chat/completions`, `/v1/messages` and `/v1/responses` with the official SDKs and fixed wire rules, and writes a local report with a diff against the previous run |

## Dashboard rebuild (2026-08)

The dashboard was rebuilt from a server-rendered htmx UI into the current
Leptos single-page app. These notes planned that rebuild; the rebuild is
finished.

- [ui-rebuild/plan.md](ui-rebuild/plan.md): goals and architecture of the rebuild
- [ui-rebuild/parity.md](ui-rebuild/parity.md): feature inventory of the old UI, used as the parity checklist
- [ui-rebuild/core-notes.md](ui-rebuild/core-notes.md): gateway internals brief for the new `/api` plane
- [ui-rebuild/design-sample.html](ui-rebuild/design-sample.html) and [ui-rebuild/usage-mockup.html](ui-rebuild/usage-mockup.html): visual contracts for the dashboard's palette and the Usage page
