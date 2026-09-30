# API reference page and client-compatible token counters — design (2026-09-28)

**Status:** Implemented (2026-09-28). Choices made while designing are in §10; decisions taken during implementation are in §12.

Builds on [2026-09-22-principals-origins-mounts-design.md](2026-09-22-principals-origins-mounts-design.md)
(the capability table, the five principals, the session cookie),
[2026-09-17-model-capabilities-design.md](2026-09-17-model-capabilities-design.md)
(the `/v1/models` `lmgw` block) and
[2026-09-27-candidate-aliases-unified-kv-design.md](2026-09-27-candidate-aliases-unified-kv-design.md)
(the request gate and its prompt count).

## 0. Findings that change the scope

Checked against the `feat/api-docs` branch. Each finding says what this design does about it.

1. **The self-admin catalog is not the argument contract of `/api/op/{name}`.**
   - Patch-style ops parse their arguments into a `deny_unknown_fields` struct (`ops::patch_from_args`, `ops/common.rs:418`). The tool's schema is a flat-scalar projection of that struct.
   - Nine ops share a tool's name but take different arguments (§4.7 table B).
   - **Design:** the op's argument schema is derived from the struct the dispatcher parses into. The catalog supplies the prose. A pinned divergence list keeps the two honest.
2. **Tool `lmgw__X` ↔ op `X` holds for 21 of 54 tools only.**
   - 9 share a name with different arguments.
   - 24 tools have no op; they are GET routes or docs-plane REST routes.
   - 44 ops are dispatcher-only (§4.7 tables A–D).
3. **`/v1/count_tokens` skips the key's alias scope and budget check.**
   - `handle_count_tokens` (`proxy/legacy.rs:424`) never calls `policy_or_refuse`.
   - So a scoped client key can count tokens on any alias, and cold-load a local model doing so.
   - **Design:** both adapters and `/v1/count_tokens` run the check (§5.1). Behaviour change; release note. *(Amended after review: the scope only, no budget — §12 entry 15.)*
4. **The universal counter already approximates, silently.**
   - An OpenAI-compatible upstream whose model tiktoken does not know is counted with `o200k_base`. Only a debug line records it (`egress/openai.rs:734–749`).
   - Anthropic and Gemini count the text wrapped as one user message (`egress/anthropic.rs:381`, `egress/gemini.rs:410`).
   - **Design:** the new `x-lmgw-count-approximate` header (§5.1) is stamped there too (§10 choice 3).
5. **`x-lmgw-*` headers exist beyond the `/v1/models` block.**
   - `x-lmgw-run`: agent attribution, `server.rs:1006`.
   - `x-lmgw-admin-token`: `POST /mcp/admin`, `server.rs:864`.
   - `x-lmgw-face`: lmgw → agent app, `web/agent_proxy.rs:90`.
   - **Design:** the one table (§4.9) has an audience column. The `/v1/models` block publishes exactly today's twelve headers plus the new one.
6. **`/tokenize` cannot be registered in `server.rs`.**
   - `tests/it/route_walk.rs:46–51` prefixes every `.route(` found in `server.rs` with `/v1`.
   - **Design:** it is registered in `proxy/tokenize.rs` and merged at the root. Its capability row still goes in `CAPABILITY_TABLE`.
7. **The UI's `api::get::<T>` calls are an incomplete source for response schemas.**
   - Five `/api` reads have no DTO anywhere, and `/api/local-model` is polymorphic by `target`.
   - The internal mini-APIs' reads have UI-private DTOs only.
   - `/api/session` answers a core-private struct.
   - **Design:** §4.6 is the full inventory, with a rule for each case. Untyped reads are marked and listed in §11.
8. **The tester cannot use `crate::api`.** Its refusal path locks the whole dashboard on `401 session_required` (`lmgw-ui/src/api.rs:45–52`). The browser also attaches the session cookie to every same-origin fetch unless told otherwise. **Design:** the tester has its own fetch path, which sends `credentials: omit` for the "API key" and "No credential" identities (§6.7).
9. **llama.cpp compatibility clients often pair `/tokenize` with `/detokenize` (and read `/props`).** Not implemented, by design; listed in §11.
10. **Two existing silent behaviours met on the way.** Neither is changed here; both are documented in the reference and listed in §11. *(Both fixed afterwards, 2026-09-29 — see §11.)*
    - `/v1/embeddings` forwards only `model` and `input`: `dimensions` and `encoding_format` are dropped (`egress/openai.rs` `build_embeddings`).
    - The Anthropic egress never forwards the client's `anthropic-beta` header.

## 1. Summary

**Goals**

- **A dashboard page, "API reference" at `/api-reference`,** that documents lmgw's own HTTP API Swagger-style.
  - It is rendered natively in Leptos from the gateway's OpenAPI 3.1 description, fetched at runtime.
  - It has a built-in request tester.
- **The OpenAPI description is built in Rust from the existing sources of truth:**
  - `CAPABILITY_TABLE`;
  - the op dispatcher, made enumerable (§4.7);
  - the self-admin catalog's prose;
  - one shared `x-lmgw-*` header table;
  - `schemars` schemas of the api-types DTOs (behind a `schema` feature only lmgw-core enables) and of core's extractor and patch structs;
  - hand-written `/v1` protocol schemas.
- **Served twice:**
  - `GET /api/openapi.json` (Admin): everything.
  - `GET /v1/openapi.json` (Inference): only the inference plane.
- **Drift guards** in `tests/it`, both ways, including live validation of every `/api` GET that can be exercised in the test gateway.
- **`POST /v1/messages/count_tokens` (Anthropic SDK) and `POST /tokenize` (llama.cpp) work for real.**
  - Each is a thin adapter onto the universal counter's machinery.
  - Any approximation is stated in a response header.
- **`/v1/models` `lmgw.endpoints` is generated** from the same registry, so it cannot advertise a route that does not exist again.

**Non-goals**

- Swagger UI or vendored JS.
- A per-backend tokenizer framework.
- `/detokenize` or other llama.cpp compatibility routes.
- Documenting the agent-app reverse proxy or the SPA's own routes.
- Per-method expansion of MCP's JSON-RPC.
- New DTOs for today's untyped reads (§11).

## 2. Facts this rests on (checked 2026-09-28)

1. **`CAPABILITY_TABLE`** (`server.rs:157–326`) lists every registered `(method, path, Cap)`.
   - `tests/it/route_walk.rs` checks it both ways: a source scan in one direction, HTTP in the other.
   - `Cap::as_str()` spells `public | inference | ledger | agent-self | admin` (`principal.rs:54–64`).
2. **`lmgw_block()`** (`server.rs:1538–1633`) hard-codes the endpoint lists.
   - It advertises `/v1/messages/count_tokens` (1559) and `/tokenize` (1567). Neither exists.
   - `POST /tokenize` falls to the SPA's GET-only catch-all and gets 405. `POST /v1/messages/count_tokens` gets 404.
   - Its header prose is hand-written in the same function.
3. **Header constants:**
   - `server.rs:595–599`: the three reasoning headers.
   - `proxy/recording.rs:319–349`: fallback, fallback-reason, candidate, reasoning-ignored, max-tokens-defaulted/raised/clamped.
   - `gate/ladder.rs:22`: rung.
   - `web/agent_proxy.rs:90`: face.
   - `x-lmgw-admin-token` and `x-lmgw-run` exist only as literals (`server.rs:864`, `:1006`).
4. **The universal counter:**
   - `handle_count_tokens` → `count_tokens_inner` → `count_tokens_route` (`proxy/legacy.rs:415–540`).
   - Admission goes through `gate::open` with `RouteCheck::Text("/v1/count_tokens")`. Deliberately not logged; no policy check.
   - Egress, per upstream:
     - llama-server: `/tokenize {model, content}` (`openai.rs:559–588`).
     - Generic, audio.cpp, sd.cpp: tiktoken with an `o200k_base` fallback (`openai.rs:734`).
     - Anthropic: `/v1/messages/count_tokens` on a single user turn (`anthropic.rs:374–405`).
     - Gemini: `:countTokens` on `contents` (`gemini.rs:402–426`).
   - `CountPlan::{Ready, Request}` is in `egress/mod.rs:104–112`.
   - Callers of `count_tokens_inner`: the handler and quickdoc ingest (`quickdoc/ingest.rs:652, 839`).
5. **The gate's exact prompt count:**
   - `gate::count::count_chat_prompt` (`gate/count.rs:155`) runs `/apply-template`, then `/tokenize` with `add_special: true`. It was measured equal to `usage.prompt_tokens`.
   - Helpers: `image_token_bound` (`:306`), `media_parts` (`:67`).
   - `gate/fit.rs:756` `on_running_server` is private.
   - `LocalHold::class`, `LocalHold::gate_facts` (`vram/local_hold.rs:258–271`), and `send_local` (`:403`).
6. **Anthropic ingress:** `ingress::anthropic::parse_messages_request` (`ingress/anthropic.rs:14`).
   - Refuses server tools (a tool without `input_schema`) as `Unsupported`.
   - Drops `redacted_thinking`.
   - Carries no passthrough keys.
7. **The op dispatcher** (`web/api.rs:742–1042`):
   - `agent_*` and `agents_restore` go to `api_agents::op` (`api_agents.rs:2093`).
   - Five key ops go through `matches!` to `api_settings::key_op` (`api_settings.rs:1153`).
   - `settings_set_full` is special-cased.
   - 49 match arms, including the legacy `embed_model_set`. Anything else gets `400 op_failed "unknown op"`.
   - 74 names in total.
8. **The self-admin catalog:**
   - 54 tools, concatenated in `mcp/selfadmin/catalog.rs`.
   - Arguments are flat scalars (pinned by a test); `writes` means effect.
   - `mcp::selfadmin::full_catalog()` is `pub` (`selfadmin.rs:157`).
9. **Typed Backends ops:** the 19 take api-types `*Args` and answer api-types responses (`web/api.rs:1007–1038`, `ops/backends.rs:148–620`).
10. **lmgw-api-types:**
    - Depends on serde and serde_json only.
    - `ImageAsset` has a hand-written `Deserialize` (`status.rs:176`).
    - `schemars 1.2.1` is already in `Cargo.lock` (through rmcp).
11. **Principals:**
    - `Principal::holds` (`principal.rs:105–134`): owner = all but ledger; agent = all but admin; key = public + inference; anonymous = public, plus inference while auth is off.
    - The session cookie is honoured for owner rows only.
    - `x-api-key` is accepted as a bearer spelling (`agents/token.rs:328–334`).
    - `GET /api/session` reports the principal of any presented credential and never answers 401 (`web/session.rs:303–330`).
12. **UI plumbing:**
    - `crate::api::get` locks the session on 401 (`lmgw-ui/src/api.rs:45–52`).
    - `pages/chat_stream.rs` parses SSE off a fetch `ReadableStream`.
    - `ModelPicker` has a `tasks` filter (`widgets/model_picker.rs:103`).
    - Also available: `ConfirmButton` (`widgets/confirm.rs:33`); split pages with `.split-rail` and `.rail-select` (`pages/model_catalog.rs:905–1000`); `@container pane` breakpoints (`app.css:332`).
13. **The SPA catch-all** serves `index.html` for any path without a `.` outside `API_PREFIXES` (`web/ui.rs:28–34, 72–80`). So `/api-reference` is a client route.
14. **`scripts/mock-openai.py`** answers `GET */models` and `POST` chat completions (unary and SSE) only.
    - `tests/it/support/gpu_world.rs` has local containers with `/apply-template` and `/tokenize`.

## 3. Module layout

### 3.1 lmgw-api-types

- **`Cargo.toml`:** `schemars = { version = "1.2", optional = true }` and `[features] schema = ["dep:schemars"]`.
- **Every `pub struct`/`pub enum` in `src/*.rs`** gets `#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]`.
  - `ImageAsset` gets a hand-written `JsonSchema` impl under `#[cfg(feature = "schema")]`: `oneOf: [string, {name: string}]`.
- **New `src/openapi_ext.rs`:** the extension-key and value constants of §4.4. They are not feature-gated, because the UI reads them. `lib.rs` gets `pub mod openapi_ext;`.

### 3.2 lmgw-core, new files

