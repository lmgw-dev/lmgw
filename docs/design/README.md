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

## Dashboard rebuild (2026-08)

The dashboard was rebuilt from a server-rendered htmx UI into the current
Leptos single-page app. These notes planned that rebuild; the rebuild is
finished.

- [ui-rebuild/plan.md](ui-rebuild/plan.md): goals and architecture of the rebuild
- [ui-rebuild/parity.md](ui-rebuild/parity.md): feature inventory of the old UI, used as the parity checklist
- [ui-rebuild/core-notes.md](ui-rebuild/core-notes.md): gateway internals brief for the new `/api` plane
- [ui-rebuild/design-sample.html](ui-rebuild/design-sample.html) and [ui-rebuild/usage-mockup.html](ui-rebuild/usage-mockup.html): visual contracts for the dashboard's palette and the Usage page