| File | Holds |
|---|---|
| `src/openapi/mod.rs` | hub: `mod` lines; `pub use serve::{admin_json, v1_json}`, `pub use build::{admin_doc, v1_doc}`, `pub use headers::{LMGW_HEADERS, headers_block, LmgwHeader, …}`, `pub use endpoints::lmgw_endpoints` |
| `src/openapi/registry.rs` | `DocRoute`, `Req`, `Resp`, `Dialect`, `Group`; `all_routes()` concatenating the planes |
| `src/openapi/tags.rs` | `TAGS: &[TagDef { id, name, group, description }]` (§4.3), final in WP1 |
| `src/openapi/exclusions.rs` | `pub const UNDOCUMENTED: &[(&str, &str, &str)]` (method, path, reason) (§4.2) |
| `src/openapi/schemas.rs` | generator settings, `named()`, `untyped()`, error-envelope refs, `prune_components()` |
| `src/openapi/params.rs` | query struct schema → OpenAPI query params; path params; header params from the header table |
| `src/openapi/build.rs` | assembly (§4.3), principals matrix, op expansion, `OnceLock` caches, `admin_doc()`, `v1_doc()` |
| `src/openapi/serve.rs` | the two GET handlers |
| `src/openapi/headers.rs` | `LMGW_HEADERS` (§4.9), `headers_block()` for `/v1/models` |
| `src/openapi/endpoints.rs` | `lmgw_endpoints()` for `/v1/models` (§4.11) |
| `src/openapi/planes/{mod.rs, inference.rs, dashboard.rs, usage.rs, docs.rs, agents.rs, session.rs}` | route lists per plane, each `pub(crate) fn routes() -> Vec<DocRoute>`. *(Owner decision, 2026-09-28: `labs.rs` — Chat, Audio lab, Image lab — was deleted; those rows moved to `exclusions.rs` instead, see §4.2.)* |
| `src/openapi/v1/{mod.rs, chat.rs, responses.rs, anthropic.rs, aux.rs, media.rs, models.rs, errors.rs, mcp.rs}` | hand-written `/v1` schemas and examples (§4.8) |
| `src/openapi/ops/{mod.rs, table.rs, args.rs, merge.rs, divergence.rs}` | op table (74 entries), hand-written argument schemas, struct + catalog merge, `pub const DIVERGENCES` |
| `src/web/op_names.rs` | `pub const MAIN_OPS`, `AGENT_OPS`, `KEY_OPS`, `SETTINGS_OPS`; `pub fn all()`, `pub fn is_op()` (§4.7) |
| `src/proxy/count.rs` | moved from `legacy.rs:415–540` (`handle_count_tokens`, `count_tokens_inner`, `count_tokens_route`, renamed `count_text_route`), plus `Count`, `Approx`, `approx_header` (§5.1) |
| `src/proxy/count_messages.rs` | `handle_messages_count_tokens`, `flatten_for_count` (§5.2) |
| `src/proxy/tokenize.rs` | `routes()`, handler, `llama_error()` (§5.3) |

### 3.3 lmgw-core, edits to existing files

| File | Edit | WP |
|---|---|---|
| `Cargo.toml` | `lmgw-api-types = { …, features = ["schema"] }`; `schemars = "1.2"`; dev: `jsonschema = { version = "<latest 0.x>", default-features = false }` | 1 |
| `src/lib.rs` | `pub mod openapi;` | 1 |
| `src/server.rs` | rows `("GET","/v1/openapi.json",Inference)`, `("GET","/api/openapi.json",Admin)` (WP1); `.route("/openapi.json", get(crate::openapi::v1_json))` in `json_api` (WP1); `lmgw_block` headers from `headers_block()` (WP2); rows `("POST","/v1/messages/count_tokens",Inference)`, `("POST","/tokenize",Inference)`, `.route("/messages/count_tokens", post(messages_count_tokens))` + thin wrapper (with `reasoning_or_400(.., to_anthropic_json)`), `.merge(crate::proxy::tokenize::routes(&state))` in `build_router` (WP3); `lmgw_block` endpoints from `lmgw_endpoints()`, `#[derive(JsonSchema)]` on `VoicesQuery` (WP6) | 1,2,3,6 |
| `src/web/mod.rs` | `pub mod op_names;` (WP1); `mod api/api_settings/audio_lab/chat/image_lab` → `pub(crate) mod` (WP4) | 1,4 |
| `src/web/api.rs` | top of `op()`: `if !op_names::is_op(&name) { return ops_result(Err(format!("unknown op '{name}'"))) }`; replace the `agent_` prefix test and the key-ops `matches!` with `op_names::AGENT_OPS/KEY_OPS.contains`; `.route("/api/openapi.json", get(crate::openapi::admin_json))` (WP1); `#[derive(schemars::JsonSchema)]` + `pub(crate)` on every `Query`/body/patch struct (WP4) | 1,4 |
| `src/web/{api_usage,api_docs,api_agents,api_settings,session,chat,audio_lab,image_lab}.rs` | the same derive + `pub(crate)` on extractor structs, `SessionView`, `SettingsFullPatch` | 4 |
| `src/ops/{routing,local_model,candidate_alias,mcp_server,settings_patch,aux_model,image_model,keys}.rs`, `src/ladder.rs` (`Rung`), plus any type those structs contain | derive `JsonSchema`; foreign types via `#[schemars(with = "serde_json::Value")]` | 5 |
| `src/capabilities/mod.rs` | derive `JsonSchema` on `ModelCapabilities`, `ReasoningCaps`, `ToolCallCaps`, `StructuredOutputCaps` | 6 |
| `src/proxy.rs` | `mod count; mod count_messages; pub mod tokenize;` + re-exports | 3 |
| `src/proxy/legacy.rs` | count code moved out to `count.rs` (module doc updated) | 3 |
| `src/proxy/recording.rs` | `pub const COUNT_APPROXIMATE_HEADER` (WP2); split `policy_or_refuse` into `policy_refusal_logged(..) -> Option<GatewayError>` + the wrapper (WP3) | 2,3 |
| `src/egress/mod.rs` | `CountPlan::Guessed(u64)` | 3 |
| `src/egress/openai.rs` | `tiktoken_count` → `(u64, bool)`; `pub(crate) fn tokenize_request(http, up, body: &Value) -> RequestBuilder`, reused by `build_count_tokens` | 3 |
| `src/egress/anthropic.rs` | `pub(crate) fn count_messages_request(http, up, model, body: &Value) -> RequestBuilder`; `build_count_tokens` calls it | 3 |
| `src/egress/gemini.rs` | `pub(crate) fn count_chat_request(http, up, model, ir, params) -> Result<RequestBuilder, GatewayError>` | 3 |
| `src/gate/fit.rs`, `src/gate/mod.rs` | `on_running_server` → `pub(crate)`, re-exported | 3 |
| `tests/it/main.rs` | `mod` lines for the new test files, all added in WP1 as empty files | 1 |
| `tests/it/egress_adapters.rs` | `gpt-9-something` now answers `CountPlan::Guessed` (line 700) | 3 |
| `tests/it/models_endpoint.rs` | new header (WP2); new endpoints (WP6) | 2,6 |

### 3.4 lmgw-ui

- **New `src/pages/api_ref/`:**
  - `mod.rs`: hub; `mod` lines + `pub use page::ApiReference;`.
  - `page.rs`, `doc.rs`, `rail.rs`, `detail.rs`, `schema_view.rs`, `tester.rs`, `identity.rs`, `send.rs`, `stream.rs`, `response_view.rs`, `json_view.rs`, `curl.rs`, `example.rs` (§6.3).
- **Edits:**
  - `pages/mod.rs`: `mod api_ref;` + `pub use api_ref::ApiReference;`.
  - `app.rs`: `<Route path=path!("api-reference") view=pages::ApiReference/>`.
  - `shell.rs`: NavItem in the "Connect" group, after MCP servers.
  - `assets/app.css`: one new section (§6.10).
  - `Cargo.toml`: web-sys feature `"RequestCredentials"`.

### 3.5 Scripts

- **`scripts/mock-openai.py`:** extended (§7.5).
- **`scripts/ui-matrix.py`:** `ROUTES` += `"/api-reference"`, `"/api-reference?op=post_v1_chat_completions"`.
- **`scripts/webkit-check.py`:** `DEFAULT_ROUTES` += `"/api-reference"`.
- **`scripts/drive/api-ref.json`:** new.
- **`scripts/drive/inapp-leaks.json`, `inapp-panics.json`:** add both api-reference paths to their route lists.

## 4. The OpenAPI description

### 4.1 What each part is generated from

| Part | Source |
|---|---|
| Paths, methods, capability | `planes/*.rs` registry, checked against `CAPABILITY_TABLE`; capability read *from the table*, never repeated |
| `/api/op/<name>` paths | `ops/table.rs`, checked against `web/op_names.rs`, which gates the dispatcher |
| Argument schemas of ops | the struct the dispatcher deserializes into (derive `JsonSchema`), else the tool's `inputSchema`, else hand-written `ops/args.rs` |
| Op and GET prose | the self-admin tool's `description` and property descriptions where a tool shares the code path (`x-lmgw-tool`), else hand-written |
| `/api` query parameters | the handler's `Query<T>` struct (derive `JsonSchema`) |
| `/api` request bodies | the handler's `Json<T>` struct, else hand-written |
| `/api` response schemas | §4.6 rules |
| `/v1` request/response schemas | hand-written in `v1/*.rs` from lmgw's own ingress parsers and serializers (§4.8) |
| `x-lmgw-*` header params and response headers | `headers.rs` `LMGW_HEADERS` |
| Security per operation | the operation's capability (§4.3) |
| Principal matrix | `Principal::holds` evaluated at build time |
| `/v1/models` `lmgw.endpoints` | the inference-plane registry's `endpoints` groups |

### 4.2 The registry and coverage

```rust
pub(crate) type SchemaFn = fn(&mut schemars::SchemaGenerator) -> schemars::Schema;
pub(crate) struct DocRoute {
    pub method: &'static str,            // as in CAPABILITY_TABLE
    pub path: &'static str,              // exactly the CAPABILITY_TABLE path ({*id} kept)
    pub tag: &'static str,               // TAGS id
    pub summary: &'static str,
    pub description: &'static str,       // "" allowed when `tool` supplies it
    pub tool: Option<&'static str>,      // x-lmgw-tool: same ops function behind a self-admin tool
    pub query: Option<SchemaFn>,         // the Query<T> struct
    pub path_ints: &'static [&'static str], // path params that are integers ({id} of Path<i64>)
    pub request: Req,                    // None | Json(SchemaFn) | Multipart(SchemaFn) | Raw(&'static str mime) | JsonRpc
    pub response: Resp,                  // Json(SchemaFn) | Untyped(&'static str why) | Binary(&'static [&'static str]) | Sse(&'static [(&'static str, SchemaFn)]) | Redirect | NoContent | Doc
    pub dialect: Dialect,                // OpenAi | Anthropic | LlamaCpp | JsonRpc | Dashboard
    pub endpoints: &'static [&'static str], // lmgw.endpoints groups: "openai" | "anthropic" | "other"
    pub model_task: Option<&'static str>,   // x-lmgw-model-task
    pub confirm_note: Option<&'static str>,
    pub example: Option<fn() -> serde_json::Value>,
}
```

- **Path translation.** `{*id}` becomes `{id}` in the OpenAPI path, and that parameter gets `x-lmgw-wildcard: true`: the value may contain `/` and is sent unescaped.
- **`x-lmgw-writes`** means *changes stored state*. For routes it is `method != GET`, except the inference-plane POSTs that only compute an answer (`/v1/chat/completions`, `/v1/completions`, `/v1/messages`, `/v1/embeddings`, `/v1/rerank`, both counters, `/tokenize`, `/v1/audio/*`, `/v1/images/*`, `/v1/tasks/*`): those are `false`. `POST /v1/responses` stays `true` (it persists by default), and so do `DELETE /v1/responses/{id}` and the `/mcp*` POSTs (a tool call can have any effect). `DocRoute` carries an explicit `writes: Option<bool>` override; `None` means the method rule. For ops it comes from the op table. *(Orchestrator amendment 2026-09-28: the draft had every non-GET confirm, which would put a confirm click in front of every chat completion — the Chat page sends without one, and so does the tester now.)*
- **Coverage rule:** every `CAPABILITY_TABLE` row is exactly one of:
  - a `DocRoute`;
  - the row `POST /api/op/{name}`, which is expanded into one path per op instead;
  - an `UNDOCUMENTED` entry.

| Excluded | Reason (the string the doc carries in `x-lmgw-undocumented`) |
|---|---|
| `* /agents/{id}/app`, `/agents/{id}/app/`, `/agents/{id}/app/{*rest}` | The old mount of an agent's app: redirects to the agent's own origin. The app is the agent's, not lmgw's API. |
| `* /agents/{id}/mcp`, `/agents/{id}/mcp/{*rest}` | An agent container's own MCP face, reverse-proxied. Its surface is the agent's (see its manifest). |
| `GET /`, `GET /{*path}` | The dashboard SPA and its bundle. |
| `GET /ui`, `/ui/`, `/ui/{*path}` | Legacy redirects to the SPA. |
| Every `/chat/api/*`, `/audio-lab/api/*`, `/image-lab/api/*` row (25 rows) | *(Owner decision, 2026-09-28.)* The dashboard's own backend; its shapes follow the UI and are no contract. Formerly documented `x-lmgw-internal: true` via a fourth `Group::Internal` tag group (§4.3) and `planes/labs.rs` (§3.2) — both removed; see §12. |

### 4.3 Document shape

- **Top level:**
  - `openapi: "3.1.0"`.
  - `info {title: "lmgw", version: CARGO_PKG_VERSION, description}`.
  - `servers: [{url: "/"}]`.
  - `tags` in `TAGS` order, each with `x-lmgw-group`.
  - `x-lmgw-principals`, `x-lmgw-undocumented` (the admin document only; the v1 document's root keeps just its own tags and security schemes, §12.8).
- **`securitySchemes`:**
  - `bearer`: http bearer, an lmgw API key of any kind.
  - `apiKeyHeader`: apiKey in the `x-api-key` header, the same keys, as Anthropic SDKs send them.
  - `session`: apiKey in the cookie `lmgw_session`. Same-origin only; owner rows only.
  - `adminToken`: apiKey in the `x-lmgw-admin-token` header, `POST /mcp/admin` only.
- **`security` per capability:**

  | Capability | `security` |
  |---|---|
  | public | `[{}]` |
  | inference | `[{bearer},{apiKeyHeader},{session},{}]` (description: anonymous only while "Require API key" is off) |
  | admin | `[{bearer},{apiKeyHeader},{session}]`, plus `{adminToken}` on `POST /mcp/admin` |
  | agent-self | `[{bearer},{apiKeyHeader},{session}]` |
  | ledger | `[{bearer},{apiKeyHeader}]` |

- **Tags and groups** (`TAGS`, final in WP1). Group order, then tag order:

  | Group | Tags (id — name) |
  |---|---|
  | Inference | `openai` OpenAI-compatible · `anthropic` Anthropic-compatible · `llamacpp` llama.cpp-compatible · `lmgw-inference` lmgw extensions · `mcp` MCP |
  | Dashboard API | `meta` API description · `session` Session · `status` Status & live feed · `models` Models · `upstreams` Upstreams · `tools` MCP servers & tools · `downloads` Downloads · `settings` Settings · `responses` Stored responses · `usage` Usage · `docs` Doc corpora · `agents` Agents |
  | Ops | `ops-routing` · `ops-models` · `ops-runtime` · `ops-downloads` · `ops-settings` · `ops-tools` · `ops-keys` · `ops-usage` · `ops-responses` · `ops-builds` · `ops-agents` |
  | Agent runtime | `agent-runtime` (every AgentSelf and Ledger route) |

  *(Owner decision, 2026-09-28: the `Internal` group — `chat-internal`, `audio-lab-internal`, `image-lab-internal` — was removed along with `Group::Internal` itself; those routes are `UNDOCUMENTED` now, §4.2.)*

- **`operationId`:**
  - Routes: `{method}_{slug}`, where the slug is the OpenAPI path with `/ { } - .` turned into `_`, runs collapsed and the ends trimmed. For example `post_v1_chat_completions`, `get_api_agents_id_runs`.
  - Ops: `op_{name}`.
- **Responses:**
  - The documented success status (200, 204 or 302) with its media types.
  - A `default` response with the dialect's error schema: `OpenAiError`, `AnthropicError`, `LlamaCppError`, `JsonRpcError` or `ApiError`.
  - Response headers from §4.9 on every non-default response.

### 4.4 Extension keys (constants in `lmgw_api_types::openapi_ext`)

| Key | Where | Value |
|---|---|---|
| `x-lmgw-capability` | operation | `Cap::as_str()` |
| `x-lmgw-writes` | operation | bool (§4.2, §4.7) |
| `x-lmgw-reveals-secret` | operation | `true` where the answer carries a plaintext credential: `key_create`, `key_reveal`, `key_rotate`, `agent_token_get`, `agent_token_rotate` (§12.10) |
| `x-lmgw-confirm-note` | operation | extra sentence for the confirm (e.g. `DELETE /api/session`: "signs this browser out of the dashboard"; `key_rotate`: "rotating owner:dashboard signs every browser out") |
| ~~`x-lmgw-internal`~~ | operation | *(Owner decision, 2026-09-28: removed — the three mini-APIs are `UNDOCUMENTED` instead, §4.2. No longer a constant in `openapi_ext`.)* |
| `x-lmgw-op` / `x-lmgw-alias-of` | op operation | op name / `aux_model_set` on `embed_model_set` (also `deprecated: true`) |
| `x-lmgw-tool` | operation | `lmgw__X` sharing the code path |
| `x-lmgw-dialect` | operation | `openai`, `anthropic`, `llamacpp`, `jsonrpc`, `dashboard` |
| `x-lmgw-model-task` | operation | `chat`, `embedding`, `rerank`, `tts`, `asr`, `image_generation`, `image_edit` |
| `x-lmgw-endpoints` | operation | `lmgw.endpoints` groups |
| `x-lmgw-untyped` | response | why no schema exists |
| `x-lmgw-sse-events` | response | `{event name: schema ref}` |
| `x-lmgw-wildcard` | path param | `true` |
| `x-lmgw-secret` | property / param | `true` on every property or param named `token`, `api_key`, `hf_token`, `update_token` (applied by `build.rs`), and on the fields flagged where they are defined: `extra_headers`, `forge_tokens`, an MCP server's `env`/`headers`, agent config `values` (§12.9) |
| `x-lmgw-prefill` | header param | `true`: the tester fills in the example (`anthropic-version` on Anthropic routes, `Accept` on `/mcp*`) |
| `x-lmgw-audience` | header param | `client`, `agent`, `owner` |
| `x-lmgw-group` | tag | group name |
| `x-lmgw-principals` | root (admin document only) | `{owner, agent, key, anonymous, anonymous_auth_off: [caps]}` from `Principal::holds` on a default `Snapshot` with auth on/off |
| `x-lmgw-undocumented` | root (admin document only) | `[{method, path, reason}]` (§12.8) |

### 4.5 Schemas

- **The generator:**
  - `SchemaSettings::draft2020_12().for_deserialize()`, with `meta_schema = None` and `definitions_path = "/components/schemas"`, so refs read `#/components/schemas/Name`.
  - It is the dashboard's reading contract: `#[serde(default)]` fields are optional, and unknown keys are allowed unless `deny_unknown_fields`.
  - Worker: verify the setter names against schemars 1.2.
- **Hand-written schemas** register through `schemas::named(g, "Name", json_schema!({...}))`. It panics on a name that already exists with different content, so every component name is unique.
- **Untyped:** `schemas::untyped(why)` = `{"type":"object","x-lmgw-untyped": why}`.
- **The v1 document** is the admin document filtered to `x-lmgw-capability == "inference"` operations, then `prune_components` drops schemas nothing reaches.

### 4.6 `/api` GET reads → documented response schema

Rule, in order:

1. The api-types DTO the handler serializes.
2. The api-types DTO the dashboard decodes the handler's ad-hoc JSON into; the live test is what proves the pairing.
3. A core-private `Serialize` struct, with `JsonSchema` derived in core.
4. Otherwise untyped, with the reason.

| Route | Handler answers with | Dashboard decodes as | Documented schema | Live test (§7.2) |
|---|---|---|---|---|
| `/api/version` | `dto::VersionInfo` | — | `VersionInfo` | ✓ |
| `/api/session` | core `web::session::SessionView` | UI-private `SessionView` | core `SessionView` (derived) | ✓ |
| `/api/session/login` | 302 to `/` or `/?login=invalid` | — | Redirect | skip: redirect |
| `/api/status` | `ops::status` JSON | `GatewayStatus` | `GatewayStatus` | ✓ |
| `/api/connect` | `dto::ConnectInfo` | same | same | ✓ |
| `/api/logs` | `dto::LogsResponse` | same | same | ✓ |
| `/api/events` | SSE `stats` `jobs` `runtime` `vram` `mcp` `updates` `request` | live bus | `text/event-stream`; `x-lmgw-sse-events`: `StatsView`, `[JobRow]`, `[RuntimeStatus]`, `VramStatus`, `[McpStatus]`, `UpdatesSummary`, `RequestRow` | ✓ first 6 frames |
| `/api/models/full` | ad-hoc JSON | `ModelsFull` | `ModelsFull` | ✓ |
| `/api/local-model` | `ops::local_model_get` | `LocalModelDetail`; `target=image` → UI-private `ImageModelDetail` | `LocalModelDetail`; description marks the image shape untyped | ✓ fixture chat row |
| `/api/local-model-check` | ops JSON | not read | untyped: "no DTO; read by lmgw__local_model_check" | skip: untyped |
| `/api/gguf-files` | modelinfo JSON | `GgufFiles` | `GgufFiles` | ✓ |
| `/api/model-inspect` | modelinfo JSON | not read | untyped | skip: untyped |
| `/api/local-model-plan` | modelinfo JSON | `PlanResult` | `PlanResult` | skip: needs a GGUF file |
| `/api/ladder-rung-plan` | ops JSON | `RungPlan` | `RungPlan` | skip: needs a GGUF file |
| `/api/llama-flags` | ops JSON | not read | untyped | skip: untyped |
| `/api/upstreams` | `dto::UpstreamsResponse` | same | same | ✓ |
| `/api/wiring` | `dto::WiringView` | same | same | ✓ |
| `/api/mcp-servers` | `ops::mcp_servers` | `McpServersResponse` | same | ✓ |
| `/api/mcp-servers/{id}/tools` | ops JSON | `McpServerToolsResponse` | same | skip: needs a connectable server |
| `/api/tools` | `ops::tools` | `ToolInventory` | same | ✓ |
| `/api/upstream-models` | `dto::UpstreamModelsResponse` | same | same | ✓ wiremock `/models` |
| `/api/hf/repo` | `ops::hf_repo` | `RepoFiles` | same | skip: HF hub |
| `/api/hf/downloads` | `ops::hf_downloads` | `DownloadsView` | same | ✓ |
| `/api/jobs` | `ops::jobs_list` | not read (SSE instead) | `JobsView` | ✓ |
| `/api/vram` | core `VramView` | `VramStatus` | `VramStatus` | ✓ |
| `/api/audio/catalog` | `dto::AudioCatalog` | same | same | ✓ |
| `/api/settings-full` | ad-hoc JSON | `SettingsFull` | `SettingsFull` | ✓ |
| `/api/responses` | ad-hoc JSON | `ResponsesIndex` | same | ✓ |
| `/api/responses/chain` | ad-hoc JSON | `ChainDetail` | same | skip: needs a stored response |
| `/api/usage/series`, `top`, `heat`, `errors`, `local`, `keys`, `prices` | `dto::Usage*Response`, `KeysResponse`, `PricesResponse` | same | same | ✓ each |
| `/api/usage/export.csv` | `text/csv` attachment | download | Binary `text/csv` | content type only |
| `/api/agents` | `Vec<dto::AgentCard>` | same | same | ✓ |
| `/api/agents/{id}` | `dto::AgentDetail` | `Value` | `AgentDetail` | ✓ seeded agent |
| `/api/agents/{id}/export` | JSON attachment | download | untyped `application/json` | skip: untyped |
| `/api/agents/{id}/runs` | `Vec<dto::AgentRunSummary>` | same | same | ✓ seeded agent |
| `/api/agents/runs/{job_id}` | `dto::AgentRunDetail` | same | same | skip: needs a run |
| `/api/docs/corpora` | ad-hoc JSON | `DocsOverview` | same | ✓ |
| `/api/docs/corpora/{id}` | ad-hoc JSON | not read | `CorpusDetail` | skip: needs a corpus |
| `/api/docs/corpora/{id}/documents`, `/api/docs/chunks`, `/api/docs/eval`, `/api/docs/golden`, `/api/docs/golden/candidates` | ad-hoc JSON | `DocumentsResponse`, `ChunksResponse`, `EvalHistory`, `GoldenResponse`, `GoldenCandidatesResponse` | same | skip: needs a corpus |
| `/api/docs/requests` | ad-hoc JSON | `DocRequestsResponse` | same | ✓ |
| `/api/docs/export` | SQLite file | download | Binary | content type only |
| `/api/docs/export/manifest` | `portability::manifest` | `ExportManifest` | same | ✓ |
| `/api/openapi.json` | this document | this page | Doc | structural test (§7.1) |

*(Owner decision, 2026-09-28: the `/chat/api/*`, `/audio-lab/api/*` and `/image-lab/api/*` rows that used to sit here — documented `untyped (internal)` — are `UNDOCUMENTED` instead, §4.2; they carry no schema at all now, typed or otherwise.)*

**Reads with no DTO today:**

- `local-model-check`, `model-inspect`, `llama-flags`, `agents/{id}/export`, and `local-model?target=image`.

They are documented untyped, with the reason shown on the page, and listed in §11.

### 4.7 The op plane

**Enumeration (`web/op_names.rs`)** has four lists, 74 names:

- `MAIN_OPS`: the 49 arms of `op()`'s match.
- `AGENT_OPS`: 19.
- `KEY_OPS`: 5.
- `SETTINGS_OPS`: `settings_set_full`.

The dispatcher refuses any name not in them *before* it looks at the arms, so an op that is not in the lists is unreachable. The dispatch itself is unchanged: the same arms, and the same messages.

**`ops/table.rs`:**

```rust
pub(crate) struct OpDoc { name, tag, summary, description: Option<&str> /* None → tool's */,
  tool: Option<&str>, args: OpArgs /* Struct(SchemaFn) | Tool | Hand(fn()->Value) | NoArgs | AliasOf(&str) */,
  response: Resp /* Json(SchemaFn) | OpOutcome | Untyped(why) */, writes: bool,
  reveals_secret: bool, confirm_note: Option<&str>, deprecated: bool, example: Option<&str /* JSON */> }
```

- **`merge.rs`:** when `args` is `Struct` and a tool exists, the tool's property descriptions override the struct's doc comments property by property. The tool description becomes the op description unless one is given.
- **Responses:** the typed Backends ops use their api-types response. An op that answers `{ok, message?, id?}` uses `OpOutcome` (api-types `models.rs:396`, extra keys allowed). WP5 reads each op's final `Ok(..)` to choose.

**A. Tool and op share name and code path (21).** Argument source is shown per row.

| Op | Tool | Args |
|---|---|---|
| `upstream_set`, `model_set`, `mcp_server_set`, `settings_set`, `aux_model_set`, `image_model_set` | `lmgw__<same>` | Struct: `UpstreamPatch`, `AliasPatch`, `McpServerPatch`, `SettingsPatch`, `AuxModelPatch`, `ImageModelPatch` |
| `price_delete`, `prices_sync`, `hf_add`, `hf_set`, `container`, `hold_set`, `image_recipes`, `image_recipe_add`, `local_model_test` | same | Tool (the dispatcher reads the same names with `arg_*`) |
| `build_run`, `container_image_delete`, `container_image_pull` | same | Struct: `BuildRunArgs`, `ContainerImageDeleteArgs`, `ContainerImagePullArgs` |
| `agent_set`, `agent_install`, `agent_delete` | same | Tool (`agent_set.manifest` also accepts an object: stated in the description) |

**B. Same name, different arguments (9).** The struct is the documented shape. These rows are the entries of `DIVERGENCES`.

| Op | Divergence |
|---|---|
| `local_model_set` | `ladder`: tool = JSON-encoded string, op = array of `Rung` |
| `candidate_alias_set` | op's `action` also takes `preview` |
| `price_set` | `scope_kind` required by the tool, defaults to `alias` in the op (Hand args) |
| `builds` | op takes no arguments (`BuildsResponse`); the tool's `id/limit/before` are the op `build_get` |
| `build_set` | op = `BuildSetArgs`; tool = the flat `BuildPatch` (`ops/backends.rs:983`) |
| `build_check_merge` | op also takes `spec` |
| `container_images` | op takes `disk`; the tool forces `true` |
| `forge_prs` | tool accepts `id` (a build) instead of `repo_url` + `forge` |
| `agent_run` | op also takes `rows`, `base_job`, `values` (Hand args) |

**C. Tools with no op (24).** They are not op docs. Where a GET route calls the same ops function, the route carries `x-lmgw-tool` and the tool's prose.

| Tool | GET route (same function) |
|---|---|
| `lmgw__status` | `/api/status` |
| `lmgw__mcp_servers` | `/api/mcp-servers` |
| `lmgw__local_model_get` | `/api/local-model` |
| `lmgw__local_model_check` | `/api/local-model-check` |
| `lmgw__gguf_files` | `/api/gguf-files` |
| `lmgw__model_inspect` | `/api/model-inspect` (the route's `path` is the tool's `gguf_path`) |
| `lmgw__local_model_plan` | `/api/local-model-plan` (same rename) |
| `lmgw__llama_flags` | `/api/llama-flags` |
| `lmgw__hf_repo` | `/api/hf/repo` |
| `lmgw__hf_downloads` | `/api/hf/downloads` |
| `lmgw__models`, `upstreams`, `logs`, `settings`, `usage`, `prices`, `docs_corpora`, `docs_requests`, `agents`, `agent_get`, `build_log`, `docs_corpus_set`, `docs_ingest`, `docs_request_set` | none: a different shape from the dashboard route, or MCP-only |

**D. Dispatcher-only ops (44), with their argument source.**

| Op | Args | Writes |
|---|---|---|
| `embed_model_set` | AliasOf `aux_model_set` (deprecated) | ✓ |
| `audio_model_set` | Struct `web::api::AudioPatch` | ✓ |
| `model_visibility` | Struct `ModelVisibilityPatch` | ✓ |
| `alias_set` | Struct `AliasFullPatch` | ✓ |
| `upstream_set_full` | Struct `UpstreamFullPatch` | ✓ |
| `settings_set_full` | Struct `api_settings::SettingsFullPatch` | ✓ |
| `key_set` | Struct `ops::keys::KeyPatch` | ✓ |
| `build_get`, `build_resolve`, `build_run_log`, `forge_refs`, `forge_pr`, `build_updates_check`, `container_image_pull_status` | Struct api-types `*Args` | — |
| `build_promote`, `build_verify`, `container_image_tag` | Struct api-types `*Args` | ✓ |
| `build_env`, `update_check` | NoArgs | — |
| `agents_restore` | NoArgs | ✓ |
| `tool_set` | Hand `{name*, enabled*}` | ✓ |
| `job_cancel` | Hand `{id*: int}` | ✓ |
| `response_chain_delete` | Hand `{chain_id*}` | ✓ |
| `responses_gc` | Hand `{scope: "rules"\|"all" = rules}` | ✓ |
| `upstream_test` | Hand `{id*: int}` | — |
| `audio_catalog` | Hand `{action*: refresh\|download, family, package}` | ✓ |
| `key_create` | Hand `{name*, kind: client\|owner = client}` | ✓ |
| `key_delete`, `key_rotate` | Hand `{id*: int}` | ✓ |
| `key_reveal` | Hand `{id*: int}` | — (reveals secret) |
| `agent_duplicate` | Hand `{id*, new_id*, name}` | ✓ |
| `agent_config_set` | Hand `{id*, values*: object, clear}` | ✓ |
| `agent_dev_url_set` | Hand `{id*, url}` | ✓ |
| `agent_enable` | Hand `{id*, enabled*}` | ✓ |
| `agent_open_chat` | Hand `{id*, values: object}` | ✓ |
| `agent_service_log` | Hand `{id*, lines: int}` | — |
| `agent_token_get` | Hand `{id*}` | — (reveals secret) |
| `agent_pull`, `agent_reimport`, `agent_reset`, `agent_run_cancel`, `agent_token_rotate`, `agent_service_start`, `agent_service_stop` | Hand `{id*}` | ✓ |

(`*` = required.) A worker writing a Hand schema reads the arm or function body and documents exactly the keys it reads.

**`writes`:**

- Table A and B ops take the tool's flag. The test allows an explicit exception list.
- Table D: as shown.
- Read ops (`builds`, `build_get`, `build_resolve`, `build_run_log`, `build_env`, `forge_*`, `container_images`, `container_image_pull_status`, `image_recipes`, `update_check`, `upstream_test`, `agent_service_log`, `build_updates_check`) are `false`.

### 4.8 `/v1` hand-written schemas (inventory)

Written from what lmgw's parsers read and its serializers emit, not from provider docs:

- `ingress/openai.rs`;
- `ingress/anthropic.rs:14, 366`;
- `ingress/responses.rs:146, 1304`;
- the `proxy/*` handlers.

`additionalProperties: true` wherever lmgw forwards unmodeled keys.

| Route | Modeled request fields | lmgw-specific / extensions | Response | Dialect, `model_task`, `endpoints` |
|---|---|---|---|---|
| `POST /v1/chat/completions` | `model`, `messages` (roles `system`/`developer`→system, `user`, `assistant`, `tool`/`function`; content string or parts `text`, `image_url` (URL or `data:` URI), `input_audio {data, format}`; assistant `reasoning_content`/`reasoning` replayed, `tool_calls`; tool `tool_call_id`, `name`), `tools` (function), `tool_choice`, `stream`, `temperature`, `top_p`, `top_k`, `max_completion_tokens` (wins) / `max_tokens`, `presence_penalty`, `frequency_penalty`, `seed`, `stop` (string/array), `reasoning_effort` | Read and forwarded: `reasoning {effort, max_tokens, enabled, exclude}`, `reasoning_budget_tokens`, `thinking_budget_tokens`, `chat_template_kwargs` (`enable_thinking` read, rest verbatim). Any other key is forwarded verbatim to openai-protocol upstreams (`response_format`, `grammar`, `n`, `n_predict`, `min_p`, …). `n_predict` is folded into `max_tokens` on guarded or ladder rows. `model` may be an alias, a local name, a candidate alias, or `prefix/…` passthrough. Request headers: the reasoning trio | `ChatCompletion`; SSE `ChatCompletionChunk` frames + `[DONE]` | openai, chat, [openai] |
| `POST /v1/messages` | `model`, `messages` (user/assistant; string or blocks `text`, `image {source base64\|url}`, `tool_use`, `tool_result`, `thinking` replayed; `redacted_thinking` dropped), `system` (string/text blocks), `tools` (custom only; server tools → 400), `tool_choice {auto\|any\|none\|tool}`, `temperature`, `top_p`, `top_k`, `max_tokens` (optional: defaulted with `x-lmgw-max-tokens-defaulted`), `stop_sequences`, `stream`, `thinking {enabled+budget_tokens\|disabled\|adaptive}`, `output_config.effort` | nothing forwarded beyond the modeled keys; `anthropic-version` header (prefill) | `AnthropicMessage`; SSE events | anthropic, chat, [anthropic] |
| `POST /v1/messages/count_tokens` (new) | §5.2 | §5.2 | `{input_tokens}` | anthropic, chat, [anthropic] |
| `POST /v1/responses` | `model`, `input` (string or items: message, `function_call`, `function_call_output`, `mcp_call`, approval responses), `instructions`, `tools` (function; `mcp {server_label, allowed_tools, require_approval}`), `tool_choice`, `stream`, `store`, `metadata`, `temperature`, `top_p`, `top_k`, `max_output_tokens`, `max_tool_calls`, `parallel_tool_calls`, `previous_response_id`, `reasoning {effort}`, `text {format}`, `include` | refused: `background: true`, `truncation: "auto"`, hosted tools; MCP tools run server-side on lmgw's registered servers; unmodeled keys forwarded | `Response`; `response.*` SSE | openai, chat, [openai] |
| `GET/DELETE /v1/responses/{id}`, `GET …/input_items` | — | stored responses (`crate::responses`) | response / deletion object / item list | openai, —, [openai] |
| `POST /v1/completions` | `model`, `prompt` (string, strings, token arrays, mixed), `stream` | body forwarded verbatim to openai-protocol upstreams only; `n_predict`/`max_completion_tokens`/`max_tokens` folded on guarded rows | `text_completion`; SSE | openai, chat, [openai] |
| `POST /v1/embeddings` | `model`, `input` (string/array) | only `model` and `input` are forwarded (§0.10) | embeddings list | openai, embedding, [openai] |
| `POST /v1/rerank` | `model`, `query`, `documents` \| `texts`, `top_n` | Jina and TEI shapes; embedding rows refused | Jina `results` | openai, rerank, [other] |
| `POST /v1/count_tokens` | `{model, input: string}` | lmgw's own; `x-lmgw-count-approximate` | `{model, tokens}` | openai, chat, [other] |
| `POST /tokenize` (new) | §5.3 | §5.3 | `{tokens}` | llamacpp, chat, [other] |
| `POST /v1/audio/speech` | OpenAI TTS (`model`, `input`, `voice`, `response_format`, `speed`, `instructions`, `language`) + audio.cpp request options | forwarded verbatim, model rewritten; row voice presets apply | audio bytes (upstream content type) | openai, tts, [openai] |
| `POST /v1/audio/transcriptions` | multipart (`file`, `model`, `language`, `prompt`, `response_format`, `temperature`) or JSON | same | JSON `{text,…}` | openai, asr, [openai] |
| `POST /v1/audio/transcriptions/details` | as above | lmgw route: words, segments, speaker turns | JSON | openai, asr, [openai] |
| `POST /v1/audio/alignments` | multipart `file`, `model`, `text`, `language` | lmgw route, `task: "align"` rows | JSON | openai, —, [openai] |
| `GET /v1/audio/voices?model=` | `model` query | lmgw route | voices and presets | openai, tts, [openai] |
| `POST /v1/images/generations` | `model`, `prompt` (optional `<sd_cpp_extra_args>{…}</sd_cpp_extra_args>`), `n`, `size`, `output_format`, `output_compression`; other keys for cloud | api-types `image_lab.rs` builder is the reference | `{created, data:[{b64_json}]}` | openai, image_generation, [openai] |
| `POST /v1/images/edits` | multipart `image` (1..n), `mask`, `model`, `prompt`, `n`, `size` | refused unless the row's `edit` flag | same | openai, image_edit, [openai] |
| `POST /v1/tasks/run`, `/v1/tasks/stream` | `{model, request: {…}}` | audio.cpp generic tasks, `request` relayed untouched | audio.cpp JSON / stream | openai, —, [other] |
| `GET /v1/models` | — | `anthropic-version` selects the Anthropic shape | `oneOf [OpenAiModelList, AnthropicModelList]`; model object `capabilities` = derived `ModelCapabilities` | openai, —, [openai, anthropic] |
| `GET /v1/models/{id}` | id (`x-lmgw-wildcard`) | — | model object (either shape) | openai, —, [openai, anthropic] |
| `GET /v1/openapi.json` (new) | — | this description, inference subset | Doc | openai, —, [other] |
| `POST /mcp` | JSON-RPC 2.0 (`initialize`, `tools/list`, `tools/call`, …); header `Accept: application/json, text/event-stream` (prefill); `Mcp-Session-Id` after `initialize` | the registered servers' tools, namespaced `<prefix>__` | JSON-RPC or SSE | jsonrpc, —, [] |
| `GET /mcp`, `DELETE /mcp` | session stream / end session | — | SSE / 200 | jsonrpc |
| `POST /mcp/admin` (Admin) | JSON-RPC, the `lmgw__*` tools | `adminToken` scheme too | JSON-RPC | jsonrpc |
| `GET/DELETE /mcp/admin` | as `/mcp` | — | — | jsonrpc |

Error schemas (`v1/errors.rs`):

- `OpenAiError {error: {message, type, param, code}}` (`error.rs:495`).
- `AnthropicError {type: "error", error: {type, message}}` (`:507`).
- `LlamaCppError` (§5.3).
- `JsonRpcError`.

### 4.9 The header table (`openapi/headers.rs`)

```rust
pub enum Direction { Request, Response }
pub enum Audience { Client, Agent, Owner, Internal }
pub enum Scope { Routes(&'static [(&'static str, &'static str)]), AllInference, None }
pub struct LmgwHeader { pub name: &'static str /* the const where it lives */, pub direction: Direction,
  pub audience: Audience, pub scope: Scope, pub schema: HeaderSchema /* Enum(&[..]) | Integer | Text */,
  pub description: &'static str /* the /v1/models prose */ }
```

| Header | Dir | Audience | Scope |
|---|---|---|---|
| `x-lmgw-reasoning`, `-effort`, `-budget` | req | client | chat/completions, messages, messages/count_tokens, responses (prose updated to name all four) |
| `x-lmgw-run` | req | agent | AllInference |
| `x-lmgw-admin-token` | req | owner | `POST /mcp/admin` (rendered as the `adminToken` scheme, not a parameter) |
| `x-lmgw-face` | — | internal | None ("set by lmgw on requests it proxies to an agent app; never read from clients") |
| `x-lmgw-fallback`, `-fallback-reason`, `-candidate` | resp | client | `GATED`: chat/completions, messages, messages/count_tokens, responses (POST), completions, embeddings, rerank, count_tokens, tokenize, audio/speech, audio/transcriptions, …/details, audio/alignments, audio/voices, images/generations, images/edits, tasks/run, tasks/stream |
| `x-lmgw-reasoning-ignored`, `-max-tokens-defaulted`, `-max-tokens-raised` | resp | client | chat/completions, messages, responses |
| `x-lmgw-max-tokens-clamped`, `x-lmgw-rung` | resp | client | chat/completions, messages, responses, completions |
| `x-lmgw-count-approximate` (new) | resp | client | count_tokens, messages/count_tokens |

- **`headers_block()`** returns the `{name: description}` map of `Audience::Client` rows. That is exactly today's twelve headers plus the new one, with today's prose moved verbatim; only the reasoning trio gains the fourth route.
- **The new header's prose:** "RESPONSE: on the token counters, why the number is not exactly what the backend would count for this request, comma-separated: `flattened` (the request's structure was counted as plain text — chat template, tool-definition and per-message overhead are not included, so the real prompt is larger), `tokenizer_guess` (the backend's tokenizer is unknown; counted with tiktoken o200k_base), `media_bound` (images counted at the model's per-image upper bound), `media_omitted` (image or audio parts are not in the number), `message_framing` (/v1/count_tokens: the backend counts messages, so the text was counted as one user message, framing included). Absent when the count is exact."

### 4.10 Examples

- **`/v1` and `/api` bodies:** hand-written `example` fns in the registry. `model` holds `"<pick a model>"`, which the tester replaces.
- **Ops:** an explicit JSON example in the table, or one generated from the argument schema:
  - each required property: first `enum` value, `"<name>"`, `1`, `false`, `{}` or `[]` by type;
  - plus `action` first when present.
- Every example must validate against its schema (§7.1).

### 4.11 Serving, caching, `lmgw.endpoints`

- **Caching:** `admin_doc()` and `v1_doc()` are built once into `OnceLock<Value>` and return `&'static Value`. Everything they read is static, and `holds` is evaluated on synthetic principals.
- **Handlers:** each serializes its document once into shared `Bytes` and answers them with `Content-Type: application/json`, `Cache-Control: no-cache` and a weak `ETag` (an `If-None-Match` hit is `304`); both routes carry tower-http's compression layer (§12.11).
- **`lmgw_endpoints()`** reads the inference plane's `DocRoute`s only, without building schemas. For each group (`openai`, `anthropic`, `other`) it lists the OpenAPI paths in registry order, deduplicated.
  - This newly lists `/v1/responses/{id}`, `/v1/responses/{id}/input_items`, and `/v1/openapi.json` under `other`.
  - `/tokenize` and `/v1/messages/count_tokens` stay listed and now exist.

## 5. The compatibility counters

### 5.1 Shared: `Count`, `Approx`, the header, the scope check

**`proxy/count.rs`** holds the code moved from `legacy.rs:415–540`, unchanged in behaviour except as listed below. Additions:

```rust
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Approx { Flattened, TokenizerGuess, MediaBound, MediaOmitted, MessageFraming } // as_str: snake_case
pub struct Count { pub tokens: u64, pub approx: Vec<Approx> }                          // sorted, deduplicated
pub(crate) fn approx_header(a: &[Approx]) -> Option<HeaderValue>                       // "a,b"; None when empty
pub(crate) async fn count_text_route(state, input: &str, route: &Route, hold: Option<&LocalHold>) -> Result<Count, GatewayError>
```

- **`CountPlan` and the flags it maps to:**
  - `CountPlan::Guessed(u64)` is new. `build_count_tokens` returns it when `tiktoken_rs::bpe_for_model` fails.
  - `Ready(n)` → no flag.
  - `Guessed(n)` → `TokenizerGuess`.
  - `Request` → no flag, except `MessageFraming` on the Anthropic and Gemini protocols.
- **`count_tokens_inner`** keeps its signature `(u64, GateHeaders)` for quickdoc.
- **`handle_count_tokens`** stamps the header.
- **Scope check** *(amended after review, §12 entry 15)*. `counter_policy` (`proxy/count.rs`) runs `policy::check_scope` — the key's alias scope, no budget — and on a refusal writes its row through `recording.rs`'s `record_refusal` (class `Chat`), which `policy_or_refuse` shares. Each counter renders the refusal in its own dialect: OpenAI, Anthropic, llama.cpp.

### 5.2 `POST /v1/messages/count_tokens`

**Request.** Exactly what the Anthropic SDKs' `messages.count_tokens()` sends:

- `model`, `messages`, `system`, `tools`, `tool_choice`, `thinking`, `output_config` (and anything newer, forwarded where the body is passed through);
- headers `anthropic-version` and `x-api-key` (accepted as a key, fact 11).

The route is in the `json_api` group, so the `max_body_mb` bound applies.

**Flow** (`handle_messages_count_tokens(state, ctx, body)`), proto `AnthropicMessages`:

1. The `server.rs` wrapper: a body parse error gets an Anthropic-dialect 400 or 413 (`body_error`). A bad reasoning header gets 400 via `reasoning_or_400`.
2. `model` must be a string, else 400 `invalid_request_error`.
3. The key's scope via `counter_policy` (logged, Anthropic dialect); no budget (§12 entry 15).
4. The body is parsed into the IR, then `gate::resolve(state, alias, RouteCheck::Text("/v1/messages/count_tokens"))`, `.using(request_facets(..))` when it parsed, and `.admit(state)` (§12 entry 16). On failure: `f.headers.stamp(error_response(proto, &f.error))`.
   - A body that does not parse goes on only when the resolve landed on an Anthropic route; otherwise it is refused (400) before admission.
   - A local row that is not running is admitted and started, as for `/v1/count_tokens` today.
5. Count by route:

| Route | How | Approx |
|---|---|---|
| protocol Anthropic | `egress::anthropic::count_messages_request(http, up, upstream_model, &body)`: the client body verbatim with `model` replaced, POST `{base}/v1/messages/count_tokens`, `anthropic-version`, `x-api-key`, extra headers. Sent via `send_local(None, ..)`, parsed by `parse_count` (`input_tokens`). Server tools count fine here. | none |
| protocol Gemini | parse IR; `params = resolve_params(&ir, &ctx, &route)`; `egress::gemini::count_chat_request`: POST `{base}/v1beta/models/{m}:countTokens` with `{"generateContentRequest": {"model": "models/{m}", …gemini::request_body(ir, params)}}`; `parse_count` | none |
| protocol OpenAI, kind LlamaServer, `hold` is `Some` and `hold.class() == Chat` | parse IR; `params = resolve_params(..)`; `body = egress::openai::chat_body(&ir, upstream_model, &params, false, kind)`; `text = on_running_server(route, hold, |root| count_chat_prompt(http, &root, &body, MediaParts::default(), None))`.text_tokens. Images: `gate_facts()` → `image_token_bound(..)` `Ok(Some(b))` → `+ images × b.tokens`. Media left out of the number is left out of `body` too: all audio, and the images when there is no bound (§12 entry 17). | `MediaBound` when images were bounded; `MediaOmitted` when images had no bound or audio is present |
| everything else (remote llama-server, aux llama rows, generic OpenAI-compatible) | parse IR; `count_text_route(state, &flatten_for_count(&ir), route, hold)` | `Flattened` + the route's own flags (`TokenizerGuess`); `MediaOmitted` if any media |

**Details:**

- **IR parse errors** (a server tool on a non-Anthropic backend, or an unknown block) are 400 in the Anthropic dialect, before admission, with the headers the resolve settled (a hold's fallback).
- **`flatten_for_count(ir)`** joins these with `"\n\n"`, in order:
  - the system text;
  - each tool as `name\ndescription\n<schema JSON>`;
  - each message as `role: ` + its text and reasoning parts, `tool_use` as `name(<args JSON>)`, and `tool_result` text.
- **Response:** `200 {"input_tokens": N}`, with `headers.stamp(..)` (fallback, reason, candidate) and `x-lmgw-count-approximate` when there are flags.
- **Upstream errors** go through the egress `map_error` and are rendered in the Anthropic dialect.
- **Nothing is reserved** in a pool, nothing is clamped, and no rung is stamped.

### 5.3 `POST /tokenize`

**Registration** (`proxy/tokenize.rs`):

```
routes(state) = Router::new().route("/tokenize", post(tokenize))
  .layer(from_fn_with_state(state.clone(), crate::server::body_limit_mw))
  .layer(DefaultBodyLimit::disable())
  .route_layer(require(state, Cap::Inference))
```

`build_router` merges it (fact 6 / §0.6).

**Request** (llama.cpp's own `/tokenize`):

- `content`: a string, or a mixed array of strings and token ids;
- `add_special` (default false);
- `parse_special` (default true);
- `with_pieces` (default false);
- `model`: **required**, as llama-server's router mode requires it. A missing `model` gets 400 "'model' is required: lmgw routes /tokenize by model name, as llama-server's router mode does — see GET /v1/models".

**Flow:**

1. The body is read as JSON whatever its `Content-Type` says, as llama-server reads it (§12 entry 18). Not JSON → 400. A body limit tripped while streaming → 413.
2. `model` check.
3. `counter_policy` (the key's scope, no budget) → llama error.
4. `gate::open(.., RouteCheck::Text("/tokenize"))` → llama error, stamped.
5. If `protocol != OpenAI || kind != LlamaServer` → **501**, stamped:

   ```
   {"error":{"code":501,"type":"not_supported_error","message":"model '<m>' is served by <protocol>/<kind> upstream '<name>', which cannot return token ids; POST /v1/count_tokens counts tokens on every backend"}}
   ```

6. Forward the client object with `model` set to `upstream_model` and every other key verbatim.
   - `send_local(hold, route, route.upstream.request_timeout(), |r| Ok(egress::openai::tokenize_request(http, &r.upstream, &body)))`.
   - The upstream's status, JSON body and content type are relayed verbatim (success, and llama.cpp's own errors), with the gate headers stamped — except an upstream 401/403, lmgw's own credential failing, which is the 502 every other route gives it (§12 entry 18).
   - The hold is dropped after the body is read.

**The response** is llama.cpp's own: `{"tokens":[ids]}`, or with pieces `{"tokens":[{"id":n,"piece":"…" | [bytes]}]}`.

**`llama_error(status, message, lmgw_code)`:**

- Shape: `{"error":{"code":<status int>,"message":…,"type":…,"lmgw_code":<GatewayError::code()>}}`.
- `lmgw_code` keeps `gpu_hold` visible to clients that check it.
- Type by status:

  | Status | `type` |
  |---|---|
  | 400, 413, 422 | `invalid_request_error` |
  | 401 | `authentication_error` |
  | 403 | `permission_error` |
  | 404 | `not_found_error` |
  | 501 | `not_supported_error` |
  | 429, 503 | `unavailable_error` |
  | else | `server_error` |

- The shared `/v1` layers answer some refusals before this route runs, in their OpenAI dialect (same `error.message`, `error.type`, but a string `code`): the gate's 401/403/429 (credential, capability, rate) and the 413 for a declared `Content-Length` over `max_body_mb`. This is documented in `llama_error`'s doc, the `LlamaCppError` schema and the operation's description (§12 entry 18).

### 5.4 What is logged

| | request log / live feed |
|---|---|
| successful count (all three counters) | no row, no event (as `/v1/count_tokens` today, `legacy.rs:422`) |
| key scope refusal (all three; no budget applies, §12 entry 15) | one row (new for `/v1/count_tokens`) |
| gate-layer refusal (auth, rate) | one row (unchanged, `server.rs:1070`) |
| gate/upstream failure after admission | no row (as today) |

## 6. The API reference page

### 6.1 Route, nav, frame

- **Route:** `/api-reference`. Deep link `?op=<operationId>`; filter `?q=`. Both use `url_state::use_query_signal`.
- **Nav:** "Connect" group, third entry after MCP servers.
  - Label "API reference", title "API reference".
  - Icon: `<path d="M5.5 3C4 3 4 4 4 5v1.5C4 7.5 3 8 2.5 8 3 8 4 8.5 4 9.5V11c0 1 0 2 1.5 2M10.5 3C12 3 12 4 12 5v1.5c0 1 1 1.5 1.5 1.5-.5 0-1.5.5-1.5 1.5V11c0 1 0 2-1.5 2"/>`.
  - The name does not collide with "Docs" (doc corpora).
- **Frame:** `PageFrame` with:
  - `title="API reference"`;
  - `sub="lmgw's own HTTP API, read live from /api/openapi.json"`;
  - `mode=PageMode::Split`, `class="api-ref"`;
  - `actions` = identity picker + `<a class="btn ghost sm" href="/api/openapi.json" download="lmgw-openapi.json">` and the same for `/v1/openapi.json`;
  - `toolbar` = `FilterBar`: query over path, summary, op name and tag; facets **Group** (5) and **Method**; noun "operations".

### 6.2 Layout

```
.page.split.api-ref > .page-body
  nav.split-rail.api-rail     group eyebrow → tag .rail-head (count) → button.rail-item.op-item[data-op]
                              (method chip + path, or op name for ops; aria-current); "Not documented here" list
                              from x-lmgw-undocumented at the end (read-only, with reasons)
  div.split-pane
    div.rail-select           Select over operations (shown < 760px)
    div.action-bar.api-op-bar method chip · path (mono, wraps) · chips: capability, writes, reveals a secret,
                              deprecated, alias of X, same as MCP tool lmgw__X (§12.13), untyped · .cap-warn ·
                              spacer · [Copy as curl] [Stop] [Send (.btn.primary)]
                              (Owner decision, 2026-09-28: no "internal" chip — those operations
                              are UNDOCUMENTED: no rail item, only a row in the rail's closing
                              "Not documented here" list, with the reason — §12.8.)
    div.fill-pane.api-op      the one scroller
      div.api-op-grid
        section.api-doc       Section "Documentation" (persist "api_ref.doc"): description (ClampText),
                              parameters table, request body (schema tree), responses (status → schema tree,
                              response headers), SSE events
        section.api-try       identity notice, params, headers, model picker, body editor / multipart form /
                              raw file, extra headers; then the response view
```

- **One scroller.** `.fill-pane` is the only thing that scrolls in the pane. JSON and SSE blocks wrap (`white-space: pre-wrap; overflow-wrap: anywhere`) and never scroll on their own. The body `<textarea>` sizes its rows to its content (the matrix exempts textareas).
- **Breakpoints:**
  - `@container pane (width >= 1500px)`: `.api-op-grid` has two columns (doc | try).
  - Below that: one column, docs first.
  - `< 760px` is the existing split rule: the rail hides and `.rail-select` shows.
- **Rail width:** `.page.split.api-ref { --rail-w: clamp(240px, 24cqi, 380px); }`.
- **Overlays:** only the existing `Popover`-based `Select`/`ModelPicker` and the `ConfirmButton`. No dialogs are needed.

### 6.3 Files (`crates/lmgw-ui/src/pages/api_ref/`)

| File | Holds |
|---|---|
| `page.rs` | `ApiReference`: `Scope::new()`, fetches `/api/openapi.json` and `/api/connect` via `crate::api::get` (session), filter and selection state, per-op drafts `StoredValue<HashMap<String, Draft>>` (memory only), `on_cleanup` aborting every in-flight request and revoking blob URLs |
| `doc.rs` | `ApiDoc::parse(&Value)`: ops, tags, groups, principals, undocumented, components; `resolve_ref`. Pure. |
| `rail.rs` | rail + narrow `Select` |
| `detail.rs` | the documentation column |
| `schema_view.rs` | collapsible schema tree: type, required, enum, default, description, `$ref` resolved; a ref already on the path renders "↻ Name (recursive)" |
| `identity.rs` | `Identity {Session, Key, None}`, key probe, held capabilities, expectation (§6.5). Pure core, unit-tested. |
| `tester.rs` | the try column (§6.6) |
| `example.rs` | example extraction/generation, model get/set in a JSON text. Pure. |
| `send.rs` | request build + send + outcome (§6.7) |
| `stream.rs` | generic SSE record reader + assembled text. Pure splitter. |
| `response_view.rs` | status, timings, headers, body/player/download |
| `json_view.rs` | pretty JSON as spans (`--code-*` tokens) with folded long strings. Pure. |
| `curl.rs` | `curl_command(&Prepared, origin) -> String`. Pure. |

### 6.4 Doc model

- **Operations** come from `paths.*.*`.
- **Keys** are read via the `openapi_ext` constants.
- **Rail order:** tag group, then tag in doc order, then by path and method; ops by name.
- **Filter:** words match path, summary, `x-lmgw-op`, tag name.

### 6.5 Identity

- **Session** (default): the dashboard's cookie (`credentials: same-origin`). Held capabilities = `x-lmgw-principals.owner`, since the cookie resolves to owner rows only.
- **API key:** a `type=password` input (`autocomplete=off`, `spellcheck=false`), held in a page signal only.
  - Never persisted, never put in a URL, a toast, the console or the curl.
  - "Check" (and automatically before the first send) probes `GET /api/session` with `Authorization: Bearer <key>` and `credentials: omit`.
  - The probe gives `{authenticated, kind, name}` → `x-lmgw-principals[kind]`, shown as "client key 'name'", "agent key", "owner key", or "matches no key".
- **No credential:** `credentials: omit`. Held capabilities = `anonymous` or `anonymous_auth_off`, per `/api/connect.auth_enabled`.
- **Expectation:** if the op's `x-lmgw-capability` is not held, show `.chip.warn.cap-warn`: "expect 401 — no credential" (anonymous, or a key that matches nothing), or "expect 403 — <kind> does not hold <cap>". Sending stays enabled.

### 6.6 Request editor

- **Path params:** inputs. Values are `encodeURIComponent`-ed, except `x-lmgw-wildcard` params, which are encoded segment by segment so `/` stays.
- **Query params:** inputs from the schema. Booleans are tri-state (unset/true/false). Empty means omitted.
- **Header params:**
  - the op's documented `x-lmgw-*` request headers, with a `Select` for enum schemas;
  - `x-lmgw-prefill` params filled from their example;
  - `agent`-audience params folded under "More headers".
  - A free "Extra headers" textarea takes `Name: value` lines.
- **Model:** when `x-lmgw-model-task` is set, a `ModelPicker` with `tasks` mapped to a static slice (unknown → `&[]`) sits above the body.
  - It writes the top-level `model` of the JSON body text, re-pretty-printed; follows the text when it parses; is disabled with a hint while the text is not valid JSON.
  - For multipart it is the `model` field; for GET it is the `model` query param.
- **Body by content type:**
  - `application/json`: a monospace textarea, prefilled with the example; "Reset to example". Must parse before sending, with an inline error.
  - `multipart/form-data`: one input per schema property; properties with `contentMediaType` are file inputs (`multiple` for arrays).
  - Raw (`Req::Raw`): one file input + a content-type field defaulting to the file's type.

### 6.7 Sending and confirm

- **Send button:**
  - `x-lmgw-writes` or `x-lmgw-reveals-secret` → `ConfirmButton` (`class="btn primary"`, `confirm="Send <METHOD> <path>?"` + confirm note).
  - Otherwise a plain `button.btn.primary.tester-send`.
- **`send.rs`:**
  - `gloo_net::http::RequestBuilder::new(url).method(..).credentials(SameOrigin | Omit).abort_signal(..)`; `Authorization: Bearer <key>` only for Key.
  - Never `crate::api`, so a 401 does not lock the dashboard.
  - Times: `t0 = js_sys::Date::now()`, headers-at, done-at.
- **By response `Content-Type`:**
  - `text/event-stream` → live frames (`stream.rs`) into a signal; assembled text for OpenAI `choices[0].delta.content`, Anthropic `content_block_delta.delta.text`, and `/v1/responses`' `response.output_text.delta` (the chat mini-API's `delta` event went with the internal drop, review R3 #7). Stop aborts.
  - JSON (`application/json`, `+json`) → parsed and pretty-printed; parse failure → text.
  - Other `text/*` (not csv) → text.
  - Everything else, or `Content-Disposition: attachment` → bytes → Blob URL → `<audio controls class="resp-audio">` for `audio/*`, `<img>` for `image/*`, plus `a.resp-download` (filename from `Content-Disposition`, else `<operationId>.bin`) with the size.
  - Transport failure → an error line.
- **Blob URLs** are revoked on replacement and on cleanup.
- **In-flight requests** belong to their op's draft; switching ops does not abort them. Leaving the page aborts all.

### 6.8 Response view

- `.resp-status` chip (ok/warn/err by class).
- "headers in X ms · done in Y ms".
- `.resp-headers` table with `x-lmgw-*` rows first and highlighted (`tr.lmgw`).
- The body:
  - `.resp-json` (json_view; strings over 2,000 chars folded as "… (n chars) — show", expandable, never truncated);
  - `.resp-sse` (`.frame` list + `.assembled`);
  - the player and download.

### 6.9 Copy as curl

The button copies (via the `CopyBtn` clipboard code) and shows the command in `pre.curl-preview` below the bar:

```
curl -sS [-N] -X POST "$ORIGIN_LITERAL/v1/chat/completions?…" \
  -H "Authorization: Bearer $LMGW_KEY" \        # Session and Key identities; none for "No credential"
  -H 'Content-Type: application/json' -H 'x-lmgw-reasoning: off' \
  --data-binary @- <<'LMGW_JSON'
{ … }
LMGW_JSON
```

- **The origin** is `location.origin`, written literally.
- **Quoting:** values single-quoted, `'` → `'\''`. The heredoc is quoted, so nothing in the body expands.
- **`-N`** when the request asks for a stream (`stream: true`), and on SSE routes.
- **Multipart:** `-F 'field=value'`, and `-F 'file=@<picked file name>'`.
- **Raw bodies:** `--data-binary @<file name>`.
- **Secrets:** JSON body properties and query params marked `x-lmgw-secret` are replaced by the literal `"<secret: name>"`. A leading `# export LMGW_KEY=…; replace <secret: …> placeholders` comment line is added when any placeholder is present.
- **Never included:** the pasted key, the cookie.

### 6.10 CSS (one new section at the end of `app.css`)

Existing tokens only (Breeze graphite + blue; no new colours):

- `.method` chip, per method: `get` `--accent`/`--accent-bg`, `post` `--ok`/`--ok-bg`, `delete` `--err`/`--err-bg`, `any` `--text-2`/`--surface-hi`.
- `.op-item` (method chip + ellipsized mono path).
- `.api-op-bar` (wraps).
- `.api-op-grid` + its `@container pane (width >= 1500px)` rule.
- `.resp-headers tr.lmgw td` in `--accent`.
- `.json .k/.s/.n/.b/.z` → `--code-key/-str/-num/-type/-meta`.
- `.resp-sse .frame` hairlines in `--border`.
- The `--rail-w` override.

## 7. Testing

### 7.1 Drift guards and structure (`tests/it/`, files created empty in WP1)

| File | Test | Asserts |
|---|---|---|
| `openapi_coverage.rs` (WP8) | `every_capability_row_is_documented_or_excluded` | each `CAPABILITY_TABLE` row is exactly one of: a documented operation (path normalized `{*x}`→`{x}`), `POST /api/op/{name}`, an `UNDOCUMENTED` entry |
| | `every_documented_operation_is_a_capability_row` | reverse direction; op paths map to `/api/op/{name}`; `x-lmgw-capability` equals the row's |
| | `exclusions_are_real_rows_with_reasons` | every `UNDOCUMENTED` entry is a table row with a non-empty reason |
| | `served_docs_follow_their_capabilities` | `/api/openapi.json`: owner 200, client 403, anon (auth on) 401; `/v1/openapi.json`: client 200, only inference ops, no `/api` path, no unreachable component |
| | `lmgw_endpoints_are_documented_inference_paths` | every path in `/v1/models` `lmgw.endpoints` is a v1-doc path, and every v1 op with a group appears |
| | `every_typed_extractor_is_referenced_by_the_doc` | source scan of `src/web`, `src/server.rs`, `src/mcp/ingress.rs`: each `Query<X>`/`Json<X>` (not `Value`/`Args`) names a type that appears in `src/openapi/**` |
| | `every_example_validates_against_its_schema` | all JSON request examples (and generated op examples) validate (`jsonschema`, root = `{"$ref": …, "components": doc.components}`) |
| `openapi_ops.rs` (WP1 + WP5) | `op_names_match_the_dispatcher_arms` (WP1) | source scan of `op()` (`web/api.rs`), `api_agents::op`, `api_settings::key_op`: string arms `"x" =>` / `"x" \|`, and `name == "x"` equal `MAIN_OPS`/`AGENT_OPS`/`KEY_OPS`/`SETTINGS_OPS` |
| | `an_unlisted_op_is_refused_before_dispatch` (WP1) | `POST /api/op/route-walk-probe` → 400 `op_failed` with the gate's own message, `op_names::refuse_unlisted` (§12.12) |
| | `every_listed_op_is_documented_and_vice_versa` (WP5) | doc `x-lmgw-op` set == `op_names::all()` |
| | `tool_and_op_arguments_agree_except_listed_divergences` (WP5) | for ops with `x-lmgw-tool`: tool props ⊆ op props, except `DIVERGENCES`; every divergence is still true (not stale) on each argument it names (§12.6) |
| | `writes_agree_with_the_tool` (WP5) | op `x-lmgw-writes` == tool `writes`, except a listed set |
| `openapi_headers.rs` (WP2) | `every_x_lmgw_literal_is_in_the_header_table` | scan of `src/**/*.rs` (minus `tests.rs` and `#[cfg(test)]` tails) for `"x-lmgw-[a-z0-9-]+"` == `LMGW_HEADERS` names, both ways |
| | `the_models_block_lists_the_client_headers` | `/v1/models` `lmgw.headers` keys == client-audience rows |
| `openapi_v1.rs` (WP6) | per-route checks | Anthropic routes carry `anthropic-version` prefill; `/v1/models/{id}` param is wildcard; `/tokenize` is under `llamacpp` |
| `openapi_dashboard.rs` (WP4) | plane checks | every documented `/api` route of the plane is a table row; each `query` schema has the handler's field names |

Core unit tests (`src/openapi/build.rs` `#[cfg(test)]`, WP1):

- `doc_is_structurally_sound`:
  - operationIds unique;
  - every `$ref` resolves;
  - every path template param is declared, and vice versa;
  - every tag is declared;
  - every op has a non-default response;
  - every security scheme exists.
- `principals_matrix_matches_holds`.
- `untyped_responses_say_why`.
- `named_schema_collision_panics`.

`count.rs` unit tests: `approx_header_is_sorted_and_deduped`, `flatten_for_count_*`. `tokenize.rs` unit test: `llama_error_shape_by_status`.

`models_endpoint.rs` updates: the new header is documented (WP2); `/tokenize`, `/v1/messages/count_tokens` and `/v1/openapi.json` are listed (WP6).

### 7.2 Live validation (`tests/it/openapi_live.rs`, WP8)

- **`CASES: &[(&str /* operationId */, Case)]`**, where `Case = Get(&str /* path?query */) | Skip(&str /* reason */)`.
  - `every_get_has_a_case` checks both ways: the doc's GET operations (admin doc) == the case keys.
- **`every_get_case_validates`:** the owner GETs each `Get` case, expects 200, and validates the body against the 200 `application/json` schema. The expected set is §4.6's "Live" column.
- **Fixtures:**
  - a wiremock upstream answering `/models` and `/chat/completions`;
  - an alias;
  - one chat request sent through it (for logs and usage);
  - a stored local chat row (`/api/local-model?id=`);
  - the seeded agents (`agents::seed::restore` or `POST /api/op/agents_restore`).
- **`events_initial_frames_validate`:** read `/api/events` until 6 frames arrived; each validates against its `x-lmgw-sse-events` schema.
- **`v1_models_validates_in_both_dialects`:** with and without `anthropic-version`, list and `{id}`.
- **Binary cases** assert the content type only (`text/csv`, SQLite).

### 7.3 The compatibility counters (`tests/it/count_compat.rs`, WP3; wiremock, `e2e_proxy.rs` setup pattern)

| Test | Expectation |
|---|---|
| `messages_count_passes_through_to_anthropic` | wiremock `/v1/messages/count_tokens` receives the client's `system`, `tools`, `thinking` with `model = tgt-model` (`body_partial_json`, `.expect(1)`); answer `{"input_tokens": N}`; no approximate header |
| `messages_count_on_gemini_counts_the_whole_request` | `…:countTokens` gets `generateContentRequest` with `systemInstruction` and `tools`; `totalTokens` → `input_tokens` |
| `messages_count_on_generic_openai_flattens_and_says_so` | no upstream call (`.expect(0)`); header contains `flattened`; `input_tokens > 0` |
| `messages_count_on_remote_llama_flattens_via_tokenize` | `/tokenize` gets the flattened text; header `flattened` |
| `messages_count_on_local_llama_renders_the_template` | `gpu_world` fixture: `/apply-template` and `/tokenize` hit; `input_tokens == World.prompt_tokens`; no header; with an image and no bound → `media_omitted` |
| `messages_count_errors_are_anthropic_shaped` | missing model 400 `invalid_request_error`; unknown alias 404 `not_found_error`; server tool on a generic route 400 |
| `messages_count_under_hold_names_the_fallback` | hold on + fallback alias → `x-lmgw-fallback` + reason |
| `counts_are_not_logged_but_scope_refusals_are` | for all three counters: no `request_logs` row or event on success; a scoped key outside its scope → 403 and one row |
| `tokenize_forwards_the_llama_shape_verbatim` | `with_pieces`, `add_special`, `parse_special` reach the mock; `model` rewritten; response relayed byte for byte |
| `tokenize_without_model_is_a_llama_400` | `{"error":{"code":400,"type":"invalid_request_error"}}` |
| `tokenize_on_a_cloud_model_is_501_not_supported` | no upstream call |
| `tokenize_relays_upstream_errors` | llama-server's 503 "Loading model" relayed with status; an upstream 401 → 502 `server_error` (§12 entry 18) |
| `count_tokens_flags_a_guessed_tokenizer` / `…_message_framing` | `/v1/count_tokens` on `gpt-9-something` → `tokenizer_guess`; on Anthropic → `message_framing` |

Added after review R1 (§12 entries 15–18): `a_spent_budget_never_refuses_a_count`, `messages_count_on_anthropic_keeps_the_rest_of_output_config`, `messages_count_on_gemini_renders_the_reasoning_header`, `tokenize_reads_json_whatever_the_content_type`, `tokenize_over_the_body_limit_is_413`; and in `count_compat/local.rs`, on the `gpu_world` containers (whose `/apply-template` now refuses media the container cannot take, as llama-server does): the parse refused before admission, a candidate alias's facet refusal and `x-lmgw-candidate` on all three counters, `media_bound` on a projector row, a scoped-away key starting nothing, and `/tokenize` on a local row across a dead container's restart.

`route_walk.rs` covers the four new rows unchanged. Posting `{}` to them yields 400, which counts as "admitted".

### 7.4 UI unit tests (native, `cargo test -p lmgw-ui`)

- `curl.rs`: quoting, heredoc, `-N`, multipart, raw, secret placeholder, no key text.
- `identity.rs`: expectation per identity × capability, auth on/off.
- `doc.rs`: parse a small fixture doc; ref resolution; rail order; filter.
- `example.rs`: generation; `model` set/get.
- `stream.rs`: split across chunk boundaries; comments; multi-line `data`.
- `json_view.rs`: folding keeps the full text.
- Path filling with wildcard params.

### 7.5 UI verification (WP9; no GPU, no cloud)

1. **Extend `scripts/mock-openai.py`.** Path dispatch before the existing chat handling, which is unchanged:
   - `POST …/tokenize`: one id per whitespace word (`crc32(word) % 50000`), id 1 prepended when `add_special`, `[{id, piece}]` when `with_pieces`.
   - `POST …/apply-template`: `{"prompt": "<role>: <content>" lines}`.
   - `POST …/embeddings`: 3-float vectors per input.
   - `POST …/audio/speech`: 0.5 s of silence as a 16 kHz mono 16-bit WAV (`audio/wav`).
   - Update its usage header.
2. **Start the pieces:**
   - `python3 scripts/mock-openai.py 8911`
   - `(cd crates/lmgw-ui && trunk build)`
   - `scripts/dev-instance.sh 127.0.0.1:8899`
   - Copy the token from the `dashboard login:` log line into `/tmp/lmgw-ux-token`.
3. **Seed with curl** (owner bearer):
   - `upstream_set {action:create,name:mock,protocol:openai,kind:generic,base_url:http://127.0.0.1:8911/v1}`;
   - `model_set {action:create,alias:mock-chat,upstream:mock,upstream_model:triage}`;
   - the same pair for `mockllama`/`mock-llama` with `kind: llama_server`;
   - `key_create {name:"tester-client"}`, keeping the returned key as `$KEY`.
4. `scripts/ui-matrix.py --routes all` → exit 0 (no FAIL, no container started).
5. `sed "s/__CLIENT_KEY__/$KEY/" scripts/drive/api-ref.json | scripts/ui-drive.py - --shots /tmp/api-ref`. The steps, as expectations:
   1. The rail lists ≥ 150 operations.
   2. `get_api_status` → Send → `.resp-status` 200 and `.resp-headers` rows.
   3. Identity None → `.cap-warn` says 401 → Send → 401, and `.login-card` is absent (the dashboard stays unlocked).
   4. Identity Key `__CLIENT_KEY__` → Check says "client key" → `get_api_status` shows 403 expected → Send → 403.
   5. Copy as curl → `.curl-preview` contains `$LMGW_KEY` and not the key.
   6. Identity Session → `post_v1_chat_completions`, pick `mock-chat`, set `stream: true`, Send (no confirm: inference does not write) → `.resp-sse .frame` count grows over 1 s → `.assembled` is non-empty.
   7. `post_v1_audio_speech` → `audio.resp-audio` and `a.resp-download`.
   8. `post_tokenize` with `mock-llama` → `tokens` in `.resp-json`.
   9. `post_v1_messages_count_tokens` with `mock-chat` → `tr.lmgw` shows `x-lmgw-count-approximate: flattened`.
   10. `post_v1_audio_transcriptions` shows an `input[type=file]`.
   11. `get_api_usage_export_csv` → download link.
   12. Deep link `?op=op_hold_set` shows the ConfirmButton.
   13. Shots at 1440x900 and 900x1200.
6. `scripts/ui-drive.py scripts/drive/inapp-leaks.json --block-writes` and `scripts/drive/inapp-panics.json --block-writes` → PASS.
7. `scripts/webkit-check.py --routes /api-reference,/api-reference?op=post_v1_chat_completions` → PASS.
8. `bash ci/check.sh` is the gate.

## 8. Release notes entry (outline, `docs/release-notes.md`)

**`## 2026-09-xx — API reference, token-counting compatibility routes`** (links this spec)

- **`/v1`.**
  - New `POST /v1/messages/count_tokens` (Anthropic SDK shape, `{input_tokens}`).
  - New `POST /tokenize` (llama.cpp shape; `model` required; 501 `not_supported_error` for backends without token ids).
  - New `GET /v1/openapi.json` (Inference).
  - `lmgw.endpoints` is now generated from the route registry: it adds `/v1/responses/{id}`, `/v1/responses/{id}/input_items` and `/v1/openapi.json`, and the two advertised routes now exist.
  - `lmgw.headers` gains `x-lmgw-count-approximate`. The reasoning headers name `/v1/messages/count_tokens` too.
- **Headers.** `x-lmgw-count-approximate: flattened | tokenizer_guess | media_bound | media_omitted | message_framing`.
- **Behaviour changes.**
  - `/v1/count_tokens` now enforces the key's alias scope (a refusal is logged). *(Amended: no budget, §12 entry 15.)*
  - It stamps `x-lmgw-count-approximate` (`tokenizer_guess`, `message_framing`).
  - The count code moved to `proxy/count.rs` (no wire change).
- **`/api`.**
  - New `GET /api/openapi.json` (Admin).
  - `/api/op/{name}` answers only the names in `web::op_names`, which is exactly today's set; same refusal.
- **MCP, store, settings.** None.

## 9. Work packages

| WP | Content | Files it owns | Depends on |
|---|---|---|---|
| **1** Foundation | Cargo/feature/schemars; api-types `cfg_attr` derives on all DTO modules, `ImageAsset` schema, `openapi_ext.rs`; the whole `openapi` skeleton: registry types, `tags.rs` (final list §4.3), `exclusions.rs` (final), `schemas.rs`, `params.rs`, `build.rs` with op expansion, header params, principals, v1 filter + prune, `serve.rs`; empty `planes/*.rs`, `v1/mod.rs`, `ops/table.rs` (empty `OPS`), `headers.rs` (empty table + types); `web/op_names.rs` + gate in `op()`; both `openapi.json` routes + rows; empty test files + `main.rs` mod lines; `op_names_match_the_dispatcher_arms`, `an_unlisted_op_is_refused_before_dispatch`, the core structure unit tests | api-types/*, `openapi/*` skeleton, `web/op_names.rs`, `web/mod.rs`, `web/api.rs` (gate + route), `server.rs` (row + route), `tests/it/main.rs` | — |
| **2** Headers | fill `LMGW_HEADERS`; `COUNT_APPROXIMATE_HEADER`; `lmgw_block` headers from the table; `openapi_headers.rs`; `models_endpoint.rs` header assertion | `openapi/headers.rs`, `proxy/recording.rs`, `server.rs` (lmgw_block headers), `tests/it/openapi_headers.rs`, `models_endpoint.rs` | 1 |
| **3** Counters | §5 entire: `count.rs` (move + Count/Approx), `count_messages.rs`, `tokenize.rs`, egress fns, `CountPlan::Guessed`, `on_running_server` visibility, policy split, routes + rows + wrapper + merge; `count_compat.rs`; `egress_adapters.rs` fix | `proxy/*`, `egress/*`, `gate/fit.rs`, `gate/mod.rs`, `server.rs`, `tests/it/count_compat.rs`, `egress_adapters.rs` | 2 |
| **6** /v1 docs | `openapi/v1/*`, `planes/inference.rs`, `endpoints.rs`, `lmgw_block` endpoints, `VoicesQuery` + capabilities derives; `openapi_v1.rs`; `models_endpoint.rs` endpoints | those | 3 |
| **4** Dashboard docs | `planes/{dashboard,usage,docs,agents,session,labs}.rs` per §4.6; derive `JsonSchema` + `pub(crate)` on every web extractor/body/patch struct and `SessionView`; module visibility in `web/mod.rs`; `openapi_dashboard.rs` | `openapi/planes/*` (not inference), `web/*.rs` | 1 |
| **5** Ops docs | `ops/{table,args,merge,divergence}.rs` (74 entries per §4.7); derives on core patch structs and `ladder::Rung` (+ contained types); `openapi_ops.rs` WP5 tests | `openapi/ops/*`, `src/ops/*.rs`, `src/ladder.rs`, `tests/it/openapi_ops.rs` | 4 |
| **7** UI page | §6 entire | `crates/lmgw-ui/**` (pages/api_ref/*, pages/mod.rs, app.rs, shell.rs, app.css, Cargo.toml) | 1 |
| **8** Coverage + docs | `openapi_coverage.rs`, `openapi_live.rs`; README "API reference" + compatibility routes; release notes; this spec's status + §12 decisions | those | 3, 4, 5, 6 |
| **9** UI verification | mock extension, `ui-matrix.py`, `webkit-check.py`, `drive/api-ref.json`, inapp drive files; run §7.5 and fix what it finds (in `pages/api_ref/*`, `app.css`) | `scripts/*`, fixes in UI | 7, 8 |
| **10** Review | adversarial review of each merged piece, `bash ci/check.sh`, merge | — | all |

**Parallel lanes after WP1:** A = 2 → 3 → 6, B = 4 → 5, C = 7. No two lanes touch the same file: `server.rs`, `recording.rs` and `models_endpoint.rs` are lane A only; `web/*.rs` lane B only; `lmgw-ui` lane C only; test files and plane files were pre-created.

## 10. Spec choices (veto if wrong)

1. **The struct the dispatcher parses into is the op's documented shape.** The catalog supplies the words. Where they differ, the page shows what `/api/op` really takes; the tool keeps its flat projection.
2. **An op not in `op_names` is unreachable.** Adding an op means adding it to the list, which forces it into the docs.
3. **`x-lmgw-count-approximate` also goes on `/v1/count_tokens`** (`tokenizer_guess`, `message_framing`). Same rule, same counter.
4. **`/v1/messages/count_tokens` on an Anthropic upstream sends the client body verbatim** (only `model` replaced), rather than a re-rendered IR. That is the only way server tools and new fields count exactly.
5. **Local llama.cpp chat rows count via `/apply-template` + `/tokenize`, i.e. exact.** Remote llama-server upstreams and aux rows are flattened (flagged): `count_chat_prompt` has no upstream authentication, and aux models have no chat template.
6. **`/tokenize` requires `model` and refuses non-llama backends with 501,** rather than returning tiktoken ids for OpenAI models. Ids from a tokenizer the backend may not use would be a hidden approximation.
7. **All three counters apply the key's scope and budget,** refusals logged, successes not. *(Amended after review: the scope only — §12 entry 15.)*
8. **Only requests that change stored state need the confirm click** (§4.2 `x-lmgw-writes`), plus the two ops that reveal a secret. Inference POSTs send directly, the way the Chat page does, even though they can start a model or spend money. *(Amended during implementation; the draft confirmed every non-GET.)*
9. **Untyped reads stay untyped here** (§11), rather than inventing DTOs inside a docs feature.
10. **The page lives in "Connect".** It is how clients connect to lmgw, next to what lmgw connects to.

## 11. Later / out of scope

- **DTOs for the untyped reads:** `local-model-check`, `model-inspect`, `llama-flags`, the agent export, and `local-model?target=image`. *(The internal mini-APIs' reads are no longer a candidate here — owner decision, 2026-09-28: they are not documented operations at all, only rows in `x-lmgw-undocumented`, §4.2, not merely untyped.)*
- **`/detokenize` (and `/props`)** for llama.cpp compatibility clients (§0.9).
- **`/v1/embeddings`** forwarding `dimensions`/`encoding_format`, or refusing them visibly (§0.10).
  *(Done 2026-09-29: `dimensions` is forwarded (Gemini: `outputDimensionality`) and the returned
  length checked, so llama-server, which ignores it, is a 400; `base64` is encoded by the gateway;
  token-id `input` is refused by position instead of skipped.)*
- **Forwarding `anthropic-beta`** on the Anthropic egress (§0.10).
  *(Done 2026-09-29: forwarded from every chat-shaped route and `/v1/messages/count_tokens`
  to an Anthropic upstream, as one header merged with the row's own `anthropic-beta`; no
  other protocol is sent one.)*
- **An exact chat-template count for remote llama-server upstreams** (auth on `count_chat_prompt`).
- **Sharing one SSE reader** between `pages/chat_stream.rs` and `api_ref/stream.rs`.
- **Out of scope:** Swagger UI, generated client SDKs, per-method MCP expansion, documenting agent apps.

## 12. Decisions taken during implementation

§10 is the design-time list, offered for veto before any code existed. These
are the calls a worker actually had to make while building against the real
source — kept here for the same reason: wrong, they are one line to revert.

1. **The internal drop (owner decision, 2026-09-28).** The three mini-APIs —
   Chat, Audio lab, Image lab — stopped being documented operations rather
   than staying documented `x-lmgw-internal`; they are listed as undocumented,
   with the reason (§12.8). `planes/labs.rs`,
   `Group::Internal`, the `x-lmgw-internal` constant and derivation, and the
   UI's "internal" chip are all gone; the 25 routes are `UNDOCUMENTED` rows
   in `exclusions.rs` instead, one shared reason: "The dashboard's own
   backend; its shapes follow the UI and are no contract." §1, §3, §4.2,
   §4.3, §4.4 and §6 are amended in place, dated to this decision.
2. **`x-lmgw-writes` means *exactly* "changes stored state," applied
   literally even where it reads oddly.** The test is "does a row get
   written," not "does this have a side effect" or "could this cost money" —
   an inference POST that only computes an answer is `writes: false` even
   though it can cold-load a model or spend a cloud provider's money (§4.2's
   own exemption list).
3. **Chat send writes.** While it was still documented, `POST
   /chat/api/threads/{id}/send` was `writes: true` despite calling the exact
   same egress adapter `/v1/chat/completions` does (which is `writes:
   false`) — the difference is that it also persists the user's and the
   assistant's messages to the thread store, which the inference route never
   does. Recorded here (moot since the internal drop, decision 1) because it
   looks, at a glance, like the same exemption should apply to both.
4. **The Anthropic protocol count merges lmgw's own reasoning controls into
   the passed-through body before sending it on** (`proxy/count_messages.rs`
   `with_lmgw_reasoning`), rather than leaving the client's body untouched.
   Anthropic's real `/v1/messages/count_tokens` has no header channel for
   `x-lmgw-reasoning*` the way `/v1/messages` does, so without this the
   count would silently ignore a documented control on exactly the route
   that promises an exact one. Only runs when a tier actually applies
   (a header, or the alias' own default); otherwise every key goes on as
   the client sent it, only `model` rewritten (the body is re-serialized,
   so "byte for byte", the earlier wording, was loose). When it runs, it
   replaces the thinking and merges `effort` into the client's own
   `output_config`, keeping its other keys (review R3 #6).
5. **`GET /v1/responses/{id}` and `.../input_items` are listed under
   `other`, not `openai`,** in `lmgw.endpoints` — resolving a
   self-contradiction the design left in: §4.8's table's own bracket
   notation said `[openai]`, but §4.11 explicitly says "this newly lists
   `/v1/responses/{id}`, `/v1/responses/{id}/input_items` ... under other."
   §4.11 is what actually specifies `lmgw_endpoints()`'s output, so it won.
6. **A divergence is checked per argument, not per op** (amended after
   review R2 #7). A struct-derived op's own JSON Schema `required` is empty
   for *every* tool-sharing op (the generator's `#[serde(default)]`-is-
   optional contract, §4.5), and its enums are `$ref`s where the tool's are
   inline, so "the two schemas differ somewhere" held for every Struct op
   whether or not it was a real `DIVERGENCES` entry — the stale check could
   never fire. Each `Divergence` now names the arguments its note is about
   (`props`), and each must still differ: on one side only, a different type
   or enum, or a different required-ness where the op's schema has a
   `required` of its own. An op with no body names none.
7. **`candidate_alias_set` is `writes: true` for the whole op, including its
   `preview` action,** which does not itself change stored state (§4.7
   table B). `x-lmgw-writes` is one flag per operation, not per enum value
   of an `action` field — modeling the latter would need a shape this
   document does not have anywhere else, for the one op where it would
   matter. The confirm click is a false positive on `preview` alone; the
   trade was judged cheaper than the alternative.
8. **The undocumented list is the admin document's alone** (design
   decision after review R3 #4). `x-lmgw-undocumented` stays in
   `/api/openapi.json` — an honest inventory of every route the description
   leaves out, the dashboard mini-APIs included, and the explicit exclusion
   list the coverage guard reads — and the page shows it as the rail's
   closing "Not documented here" list, each with its reason. The earlier
   wording ("left out entirely", "never reach the rail") was wrong about
   that. `/v1/openapi.json` drops it, and drops `x-lmgw-principals` too (the
   page reads the admin document, the only one it fetches), and keeps only
   the tags and security schemes its own operations use (review R2 #5).
9. **Secrets are flagged two ways** (review R2 #3). Names that are a
   credential wherever they appear (`token`, `api_key`, `hf_token`,
   `update_token`) stay a name rule in `build.rs`. Names that are ordinary
   words elsewhere (`env`, `headers`, `values`, `extra_headers`,
   `forge_tokens`) are flagged where they are defined: the schemars field
   transform `lmgw_api_types::openapi_ext::secret` on a struct field (a
   transform, so the key is not a string literal beside every field), or
   `x-lmgw-secret` written into a hand schema. The curl redaction replaces a
   flagged value whole, object or list included. `self_admin_token` left
   the list: nothing has had it since the principals rework.
   `secret_inputs_per_operation` pins the full set.
10. **`x-lmgw-writes` and `x-lmgw-reveals-secret` corrections** (review R2
    #8). `agent_token_get` writes (`token::ensure` mints a missing token and
    always rewrites its scope); `key_create`, `key_rotate` and
    `agent_token_rotate` reveal a secret (each answer carries the new
    plaintext); `POST /api/docs/search` does not write. The ledger's
    `POST /api/agents/runs/{job_id}/events` documents two media types,
    `application/json` (object or array) and `application/x-ndjson`, instead
    of a `"string"` in the JSON schema's `type`.
11. **Every schema is registered once** (review R2 #1). schemars'
    `root_schema_for` copies every definition built so far into each root it
    returns; the admin document carried 125 such copies and weighed 16.1 MB.
    Every `SchemaFn` result goes through `schemas::embed`, which drops the
    copy (the definitions still land once in the root `components`); the
    admin document is ~0.56 MB. Stripping was chosen over switching the
    registry to `subschema_for`, which would have turned every inline body
    into a `$ref` and changed what the query-parameter expansion and the
    tool-prose overlay read. The handlers serve cached bytes with an ETag,
    and these two routes are the only compressed ones in the gateway.
12. **An op sent with no body gets `{}`, and the gate says it is the gate**
    (review R2 #2, #6). `POST /api/op/{name}` with an empty body, whatever
    its `Content-Type`, runs with no arguments (`web::op_body`) — the ops
    documented without a `requestBody` were a `415` from the tester's Send
    and its curl. A non-empty body keeps `Json<_>`'s own `400`/`415`/`422`.
    The unlisted-name refusal (`op_names::refuse_unlisted`) is worded apart
    from the dispatchers' own fall-through, pointing at
    `GET /api/openapi.json`, so a test can tell the gate answered.
13. **The page shows `x-lmgw-tool` and `x-lmgw-alias-of`** (review R3 #8) as
    chips in the action bar — "same as MCP tool lmgw__X", "alias of X" —
    beside the existing deprecated chip. A key probe that authenticated but
    has a kind the page does not know is labelled by that kind, never
    "matches no key".
14. **The structural check is by hand** (review R3 #2), not a validator
    dependency: none could be vetted without network access, and the
    published 3.1 meta-schema would have had to be vendored for the
    `jsonschema` dev-dependency. `both_documents_are_structurally_valid_openapi_3_1`
    states the 3.1 field tables it checks, and a sibling test feeds it a
    broken document so it cannot go quiet.
15. **The counters check the key's alias scope and no budget at all**
    (design decision after review R3 #1, which also settles R1 #8).
    §5.1 said "scope and budget", and all three counters enforced the key's
    budget and the gateway's global one — for the owner and anonymous
    callers too, at a spend query per count. A count costs nothing, so a
    budget refusal there only blocks free work: sizing the request a client
    is about to trim to fit. The scope stays: it is access, and counting on
    a local model starts its container, so a key fenced off an alias must
    not cold-load it by counting (`policy::check_scope`). The refusal's
    `request_started` is opened only once there is a refusal to log, behind
    a drop guard, so a client that hangs up mid-write no longer leaves the
    in-flight gauge one over (review R1 #7).
16. **`/v1/messages/count_tokens` parses before admission** (review R1 #2,
    #3). The body was read into the IR after `gate::open`, so a count only
    an Anthropic route could make (a server tool, an unknown block) could
    evict and cold-load a local model and then answer 400. It is parsed
    first; one that does not parse goes on only when the resolve landed on
    an Anthropic route (the provider reads it itself), and is refused before
    admission otherwise. One that parses states its facets through
    `Routed::using`, as `/v1/messages` does: a candidate alias refuses a
    facet it does not enable, and the outside-VRAM swap avoids a fallback
    that cannot see the images. An unparseable body on an Anthropic route
    states no facets — lmgw cannot read them — so a candidate alias whose
    hold fallback is Anthropic counts it without the facet check.
17. **A template count leaves out of the render what it leaves out of the
    number** (review R1 #1). llama-server refuses an image part on a row
    without a projector (and audio on a projector that does not hear) with
    a 500 before rendering anything, so a count that only had to say
    `media_omitted` was a 502. The body `/apply-template` gets drops all
    audio, and the images when the row gives no per-image bound; an image
    with a bound is still rendered (its marker is in the text count) and
    added at the bound. Audio cannot arrive in Anthropic's dialect today, so
    that half is pinned by a unit test.
18. **`/tokenize` is llama-server's in what it accepts, lmgw's in what an
    upstream credential failure means** (review R1 #4–#6). The body is read
    as JSON whatever its `Content-Type`, as llama-server reads it. An
    upstream 401/403 goes through the egress `map_error` to the 502 every
    other route gives it, since it is lmgw's own key failing; every other
    upstream answer is still relayed verbatim. The refusals the shared
    `/v1` layers make before the route runs stay OpenAI-shaped and are
    documented as the exception (§5.3), rather than giving `/tokenize` a
    body-limit layer of its own.
19. **`POST /v1/completions` without `model` is a 400** (WP9b). It routed
    the placeholder name `?` and answered 404 "unknown model alias: ?";
    every other ingress answers 400 "missing 'model'", and now this one
    does too, logged like `/v1/chat/completions`' parse errors.
