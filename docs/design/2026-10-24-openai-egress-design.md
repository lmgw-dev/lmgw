# The OpenAI platform gets its own egress: `openai` and `openai_compatible`

Design, 2026-10-24; the owner answered its open questions the same day (decisions 15–21).
Today the protocol `openai` (`Protocol::Openai`,
`config/routes.rs:16`) stands for api.openai.com and for every OpenAI-shaped provider at once:
Kilo and other OpenRouter-style aggregators, Gemini's OpenAI-compatible endpoint, Groq, external
audio.cpp servers, and the synthetic audio and image upstreams. So lmgw can rely only on what all
of them share. This design splits the protocol the way `llama_cpp` was split off on 2026-10-06
(`2026-10-06-llama-egress-design.md`). `openai` becomes the OpenAI platform only, and a new
`openai_compatible` takes every other OpenAI-shaped provider. Both egresses compose
`egress/openai_wire`. The first features built on the platform egress are service-tier pricing,
`/v1/responses` without a flag, the OpenAI reasoning vocabulary, a body OpenAI's reasoning models
accept, and OpenAI's own error codes.

**Built so far** (2026-10-25): WP0, WP1, WP2, WP3, WP4, WP5, WP5b, WP9, WP10, WP7 and WP8 (WP3's deviations
are at the end of §7.1 and §8, WP4's at the end of §5.2, §5.3, §5.5, §5.6 and §5.7, WP10's in
§14.14's draft calls made while building and in the §14 passages its review and verify rounds
changed). Two things moved
between packages. The
§1.1 sites that make a stored `openai` row mean the platform only (`catalog/request.rs`, the
System One guard, the platform egress dropping rerank and System One) landed with WP2's
migration rather than WP1, because before 0090 every stored row is still `openai`; WP1 left
them as they were. I1's route-corpus value changed in WP2 for the same reason (the generic
route row can be stored as `openai_compatible` only from 0090 on). WP2 shipped
`forwards_responses` false for `openai`; WP5 turned it on and re-blessed only the platform's
`/v1/responses` route fixture (native now). WP5 was built before WP3 and WP4; WP4 added the
`service_tier` read to its passthrough (`openai_wire/response_object.rs`), and a stream cut
before its terminal event keeps its `incomplete` error and carries §5.2's note in `price_note`
as well. The WP5 review round took migration 0091 (`native_responses`, `responses.key_id`, §6),
so WP4's migration is 0092. WP5b (hosted tools, §6.1) followed WP5; its review round took 0093
(`native_response_items`) and 0094 (`budget_floor_micro`). WP9 followed WP5b with no migration
(its deviations are in the "As built" notes of §13.2 to §13.5). WP10 followed WP9 on 2026-10-25,
after the owner's answers to §14.14, with migration 0095. WP7 followed WP10 on 2026-10-25 with no
migration (its draft calls are in §7.2.5, what it built in §7.2.6). WP8 followed WP7 on 2026-10-25
with migration 0096, after a round on the WP7 review's residuals (its draft calls are in §7.3.4,
what it built in §7.3.5), and its review's fix round followed the same day (0096, still
unreleased, took the items' hash column; §7.3.5 lists what changed), then a verify round's
fixes with no migration change. WP6 and the probe run of WP8 need explicit approval because they spend money. The owner's decisions 22 and 23 (2026-10-24) add WP9, budget
lower bounds for unpriced rows a client can cause (§13), and WP10, the fallback on an upstream
quota error (§14). Decisions 25 and 26 (2026-10-24) replace WP10's first design: one fallback per
model and alias for every reason, and no global fallback (§14). §7.2 and §7.3 (WP7, WP8) were refreshed against what is built the
same day. §11 has the build order and each built WP's commits.

**Today** (lines at `main` c647b27e):
- Every `openai` row leaves through `egress::openai::OpenaiEgress` (`egress/mod.rs:159`, `:166`).
  That egress composes the shared body (`openai_wire/body.rs`) with a scalar `reasoning_effort`
  step and text-only tool results (`egress/openai.rs:24-82`).
- `upstreams` has `CHECK (protocol IN ('openai','anthropic','gemini','llama_cpp'))` and
  `kind IN ('generic','llama_server','audio_cpp')`, plus the two llama CHECKs
  (`migrations/0058_llama_cpp_protocol.sql:37-56`). An `openai` row may be of kind `generic` or
  `audio_cpp`. 0080 added `catalog_all_modalities` and turned it off for `openai` rows on Gemini's
  host. The latest migration is 0089.
- Nothing reads `service_tier` anywhere in the code (no match in `crates/`). The `prices` table
  has no tier dimension (`0070_billable_units.sql:55-89`, unique on
  `(scope_kind, scope_key, source, unit)`, `:114`). So a flex request is priced at Standard (about
  2× what it costs) and a Fast request at Standard (half of what it costs), and nothing says so.
- Native `/v1/responses` passthrough records **no tokens and no cost** (`responses.rs:621-638`:
  "leaves tokens NULL rather than guessing"). Every native Responses request is unpriced today.
  (WP5 fixed this, §5.2.)
- The shared body sends `max_tokens` (`openai_wire/body.rs`, the `params.max_tokens` arm). OpenAI
  documents `max_tokens` as deprecated and "not compatible with o-series models".

## Decisions

*Owner, 2026-10-24:*
1. **`openai` is the real OpenAI platform only.** A new protocol `openai_compatible` covers every
   other OpenAI-shaped provider.
2. **Every existing `openai` row becomes `openai_compatible`.** The UI selector already calls
   `openai` "OpenAI-compatible" (`lmgw-ui/src/pages/upstreams.rs:504`). An existing api.openai.com row is switched to `openai` by hand afterwards. The selector gets a new "OpenAI" option.
   The migration and its guard follow the `llama_cpp` pattern (§4).
3. **No backward compatibility.** There is no shim and no alias for the old meaning. What breaks
   for callers that write `protocol: "openai"` is named and planned (§4.5).
4. **Both egresses keep composing `egress/openai_wire`**, as `llama_cpp` does.

*The owner's standing rule, applied here:* no hidden defaults. A price lmgw does not have is
unknown, not Standard (§5.6).

*Taken in the draft; the owner's 2026-10-24 answers confirm 8 (as 21) and 13 (as 19):*
5. **Where the code lives.** Today's `egress/openai.rs` is the compatible egress already, so it
   moves unchanged to `egress/openai_compatible.rs` (`OpenaiCompatibleEgress`). A new
   `egress/openai.rs` holds the platform `OpenaiEgress`. It starts byte-identical to the compatible
   egress (WP1) and then takes the intended changes one commit each (§3.3).
6. **Spelling.** `Protocol::OpenaiCompatible`, serialized `openai_compatible` with an explicit
   `#[serde(rename)]` (`rename_all = "lowercase"` would give `openaicompatible`).
   `speaks_openai_http()` is true for `Openai`, `OpenaiCompatible` and `LlamaCpp`.
7. **The invariant.** `openai` is always kind `generic`. `openai_compatible` is `generic` or
   `audio_cpp` (an external audio.cpp server), and `audio_cpp` exists on `openai_compatible` only:
   `anthropic` and `gemini` rows are `generic` (or `llama_server` for `anthropic`). The migration
   turns a stray `anthropic`/`gemini` + `audio_cpp` row into `generic` and names it. `llama_server` keeps its own rules
   (`llama_cpp` or `anthropic`). The rule is enforced three times, as before: table CHECKs,
   `settle`, and the migration.
8. **`supports_responses` means something only on `openai_compatible`.** It is 0 everywhere else:
   `llama_cpp` already (0058), now `openai` (implied, §6), `anthropic` and `gemini` (neither
   serves the route). The migration clears it on the last two and names each row it clears (decision 21).
9. **Synthetic upstreams** (audio.cpp, sd-server: `config/snapshot.rs:381`, `:392`) become
   `openai_compatible`. They are not the OpenAI platform.
10. **The tier is a property of the response, not of the row.** lmgw reads the tier OpenAI says
    served the request and prices at that tier. It never assumes the requested tier (§5).
11. **The price tiers are OpenAI's pricing tiers**: `standard`, `flex`, `fast`, `ultrafast`,
    `scale`. Response values map onto them by OpenAI's documented equivalences (§5.4). Every
    existing price row becomes `standard`.
12. **"Responses implied" keeps the loop where only the loop can serve.** An `openai` row
    forwards `/v1/responses` natively unless the request needs lmgw's own loop: MCP tool blocks
    that lmgw serves, or a `previous_response_id` held in lmgw's own store (§6). The same check
    fixes this gap on flagged `openai_compatible` rows. WP2 shipped `forwards_responses` = false
    for `openai` (native passthrough recorded no tokens or cost and had no loop check);
    WP5 turned it on together with metering and the check.
13. **`protocol: "openai"` on a host that is not OpenAI's is accepted, with a note.** `settle`
    accepts any `base_url` (tests and proxies need it). It adds a note when the host is not
    `api.openai.com` or a regional `*.api.openai.com` host, and the row view and the Test button
    show the same note (decision 19).
14. **The move is byte-identical; intended changes come after it, one commit each**, against
    the egress corpus (§10).

*Owner, 2026-10-24, the former §12 questions, each answered as recommended (Q1 → 15, Q2 → 16,
Q3 → 17, Q4 → 18, Q5 → 19, Q6 → 20, Q7 → 21):*

15. **A tier with no price on file is unpriced, with a note** (§5.6). A flex or Fast request whose
    scope has only a Standard row is recorded unpriced, with a note naming the tier and saying a
    Standard row exists. Until flex or Fast rows are entered, that traffic counts toward no
    budget; the Prices page lists those scopes so it is findable. Pricing at Standard was rejected:
    it overcharges flex about 2× and undercharges Fast 2×, and the undercharge is what a budget
    must not do.
16. **Chat over the Responses wire for `openai` rows is built, as WP7, after WP5** (§7.2). On
    Chat Completions OpenAI returns no reasoning text, carries no reasoning between turns and has
    no function calling on GPT-6 Astra and GPT-6.1 Sol.
17. **The encrypted-reasoning seal travels in-process and through `/v1/messages`, not through Chat
    Completions** (§7.3). In-process threads store it; `/v1/messages` clients carry it in the
    thinking block's `signature` behind an `lmgw-openai:` prefix. Chat Completions has no field for
    it, and inventing one would break the rule that OpenAI-shaped routes copy the protocol.
18. **`strict: true` for `/v1/messages` schemas stands for both protocols** (§8). An early
    strict-subset check on `openai` rows is not built yet (Later).
19. **`protocol: "openai"` on a host that is not OpenAI's is accepted with a note** (decision 13).
    The note shows in the write's response, on the row and on its Test. Refusing with a `force`
    flag was rejected: proxies and test servers need other hosts.
20. **Long-context and the other price dimensions get a short follow-up design after WP4**
    (§5.1, Later): the context band above 272K input tokens, the 12-hour cache write and the
    regional uplift.
21. **The migration clears `supports_responses` on `anthropic` and `gemini` rows and names each in
    its notice** (decision 8, §4.2). Neither API serves `/v1/responses`, so no working setup
    changes.

*Owner, 2026-10-24, approved as proposed:*

22. **Budgets get a lower bound for the unpriced rows a client can cause** (§13, WP9). This
    extends the hosted-fee floor already built (`budget_floor_micro`, migration 0094):
    - a tier-gap row whose served tier is not flex (`auto` or none reported, `fast`/`priority`,
      `ultrafast`, `scale`) floors at the standard price, since every one of those costs at least
      Standard;
    - flex asked for by a budgeted key, when that scope has no flex price, is refused up front
      with an error that names the missing price. Flex is cheaper than Standard, so Standard is
      no lower bound for it;
    - a native `/v1/responses` stream cut before its terminal event (usage unknown) floors from a
      local count: the prompt through the platform's local tokenizer (tiktoken, as the platform's
      count, `egress/openai.rs`), plus the output deltas lmgw relayed. OpenAI does not store a cut
      response (live, 2026-10-24), so this is the only source.
23. **An upstream quota error makes the fallback answer; a rate limit does not** (§14, WP10).
    `UpstreamLimited { limit: Quota }` hands the request to the configured fallback, as the
    other `FallbackReason`s do. `Rate` and `FlexCapacity` stay 429 to the client. It applies the
    standing rule "a configured fallback is always used, no content-based exceptions; privacy =
    choosing an alias without fallback".
    *Owner, later on 2026-10-24:* a passthrough model has no fallback; a fallback may name an
    alias or another model (a local model id or a passthrough id); and fallbacks chain with no
    depth cap, a cycle refused at save and guarded at use (§14.2). The fallback is the model's or
    alias's one fallback (decision 25), not a quota-only field: the first design's
    `models.fallback` "when this alias's upstream is out of quota" was withdrawn the same day.

*Owner, 2026-10-24, after the WP9 review:*

24. **A budget is enforced only on what is priced** (§13.3). A request whose caller has a budget
    (its key's, or the global one) is refused up front, `403` `permission_error` with the code
    `budget_unpriced`, when its cloud route's scope prices none of the units the request is
    billed in. Local routes and callers without a budget are unaffected. A budget can only be enforced when the models' prices are known; making a user enter prices is preferred over a budget that looks kept while an unpriced model spends tokens. Flex without a flex price becomes one
    case of this rule, and its code `budget_tier_unpriced` is renamed `budget_unpriced` (alpha, no
    alias for the old name). It replaces §13.3's draft call that a scope with no price at any tier
    is not refused.

*Owner, 2026-10-24, after the first WP10 design:*

25. **One fallback setting per model and per alias, for every reason** (§14). One fallback setting serves every reason; the multi-setting design was rejected (the owner's decision, 2026-10-24). Every model and alias kind that can have a fallback
    (the local chat, aux, audio and image rows, candidate aliases, plain aliases) has one field,
    used whenever it cannot answer: the GPU hold, a benchmark, VRAM outside lmgw, a deferred
    background request, an unavailable candidate, an exhausted upstream quota. The reason is
    recorded (`x-lmgw-fallback-reason`, `x-lmgw-fallback-chain`, `request_logs.fallback_reason`),
    never configured. `hold_fallback_mode`/`hold_fallback` and the candidate aliases'
    `fallback_mode`/`fallback` become that one field (migration 0095, §14.10).
26. **The global fallback goes away** (§14). The global fallback setting is removed (the owner's decision, 2026-10-24). `settings.hold.fallback_alias` and the `inherit`
    mode are removed; every model and alias has only its own fallback, or none. The migration
    writes the inherited value into each row that inherited it (none where the global was unset),
    so no row's behaviour changes, and then drops the setting (§14.10).

*Owner, 2026-10-25, answering §14.14:*

27. **A rename never breaks a chain** (§14.2). Renaming an alias or a row, or changing a class
    prefix, rewrites every other row's and alias's fallback that names it, in the same write
    transaction.
28. **An unreachable upstream and a disabled model are reasons too** (§14.3, §14.4). `unreachable`
    (a connect error, nothing sent, caught where the quota is caught) and `disabled` (a disabled
    upstream or a disabled row) hand the request to the fallback in WP10. Nothing is retried once
    a byte went to the client.
29. **A fallback is the same kind of model** (§14.2). A fallback must be the same model type;
    between chat models a modality difference is allowed with a warning (the owner's decision,
    2026-10-25). Every setter refuses a fallback of another model type; between chat models a
    difference in modalities is saved with a warning.

## 1. Inventory

Sorted into **(a)** real OpenAI only, **(b)** any OpenAI-shaped HTTP (`Openai` and
`OpenaiCompatible`, usually with `LlamaCpp` through `speaks_openai_http`), **(c)** compatible only.
"Exh." marks an exhaustive match, which the compiler flags. Every other site changes silently when
the new variant is added. Test code is in §1.7.

### 1.1 `Protocol::Openai` outside tests (32 sites)

| Site | What | Sort | Then |
|---|---|---|---|
| `egress/mod.rs:166` (exh.) | `for_protocol` dispatch | — | new arm: `OpenaiCompatible => &OPENAI_COMPATIBLE`; `Openai => &OPENAI` is the platform |
| `egress/mod.rs:203`, `:215` (exh.) | legacy `/v1/completions` body is untouched | b | both arms `Ok(())` |
| `egress/openai.rs:86` | `proto()` | c | moves with the file: `OpenaiCompatible`. The new platform egress returns `Openai` |
| `config/snapshot.rs:381`, `:392` | synthetic audio and image upstreams | c | `OpenaiCompatible` (decision 9) |
| `store/rows.rs:66` | an unreadable protocol reads as `Openai` | c | `OpenaiCompatible` plus an ERROR naming the row. Never the platform, which would send OpenAI-only fields. The CHECK makes it unreachable |
| `ops/upstream_settle.rs:28` | `NEW_ROW` default | c | `OpenaiCompatible`, as the UI default (§1.5) |
| `ops/common.rs:119` | protocol for an override shape check | b | `OpenaiCompatible` (the shape check ignores it) |
| `capabilities/exposed.rs:860` | image row's override | c | `OpenaiCompatible` (sd-server) |
| `capabilities/hears.rs:156` (exh.) | the egress encodes `input_audio` | b | both `true` (OpenAI's audio models take it) |
| `capabilities/mod.rs:845` (exh.) | reasoning controls shown | b | both `OPENAI_REASONING_CONTROL` |
| `capabilities/notes.rs:716` (exh.) | reasoning note | b | both |
| `catalog/request.rs:24` | `?output_modalities=all` asked of `openai`+`generic` | c | `OpenaiCompatible`. OpenAI answers the same list either way (0080's comment), so the platform is never asked |
| `catalog.rs:680`, `:732` (exh.) | `GET /models`, `data[]` parse | b | both. On the platform `parse_openai_entry` (`catalog.rs:761-775`) finds only `id` and `created`: no context, prices or reasoning (§1.6) |
| `web/admin.rs:33` (exh.) | Test button's catalog request | b | both |
| `proxy/count.rs:423` (exh.) | count is exact, no framing | b | both |
| `proxy/count_messages.rs:209` (exh.) | count on the body | b | both |
| `proxy/image.rs:50` | `image_error`'s fallback mapper hard-wired to OpenAI's | c | the route's egress (`for_protocol(route.upstream.protocol)`) |
| `proxy/image.rs:92` | `/v1/images/*` needs `!= Openai` → refused | b | `Openai \| OpenaiCompatible` (OpenAI serves `gpt-image` there; llama_cpp stays refused) |
| `proxy/audio.rs:114` | audio error mapping hard-wired | b | the route's egress, as above |
| `proxy/recording.rs:728` (exh.) | which reasoning fields a route ignores | a/c | split: compatible keeps today's arm; the platform's arm also names an OpenRouter `reasoning` object's `enabled`/budget as translated (§7.1) |
| `proxy/reasoning_fit.rs:107`, `:113` (exh.) | log wording of the off control | b | both |
| `reasoning_fit/rule.rs:47`, `:60` (exh.) | default off is `Off::Control` | b | both |
| `reasoning_fit/refusal.rs:105` (exh.) | a refusal names `reasoning_effort` | b | both |
| `route_guards/decision.rs:73` (exh.) | `/v1/systemone` forwards to `openai`/`llama_cpp` | c | `OpenaiCompatible \| LlamaCpp`. OpenAI has no System One route; an `openai` row is refused before admission, like Anthropic's |
| `realtime/voice/fallback.rs:54` | `(Openai, Generic)` → `Engine::OpenAi` (OpenAI voice names) | b | both protocols with `Generic`; today's meaning, unchanged |
| `web/chat_turn.rs:1343` (exh.) | Continue is impossible on OpenAI-shaped routes | b | both |
| `web/chat_sampling.rs:83` (exh.) | the Chat's sampler split | b | both `(false, false, false, true, true)` |

`config/routes.rs:30`, `:39`, `:52` (`as_str`, `parse`, `speaks_openai_http`) gain the variant.
`parse` is a string match, so a round-trip test covers every variant through `as_str`, `parse` and
serde, as for `llama_cpp`.

### 1.2 `speaks_openai_http()` (4 sites, all b)

`gate/open.rs:178` (legacy `/v1/completions`; OpenAI still serves it for its instruct models),
`capabilities/task.rs:70` (chat endpoints), `capabilities/mod.rs:814` (task by name),
`proxy/audio.rs:70` (audio routes). None of them changes; the predicate gains the variant.

### 1.3 `supports_responses` in core (39 non-test sites)

| Sites | What | Then |
|---|---|---|
| `responses.rs:144` | the one decision: native passthrough or the loop | `route.upstream.forwards_responses(&req)`: `Openai` → true unless decision 12's check says the loop; `OpenaiCompatible` → the flag, same check; others → false (§6) |
| `responses.rs:12` | module doc | reworded |
| `config/routes.rs:133`, `config/snapshot.rs:436` | the field; synthetic llama upstreams `false` | unchanged |
| `ops/upstream_settle.rs:21-196` (13 sites) | `settle`'s llama rule | generalized: refused when sent `true` on any protocol but `openai_compatible`, cleared with a note when the row had it (§4.3) |
| `ops/routing.rs:34`, `:69`, `:83`, `:110`, `:136`; `web/api.rs:1029`, `:1048`, `:1081`, `:1155` | the two write paths carry it through `settle` | unchanged |
| `ops/reads.rs:361`, `store/rows.rs:76`, `store/upstreams.rs:21`, `:57`, `:70`, `:90`, `:104`, `:112`, `:125` | read and write the column | unchanged |
| `store/migration_guards/llama_cpp.rs:33-44` | 0058's notice | unchanged; 0090 gets its own guard (§4.2) |
| `mcp/selfadmin/catalog/routing.rs:80` | MCP doc | rewritten: "`openai_compatible` only; implied on `openai`" |
| `examples/seed_usage.rs:64` | seeds `openai` rows | `openai_compatible` |

Outside core: `lmgw-api-types/src/upstreams.rs:35` (doc), and the UI checkbox
(`upstreams.rs:481`, `:566`, `:651-652`), shown only for `openai_compatible` (§1.5).

### 1.4 `"openai"` strings that mean the protocol

| Site | Then |
|---|---|
| `config/routes.rs:30`, `:39` | as §1.1 |
| `mcp/selfadmin/catalog/routing.rs:38-44` | enum gains `openai_compatible`; the description says `openai` is api.openai.com only. The OpenAPI op `upstream_set` takes its schema from this tool (`openapi/ops/table/routing.rs:12-16`), so it follows |
| `mcp/selfadmin/catalog/routing.rs:97` | `catalog_all_modalities` doc: "`openai_compatible` row of kind `generic`" |
| `ops/routing.rs:58`, `ops/upstream_settle.rs:68` | error texts list the five protocols |
| `openapi/planes/inference.rs:203` | `/v1/systemone` doc: "`openai_compatible` or `llama_cpp`" |
| `store/migration_guards/llama_cpp.rs:50`, `:60` | 0058's history; unchanged |
| `lmgw-api-types/src/upstreams.rs:20-25`, `:38` | docs list the protocols and the kind rule |
| `examples/seed_usage.rs:65`, `:226` | `openai_compatible` |

Not protocol, and unchanged: `openapi_ext.rs:178`, `:209`, the `"openai"` tag of every OpenAI-shaped
client route in `openapi/planes/inference.rs` and `openapi/tags.rs:25`, the `/v1/models` shape
groups (`openapi/v1/models.rs:34-36`, `openapi/endpoints.rs:49`), and `ingress/mod.rs:46`.

### 1.5 The UI

- `upstreams.rs:161`: the new row's default `"openai"` becomes `"openai_compatible"`.
- `upstreams.rs:502-510`: options `("openai", "OpenAI")`, `("openai_compatible",
  "OpenAI-compatible")`, Anthropic, Gemini, llama.cpp.
- `upstreams.rs:500`: `openai()` (shows "list every output modality") tests
  `openai_compatible`.
- `kind_after_protocol` (`:698-707`): `openai` is always `generic`. Moving into `openai` resets the
  kind to `generic`, and the kind select is hidden for it as for `llama_cpp` (`:617-626`).
  `audio_cpp` survives only under `openai_compatible`. The unit tests at `:713-755` gain those
  rows, and `"openai"` there becomes `"openai_compatible"` where it meant the compatible protocol.
- `:645-656`: the "native /v1/responses" checkbox shows only for `openai_compatible`. On `openai`
  a line says "/v1/responses is forwarded natively".
- The Upstreams list shows decision 13's note on an `openai` row whose host is not OpenAI's.
- `model_catalog.rs` names makers (`openai/gpt-5`, `:54-56`, `:1407-1414`), not protocols, and is
  unchanged.
- The Prices page gains the tier column (§5.7).

### 1.6 Catalog and price sync

- `catalog::sync_prices` (`catalog.rs:609-670`) prices from catalog `pricing` objects
  (Kilo/OpenRouter shape, `parse_pricing_openai`). OpenAI's `/v1/models` carries none, so an
  `openai` row syncs no rows and reports every model as unpriced (`unpriced_models`), as it does
  today. The owner's manual rows price it. Synced rows are `standard` (§5.4).
- `catalog.rs:272` (local kinds skip the sync) is unchanged.
- OpenAI's catalog gives no context length, output cap, reasoning levels or modalities. On an
  `openai` row these stay unknown until the owner's capabilities override sets them, as today.
  Reading them from another source is not part of this design (Later).

### 1.7 Tests and callers outside core

- 306 `Protocol::Openai` sites in 101 test files, plus `"openai"` strings in about 20 more
  (`tests/it/upstream_settle.rs`, `migrations/llama_cpp.rs`, `structured_output/classify.rs`,
  `mcp_selfadmin/*`, the goldens, …). Nearly all of them build a wiremock OpenAI-shaped upstream
  and become `OpenaiCompatible` mechanically, as WP2b of the llama design did for 19 files. The
  ones that assert OpenAI's own texts (the `reasoning_fit/refusal.rs` fixtures
  `OPENAI_NO_PARAM`/`OPENAI_NO_NONE`, the `openai_*` structured-output fixtures) are listed in the
  WP2 commit with their choice.
- Support worlds that build upstreams: `support/gpu_world.rs:1180`, `docs_world.rs:123`,
  `realtime_audio.rs:146`, `realtime_tts.rs:174`, `:234`, all compatible.
- Outside lmgw: `scripts/webrtc_interop.py:69` and Kai's
  `kai/src/voice/lmgw_live_tests.rs:368` create mock upstreams with `protocol: "openai"` (§4.5).
  kai-mobile writes no protocol.

## 2. The shared module

`egress/openai_wire/` stays the shared base of three egresses. Nothing moves out of it. It gains:
- `decode.rs`: `OpenaiDecoder` and `parse_completion` read the top-level `service_tier` **when
  asked** (a `read_tier: bool` on the decoder, a flag on the parse call). The compatible egress
  never asks, so its rows have no tier (§5.2). `parse_usage` is unchanged. It already reads
  `prompt_tokens_details.cache_write_tokens` (`decode.rs:42`), and OpenAI's Chat reference lists
  that field (checked 2026-10-24, below). It stays shared: a compatible row relaying OpenAI's
  usage keeps its cache writes.
- `body.rs`: `build_chat_body` takes a third seam, a `BodyDialect {cap, llama_samplers}`: the name
  of the output cap (`max_tokens` or `max_completion_tokens`) and whether the modelled llama
  sampler extensions (`LLAMA_SAMPLERS`: `top_k`, `min_p`, `repeat_penalty`) are written. The
  compatible and llama egresses pass `BodyDialect::COMPATIBLE`, so their bytes do not change;
  the platform passes its own `egress::openai::DIALECT` (WP3).
- `structured.rs`: the classifier splits into an OpenAI half (`openai_refusal`) and a tolerant
  half (§8). The compatible and llama egresses call both (`refusal`), the platform egress the
  OpenAI half (WP3).

The Responses wire for the platform's chat (§7.2), if taken, is platform-only and lives under
`egress/openai/responses/`, not in `openai_wire`.

## 3. The two egresses

### 3.1 `openai_compatible`

`git mv egress/openai.rs egress/openai_compatible.rs`, `OpenaiEgress` renamed
`OpenaiCompatibleEgress`, and its `proto()` returns `OpenaiCompatible`. Its behaviour does not
change: the scalar `reasoning_effort` step, the OpenRouter `reasoning` object reconciled by the
shared builder, `max_tokens`, the sampler extensions, the Jina rerank, System One, the tiktoken
count, and `timings` (a generic row in front of llama-server or llama-swap). Its doc says "every
OpenAI-shaped provider that is not the OpenAI platform".

### 3.2 `openai` (the platform)

`egress/openai.rs` holds a new `OpenaiEgress`, with children `reasoning.rs`, `errors.rs`,
`tier.rs` and `tests.rs`, plus `responses/` if §7.2 is taken. At WP1 it composes `openai_wire`
exactly as the compatible egress does. The corpus proves the two give the same bytes. It never
offers rerank or System One (trait defaults: `Unsupported`). OpenAI has neither, and the route
guard refuses System One before admission (§1.1).

### 3.3 Intended changes on the platform egress

| | Change | Section | WP |
|---|---|---|---|
| P1 | The output cap goes as `max_completion_tokens` | §7.1 | WP3 |
| P2 | The modelled `top_k`, `min_p`, `repeat_penalty` are not written; they are named as ignored (`x-lmgw-params-ignored`). A client's own unknown keys still go verbatim | §7.1 | WP3 |
| P3 | An OpenRouter `reasoning` object is translated into `reasoning_effort`, not forwarded | §7.1 | WP3 |
| P4 | OpenAI's error codes: `context_length_exceeded` → `ContextExceeded`, flex `429` "resource unavailable" named, quota apart from rate | §8 | WP3 |
| P5 | `service_tier` read, stored and priced | §5 | WP4 |
| P6 | `/v1/responses` native without a flag, with the loop check | §6 | WP2 (column) and WP5 |
| P7 | Chat over OpenAI's Responses wire | §7.2 | WP7 (decision 16) |
| P8 | Encrypted reasoning carried between turns | §7.3 | WP8 (decision 17) |
| I1 | `request_logs.egress_proto` says `openai_compatible` on every former `openai` row (`proxy/recording.rs:300`, `:1147`) | — | WP1 |

## 4. The protocol and the migration

### 4.1 Migration 0090

It follows 0058's twelve-step rebuild: `-- no-transaction`, `PRAGMA foreign_keys = OFF`, one
`BEGIN … COMMIT`, the `sqlite_sequence` high-water mark carried over, and the FK check in Rust
after the migrator (`store/migration_guards/foreign_keys.rs`). The new table keeps every column
0058 had, plus `catalog_all_modalities` from 0080 with its CHECK.

- **CHECKs:**
  - `protocol IN ('openai','openai_compatible','anthropic','gemini','llama_cpp')`;
  - `kind IN ('generic','llama_server','audio_cpp')` (unchanged);
  - `protocol <> 'openai' OR kind = 'generic'` (decision 7);
  - `supports_responses = 0 OR protocol = 'openai_compatible'` (decision 8; this subsumes 0058's
    `supports_responses = 0` half for `llama_cpp`);
  - 0058's two llama CHECKs, the first reduced to `protocol <> 'llama_cpp' OR kind =
    'llama_server'`.
- **The copy:** every column and id. The `CASE`s are idempotent:
  - `protocol = 'openai'` → `'openai_compatible'` (decision 2, every row, whatever its host);
  - `supports_responses` → 0 where the protocol is `anthropic` or `gemini`.

  A replay after a commit sqlx did not record maps nothing twice, because no row is `openai`
  any more.
- **Rows that must not exist** cannot be copied silently. An `openai` row of kind `llama_server`
  cannot exist since 0058. `openai`+`audio_cpp` becomes `openai_compatible`+`audio_cpp`, which is
  allowed. So nothing is refused.

### 4.2 The guard: `store/migration_guards/openai_compatible.rs`

A pre-migration notice in the 0058 style (`llama_cpp.rs`), gated on `_sqlx_migrations` existing,
90 not applied, and the `upstreams` table present. Per row:
- INFO: "upstream '<name>' (id N) was protocol openai, which is now spelled openai_compatible —
  nothing about it changes";
- for a row whose `base_url` host is `api.openai.com` or `*.api.openai.com`, a second line: "this
  looks like the OpenAI platform: set its protocol to openai on the Upstreams page to use the
  OpenAI egress (service tiers, /v1/responses without the flag)". It only says this; the row is
  not changed (decision 2);
- WARN for every `anthropic`/`gemini` row whose `supports_responses` is cleared: "neither API
  serves /v1/responses; lmgw synthesizes it from now on".

`migration_guards.rs:1-21` lists the new module.

### 4.3 `settle`

`settle(protocol, kind, supports_responses, current)` gains these rows. The llama rows are
unchanged except that `openai_compatible` stands where they say `openai`:

| protocol sent | kind sent | result |
|---|---|---|
| `openai` | none or `generic` | `openai` + `generic` |
| `openai` | `audio_cpp` | refused: "protocol openai is the OpenAI platform; an audio.cpp server is openai_compatible" |
| `openai` | `llama_server` | refused: "use protocol llama_cpp" (the 0058 text, reworded) |
| `openai` | none, current kind `audio_cpp` or `llama_server` | kind `generic`, said in the response |
| `openai_compatible` | `llama_server` | refused: "use protocol llama_cpp" |
| `openai_compatible` | none, current kind `llama_server` | kind `generic` (leaving llama) |
| none | `audio_cpp`, current protocol `openai` | refused: "send protocol openai_compatible as well" |

`supports_responses`: sent `true` on anything but `openai_compatible` is refused, by protocol
("implied on openai", "anthropic/gemini do not serve it", the llama text). A row that had it on
and leaves `openai_compatible` gets it cleared, with a note. An `openai` result whose `base_url`
host is not OpenAI's gets decision 13's note. The dashboard path (`web/api.rs:1020-1081`) and the
MCP path (`ops/routing.rs:48-136`) both call `settle`, so both get the same notes.

### 4.4 Surfaces

- MCP `upstream_set` (`catalog/routing.rs:38-44`, `:52-58`, `:80`, `:97`), and through it the
  OpenAPI op: the enum and every description (§1.4).
- `lmgw-api-types/src/upstreams.rs:20-25`, `:35-40`: docs.
- UI (§1.5).
- `Upstream::forwards_responses()` (§6) is the only reader of the flag outside settle and the store.

### 4.5 What breaks for callers that write `protocol: "openai"`

The API types carry the protocol as a `String` (`UpstreamView.protocol`), so the break is a
**change of meaning, not a parse error**. A caller that creates `protocol: "openai"` for a
compatible provider still succeeds, and gets a platform row: OpenAI-only body changes (P1–P3),
`/v1/responses` forwarded natively, no `?output_modalities=all`, no System One. Decision 13's note
is the only signal. So every such caller is changed in the same landing:
- `scripts/webrtc_interop.py:69` and `examples/seed_usage.rs`: `openai_compatible`;
- Kai's `kai/src/voice/lmgw_live_tests.rs:368` (a mock audio upstream): `openai_compatible`. Kai
  raises its lmgw pin at its next landing, as usual;
- the tests (§1.7);
- an `update` that sends `protocol: "openai"` to a compatible row moves it to the platform; the
  response's note says so;
- reading clients see `openai_compatible` in `/api/upstreams`, `lmgw__upstreams` and the Traffic
  page's egress column (I1). Kai and kai-mobile do not match on protocol values.

## 5. Service-tier pricing

### 5.1 What OpenAI says (checked 2026-10-24)

From the API reference
(<https://developers.openai.com/api/reference/resources/responses/methods/create.md>,
<https://developers.openai.com/api/reference/resources/chat.md>,
<https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events.md>),
the Fast mode guide (<https://developers.openai.com/api/docs/guides/fast-mode.md>), the Flex guide
(<https://developers.openai.com/api/docs/guides/flex-processing.md>) and the pricing page
(<https://developers.openai.com/api/docs/pricing.md>):
- **Request values:** Responses takes `auto | default | flex | scale | priority | fast |
  ultrafast`. Chat Completions takes the same set without `ultrafast`. Unset means `auto`, the
  project's setting, `default` unless the project is configured otherwise. A project can make
  Fast its default, so a request that names no tier can still be served at Fast.
- **The response reports the tier that served the request**, "which may be different from the
  value set in the parameter". This is the top-level `service_tier` of the Responses object, of
  the chat completion object, and of every `chat.completion.chunk`. The reference's examples show
  `"default"`. **On a Responses stream** (live, 2026-10-24) `response.created` and
  `response.in_progress` echo the tier the request asked for; only the terminal event carries the
  tier that served it.
- **Fast is Priority:** "Priority processing was renamed Fast mode on July 30, 2026". Both request
  values work, and "for GPT-5.6 and earlier, the response returns `priority`". Under ramp-rate
  limits OpenAI "may downgrade some Fast mode requests to standard speeds and charge standard
  rates", and then the response says `default`.
- **Flex** is priced "at Batch API rates". It can fail with `429 Resource Unavailable`, which is
  not charged.
- **Ultrafast:** the reference says it is "currently available for `gpt-5.6-sol`", while the
  pricing page lists Ultrafast prices only for `gpt-6-astra` and `gpt-6.1-sol`. A response served
  there says `ultrafast`.
- **Scale:** in the request and response enums, but the pricing page has no Scale prices and no
  Scale guide was found (its URL is 404).
- **Pricing tables per tier:** Standard, Batch, Flex, Fast, Ultrafast. Newer models have short-
  and long-context columns (long context is above 272K input tokens), each with input, output,
  cached input, and 30-minute and 12-hour cache writes. Example, `gpt-5.6-sol`, short context, per
  1M: Standard $4.00 / $0.40 cached / $20.00 out; Batch $2.00 / $0.20 / $10.00; Fast $8.00 /
  $0.80 / $40.00. Regional (data-residency) endpoints add 10% for models released on or after
  2026-03-05.
- **Usage carries no money.** The Responses `usage` has `input_tokens`,
  `input_tokens_details.{cache_write_tokens, cached_tokens}`, `output_tokens`,
  `output_tokens_details.reasoning_tokens` and `total_tokens`. There is no cost field, and cost
  per request is not returned anywhere (only the admin Costs API, in daily buckets). The Chat
  `usage` has `prompt_tokens_details.{audio_tokens, cache_write_tokens, cached_tokens,
  image_tokens, text_tokens}`.

So the tier is the one price dimension a request tells lmgw about, and lmgw dropped it before
WP4. Long context, the 12-hour cache write and the regional uplift are dimensions too. They are
left for a follow-up design (decision 20, Later).

*Re-checked with WP4 (2026-10-24), the chat reference and the Fast mode, Flex and pricing pages:*
the facts above hold. Two details matter for §5.2's "absent is unknown". The chat reference's
own `service_tier` text says "When the `service_tier` parameter is set, the response body will
include the `service_tier` value based on the processing mode actually used", which reads as if
an answer to a request that sets none may carry none; and its create-completion example
response shows `"service_tier": "auto"`, a value that names no price tier (§5.4). Both make such
answers unpriced with a note as built. WP6's probe settles what OpenAI really sends at no tier;
if it omits the field there, every platform request that names no tier is unpriced until the
owner decides otherwise.

### 5.2 Reading it

- **Chat, whole:** `parse_completion` with `read_tier` puts the raw string on
  `Completion.service_tier: Option<String>`.
- **Chat, streamed:** every chunk carries it, so the decoder keeps the latest value and emits
  `StreamDelta::ServiceTier(String)` once it is first seen and again whenever it changes. The
  final chunk, which `stream_options.include_usage` makes carry `usage` (`body.rs`, `decode.rs:214`),
  carries it too.
- **Responses wire** (§7.2), whole and streamed: `response.service_tier` from the body, or from
  the `response.completed` / `response.incomplete` / `response.failed` event's response object.
- **Native `/v1/responses` passthrough** (§6): today the row records no usage at all
  (`responses.rs:621-638`). The stream is passed through untouched as now, but a tee parses each
  SSE event as it goes by, without buffering, and keeps the terminal event's `usage` and
  `service_tier` (only the terminal event: `response.created` and `response.in_progress` echo
  the requested tier, so a cut stream's row has tier NULL). A whole body is read whole. The row is then written when the stream ends, not
  at its headers. It is written from a task of its own, so a client that hangs up mid-stream still
  gets its row with whatever usage arrived (the pattern of 7882b33f..c647b27e). A stream cut
  before its terminal event leaves usage NULL with the note "stream ended before the upstream
  reported usage" (as built: the row's error, kind `incomplete`, until `price_note` exists). The same tee serves flagged `openai_compatible` rows, without the tier.
- **Not a tier:** a response without `service_tier` on an `openai` row has an unknown tier, not
  `default`. A project can default to Fast, so absence proves nothing. The probe (WP6) shows
  whether OpenAI ever omits it.

The tier is not put into `ir::Usage` (it is `Copy`, merged latest-wins and summed across turns).
It is one fact per upstream response, and every model turn has its own row.

*As built (WP4):* the platform egress reads it (`openai_wire::ReadTier`, `parse_completion_with`,
`OpenaiDecoder::reading_tier`); the compatible and llama.cpp egresses never do. Every row writer
takes a `pricing::ServedTier`: `Untiered` (no tier dimension: every protocol but `openai`, and
the `openai` routes whose answers name no tier, such as embeddings, audio, images and legacy
completions) or `Reported { served, requested }`, built by `ServedTier::of(protocol, …)` on the
chat, Chat-tab, in-process turn and native Responses paths. The synthesized Responses loop is
priced per turn like any in-process call; the Responses wire for the platform's chat is WP7, so
until then a synthesized turn's tier comes from its Chat Completions answer. A stream cut before
its terminal event keeps the `incomplete` error kind and gets the same words as its
`price_note`; a stream its client left before the terminal event (`canceled`) gets "the client
left before the upstream reported usage". Such a note wins over a tier note: with no usage the
tier is moot.

*As built (WP4, decision 14):* the platform egress sends `service_tier: "auto"` whenever the
client set none. OpenAI's reference makes an unset tier `auto`, the project's setting, so the
behaviour is the same, and a request that sets the parameter gets the tier actually used in its
answer; the Flex guide has callers send `auto` explicitly. It is written in
`egress::openai::chat_body` (so chat whole and streamed, in-process turns, the Chat tab and the
synthesized Responses loop's turns) and in `responses::native::upstream_body` only on an `openai`
route (a flagged compatible row shares that forward and is sent nothing). The `openai_compatible`
egress and the count bodies are untouched. It is added after the request's own tier is read, so
`requested_service_tier()` stays what the client asked and a note never says "(requested: auto)".

### 5.3 Storing it

Migration 0092 (with WP4) adds:
- `request_logs.service_tier TEXT`: the raw value OpenAI reported, NULL when none was read
  (every non-`openai` row);
- `request_logs.price_note TEXT`: why a row's cost is what it is, when that is not plain. Today it
  is used by §5.6, §5.2's cut stream and §6.1's hosted-tool fees. NULL otherwise;
- `prices.service_tier TEXT NOT NULL DEFAULT 'standard' CHECK (service_tier IN
  ('standard','flex','fast','ultrafast','scale'))`, and `idx_prices_scope` rebuilt as
  `(scope_kind, scope_key, source, unit, service_tier)`. SQLite cannot change a CHECK in place,
  but this is a new column with its own CHECK, so `ALTER TABLE ADD COLUMN` does it. Only the index
  is dropped and recreated. Every existing row becomes `standard` (decision 11).

`store::upsert_price` keys on the tier as well. The catalog sync writes `standard` only.

*As built (WP4):* `migrations/0092_service_tier_prices.sql`, as above: three `ADD COLUMN`s and
the index dropped and recreated, in sqlx's own transaction. `prices` is not rebuilt (0070's
precedent was a rebuild because a CHECK changed; here the CHECK is the new column's own), and
no pre-migration guard stands in front of it: every existing row takes the default, the wider
unique index cannot conflict, so nothing can be lost or refused. On a database with manual OpenAI price rows (scope `6:*`), those rows become `standard` with everything else on them kept
(`tests/it/migrations/service_tier_prices.rs` covers that shape). `list_prices` orders a
scope's rows of one unit standard first. The file also adds `request_logs.priced_alias` (the
alias the cost was priced under when a fallback answered, NULL otherwise; the re-check in §5.6
reads it) and a partial index `idx_request_logs_price_note ON request_logs (service_tier) WHERE
price_note IS NOT NULL`, so the unpriced-tiers query reads the noted rows only (a test checks its
query plan).

### 5.4 Response value → price tier

| `service_tier` reported | price tier | source of the mapping |
|---|---|---|
| `default` | `standard` | "standard pricing and performance" (reference) |
| `flex` | `flex` | |
| `priority`, `fast` | `fast` | the renaming (Fast mode guide) |
| `ultrafast` | `ultrafast` | |
| `scale` | `scale` | priced only by a manual row; there is no public price |
| `auto`, anything else, absent | none | unpriced with a note (§5.6) |

On an `openai_compatible` row no tier is read, and the price tier is `standard`. The tier
dimension does not exist there.

### 5.5 Resolution order

`Snapshot::sheet_for` (`config/snapshot/sheet.rs:29-51`) and `sheet_rows` (`:58-119`) take the
price tier. Per unit, the chain stays the same (alias manual, alias catalog, upstream model
manual, upstream model catalog), each restricted to rows of that tier. A local upstream still
short-circuits to free. `recording.rs:416` passes the tier mapped from the route's response. The
other readers (`ops/usage_prices.rs:67`, `web/api_usage.rs:865` (the local counterfactual), the
Decision lab's price line) pass `standard`, because they ask what a scope costs, not what one
response cost.

*As built (WP4):* `sheet_for` and `sheet_rows` take `Option<PriceTier>`; `None` (an answer that
named no price tier) selects no row. `Sheet::tier_gap` is set when a unit has a usable row at
some other tier along the same chain but none at this one, and names every tier that has one
(`TierSet`); `price_request` treats a gap as an unknown part. A unit with no row at any tier is
absent, as before. So a flex token row beside a standard-only per-request fee is a gap for the
fee's unit. `price_call` (`proxy/recording.rs`) is the one place a row's tier reaches the sheet.

### 5.6 A tier with no price

Served at tier T with no usable row at T in some unit: that unit is **unknown**. The request's
cost is NULL, by the existing rule that one unknown part makes the whole unknown
(`pricing.rs:1-31`). `price_source` is `unknown`, and `price_note` says, for example: "served at
service tier flex (requested: default); no flex price on file for 'gpt-5.6-sol' — a standard one
is". The note names the tier reported, the tier requested when the client or alias set one, and
whether a `standard` row exists. Nothing falls back to Standard.

The trade-off: unpriced traffic adds nothing to the spend a budget enforces (`recording.rs:420-425`
says so). So until the owner enters flex or fast rows, flex and fast traffic escapes budgets.
That is visible but real (decision 15). To make it findable, the Prices page lists every scope that
answered at a tier it has no row for, with the request count since the first such request.
*Since decision 22* the escape is closed for the tiers that cost at least Standard: they count
toward budgets at the standard price as a lower bound, and flex without a flex price is refused
to a budgeted key (§13). The row itself stays unpriced.

*As built (WP4):* the note's wording is `pricing::gap_note`: "served at service tier flex
(requested: flex); no flex price on file for 'gpt-5.6-sol', a standard one is"; a value with no
price tier says "…which names no price tier; unpriced rather than priced at another tier", an
answer with none "the upstream reported no service tier; unpriced rather than assumed standard"; a row with no
tier dimension at all (`Untiered`, such as embeddings) whose scope has no Standard row says "no
standard price on file for 'text-embedding-3-small', a flex one is".
The note is written only on a row with something to price (tokens, a quantity, an answered
request), so a request that failed before it spent anything carries none. The list
(`ops::unpriced_tiers`, `PricesResponse.unpriced_tiers`, `lmgw__prices`) is read from the raw
request log (`store::unpriced_tier_traffic`), so the log's retention bounds how far back its
count reaches, and each group is asked of today's rows through `sheet_for` at its tier: a group
whose tier has a row now drops off (its old rows keep their cost). The re-check asks the alias
the row was priced under (`request_logs.priced_alias`, the fallback that answered), not the one
requested. Each entry names the units the row has to price (`missing_units`), and "Price flex"
opens the editor on the first of them. It names the row to enter on the upstream model's scope;
`priority` and `fast` are one entry. Groups answered `auto`, with a value that is new, or with no
tier are listed too, as "no tier reported" (`reported` says which), with no price action: no row
fixes them. The query finds the no-tier ones by their note's opening words
(`pricing::NO_TIER_NOTE`). An `Untiered` row's "no standard price" note is not on the list yet.

### 5.7 Surfaces

- Prices page and `lmgw__prices`: a tier column; `lmgw__price_set` and the dashboard form take
  `service_tier` (default `standard`); `lmgw__price_delete` is unchanged (by id). The OpenAPI op
  table follows the tool schema.
- Logs and Traffic: the row detail shows "tier: flex" and the note. `lmgw__logs` returns both
  fields.
- Usage page: unpriced counts already show. A grouping by tier is not part of this design.
- **Relayed to the client:** the OpenAI chat ingress puts the reported `service_tier` on its
  response and chunks, as OpenAI does, only when an `openai` row reported one. It follows the
  rule that OpenAI-shaped routes copy the protocol. The Responses ingress does the same on
  synthesized responses. Anthropic's `/v1/messages` has a `service_tier` in its own usage with
  its own vocabulary, so nothing is mapped there.
- **Asking for a tier:** a client's `service_tier` goes verbatim through the passthrough loop,
  as today. An alias that always asks for flex is not part of this design (Later).

*As built (WP4):* the Prices page has a Tier column, a tier picker in the editor (fixed on an
existing row, like scope and unit) and an "Unpriced tiers" band under the unpriced models, each
entry with a "Price flex"-style action that opens the editor on its scope and tier. Traffic's row
detail shows "Service tier" and "Price note"; the live `request` frame carries both. The OpenAI
chat stream puts the tier on every chunk after the upstream first named it: the role chunk
leaves before the upstream has said anything and has none. A synthesized Responses object
carries the last turn's tier (`LoopEvent::ServiceTier`). The Chat tab, realtime and
`/v1/messages` relay none.

## 6. `/v1/responses` without the flag

`Upstream::forwards_responses(req) -> bool` replaces the bare flag read at `responses.rs:144`:
- `Openai`: true (since WP5; WP2 shipped it false until metering and the loop check existed);
- `OpenaiCompatible`: the flag;
- every other protocol: false;

and, on both OpenAI-shaped protocols, false when the request needs lmgw's loop:
- a `{"type":"mcp"}` tool block whose `server_label` lmgw serves (the self-admin label and every
  registered MCP server: what `mcp::exec::resolve` would resolve, `responses.rs:887`), since
  OpenAI would try to reach those servers itself. An `mcp` block with a `server_url` that is not
  lmgw's goes native: it is OpenAI's hosted MCP tool. As built (`mcp::exec::serves`) the check
  is `resolve`'s own predicate: a built-in label, or a registered server `resolve` would find
  there for the caller, one its tool scope may reach and, for a bare server (no prefix), one that
  lists it something. For that last case the servers are listed first, the same set `resolve`
  lists, and only then. A server outside the caller's scope, and a bare one that lists it
  nothing, count as not served, as `resolve` answers them. So the path a request takes tells a
  scoped key that a label is served only where `resolve` (and `GET /v1/mcp/servers`) would tell
  it anyway. A service agent's sleeping bare server is not woken for the question, so it counts
  as not served until it is up;
- a `previous_response_id` found in lmgw's own response store: a chain lmgw synthesized continues
  in the loop.

Taken together, a request that needs the loop and also names a chain only OpenAI holds (a
`previous_response_id` lmgw does not have) cannot be served by either path. It is refused with a
400 naming both. Today the flag on a compatible row sends such requests native, where they fail
upstream. Today an `openai` row without the flag synthesizes everything. So the decision changes
an api.openai.com row once it is switched: its plain `/v1/responses` traffic goes native
(and is priced, §5.2), and loop traffic stays in the loop. The migration's notice (§4.2) and the
release note say so.

The structured-output early refusal and the fallback image fit (`responses.rs:121-128`, `:157`)
run before the decision, as now.

**An `mcp` block that is not lmgw's to serve.** A block with a `connector_id`, or with a
`server_url` that is not lmgw's own `/mcp` (the host the request was sent to, or one of the
bind address's own host names on its port), names a server the upstream is to dial: OpenAI's
hosted MCP tool. It never counts as served by lmgw, whatever its label, so on its own it goes
native with the body. When the request takes the loop anyway (the upstream does not forward
`/v1/responses`, or another block or a stored chain needs the loop), lmgw does not serve such a
block from a same-label server of its own: it is refused with a 400 naming the label and why the
request is in the loop. A block whose `server_url` is lmgw's own `/mcp` is read by its label, as
one without a url. Realtime still reads only the label (its `server_url` is accepted and unused).

**Hosted (non-`mcp`) tools** (`web_search`, `file_search`, …) pass natively since WP5b (§6.1).

**Retrieving a native response.** `GET`, `DELETE /v1/responses/{id}` and
`GET /v1/responses/{id}/input_items` answer from lmgw's own store first. Each native answer's
upstream id (from the whole body, or from `response.created`, recorded before that event reaches
the client) is kept in `native_responses` (migration 0091) with the upstream row, the key that
made it and the alias/upstream model; an id found there is forwarded to that upstream with the
query string, and its answer relayed as sent. A request with `store: false` records nothing,
since the upstream keeps nothing. An id in neither place is lmgw's 404. Both stores are scoped
to a key: an owner key sees every response, any other key only those it made, and a request
without a key only those made without one (rows from before 0091 are those). A foreign id answers
exactly as an unknown one does, on GET, DELETE, input_items and as a `previous_response_id`.
The native path scopes the same way: a `previous_response_id` that is in neither store answers
lmgw's 404 for a client key, and only the owner key may chain from ids lmgw never saw (made outside
lmgw on the same provider key). lmgw creates no conversations, so a body's `conversation` is a
client key's 404 too and passes only for the owner. A record in `native_responses` is never
rewritten by a later answer with the same id, and an empty id is not recorded.

**Hang-ups and ends on the native path.**
- A whole answer: the forward selects on its client's hang-up while it waits for the headers and
  the body, each bounded by the upstream's `request_timeout` where one is set. A client gone before
  the body arrived ends the forward (the upstream connection closes) and leaves the plain `499`
  `client_disconnected` row with unknown usage, its message saying no usage was reported. A body
  that arrived after its client left is the `499` with the body's full usage, counted as answered.
  *Changed in WP10 (§14.9):* the forward no longer ends on its client's hang-up; it reads the
  answer on, bounded by `request_timeout`, so the row is that `499` with the usage and the budget
  gets its cost.
- A stream cut on its client's hang-up before the terminal event is cut, not drained: draining
  would spend up to `max_output_tokens` for nobody and undo the 2026-10-22 runaway fix. Its row
  is `canceled` with whatever usage had arrived. The usage of a cut stream cannot be recovered:
  OpenAI does not store a response cut before its terminal event, and a `GET /v1/responses/{id}`
  after the cut answers 404 (live, 2026-10-24, twice). The idea of fetching it afterwards is
  dead. lmgw drops the cut response's record and its item ids when the stream ends, so a later
  fetch is lmgw's own `not_found`, not OpenAI's relayed 404.
- *After the WP9 review:* a chat-wire stream (`/v1/chat/completions`, `/v1/messages` and the
  in-process turns, on `openai` and `openai_compatible` rows alike) is read past its finish
  chunk even when its client or consumer has gone (`drive_upstream_until`): an OpenAI-shaped
  stream reports its usage only in a chunk after `finish_reason`, and draining after the finish
  costs no generation, so this is not the drain the rule above forbids. The rest goes to nobody;
  the reading ends when the upstream ends the body or a gap passes `request_timeout`, as any gap
  does. The row is `canceled` with the real usage. A native Responses stream carries its usage
  in the terminal event itself, so it has no point past the content to read on to: its cut stays
  the cut before the terminal event.
- A stream gap longer than the upstream's `request_timeout` ends the relay as
  ("timeout", "the upstream sent nothing for N s mid-stream"), as the chat relay does
  (*changed 2026-11-05*). `request_timeout` bounds the wait for the headers and every silence
  between events after them, on every streamed relay (the chat relay with its in-process and
  Chat-page callers, this passthrough, legacy completions; `proxy/silence.rs`); the stream's
  total length stays unbounded. A gap is counted in SSE events, not bytes: once the answer
  shows, keep-alive comments (llama-server's `:` every 30 s, OpenRouter's `: OPENROUTER
  PROCESSING`) and Anthropic's `ping` do not break it. The byte clock before this was reset by
  every keep-alive, so a provider that stalled behind its aggregator held the client's stream
  open until the client gave up (the live check of 2026-11-05: 181 s, a `canceled` row). A
  real event always breaks a gap, reasoning and thinking deltas included.
  *Changed 2026-11-05*: keep-alives still break a gap while the model may be thinking out of
  sight, which is where a provider legitimately sends nothing else for minutes: before the
  stream's first visible output (a text, refusal, tool-call or function-call delta, any other
  answer content), and inside a hidden-thinking span: an Anthropic `content_block_start` of
  type `thinking` or `redacted_thinking` until the `content_block_stop` of its index (with
  `thinking.display` omitted only pings come in between), and a Responses
  `response.output_item.added` of type `reasoning` until the `response.output_item.done` of its
  `output_index`. *Changed 2026-11-05*: an Anthropic `compaction` block is such a span too, until
  the `content_block_stop` of its index, and no visible output: at threshold compaction
  Anthropic writes the summary between the block's start and its single `compaction_delta`,
  with only pings in between. Chat Completions, legacy completions and Gemini have no such span (reasoning
  comes as deltas, and Gemini sends no keep-alives). In that state every byte read, half an
  event too, restarts the clock (`proxy/silence/sight.rs`). So a stream that stalls before any
  output while its upstream keeps sending keep-alives is not yet bounded by
  `request_timeout`: it stays open until the upstream or the client ends it, as before this
  change. *Changed 2026-11-05*: a tool the provider runs is work out of sight too, by the same
  ruling, since only keep-alives come while it runs, after visible output as well. An Anthropic
  server tool is a span from its block's `content_block_start` (a type ending in `_tool_use`
  other than a client's `tool_use`: `server_tool_use` for web search, web fetch, code execution,
  tool search, the MCP connector's `mcp_tool_use`) across the gap after its stop to the
  `content_block_stop` of the result block that answers it (a type ending in `_tool_result`,
  paired by `tool_use_id`; the tool runs between the two blocks, and the result "arrives
  complete in a single `content_block_start` event",
  <https://platform.claude.com/docs/en/agents-and-tools/tool-use/server-tools>). A client
  `tool_use` block ends every wait for a result not begun, since the API then runs no server
  tool and returns. *Changed 2026-11-05*: so does every other block that is neither a
  `_tool_use` nor a `_tool_result` one (a text, a thinking or compaction block): the model has
  visibly moved on, so a result whose type or `tool_use_id` did not match its tool no longer
  holds the wait open while the model streams text, and a stall behind pings after it is cut.
  Parallel server tools still pair, since their blocks come in a row. A tool block without an
  `id`, or a result without a `tool_use_id`, pairs with the oldest wait it could be. A Responses
  hosted call is a span from its `response.output_item.added` to
  its `response.output_item.done` and on until the next item is added or text shows: every item
  type ending in `_call`, and `mcp_list_tools`, but the calls the client runs
  (`function_call`, `custom_tool_call`, `computer_call`, `local_shell_call`,
  `apply_patch_call`, a `shell_call` whose environment is `local`, a `tool_search_call` whose
  `execution` is `client`). The gap after the item covers a hosted shell's and a server tool
  search's result, which is an item of its own (`shell_call_output`, `tool_search_output`).
  *Changed 2026-11-05*: a hosted call whose `response.output_item.done` never came (an
  aggregator dropped it) ends at the next added item that is no span (not a reasoning item, not
  another hosted call) or at visible text; a hosted call added while another is open leaves
  that one open, since the docs do not say whether hosted calls may overlap. The gap after a
  call's done is bounded by one more `request_timeout`: keep-alives restart the clock only
  within that time after the done, so a stall there is cut at the latest two timeouts after
  it. Once the answer is finished (a `finish_reason`, Gemini's `finishReason`, Anthropic's
  `message_delta` with a `stop_reason` or `message_stop`, a Responses terminal event;
  *changed 2026-11-05*: a `message_delta` without a `stop_reason` finishes nothing, as the
  decoder does not stop on it either, and an empty `finish_reason` is none on the decoder as
  on the clock) nothing is out of sight any
  more, a span left open included (a `pause_turn` may end on a server tool not run yet). The
  client gets its protocol's mid-stream error before the end: on this passthrough `response.failed` on the
  Response object `response.created` opened, marked failed with `server_error`, its `output`
  the items done so far and its `sequence_number` one past the upstream's last (an `error`
  event before any Response object), also after a body that broke; an upstream that sent its
  own `error` event gets none appended, and its row keeps that message. The raw passthroughs
  (this one, legacy completions) open the appended frame with a blank line, so one appended
  after half a relayed event is still an event of its own. On chat and legacy completions it
  is OpenAI's error chunk and `[DONE]`; on `/v1/messages` `event: error`; on the Responses loop
  its `response.failed`. A legacy stream whose last frame (a `finish_reason`, `[DONE]`) came
  is whole: a silence or a broken body after it is no error. *Changed 2026-11-05*: so is a
  chat-relay stream past its terminal delta (a `finish_reason`, Anthropic's `message_delta`
  with its `stop_reason`, Gemini's `finishReason`, a Responses terminal event), where only the
  usage tail, `[DONE]`, `message_stop` or the body's end is still to come. A silence there, or
  a broken body, ends the drain without an error, and the client's stream ends as the body's
  end would have ended it: `[DONE]` (after the usage chunk where `include_usage` asked and usage
  came) on `/v1/chat/completions`, `message_delta` and `message_stop` on `/v1/messages`,
  `response.completed` on the Responses loop. The row is a success. Usage that came is kept,
  and a stream whose usage never came is counted as one whose body ended before its usage
  (the price note "stream ended before the upstream reported usage", §13.6). The lease is not
  released at once, since the body did not end. Before this the gap ended as a `timeout` with
  the protocol's mid-stream error after a whole answer. The media relays
  (`/v1/audio/*` streams) and the streamed `GET /v1/responses/{id}` passthrough have no
  silence bound yet.
- A stream that ends with no terminal event and no error, and a whole body with no readable
  usage, leave the error kind `incomplete` ("stream ended before the upstream reported usage",
  "the upstream's answer reported no usage"), so an unpriced row is visible as such. A transport
  error after the terminal event leaves the whole answer's row as it was.

### 6.1 Hosted tools (WP5b)

The owner's rule "a gateway translates, never refuses or drops what an upstream has natively"
applied to the native path. *As built:*

- **Accepted at parse, refused only in the loop.** `parse_request` used to refuse hosted tools
  and several input shapes before routing. It now records each as a native-only part
  (`ingress/responses/native_only.rs`, `ResponsesRequest.native_only`): a tool of any type but
  `function`/`mcp`; a `tool_choice` of any type but `function` (hosted, `allowed_tools`, `mcp`,
  `custom`, …); an input item lmgw has no reading for (`item_reference`, also in its untyped
  `{id}` spelling, every hosted tool's `*_call` and `*_call_output`, `compaction`, …); a content
  part it has no reading for (`input_file`, an `input_image` by `file_id`); and
  `truncation: "auto"`. The native path forwards the body as sent. On an upstream that does not
  forward `/v1/responses` the first such part is refused with the error the parser used to give
  (same message, code and status), so a loop client sees what it always saw. A tool or tool
  choice without any `type` is still a parse refusal: no upstream reads it. The precedent was
  `ResponseFormat::Other` (§8).
- **Both needs.** A request the loop must serve (an `mcp` label lmgw serves, a
  `previous_response_id` lmgw stored) that also carries a native-only part is a 400 naming both
  (`responses/path.rs`, `in_the_loop`), as the existing "both needs" refusal does.
- **`background: true`** stays refused on every path, the error now saying why: a background
  response answers at once and finishes later, out of the relay's sight, so its usage never
  reaches a row and no budget sees it.
- **Facets.** A native-only part is out of the IR, so it carries the capability facet it uses
  (candidate-aliases §4.6): a hosted tool, a `tool_choice` naming one and a hosted tool's
  `*_call` record are tool calls, an `input_image` by `file_id` is vision. `responses::uses`
  adds them, so a candidate alias that enables neither refuses them like any other part.
- **What the upstream holds under the provider key** (`responses/references.rs`). The provider
  key is the owner's, so files, vector stores, containers, skills, prompt templates and stored
  items are the owner's, and lmgw proxies none of `/v1/files`, `/v1/vector_stores`,
  `/v1/containers`, `/v1/skills` or prompts. For a client key: a `file_id` / `file_ids` anywhere
  in the body but `metadata` (a `prompt`'s `variables` take `input_file` and `input_image`
  values), a `file_search` tool's `vector_store_ids`, a `code_interpreter` container id, a
  `shell` tool's `container_reference` and any `skill_id` in a tool (a `shell` environment's
  `skills`) never pass; nor does a `prompt` (lmgw creates no prompt templates, so no client key
  owns one: refused like `conversation`). Each answers `responses::no_stored_response`, exactly
  as an unknown id. The owner key passes.
- **Stored items are the key's that was given them** (migration 0093). OpenAI resolves an input
  item's `id` (a `reasoning` item's `rs_…`, a `message`'s `msg_…`, a `*_call`'s) and an
  `item_reference` against what it stores under the provider key, so a leaked id would pull the
  owner's items into a client key's prompt. The meter records every native answer's output item
  ids under its key (`native_response_items`, ON DELETE CASCADE from `native_responses`, so the
  same retention GC and upstream deletion take them): each `response.output_item.done` item as it
  passes and the terminal Response's `output`, before the event reaches the client. On the native
  path a client key's every input item `id` and `item_reference` id must be among its own;
  otherwise `no_stored_response`. A `store: false` answer is recorded too (`native_responses.
  stored = 0`: never fetched or continued, only what its items hang off), so a key replays its
  own stateless reasoning. A `reasoning` item that carries `encrypted_content` is checked as
  well: the create reference describes the field ("The encrypted content of the reasoning item")
  and the reasoning guide the stateless replay, but neither says a present `id` is then not
  looked up (<https://developers.openai.com/api/reference/resources/responses/methods/create.md>,
  <https://developers.openai.com/api/docs/guides/reasoning.md>, checked 2026-10-24). What is
  not recorded yet: the ids OpenAI gives a request's own input items (visible through
  `input_items`), so a client key's reference to one of those is refused. The vector-store,
  container and skill cases go beyond the original scope, which named only `item_reference` and
  `file_id`; they are the same hole a `file_search`, `code_interpreter` or `shell` tool would
  open.
- **Hosted-tool fees are unpriced.** Their per-call fees are in no `usage` object. Checked
  2026-10-24 against <https://developers.openai.com/api/docs/pricing.md>: web search
  "$10.00 / 1k calls" ($25 for the preview on non-reasoning models), file search
  "$2.50 / 1k calls", "Hosted Shell and Code Interpreter" per 20-minute container session, and
  image generation at the image model's own token rates. The Response `usage` has only input,
  output, cache and reasoning counts
  (<https://developers.openai.com/api/reference/resources/responses/methods/create.md>).
  Remote MCP has "No additional fees apply per tool call"
  (<https://developers.openai.com/api/docs/guides/tools-connectors-mcp.md>), so `mcp_call`
  is not billable. Client-run calls (`function_call`, `computer_call`, `local_shell_call`,
  `custom_tool_call`, `apply_patch_call`) cost tokens only. The native meter counts
  `web_search_call`, `file_search_call`, `code_interpreter_call`, `image_generation_call` and a
  `shell_call` whose environment is not `local` in the terminal Response's `output`
  (`openai_wire/response_object.rs`, `BILLABLE_CALLS`), on both native protocols. A stream cut
  before its terminal event counts the `response.output_item.done` items of those types that
  passed; the terminal Response's count replaces them when it comes. Any such call makes the
  row unpriced (cost NULL, never the token part alone). Its `price_note` reads "hosted tool fees
  not priced yet: 2 web_search_call, 1 code_interpreter_call; tokens alone would be 0.002750
  USD, which budgets count". The notes join, never replace each other: a tier note first, so the
  no-tier list still finds its own by their opening words; a cut stream's or a usage-less body's
  note stands in for the tier's (moot then) but not for the hosted part, so that row stays on
  the hosted list. A local route's real zero is left alone.
- **Budgets count the token part** (migration 0094). Unpriced traffic adds nothing to a budget's
  spend (usage-analytics cost-policy design; billable-units §7), which would let a budgeted
  client key send `tool_choice: {"type": "web_search"}` on every request and spend tokens
  without limit. A row unpriced *only* for hosted-tool fees therefore keeps its known part as
  the budgets' lower bound (`Cost::budget_floor_micro`, `request_logs.budget_floor_micro`, summed
  into `usage_hourly.budget_floor_micro`): `price_call` advances the cached key and global spend
  by it, and `store::key_spend_micro` / `total_spend_micro`, which re-seed that cache, add the
  column to `cost_micro`. Nothing else reads it: the row's cost, every total, the rollup's
  `cost_micro`, the unpriced remainder and a run's meters keep the row unpriced, never a partial
  sum shown as its cost. A row unpriced for any other reason too (a tier gap, no token price)
  has no bound. *Draft call, 2026-10-24; approved and extended the same day (decision 22):* a tier gap above Standard and a cut native stream get a bound too (§13).
- **Findable.** A sibling of WP4's list, not part of it: no price row can fix such a row, so the
  "Unpriced tiers" list's re-check against today's rows does not fit. `ops::unpriced_hosted_tools`
  (`PricesResponse.unpriced_hosted_tools`, `lmgw__prices`' `unpriced_hosted_tools`) groups the
  noted rows by alias and upstream model, with the calls of each type read out of each row's
  note (`HostedCalls::from_note`) and the requests since the first. `store::hosted_fee_traffic`
  returns the rows ungrouped, through the 0092 partial index of noted rows: each note carries its
  own token amount, so a `GROUP BY price_note` grouped nothing. The Prices
  page shows it as a "Hosted tool fees" band below "Unpriced tiers", without an action. An entry
  leaves the list only when its rows leave the log.
- **`SseDecoder`** (`sse.rs`) scans only the bytes a feed added: a multi-megabyte event (an
  `image_generation` partial image) in many small chunks used to be rescanned whole on every
  feed. The U+FFFD resync stays; a CRLF split across two chunks is now one line break (a lone
  `\r` at a chunk's end used to become a `\n`, and the next chunk's `\n` a false event boundary).
  The search runs over bytes (`memchr::memmem`): its resume point, one byte short of the end,
  can sit inside a multi-byte char, where a `str` slice panicked. A trailing `\r` is held until
  the next byte, so a server that ends lines in lone CRs has each event handed out one chunk
  late; `finish()` flushes it at the end of the stream.
- **Not yet:** pricing the per-call fees themselves. It needs a new price unit (per 1k calls of
  a tool, per container session) and a quantity on the row; that is a follow-up (Later).

## 7. Reasoning on the platform

### 7.1 The chat body (P1–P3)

From the reference (checked 2026-10-24): `reasoning_effort` takes `none, minimal, low, medium,
high, xhigh, max`, "model-dependent". The reasoning guide adds that GPT-6 Astra refuses `none`
with 400 and GPT-6.1 Sol refuses `none` and `minimal`
(<https://developers.openai.com/api/docs/guides/reasoning.md>). `max_tokens` is deprecated and
"not compatible with o-series models"; `max_completion_tokens` is the cap.
- **P1:** the platform egress writes `max_completion_tokens`. The gate's clamp and fit read the IR
  `max_tokens` as before (`gate/clamp.rs:39-41`, `gate/fit.rs:593-601`). Only the spelling on the
  wire changes.
- **The vocabulary:** lmgw's canonical levels (`catalog.rs:44`) are OpenAI's minus `none`, which
  is the spelling of off (`egress/openai.rs:58-66`). Every level goes verbatim. A model that
  refuses one is handled by the existing refusal learning (`reasoning_fit/refusal.rs`). That
  learning reads OpenAI's texts, which on the platform are known rather than relayed. No per-model
  table is added: it would go stale with every model.
- **P2:** OpenAI rejects parameters it does not know, so the modelled llama samplers are not
  written, and they are never dropped silently. The Chat names them through its existing sampler
  split (`web/chat_sampling.rs:80-110`). On the API routes a new `x-lmgw-params-ignored` header
  names them, beside `x-lmgw-reasoning-ignored`; the header also needs its `LMGW_HEADERS` entry
  and API-docs row. A client's own unknown keys still go verbatim: the client sent them.
- **P3:** an OpenRouter `reasoning` object from the client (`has_reasoning_object`) is an
  aggregator dialect, not OpenAI's. The platform egress reads its `effort` (or `enabled: false`)
  into the control, which the ingress already resolves, sends `reasoning_effort`, and drops the
  object. Its `max_tokens` budget has no OpenAI spelling, so it is named as ignored
  (`recording.rs:728`'s platform arm).

**As built (WP3).** The Chat reference that P1–P3 rely on is
<https://developers.openai.com/api/reference/resources/chat.md> ("Create chat completion"; the
`…/chat/subresources/completions/methods/create.md` path 404s, checked 2026-10-24). Its parameter
list has no `top_k`, `min_p`, `repeat_penalty` and no `reasoning` object.
- P1 and P2 are `egress::openai::DIALECT` (`max_completion_tokens`, no llama samplers).
- P2's header is `proxy::params_ignored`, stamped by `with_annotations` on
  `/v1/chat/completions` (unary and streamed), `/v1/messages` and the synthesized `/v1/responses`
  loop. Native `/v1/responses` passthrough forwards the client's body verbatim, so it names
  nothing.
- **One table for every egress's drops** (`egress/sampling.rs`), which the header and the Chat's
  split (`web/chat_sampling.rs`) both read, so the two cannot disagree again. Each row is what
  that egress's body builder writes, and a unit test builds each egress's body to hold the table
  to it. llama.cpp and OpenAI-compatible take everything (`BodyDialect::COMPATIBLE` writes the
  llama samplers; the Chat's old split had said a compatible row takes no `top_k`, `min_p` or
  `repeat_penalty`, which its egress sent all along); the platform takes no llama samplers;
  Anthropic takes `top_k` but no `min_p`, `repeat_penalty`, penalties or `seed`; Gemini takes
  `top_k` and `seed` but no `min_p`, `repeat_penalty` or penalties. The header's values are the
  six names in that order (`top_k`, `min_p`, `repeat_penalty`, `presence_penalty`,
  `frequency_penalty`, `seed`), an alias default included. In-process sends other than the Chat
  (agents, realtime, folder chat) do not surface a dropped sampler yet.
- P3 is `egress/openai/reasoning.rs`. The object leaves the finished body even when it set
  nothing lmgw reads (only `exclude`), and `reasoning_ignored`'s platform arm no longer lets the
  object rescue a bare "on". Its `exclude` is not named yet. `exclude: true` is what the
  platform's chat wire does anyway, and `exclude: false` asks for text it never returns.

Chat Completions returns no reasoning items and no reasoning text for OpenAI's models. The
reference's chat message has no reasoning field. So the Chat tab shows no reasoning for an
`openai` row on this wire, and nothing can be carried between turns.

### 7.2 Chat over the Responses wire (P7, decision 16, WP7)

The reasoning guide says reasoning models "work better with the Responses API", and that Chat
Completions "does not support function calling with GPT-6 Astra or GPT-6.1 Sol". So on the Chat
wire lmgw's tool loops cannot use OpenAI's newest models at all, and no reasoning text ever comes
back. WP7 makes the platform egress's `build_chat` post `{base}/responses` for lmgw's own sends:
`/v1/chat/completions`, `/v1/messages`, the synthesized Responses loop, and the in-process callers
(the Chat, agents, realtime, folder chat, MCP sampling). Native passthrough (§6, WP5/WP5b) is not
touched: it forwards the client's body verbatim, as now. The compatible and llama.cpp egresses
keep the Chat wire.

*Refreshed 2026-10-24 against what is built.* Since this section was first written, WP5 gave the
platform a native path that reads a Response object (`openai_wire/response_object.rs`,
`read_response`: usage, served tier, output item ids, billable hosted calls), WP5b records each
native answer's output item ids under its key (`native_response_items`, 0093), and WP4 prices
every row at the tier its answer reported. Two probe facts from 2026-10-24 apply: on a Responses
stream only the terminal event carries the tier that served the request (`response.created` and
`response.in_progress` echo the requested one), and `include: ["reasoning.encrypted_content"]` is
legacy, since `encrypted_content` comes by default with `store: false`. The gate's "counted
equals posted" check exists for ladder rows only (local, `gate/send.rs`), so the platform has no
gate count to change: its count stays tiktoken over the text (`build_count_tokens`).

#### 7.2.1 Which wire a send takes

`egress/openai/responses/wire.rs`: `fn wire_for(ir: &ChatRequest, params: &Params) -> Wire`
(`Responses` or `Chat`). The Responses wire is the default. A send takes the Chat wire when it asks
for something only Chat Completions has, so nothing is dropped (the rule "a gateway translates,
never refuses or drops"):
- a modelled field the Responses API has no spelling for: `stop`, `seed`, `presence_penalty`,
  `frequency_penalty` (the create reference's echoed request fields have none of them, checked
  2026-10-24);
- an `input_audio` content part (the Responses input has `input_text`, `input_image` and
  `input_file` only, same check);
- a passthrough key in a `CHAT_ONLY` list beside it: `n`, `logit_bias`, `logprobs`,
  `top_logprobs`, `audio`, `modalities`, `prediction`, `web_search_options` (and the legacy
  `functions`, `function_call`). `top_logprobs` is a Responses field too, but there it goes with
  `include: ["message.output_text.logprobs"]`, where a chat client's goes with its `logprobs`, so
  it keeps the Chat wire beside it;
- a cap below 16 (`max_tokens` under `MIN_OUTPUT_TOKENS`, *added 2026-10-25, WP7 review F-B2*):
  the Responses API refuses a `max_output_tokens` below 16 (`integer_below_min_value`, reported
  against it; the create reference names no minimum), Chat Completions takes it, and a token-limit
  probe (`max_tokens: 1`) or a short classifier sends one. It goes on the Chat wire with the
  caller's exact cap, never raised.

Every other passthrough key goes into the Responses body verbatim, as on the Chat wire.

*Fact (live probe 2026-10-25, P1):* GPT-6 itself (luna, astra) refuses `stop` on either wire, a
`400 unsupported_parameter`, `param: "stop"` ("Unsupported parameter: 'stop' is not supported with
this model."), so taking the Chat wire for `stop` only makes the refusal OpenAI's own; older models
take it there. The body is `tests/fixtures/openai_platform/stop_unsupported.json`.
`egress/sampling.rs`'s platform row does not change: a send that sets penalties or `seed` takes
the Chat wire, which writes them, and the llama samplers stay dropped and named on both.

#### 7.2.2 The body (`egress/openai/responses/body.rs`)

- `model`, `store: false`, `stream`, and `service_tier` through `ask_for_the_tier` (§5.2). No
  `include` (probe fact above).
- System text → `instructions` (`ir.system_text()`, as the Anthropic and Gemini egresses hoist
  it). A system message after the first user turn goes into `input` as a `developer` message at
  its place, so the order holds. *Owner, 2026-10-24: as drafted (§7.2.5).*
- User content: text → `input_text`; an image → `input_image` (`image_url`, a data URL or the
  URL as sent, and `detail: "auto"`: the IR carries no detail, so a chat client's
  `image_url.detail` is not carried yet); a PDF or other file → `input_file` (`filename`,
  `file_data`; not yet, the IR has no file part).
- Assistant text → `{"role": "assistant", "content": "<text>"}`, the input's `EasyInputMessage`,
  each run of text one (*changed 2026-10-25, WP7 review*: an output `message` item, as first built,
  needs the `id` and `status` its schema requires, and the probe replayed no assistant text, so
  nothing showed that form accepted). A refusal replays as its text. A tool call →
  `function_call` (`call_id`, `name`, `arguments` as a JSON string). Reasoning → a `reasoning`
  item only with an OpenAI seal (§7.3, WP8). Unsealed reasoning text is not sent; the Chat wire
  sends it as `reasoning_content` (llama-server's and DeepSeek's name, which OpenAI's Chat
  Completions does not document), and the Responses input has no place for it. An assistant
  message's `phase` (gpt-5.3-codex and later ask that it be preserved and resent) is not read or
  carried yet; it goes with WP8's seals.
- A tool result → `function_call_output` (`call_id`, `output`). A text-only result goes as a
  string. A result with images goes in the array form ("An array of content outputs (text,
  image, file) for the function tool call", create reference, checked 2026-10-24), so the
  platform's tool results stop being flattened to text (`FlattenToolResults` stays for the Chat
  wire). Only an image OpenAI takes goes as `input_image`: PNG, JPEG, WEBP or a still GIF ("non-
  animated GIF", the vision guide), by its mime and its bytes (the llama.cpp egress's
  `ImageFormat` readers, a GIF's images counted); any other (an MCP tool's SVG) is its placeholder
  with a WARN, as on the Chat wire (*added 2026-10-25, WP7 review F-B1*), since one would make
  every later send of the thread a 400.
- Tools → `{"type": "function", name, description, parameters, strict}` (no `description` key
  when there is none, as 135fb92c made it on the Chat wire). `strict` is the client's own
  (`ToolDef::strict`, a chat, Anthropic or Responses tool's), and where it said none each ingress
  fills its own dialect's default: the chat and Anthropic ingresses `false` (neither's tool is
  strict unless it says so), the Responses ingress nothing, so the body writes no key and OpenAI's
  own default holds ("strict validation when the schema is compatible", the create reference,
  checked 2026-10-25). An in-process caller's tool (lmgw's MCP tools) says nothing either. The
  Chat wire writes `strict: true` only, since not strict is its own default (*changed
  2026-10-25, WP7 review F-R4, then the WP7 review's residual b*); `tool_choice` → `auto`,
  `none`, `required` or `{"type": "function", "name"}`; `parallel_tool_calls` passes through.
- Format → `text.format` (`json_schema` with `name`, `schema`, `strict`; `json_object`; `text`),
  the inverse of `ingress/responses.rs`'s `parse_text_format` (`:311`). `strict` stays as
  decision 18 has it.
- Reasoning → `reasoning: {effort, summary: "auto"}`. An off goes as `effort: "none"` through
  `reasoning_fit`, as on the Chat wire. A Responses client's own `reasoning.summary` goes as it
  sent it (`Params::reasoning_summary`): a kind verbatim, `null` as no summary; `auto` is lmgw's
  only where the client said nothing, and only that one is the lesson's to take back (*changed
  2026-10-25, WP7 review F-R4*). A chat client's `verbosity` and a Responses client's
  `text.verbosity` both go as `text.verbosity`. The refusal learning already reads a message naming
  `reasoning.effort` (`reasoning_fit/refusal.rs:106`). A summary is opt-in, and some orgs need
  verification first; a refusal that names `reasoning.summary` is learned per route (the
  pattern of `state.reasoning_learned`, a second lesson kind), after which the summary is left
  out, with an INFO line.
- The cap → `max_output_tokens`. `temperature` and `top_p` as they are.

A shared `egress/openai_wire/responses_items.rs` holds the item and event names that the egress
and the Responses ingress encoder (`ingress/responses.rs:781` on) both use.

#### 7.2.3 Reading the answer (`egress/openai/responses/decode.rs`)

The stream decoder runs `crate::sse::SseDecoder` and maps:
- `response.output_text.delta` → `TextDelta`; `response.refusal.delta` → `RefusalDelta`, a
  whole Response's `refusal` part `ContentPart::Refusal`, as the Chat wire's decoder reads
  `message.refusal` and `delta.refusal` (`openai_wire/decode.rs`, *changed 2026-10-25, WP7 review
  F-R1*). The OpenAI chat ingress writes it as `message.refusal` / `delta.refusal`, the Responses
  ingress as a `refusal` part (`response.refusal.delta`, `.done`), the Anthropic ingress as text,
  and every consumer without a refusal of its own (the Chat, agents, realtime, sampling) and every
  replay upstream reads it as text. A client's `refusal` part is read back on an assistant message
  only; on any other role both OpenAI ingresses refuse the request with a 400, as OpenAI does,
  rather than drop it, and a tool turn's stored record trims a final refusal as it trims a final
  text (the Chat's `content` holds both; *added 2026-10-25, the WP7 review's residuals e, f*);
- `response.reasoning_summary_text.delta` → `ReasoningDelta`, with a blank line between summary
  parts;
- `response.output_item.added` of a `function_call` → `ToolCallStart` (index, `call_id`, name);
  `response.function_call_arguments.delta` → `ToolCallArgsDelta`;
- `response.output_item.done` of a `reasoning` item → its seal (§7.3). The reference warns that
  the copy in `output_item.added` "may be incomplete";
- the terminal event (`response.completed`, `response.incomplete`, `response.failed`) → usage
  and `StreamDelta::ServiceTier` through `read_response`, then `Stop`: tool calls if any
  `function_call` item was output, `content_filter` for `incomplete_details.reason:
  "content_filter"`, `length` for every other incomplete one (`max_output_tokens`,
  `max_messages`, `steered`, one added later, or none, its raw reason logged; *changed
  2026-10-25, WP7 review F-R2*), otherwise stop. A `response.failed` or an `error` event is
  `StreamDelta::Error`, its error read by `map_error`'s rules. The tier comes from the terminal
  event only, never from `response.created`;
- a stream that ends without its terminal event was cut: the decoder's end hook
  (`EgressStreamDecoder::on_end`, none by default) ends it with the error "upstream stream ended
  before its terminal event", never a clean stop, and the chat and Anthropic encoders write no
  finish after an error; the cut stream's budget floor stands (*added 2026-10-25, WP7 review
  F-R3*). The Chat wire's decoder (`openai_wire/decode.rs`, `OpenaiDecoder`) has no end hook
  yet: a Chat-wire stream cut before its `finish_reason` still ends with a clean stop, on the
  platform's Chat-wire sends and on every OpenAI-compatible and llama.cpp row.

*Fact (live probe 2026-10-25, P3):* `summary: "auto"` does not always bring a summary: a
`reasoning` item may come with an empty `summary`, which the decoder reads as no reasoning text.
A real gpt-6-luna stream (96 summary deltas, then a call) is
`tests/fixtures/openai_platform/luna_reasoning_then_call.sse`, its `encrypted_content` scrubbed.

Usage as §5.1 has it: `input_tokens` is the total including cache (the IR's meaning),
`cached_tokens` and `cache_write_tokens` come from `input_tokens_details`, `output_tokens`
includes the reasoning, `reasoning_tokens` comes from `output_tokens_details`. A whole body is the
same Response object, read by `parse_completion` through `read_response` plus its `output` items.
A whole body with `status: "failed"` in a 2xx is `GatewayError::UpstreamFailed`: an upstream
error (`502`, kind `upstream`) with OpenAI's own `error.code` and `message`, which carries the
usage and tier the Response reported, so the public unary send, its client-gone row and the
in-process unary turn write their row with that usage, or the prompt's local floor when it
reported none (decision 22), as a stream's `response.failed` is counted (*changed 2026-10-25, WP7
review I1*). The `/v1/responses` loop's own failed Response carries that code too, not
`upstream_error`, which stays for a failure without one (*added 2026-10-25, the WP7 review's
residual g*). That Response is a stream's `response.failed`; a whole run that fails answers the
error envelope with the code (*changed 2026-11-05*, api-compat design §9 finding 10).

Errors: `map_error` and `openai_refusal` read the same `error.{type, code, param, message}`
shape on both wires. The classifier already reads `text.format.type` (§8); WP7 adds the
`text.format` spellings of the schema refusal (`param: "text.format"` or
`"text.format.schema"`), with hand-written bodies until WP6 captures them.

The answers of these sends are recorded like native ones for key scoping: their response id goes
into `native_responses` with `stored = 0`, and their output item ids into `native_response_items`
under the request's key. That is what lets a client key send back the seals of §7.3 and nothing
else. An in-process send without a key (the owner's Chat, lmgw's own consumers, and so the
`/v1/responses` loop of an anonymous client, auth off, which the in-process path cannot tell from
them) records nothing (*changed 2026-10-25, WP7 review I4*): `NULL` is the anonymous principal's scope, which an
anonymous request's item check reads, and the owner's reader checks no items, so a row under
`NULL` would only have let an anonymous request (auth off) send back the owner's items.

#### 7.2.4 Steps (WP7, one commit each)

1. `wire.rs` and its units (every `CHAT_ONLY` key, each modelled field, audio parts).
2. `body.rs` behind the wire choice, not switched on yet; body units.
3. `decode.rs`, whole and streamed; units on hand-written SSE (each event above, a cut stream, a
   terminal `incomplete`, an `error` event, the tier echo on `response.created` ignored).
4. Errors: the `text.format` refusal spellings; the summary lesson.
5. Recording the sends' ids and item ids (`native_responses`, `native_response_items`).
6. Switch on: `build_chat` posts `/responses` when `wire_for` says so. This commit re-blesses the
   platform's egress and route corpus cases, and only those: the compatible and llama.cpp cases
   never change.

Proven by: the corpus; an end-to-end wiremock run each of the Chat tab (a reasoning summary
shown), an agent tool loop over two turns, `/v1/messages` (a `thinking` block, then `tool_use`),
`/v1/chat/completions` (the client gets the chat-completions shape, tool calls included) and a
Chat-wire send (`stop` set); the tier read from the terminal event only; a probe run of a tool
loop on a GPT-6-family model, (needs explicit approval because it spends money).

#### 7.2.5 Questions (WP7), decided

- **A send that needs both wires** (tools on GPT-6 Astra, which the Chat wire refuses, plus `stop`).
  *Decided (owner, 2026-10-24):* the Chat wire, and OpenAI's own refusal reaches the client. Nothing
  is dropped silently, and the refusal names the conflict.
- **Mid-conversation system messages as `developer` input messages** (7.2.2). *Decided (owner,
  2026-10-24):* as drafted; `instructions` alone would move them to the front.
- **`summary: "auto"` on every send.** Summaries are what decision 16 is for, and the lesson
  stops asking an org that refuses them. *Decided (owner, 2026-10-24):* always, with the lesson.
- **What a `/v1/chat/completions` client sees of the summaries.** *Decided (owner, 2026-10-24):*
  whatever the OpenAI ingress encoder does with `ReasoningDelta` today, unchanged. Chat Completions
  has no reasoning field of OpenAI's own, and "stay true to the OpenAI API" forbids inventing one.
- **gpt-6-astra with `seed` or a penalty, and tools** (the live probe's P2: the Chat wire refuses
  its function tools, `param: "reasoning_effort"`, `code: null`, "Function tools with
  reasoning_effort are not supported for gpt-6-astra in /v1/chat/completions. To use function
  tools, use /v1/responses or set reasoning_effort to 'none'.", though lmgw sent no
  `reasoning_effort`; `tests/fixtures/openai_platform/astra_chat_tools_refused.json`).
  *Decided (owner, 2026-10-25):* OpenAI's refusal reaches the client unchanged; lmgw does not set
  `reasoning_effort: "none"` on the client's behalf.
- **Anthropic's `thinking: {type: "enabled", budget_tokens}` on an `openai` row** (P5: no effort
  goes, `x-lmgw-reasoning-ignored` says so, §7.1's rule). *Decided (owner, 2026-10-25):* it stays
  ignored there: no mapping of a budget to an OpenAI effort, and the header stays.
- **When a request waits for a model list** (the catalog reads on `timeout_ms`, §14.2).
  *Decided (owner, 2026-10-25):* only when none is cached. A stale list answers at once and its
  refresh runs in the background; nothing cached, or a cached failure past `ERR_TTL` (60 s), is
  waited for, and requests that miss at once share one read (single-flight, `catalog::read_once`).
  A read and its cached answer are keyed by the row's id and the configuration it was read with
  (address, key, headers, protocol, kind; `catalog/read_key.rs`), so a request that queued across
  a write and reads with the old configuration caches that configuration's list only, and a read
  task that dies, a panic included, leaves its place in flight from inside the task (*added
  2026-10-25, the WP7 review's residuals c, d*).

*Draft calls made while building (WP7, 2026-10-25):*

- ~~**A refusal is text.**~~ *Changed after the WP7 review (F-R1, 2026-10-25):* the refusal is
  its own part (`ContentPart::Refusal`, `StreamDelta::RefusalDelta`, `LoopEvent::Refusal`), read
  off both wires, and each client wire writes it as its own refusal where it has one (§7.2.3): a
  structured-output client's SDK reads a refusal apart from the answer, and the model's prose in
  `content` is no JSON of its schema.
- **A client's own `store` goes as it sent it.** lmgw writes `store: false` only where the client
  set none. A client's `store: true` (Chat's stored completions) keeps the response at OpenAI;
  whether `encrypted_content` still comes then is for WP8 and the probe.
- **`CHAT_ONLY` has two keys more than the list in §7.2.1:** the legacy `functions` and
  `function_call`, which Chat Completions still documents (deprecated) and the Responses request
  does not have.
- **A chat client's `verbosity` is `text.verbosity`** on the Responses wire, translated, not
  forwarded under a name the Responses API does not take.
- ~~**Function tools go with `strict: false`** where the client said none.~~ *Changed after the
  WP7 review (residual b, 2026-10-25):* each ingress fills its own dialect's default where the
  client said none, so the client's meaning survives the wire change. A chat or Anthropic
  client's tool goes `strict: false` (neither dialect's tool is strict unless it says so); a
  Responses client's goes with no key, so OpenAI's own default holds, strict when the schema is
  compatible and not strict otherwise (the create reference, checked 2026-10-25). An in-process
  caller's tool (lmgw's MCP tools, which are not written for strict mode) goes with no key too:
  OpenAI falls back to non-strict for a schema strict mode cannot take, so nothing is refused,
  and a schema it can take gets strict arguments. On the Chat wire a Responses client's tool
  without `strict` goes as not strict: Chat Completions has no spelling for "strict when
  compatible". A client's own `strict` goes as sent (*changed 2026-10-25, WP7 review F-R4*).
  The Anthropic egress does not write a tool's `strict` yet.
- **An image goes with `detail: "auto"`**, OpenAI's default: the IR carries no detail yet, so a
  chat client's `image_url.detail` is not carried (a gap from before WP7, on both wires). The IR
  has no file part yet, so nothing goes as `input_file`.
- **An incomplete response stops for its reason before its calls** (`length`,
  `content_filter`): a call cut short is not a call to run. Only a complete one with a
  `function_call` stops as tool calls. A reason lmgw has no stop for is `length` (*changed
  2026-10-25, WP7 review F-R2*).
- **A whole Response with `status: failed`** in a 2xx is an error, "the upstream's response
  failed: <message> (<code>)", as a stream's `response.failed` is. *Changed after the WP7 review
  (I1, 2026-10-25):* an upstream error (`UpstreamFailed`, OpenAI's code on the wire) that keeps
  the Response's usage and tier for its row, not a transport error that dropped them.
- **Which wire an answer is on is read off the answer**: a Response object or a chat completion,
  a stream event with a `type` or a chat chunk. `build_chat` alone decides the wire; the reader
  needs no word from it, so the trait did not change.
- **The summary refusal is matched by OpenAI's shape**: `param: reasoning.summary`, or a message
  naming `reasoning.summary` or reasoning summaries, and, when the send's `reasoning` held the
  summary alone, one refusing `reasoning` itself (*widened 2026-10-25, WP7 review F-R5*): its
  `param`, or OpenAI's "Unsupported parameter: 'reasoning'" / "'reasoning' is not supported"
  wording. A refusal reported against any other `param` is never the summary's (*narrowed
  2026-10-25, the WP7 review's residual a*): `input`'s "Item 'rs_…' of type 'reasoning' was
  provided without its required following item.", which a replayed seal can draw (§7.3), quotes
  'reasoning' as the item's type. The lesson
  lasts the process's life, as the off's does, so each restart costs one refused send per route
  that refuses summaries. The lesson travels to the body as `Params::reasoning_summary_refused`,
  which serde never reads or writes; it takes back lmgw's own `auto` only, never a client's own
  kind. *Answered by the probe (P4, 2026-10-25):* gpt-4o-mini, a model without reasoning, takes
  `reasoning: {summary: "auto"}` on `/responses` and answers, so the summary is not gated on the
  row's reasoning capability; no summary refusal was captured, and the bodies stay hand-written.
- **The tier on a platform chat stream comes late.** On the Responses wire only the terminal event
  names the tier that served the request, so a `/v1/chat/completions` stream client sees
  `service_tier` on the finish and usage chunks only, where the Chat wire put it on every chunk
  after the first. Nothing earlier is honest: `response.created` echoes the requested tier.
- **Items are recorded from the terminal Response only**, under the request's key: the public
  key of a `/v1/chat/completions` or `/v1/messages` request, an in-process caller's `KeyRef` id
  (a device's Chat, an agent's, the `/v1/responses` loop's client); an in-process caller without
  a key records none (§7.2.3, WP7 review I4). A stream cut before its terminal event records
  nothing, as OpenAI stores nothing of it.

#### 7.2.6 As built (WP7)

- `egress/openai/responses/` holds the wire: `wire.rs` (`wire_for`, `chat_only`, `CHAT_ONLY`),
  `body.rs` (`responses_body`), `decode.rs` (`parse_response`, `ResponsesDecoder`, and
  `PlatformDecoder`, which reads either wire). `egress/openai.rs`'s `chat_body` picks the body,
  `chat_wire_body` is the Chat one, `build_chat` posts `{base}/responses` or
  `{base}/chat/completions`, and logs at debug why a send took the Chat wire.
  `openai_wire/responses_items.rs` holds the names, which the Responses ingress encoder uses too
  (byte for byte as before).
- **The summary lesson** is `proxy/reasoning_fit/summary.rs`, a second kind of lesson in
  `Learned`; `send_chat` retries once without the summary, the refused attempt's row saying so,
  before the off's own retries.
- **Errors**: `structured::openai_refusal` reads the schema refusal under `param` `text.format`
  or `text.format.schema` whichever field its message names; every other code reads as on the Chat
  wire, since the error shape is the same (WP3, WP10's quota and unreachable hand-over unchanged).
- **Recording** is `proxy/answer_items.rs`: `Completion.upstream_items` and
  `StreamDelta::UpstreamItems` carry the answer's id and item ids out of the decoder, and each of
  the five send sites (the public unary and stream, the in-process unary and stream, the Chat's
  stream) writes `native_responses` (`stored = 0`) and `native_response_items` through
  `detached`, before its answer goes on: a stream (the public one and the Chat's) as the event
  passes, ahead of the stop, whether or not a stop wins after it. The in-process sites go through
  `remember_in_process`, which records nothing for a caller without a key.
- **The fix round after the review and the probe (2026-10-25)**: a failed whole Response is
  `GatewayError::UpstreamFailed` with its usage (I1); the upstream Test's catalog read has the
  row's `timeout_ms` (I2); catalog reads are single-flight, a stale list answers at once while its
  refresh runs in the background, `/v1/models` reads its upstreams at once (I3 and the owner's
  answer); keyless in-process sends record no items (I4); the Chat records a device's items as
  they pass (I5); only PNG, JPEG, WEBP and still GIF tool images go as `input_image` (F-B1); a cap
  below 16 takes the Chat wire (F-B2); the refusal is its own IR part on both wires and every
  client wire (F-R1); an unknown incomplete reason is `length` (F-R2); a stream cut before its
  terminal event ends in an error (F-R3, `EgressStreamDecoder::on_end`); a client's
  `reasoning.summary`, `text.verbosity` and tool `strict` are carried (F-R4); the summary lesson
  reads a refusal of `reasoning` itself when the summary was all it held (F-R5); a replayed
  assistant text is an `EasyInputMessage`.
- **Corpora**: the switch re-blessed the platform's cases only, 73 egress and 6 route cases
  (commit 1fa61537 lists them); every case that sets `stop`, `seed` or a penalty, or carries
  audio or a Chat-only key, kept its Chat-wire bytes, and no generic or llama fixture changed.
- **Tests**: units beside each module; `tests/it/platform_responses_wire.rs` runs §7.2.4's
  "Proven by" end to end (the Chat tab's summary, a two-turn tool loop, `/v1/messages`' thinking
  then `tool_use`, `/v1/chat/completions`' tool calls whole and streamed, `stop` on the Chat wire,
  the recorded items, the summary lesson); the platform tests whose upstream now answers on the
  Responses wire mock it through `tests/it/support/responses_wire.rs`, and a Chat-wire stream keeps
  its own tier test. The fix round added `tests/it/platform_refusal.rs` (the refusal on every
  client wire), `budget_floors/failed.rs` and a hang-up case (a failed whole Response's row),
  `catalog/read_tests.rs` (single-flight, the stale list, an invalidated read), the probe's
  bodies in `tests/fixtures/openai_platform/` (the `stop` and astra refusals, a real luna
  stream), and units beside each change.
- **Not yet:** the seals (WP8); an assistant message's `phase` (gpt-5.3-codex and later ask that
  it be preserved and resent; it goes with the seals, WP8); a chat client's `image_url.detail`
  (the IR carries no detail); an `input_file` part (the IR has no file part). The probe run of a
  tool loop on a GPT-6-family model ran on 2026-10-25 (gpt-6-luna, all six checks passed).
- **For WP8:** the decoder's `item_done` is where a `reasoning` item's complete copy (its
  `encrypted_content`) is read; `body.rs`'s `assistant_items` drops every reasoning part today,
  which is where a same-upstream seal goes (a replayed assistant text is now `{"role":
  "assistant", "content": "<text>"}`, so a seal's `reasoning` item goes beside such messages, and
  `phase` with them); the reasoning items' `rs_…` ids are recorded under the request's key, which
  the seal's key check reads, but not for an in-process caller without a key (the owner's Chat,
  whose seal check passes as the owner's; an anonymous client of the `/v1/responses` loop, auth
  off, has none recorded, so its seals would need the owner's pass or a scope of their own); the
  IR has `ContentPart::Refusal` now, which a seal never rides on. WP7 took no migration, so 0096
  is still WP8's.

### 7.3 Encrypted reasoning carried between turns (P8, decision 17, WP8)

With `store: false`, a reasoning item's `encrypted_content` is what lets the next turn see the
model's earlier reasoning. The reference says it "is populated by default for reasoning items
returned by `POST /v1/responses`", the reasoning guide calls `include:
["reasoning.encrypted_content"]` "legacy", and the 2026-10-24 probe confirmed it comes without
`include` when `store` is false. WP7 reads each item from `response.output_item.done`. WP8 builds
on WP7: without the Responses wire, no seal ever reaches lmgw's own sends.

#### 7.3.1 The seal in the IR

`ContentPart::Reasoning {text, signature}` (`ir.rs:89-93`) holds Anthropic's signature. It gains
`sealed: Option<Seal>`, serialized only when set: `Seal { by: SealBy::Openai, upstream_id: i64,
item_id: String, data: String }`. One reasoning item is one `Reasoning` part (its summary text plus
its seal), so a turn that reasons between tool calls keeps each item where it stood. `upstream_id`
is kept (owner, 2026-10-24, §7.3.4): the content is encrypted for the org that made it (an
assumption WP6 can check), so a seal goes back only to the upstream row it came from. A new
`StreamDelta::ReasoningSeal(Seal)` carries it out of WP7's decoder. The in-process stream assemblers
attach it to the reasoning part being built, and `Completion` carries it in its parts.

*Review round (2026-10-25, W-R2):* the seal also records `config`, a fingerprint of the row's
configuration when it answered (`ir::SealScope`, `ir/seal/scope.rs`: 128 bits of SHA-256 over the
row's address, key and extra headers, never the key in clear, as `catalog/read_key.rs` keys a
catalog read by the configuration it went out with). *Verify round (V-N2):* 128 bits of
HMAC-SHA256 under the install's own seal key instead, over the same inputs: the carrier goes to
every client key, and an unkeyed hash let a client check a guessed provider key of a row offline
and see the owner rotate one. The key is 32 random bytes generated once per install
(`store::seal_key`), kept in the `settings` table under `seals:config_key` in the 0600 database
beside the install's other secrets (the upstream keys, the owner keys' plaintext), read by no API,
op, log line or export, and installed at the gateway's start before anything seals
(`ir/seal/key.rs`; the egresses have the row and nothing else, so the key is the process's, one
install per process). A row whose key or headers change still degrades its old seals to their
text, never a refusal. A seal goes back only while the row still
has that configuration (`Seal::is_for`): after the owner gives a row another key (another project
or organization), another address or another `OpenAI-Project` header, the old seals replay as on
a row that never sealed them, their text kept, and the next answer's seals are the new
configuration's. The carrier became `lmgw-openai:<upstream_id>:<config>:<item_id>:<data>`.

Every egress sends only its own provider's seals. The platform egress sends `{"type":
"reasoning", "id": item_id, "summary": [{"type": "summary_text", text}], "encrypted_content":
data}` for each part whose seal names the upstream it is sending to, and drops the rest as today.
*Review round (S-R1):* only when an answer of the same assistant turn follows the item: a message
(text or refusal) or a `function_call` before the next user, developer or tool-result item
(`egress/openai/responses/body/followed.rs`). OpenAI refuses a reasoning item without one ("…
was provided without its required following item"), and a turn can hold one: a reply stopped or
cut after its reasoning, one whose `max_output_tokens` went to reasoning alone, a `/v1/messages`
client's thinking block with nothing after it. Sent, it would fail every later turn of that
conversation on the row. Left out, it is a translation: its text stays in the stored thread, and
only the platform's view of that turn lacks the encrypted reasoning. The rule reads a turn, not
one IR message, because the Chat replays a tool turn's record and its final answer as two
assistant messages, the record's last item ahead of the answer.
The Anthropic egress never sends an OpenAI seal, and its `signature` stays Anthropic's. Gemini
drops both. `reasoning.context` decides which turns OpenAI renders (`all_turns` on the GPT-5.6
family), so lmgw sends every sealed item it holds and leaves that to OpenAI.

#### 7.3.2 Where the seal lives between turns

- **The Chat.** A tool turn's IR is stored whole (`chat_messages.ir_messages`, 0016, replayed at
  `web/chat_turn.rs:1504`), so its seals ride along with no change. A plain turn keeps its trace
  in `chat_messages.reasoning` (0010), which is text. Migration 0096 adds
  `chat_messages.reasoning_seals TEXT NULL` (a JSON array of seals), written beside `reasoning`
  (`store/chat_messages.rs`: the insert and both updates) and replayed into the turn's reasoning
  parts. A regenerated reply clears it with the reasoning. *Review round (W-N4, draft call 5
  changed):* each seal is stored with its place in the reply's text (`store::ReplySeal {at,
  seal}`, `at` the byte offset where the item was answered: the plain stream's text so far when
  the seal came, the tool loop's record text ahead of it less a continue's prefix, a
  continuation's shifted past the row's text), and the replay splits the text there, so a reply
  answered `[reasoning, text, reasoning, text]` replays in that order. The record of a tool turn
  now loses only its trailing text and refusal, so the same order holds in a record.
  *Verify round (V-F2):* a reply whose text is cut short keeps each seal where it can stand. A
  continue trims the reply's trailing whitespace and clamps every offset to the trimmed text
  (`ReplySeal::clamped`), so an item answered in that whitespace replays ahead of the
  continuation, not inside a word of it. A spoken reply cut to what was heard (chat-voice
  design §8.3, `store::cut_chat_reply` and a temporary thread's cut) keeps only the seals
  answered within the heard text (`ReplySeal::heard`); the rest go with the unheard words, as
  their text does, and the trace stays whole.
  *Review round (S-R2, decided: seals are the upstream's to read, never a page's, a device's or an
  export's):* `GET /chat/api/threads/{id}`, the chat feed's `message.*` records and both exports
  show a tool turn's record without its seals (`ir::strip_seals`, `ir/seal/unsealed.rs`), its
  reasoning text kept; the column is never in the row's JSON. *Verify round (V-F3):* the
  Traffic · Responses view (`GET /api/responses/chain`, which admin devices read too) shows a
  stored response's reasoning items with their `encrypted_content` as its size only,
  `<sealed reasoning, 1.2 KiB>` (`web/responses.rs`); in a loop answer it is the whole carrier.
  The store keeps it for the chain's next turn.
- **What it costs to keep** (WP8 review S-R4). An `encrypted_content` is kilobytes per item, and
  a seal can be kept up to three times: a plain reply's column, a tool turn's record, and the
  `/v1/responses` loop's stored chain IR. Nothing caps it (no hidden limits): it lives and goes
  with the thread, the record or the stored response, under their own retention.
- **Agents, realtime, folder chat.** They keep the IR for their run or session, so nothing is
  stored.
- **`/v1/messages`.** An Anthropic client sends thinking blocks back with their `signature`
  verbatim. The Anthropic ingress encoder writes an OpenAI seal there as
  `lmgw-openai:<upstream_id>:<item_id>:<data>` (decision 17's prefix; the upstream id is the
  one kept above; `<upstream_id>:<config>:<item_id>:<data>` since the review round, §7.3.1), and
  the parser (`ingress/anthropic.rs:326`) reads it back into `sealed`. The
  precedent is Gemini's thought signatures inside tool-call ids (`ir/call_id.rs`).
- **`/v1/chat/completions`.** Its wire has no field for it, so nothing is carried (decision 17).
- **`/v1/responses`.** On `openai` rows it is native, and the client handles its own items. On
  the synthesized loop (an `openai` row reaches it only when the loop must serve, §6), the
  encoder writes `encrypted_content` on the reasoning output item when the part has an OpenAI
  seal (`ingress/responses.rs:1296-1310` writes only the summary today), and the parser reads it
  back (`:613-620` reads only the text today). lmgw's own response store keeps the IR, so a
  stored chain keeps its seals.

**A seal from a client is checked like a native item id** (§6.1). The owner key passes. For a
client key, the seal's `item_id` must be among the key's own (`native_response_items`, which WP7
fills for these sends), otherwise the answer is `no_stored_response`, exactly as on the native
path: a leaked `rs_…` id with forged content must not pull the owner's stored items into a
client's prompt. *Review round (S-R3 = W-R3, decided):* the recorded answer must also be the
row's the seal names (`native_responses.upstream_id`), and the item must have had exactly the
seal's data: migration 0096 adds `native_response_items.encrypted_sha256` (the SHA-256 of an
item's `encrypted_content`), which every path that records items writes (the platform's own sends
and the loop's through `proxy::answer_items`, native passthrough's meter and whole answers), and
`store::first_item_not_of` compares it (`store::ItemClaim`). The native path's item check claims
the same of a reasoning item as OpenAI wrote it. So another turn's encrypted reasoning behind a
key's own id is `no_stored_response`. *Verify round:* the hash is taken from the data the seal
is (V-F4): a stream's seal comes from each reasoning item's `output_item.done`, so the decoder
records that item by the same copy, not the terminal event's, and adds a sealed item the terminal
`output` left out. A later record of an item of the same answer and key fills a hash the first
lacked (`COALESCE`), as native passthrough's item `done` and then its terminal event do, and
never replaces one; a recorded `NULL` matches no claimed hash. *Changed 2026-11-05 (api-compat
design §9 finding 9):* OpenAI encrypts an item anew each time it writes it,
so the `done` and the terminal event carry different encrypted content for one id; migration 0100
moves the hashes into `native_response_item_contents`, one row per item and hash, a later record
adds its hash, and a returned item passes with any of them. A client key's reasoning item with
`encrypted_content` but no `id` is a 400 naming the item on the native path (V-N1): nothing
recorded vouches for it, and OpenAI requires the id. A reasoning item without encrypted content
whose id the reader was never given is left out of the native body (V-F1, §7.3.4).

#### 7.3.3 Steps (WP8, one commit each)

1. `Seal`, `ContentPart::Reasoning.sealed`, `StreamDelta::ReasoningSeal`; IR serde round trip.
2. WP7's decoder emits seals; the platform egress sends same-upstream seals; the Anthropic and
   Gemini egresses never send them (body goldens for each).
3. Migration 0096 and the Chat's store and replay.
4. The `/v1/messages` prefix, written and read, with the key check.
5. The Responses ingress and encoder, with the key check.

Proven by: the units above; a test that a client key's foreign item id is refused on both
ingresses; and a probe run (needs explicit approval because it spends money), in which turn two's `input_tokens` include turn
one's reasoning item.

#### 7.3.4 Questions (WP8), decided

- **The upstream id in the seal.** *Decided (owner, 2026-10-24):* keep it. Without it a seal from
  one OpenAI project would go to another and fail there. The probe can check whether OpenAI refuses
  or ignores a foreign seal.
- **A seal sent to another model on the same upstream** (an alias switched mid-thread). *Decided
  (owner, 2026-10-24):* send it and let OpenAI decide. If the probe shows a refusal, learn it per
  route and drop such seals.
- **A client seal that fails the key check: refuse or drop.** *Decided (owner, 2026-10-24):* refuse
  with `no_stored_response`, as the native path does. Dropping would hide that the request carried
  something it may not use.
- **A column for plain turns, or `ir_messages` for them.** *Decided (owner, 2026-10-24):* the
  column. `ir_messages` means "a tool turn" to the replay and the UI (`final_answer` trims the
  record's text), and a plain turn with a seal is not one.

*Draft calls made while building (WP8, 2026-10-25):*

- **The `/v1/responses` loop carries the seal in the carrier too.** Its reasoning output item's
  `encrypted_content` is `lmgw-openai:<upstream_id>:<item_id>:<data>`, the `/v1/messages`
  signature's form, under the `rs_` id lmgw mints when the item opens. The loop announces the item
  (`response.output_item.added`) before its seal comes (the seal is in the upstream item's `done`),
  so it cannot carry OpenAI's own id, and the Responses API has no field for the upstream row. A
  client treats `encrypted_content` as opaque. On the native path lmgw sends such an item to its
  row as OpenAI wrote it (OpenAI's id and `encrypted_content`) and leaves out another row's, as the
  loop's own body does (`responses/seals.rs`); the key check reads the carrier's item id (and,
  since the review round, its row and data, §7.3.2), and a carrier made under the row's earlier
  configuration is left out like another row's (§7.3.1).
- **An anonymous `/v1/responses` loop client (auth off) records its answers' items under no key**,
  as a public `/v1/chat/completions` or `/v1/messages` request's are (`KeyRef::public`, set only
  where the loop runs a client's own request). WP7 I4 kept lmgw's own keyless callers from
  recording under `NULL`, because an anonymous request could then send back the owner's items.
  That still holds: the owner's Chat, MCP sampling, knowledge and quickdoc record nothing. Without
  this, an anonymous loop client's every seal would be refused as unknown, because its items were
  never recorded.
- **A seal is checked on both `/v1/responses` paths**, before anything goes upstream
  (`proxy::seals`, after the policy check). On `/v1/messages` and `/v1/chat/completions` the
  check comes after the key's policy, so a key that may not use the alias learns nothing of the
  items.
- **A turn that ran no tool keeps no record, even when it carried seals.** The Chat tool loop's
  record of such a turn (its reasoning parts, and since the review round the text answered between
  them, once the trailing answer is trimmed) is no record; its
  seals go to the reply's `reasoning_seals` column, as a plain reply's do. So `ir_messages` keeps
  meaning a tool turn (the column question above).
- **A plain reply's trace replays on its first sealed part.** The column keeps the trace as one
  text, so the items' own summaries cannot be split back: the first part carries the whole trace,
  every later part none. OpenAI reads an item by its `encrypted_content`, and every other
  upstream replays the parts' texts joined (empty ones adding nothing since the review's W-N2).
  ~~A plain reply whose answer came as several messages between reasoning items replays its items
  ahead of its one text.~~ *Changed in the review round (W-N4):* each seal keeps its place in the
  reply's text (§7.3.2), so such a reply (a GPT-5.3-codex commentary, say) replays in the order
  it was answered. An upstream that joins a message's texts (`Message::joined_text`) joins the
  split pieces with a line break, as it joins a tool turn's.
- **A whole answer's reasoning items are parts of its content only when one came sealed**, and so
  is a streamed turn's (`Completion::reasoning_in_place`). An answer without a seal keeps the
  shape every consumer read before WP8: its trace apart, leading the turn.
- **A client's `store: true` needs no `include`.** The loop models `store` and always sends
  `false`, so `encrypted_content` comes. Only a `/v1/chat/completions` client's `store` reaches
  the platform as sent, and that wire carries no seal (decision 17). So no
  `include: ["reasoning.encrypted_content"]` is sent.
- **A seal is read from an item's `done`, and the items are recorded from the terminal event**
  (§7.2.5). So a seal from a stream cut before its terminal event reaches its client but has no
  recorded item: a client key that sends it back is refused as unknown. The owner's pass, and the
  in-process callers keep it in their IR.
- **A seal sent to another model, or to another organization or project.** *Doc fact (checked
  2026-10-25, the reasoning guide, <https://developers.openai.com/api/docs/guides/reasoning.md>):*
  "Persisted reasoning can be reused only within the same model family … the API omits
  incompatible reasoning". So a seal sent to another model is omitted, not refused, which is what
  the "send it and let OpenAI decide" call above expected. What OpenAI does with a seal from
  another organization or project is undocumented: a probe case. Since the review round a row's
  seals stop going out once its configuration changes (W-R2, §7.3.1), so that case arises only
  for two rows in one project or a configuration changed back.

*Draft calls made in the review's fix round (WP8, 2026-10-25):*

- **A foreign carrier is refused even on a route that would drop it.** A client key's
  `/v1/messages` request whose thinking block carries a carrier it was not given is
  `no_stored_response` on every alias, an Anthropic or llama.cpp route included, where the seal
  would be dropped rather than sent. It is consistent with "refuse, don't drop" (above): the
  request carried something it may not use, wherever it goes.
- **The trace's blank line between two reasoning items stays in the stream.** The review (W-N1)
  suggested putting the decoder's `\n\n` only between two summary parts of one item and having
  the joiners add it between items. An unsealed item has no boundary a joiner could read (the
  item's `done` brings no seal), so the plain Chat's trace and a `/v1/chat/completions` client's
  `reasoning_content` would run two items together. Instead the decoder keeps the line, which
  joins the whole trace as a whole answer's `reasoning` does, and each consumer that keeps items
  apart drops it where an item starts: a `/v1/messages` thinking block, a `/v1/responses` loop
  reasoning item, the in-process assembler's part (`agent/turn_order.rs` did already).
- **Every thinking block lmgw writes has a `signature`** (W-N3): an empty one on an unsealed
  block (in place beside sealed ones, the leading trace of an unsealed answer, a stream's
  `content_block_start`), as Anthropic's `ThinkingBlock` has one, which reads back as none.
- **A resumed turn's seals stay in its record.** A gated reply resumed after its calls were
  decided keeps a record (the prior turn's), so the resumed turn's record is kept even when it ran
  no tool, its seals in place there, rather than moved to the column the record's replay would
  never read.
- **A loop transcript on the native path goes without the loop's made-up ids** (W-R1, the
  review's "forward without their `id`" pick): an assistant `message` or a `function_call` input
  item is whole, its content the client's own, so the item check does not refuse its id, and one
  whose id the caller was never given goes without it (`responses/loop_items.rs`): a
  `function_call` with its optional `id` left out, a `message` as the `EasyInputMessage` lmgw's
  own bodies send (an output `message` input item requires its `id`), its refusal parts as their
  text and its `phase` kept. Skipping only the check would send the platform ids it never made,
  which it may look up. The owner's items go the same way: an id the owner was never given is as
  unknown to the platform. The loop's `mcp_call`, `mcp_list_tools` and `mcp_approval_request` items
  are not translated yet: the platform requires their `id`, and their label names a server only
  lmgw serves; such a transcript stays in the loop as long as it sends that server's `mcp` tool.
- **A reasoning item as OpenAI wrote it is checked by its encrypted content too** (S-R3): on the
  native path a client key's raw reasoning item with `encrypted_content` must carry the content
  the key was given under that id, as a carrier must.
- **The seal's fingerprint is in the carrier.** The client gets 128 bits of a hash of the row's
  address, key and headers, which shows nothing of them; without it the native path could not
  tell a stale carrier from a current one without a lookup. (Keyed since the verify round, V-N2,
  §7.3.1.)

*Draft calls made in the verify round's fixes (WP8, 2026-10-25):*

- **A reasoning item without encrypted content whose id the reader was never given is left out,
  not refused** (V-F1): on the native path, for a client key and the owner alike. The loop writes
  such an item (a summary under an id it minted) for a step whose encrypted reasoning never came:
  a fallback answered it, or the platform sent none. The platform cannot use a summary alone under
  `store: false`, and refusing would fail every such loop transcript. Left out, it is a
  translation: the client keeps its summary, and it names nothing of anyone's. So a client key's
  summary-only item with an id it was not given, `no_stored_response` before, now goes without
  that item; one with `encrypted_content` is still checked and refused as before. The owner's
  own summary-only item from an answer lmgw never relayed (a `store: true` answer made outside
  lmgw) goes the same way, as W-R1's assistant messages do.
- **A spoken reply's seals past the cut are dropped, not clamped** (V-F2). Their items reasoned
  toward words that were never heard, and the cut gives the model only the heard text. A seal at
  the cut itself stays: it stands at the end, and the body leaves it out until something follows
  it (S-R1). A continue clamps instead, because the continuation follows the item.
- **Encrypted reasoning without an id is a 400, not `no_stored_response`** (V-N1), and only for a
  client key on the native path: the request is malformed for OpenAI too, and it names no id to
  answer not-found for. The owner's goes to the platform, which answers it. The loop reads no raw
  `encrypted_content` yet (§7.3.5), so it needs no check there.
- **The seal key lives in the database, not in the Secret Service** (V-N2): the database already
  holds the upstream keys the fingerprint stands for, so a wallet item would guard nothing the
  0600 file does not. A dev instance started on a copy of the database keeps the key, and with it
  its threads' seals. The key is the process's, installed once at start: a second gateway in one
  process (the tests') keeps the first one's. A row that does not read as 32 bytes is replaced,
  and the seals made under the old key degrade to their text.
- **A hash is filled only for the same answer and key** (V-F4): a later record of an item id
  under another response or key changes nothing, as the first record of an id stays.
- **The Traffic · Responses placeholder is `<sealed reasoning, N>`** in the view's own size
  format (V-F3); the summary stays visible.


#### 7.3.5 As built (WP8)

- **The IR**: `ir/seal.rs` (`Seal`, `SealBy`, the carrier `Seal::carrier` / `from_carrier`, a
  broken carrier refused with a 400); `ContentPart::Reasoning.sealed`, serialized only when set;
  `StreamDelta::ReasoningSeal`, `LoopEvent::ReasoningSeal`.
- **The decoder** (`egress/openai/responses/decode.rs`) reads a reasoning item's seal from its
  `output_item.done`, after its summary, and a whole Response's in place. The seal names the row
  through `Egress::parse_completion_from` and `Egress::decoder_for`, which every send site reads
  with; `new_decoder`'s seals name no row (`0`). The in-process assembler
  (`agent/turn_order.rs`) folds a sealed turn in the order it was answered, and
  `Completion::assistant_parts` is the turn the loop records.
- **The body** (`responses/body.rs`) sends each part the row sealed as its `reasoning` item (`id`,
  `summary`, `encrypted_content`) where it stood, and nothing for unsealed reasoning or another
  row's seal. Every other egress sends no seal (`tests/it/egress_adapters.rs`).
- **The Chat**: migration 0096 `chat_messages.reasoning_seals`, `store/chat_seals.rs`, written by
  the insert, the continue, the resume and the keep copy, and cleared by an edit. It is never in
  the row's JSON. The replay is `web/chat_turn/replay_seals.rs`. A tool turn's seals ride in its
  record.
- **`/v1/messages`**: the thinking block's `signature` (whole, and a `signature_delta` closing
  the block on a stream), read back by the parser; Anthropic's own signatures are untouched.
- **`/v1/responses`**: the loop's `encrypted_content`, and the parser reads it back; the native
  path is `responses/seals.rs`. The key check is `proxy::seals` (owner passes; a client key only
  its own items, else `no_stored_response`).
- **Tests**: units beside each module; `tests/it/platform_seals/{chat,messages,responses}.rs` cover
  the Chat's plain and tool turns, an edit, `/v1/messages` whole and streamed, its round trip, the
  key check (a foreign, a forged and a broken carrier), and Anthropic's own signature. On
  `/v1/responses` they cover the loop whole and streamed, its round trip, the native path's
  unwrapping, the key check on both paths and the anonymous loop client;
  `tests/it/migrations/chat_reasoning_seals.rs`. No fixture changed.
- **Not yet:** the probe run (needs explicit approval because it spends money), in which turn two's `input_tokens` would
  include turn one's reasoning item and which would show what OpenAI does with a seal sent to
  another model on the same row (§7.3.4: learn per route if it refuses) or from another project.
  Also not yet:
  - an assistant message's `phase` (gpt-5.3-codex and later), read, carried or resent;
  - a reasoning item whose `encrypted_content` OpenAI wrote itself (a native answer's) arriving
    in the loop, which is read for its summary only, as before;
  - recording a cut stream's items;
  - a carrier signature that `/v1/messages/count_tokens` forwards to an Anthropic upstream as the
    client sent it;
  - realtime and folder chat storing seals (they keep their IR for the session, §7.3.2);
  - the Anthropic egress writing a tool's `strict` (§7.2.5);
  - the loop's `mcp_call`, `mcp_list_tools` and `mcp_approval_request` items translated for the
    native path (§7.3.4);
  - a store-time rule for a reply with no answer after its seals (the body leaves such an item
    out, §7.3.1; the seal is stored all the same).
- **Review fix round** (2026-10-25, the WP8 review's findings): W-N1, W-N2, W-N3
  (b591e4c7); S-R1, W-N4 and draft call 5 (810da77e: `body/followed.rs`, `store::ReplySeal`,
  `trim_trailing_text` on the trailing answer only, the split replay); S-R2 (82ecab5c:
  `ir/seal/unsealed.rs` on the thread wire, the feed and both exports); W-R2 (bb2e1b2e:
  `ir/seal/scope.rs`, the carrier's `config`); S-R3 = W-R3 (1935ce11: 0096's
  `native_response_items.encrypted_sha256`, `store/responses/items.rs`); W-R1 (ecc5b75d:
  `responses/loop_items.rs`); the catalog doc nits (179c5c4b). Tests:
  `tests/it/platform_seals/{chat,messages,responses,surfaces}.rs` (the codex-style interleaving, a
  cut, a reasoning-only and a stopped reply, a thinking block with nothing after it, the key
  change, the forged data and the other row, the loop transcript taken native),
  `tests/it/chat_feed/messages.rs`, `tests/it/responses_native/scoping.rs`, and units beside each
  module. No fixture changed. A dev database that ran the first WP8 build has 0096 applied with
  another checksum and needs a fresh copy (0096 was never released).
- **Verify round fixes** (2026-10-25, the WP8 verify round): V-F1 (be9b43cf:
  `responses/loop_items.rs` `goes_as_given`), V-N1 (d05efb79: `responses/seals.rs`
  `first_without_id`, `responses/path.rs`), V-F2 (d0006f73: `ReplySeal::clamped` and
  `::heard`, the continue in `web/chat_turn.rs`, `store::cut_chat_reply`, the temporary
  thread's cut), V-F3 (a3d09e49: `web/responses.rs` `sealed_shown`), V-F4 (879f3932: the
  decoder's `with_sealed`, the `COALESCE` upsert in `store/responses/items.rs`), V-N2
  (d795cd41: `ir/seal/key.rs`, `store/seal_key.rs`, the HMAC in `ir/seal/scope.rs`). V-N3
  (`Seal.config` without a serde default) and V-N4 (the split reply's line break) needed
  nothing: WP8 never reached a release, so no compat shim. Tests:
  `tests/it/platform_seals/responses/{unsealed,no_id}.rs`, `platform_seals/cut.rs`,
  `platform_seals/surfaces.rs` (the Responses view), `responses_native/scoping.rs` (a
  summary-only id left out), and units beside each module. No migration change (the seal key is
  a `settings` row), no fixture changed, `lmgw-api-types` and `lmgw-client` unchanged. Not yet
  after this round: a rotation of the seal key (it stays the install's for its life).
- **For the probe** (needs explicit approval because it spends money): turn two's `input_tokens` with turn one's item; a seal
  sent to another model on the row (the guide says omitted); a seal from another project (W-R2,
  undocumented); `[reasoning, reasoning, message]` (two items, then the one that follows both),
  which the body sends; an assistant message and a `function_call` without their ids
  (W-R1's shapes); whether a stored item's id is looked up when `store` is false; and what
  the platform does with a summary-only reasoning item under `store: false` (V-F1 leaves one out
  unless the reader was given it).

## 8. Structured output and errors (P4)

The structured-output design's §9.1 has one classifier for every OpenAI-shaped egress. It reads
OpenAI's refusals (`param: response_format.type`, `Invalid schema for response_format`, …) and also
"an OpenRouter-shaped aggregator relaying that refusal", told apart by body shape. With the split:
- the **platform** egress reads OpenAI's shape only: `error.{type, code, param, message}`, with
  `code` and `param` exact;
- the **compatible** and llama egresses keep the tolerant classifier (OpenAI's texts relayed,
  llama.cpp's texts for a generic row in front of llama-server, the aggregator shape). Its
  fixtures stay as they are; the OpenAI-only fixtures run against the platform egress as well;
- more of OpenAI's codes are mapped: `context_length_exceeded` → `ContextExceeded` (the numbers
  read from the message where present, otherwise empty, as llama's backstop); `429` with
  `insufficient_quota` stays apart from a rate limit in the message and the log; flex's `429
  Resource Unavailable` gets its own message ("OpenAI had no flex capacity; not charged — retry,
  or send service_tier auto"). There is no automatic retry at another tier, and nothing retries a
  429. A configured fallback does not apply either: fallbacks are chosen at resolve and admission
  time only (`FallbackReason`: the hold, outside VRAM, a background candidate, an unavailable
  primary, a benchmark), and no upstream error triggers one, so the client gets the refusal.
  *Decision 23 (owner, 2026-10-24)* changes this for the quota: an exhausted quota makes the
  model's or alias's one fallback answer (decision 25), and a rate limit or flex's 429 still goes
  to the client (§14, WP10).
  The exact codes and bodies come from the probe (WP6) before they are matched.

**As built (WP3), and where it differs.** The probe has not run, so WP3 matched OpenAI's
documented codes (checked 2026-10-24) and leaves the bodies for WP6 to confirm:
<https://developers.openai.com/api/docs/guides/error-codes.md> and
<https://developers.openai.com/api/docs/guides/flex-processing.md>.
- **The quota family is wider than `insufficient_quota`.** The error-codes guide lists
  `credit_balance_exhausted`, `organization_spend_limit_exceeded`, `project_spend_limit_exceeded`
  and `organization_usage_limit_exceeded`. It says "the broader `error.type` can still be
  `insufficient_quota`" and "retrying billing, spend, or quota errors won't restore API access".
  All of them, and a type of `insufficient_quota`, read as quota.
- **New variants instead of `Upstream`.** The 429s become
  `GatewayError::UpstreamLimited { limit: Rate | Quota | FlexCapacity, code, error_type, message,
  hints }`. Their log kind is the family's word: `rate_limit_exceeded`, `insufficient_quota` (the
  whole quota family) or `resource_unavailable`. On the OpenAI and Responses wires they stay 429,
  as OpenAI answers, with OpenAI's own `code` and `type` as it sent them
  (`credit_balance_exhausted`, `project_spend_limit_exceeded`, `tokens`, `requests`, …), the
  family's word only where it sent none. On `/v1/messages` a quota is Anthropic's `402
  billing_error` (its SDKs retry a 429; <https://platform.claude.com/docs/en/api/errors>, checked
  2026-10-24), a rate limit and flex's 429 its `429 rate_limit_error`. The upstream's
  `Retry-After`, `retry-after-ms` and `x-ratelimit-*` headers ride on the error (`hints`, taken
  at `egress::chat_error`) and are copied onto lmgw's answer on chat (unary and streamed),
  `/v1/messages` and `/v1/responses` (native, and the loop's unary answer). A quota says
  "retrying will not help". The flex body
  is not documented ("a `429 Resource Unavailable` error code", "you will not be charged"), so
  it matches the code `resource_unavailable` or a message opening "Resource Unavailable"; the
  corpus body is a stand-in.
- **A context refusal without both numbers** is `ContextExceededAsWorded { model, message }`,
  with the same code and status and OpenAI's words. It is not `ContextExceeded` with empty
  numbers, because changing that variant's fields would have changed the compatible and llama
  corpus fixtures, which render it. The wording with numbers ("maximum context length is N
  tokens. However, your messages resulted in M tokens", or "… you requested T tokens", either
  with an optional breakdown of `N in the messages|functions|completion` parts) is
  `ContextExceeded`: `N in the completion` is the max output, the prompt is the total less it
  (the functions count towards the prompt), and a breakdown with another part, or whose parts do
  not add up to the total, is `ContextExceededAsWorded`. `context_length_exceeded` is in no
  current OpenAI page, so its body is the long-standing one until WP6.
- **llama-server's context refusal is not read on a platform row** (it reads OpenAI's shape
  only), so `errors__openai_platform` and the route corpus's `completions_context_refusal` now
  show it as the generic upstream 400.
- **The platform's structured classifier** (`structured::openai_refusal`) reads a `400
  invalid_request_error`: `param: response_format` with the "Invalid schema for response_format"
  message, or `code: invalid_json_schema` with a format `param`; `param` `response_format.type`
  or `text.format.type` with `code: invalid_value`; and the `param: null` "Invalid parameter:
  'response_format' of type '…' is not supported with this model." The aggregator and llama.cpp
  texts stay generic there.
- **A 4xx lmgw has no word for** (WP3 follow-up, 75b01a10..a5490843) reaches the client as
  OpenAI sent it: `GatewayError::UpstreamRelayed` (boxed, so the error type stays small) keeps
  OpenAI's message, type, code, param and status; Anthropic clients get the nearest type. The
  whole upstream error body is logged at debug level when that is switched on.

**The open review call** "a schema from `/v1/messages` sent to OpenAI goes with `strict: true`"
(review 2026-10-22, a draft call the review accepted). With the split, `strict: true` means
exactly OpenAI's strict mode on `openai` rows: every property in `required`,
`additionalProperties: false`. A `/v1/messages` schema with an optional property comes back as
OpenAI's `invalid_json_schema` with OpenAI's own message, as that call accepted. On
`openai_compatible` rows the provider behind the aggregator may treat `strict` differently or
ignore it. The split does not force a change: the call can stand for both protocols. It also
makes a platform-only refinement possible, an early check of OpenAI's strict subset that refuses
before the round trip and names the property (decision 18: not yet, Later).

## 9. Upstream-reported cost

lmgw reads no cost from upstream responses. Neither OpenAI nor Kilo sends one (as of 2026-10-24),
and pricing stays on price rows.

## 10. Tests

### 10.1 The corpus (WP0)

`tests/it/egress_golden.rs` and `route_golden.rs` already name their upstreams abstractly. WP0 adds
an `openai_platform` upstream to every case that applies, captured on `main` through today's
OpenAI egress, so WP1 proves that both new egresses give today's bytes. Cases: text, tools and
every `tool_choice`, reasoning off, on, each level and the OpenRouter object, `max_tokens`, every
sampler field, structured output in each type, stream on and off, embeddings with `dimensions`,
the count plan, recorded error bodies through `map_error` and `structured_refusal`, and SSE with
`usage` in the final chunk.

### 10.2 Units and ITs

- `Protocol` round trip through `as_str`, `parse` and serde for all five variants.
- Migration 0090 (`tests/it/migrations/`, `db_at_version(89)`): every row shape (`openai`
  generic and `audio_cpp`, rows on OpenAI's host and on others, `anthropic`/`gemini` with
  `supports_responses`); every CHECK refusal; ids, aliases, prices and the sequence kept (insert
  after deleting the top id); a crash mid-rebuild and a replay after commit; the FK check; each
  notice line.
- `settle` row by row on both write paths (`tests/it/upstream_settle.rs`), the notes included.
- The tier: decoder units over whole and streamed chat bodies (tier on every chunk, a tier that
  changes mid-stream, none); the compatible decoder ignores a `service_tier` it is sent; §5.4's
  mapping; `sheet_for` per tier with each fallback-free case of §5.6 and its note;
  `upsert_price` on the new key; migration 0092.
- Native passthrough: usage and tier read from a wiremock SSE stream, whole body, a stream cut
  before `response.completed`, and a client hang-up mid-stream (the row is still written).
- `forwards_responses`: each protocol, the flag, an lmgw `mcp` label, a foreign `server_url`, a
  stored and an unknown `previous_response_id`, and the refusal of both together.
- P1–P4: each re-blesses only its own cases.
- §7.2/§7.3 (WP7, WP8): body goldens for the Responses wire, decoder units on recorded SSE, the
  seal round trip through `/v1/messages`, the Chat thread store, and the Responses ingress; the
  Anthropic egress never sends an OpenAI seal (detail in §7.2.4 and §7.3.3).
- §13 (WP9) and §14 (WP10) list their own tests (§13.5, §14.13).

### 10.3 The live probe (WP6)

On a dev copy (`scripts/dev-instance.sh`), against the api.openai.com row switched to `openai` in
the copy only, through lmgw, with the cheapest model the owner names, `max_completion_tokens` 16,
and the prompt "Reply with ok.". It needs explicit approval, since it spends money on the owner's provider key, and
uses cloud aliases only (no GPU). It captures, into `tests/fixtures/openai/`:
- Chat whole and streamed at no tier, `default` and `flex`; Responses the same;
- the `service_tier` each reports, and whether one is ever absent;
- an explicit `service_tier: "auto"` (what the platform egress sends when the client set none):
  is it answered as `default` (the project's setting resolved) or as `auto` (§5.4 prices neither
  `auto` nor an absent tier)?
- error bodies: an unknown parameter, `max_tokens` on a reasoning model, a too-long prompt
  (assembled, not sent large: the cheapest way to provoke `context_length_exceeded` is a model
  with a small window, if one exists; otherwise this one comes from the reference),
  `reasoning_effort: "none"` on a model that refuses it, and a strict-schema refusal.
- *Added for WP7–WP10* (2026-10-24), each only where it costs nothing extra or rides on a
  request already made: whether a quota error ever arrives inside a 2xx stream (§14.4); a
  Responses-wire answer's `reasoning` item, with and without `summary: "auto"`, and the
  refusal an unverified org gets for the summary (§7.2.2); a seal sent back on turn two, to the
  same model and to another, and what a seal from another project gets (§7.3.4); the
  `text.format` schema refusal's `param` (§7.2.3).

The first answer with an `x-lmgw-fallback` header ends
the probe.

## 11. Work packages (build order)

Each WP ends green on `bash ci/check.sh`, is committed with explicit paths, and is green against
the corpus. "Re-blessed" means a §10.1 fixture rewritten with `LMGW_BLESS=1`.

| WP | Delivers | Proven by | Built |
|---|---|---|---|
| WP0 | §10.1's `openai_platform` cases captured on `main`; no source change | the corpus passes on `main` | 9b2a6d5b |
| WP1 | `Protocol::OpenaiCompatible` and its serde; `git mv` to `openai_compatible.rs`; the new platform `OpenaiEgress` (same composition); `for_protocol`; the synthetic upstreams; the §1.1 audit, one line per site in the commit; I1 | both egresses render every case as `main` did; nothing re-blessed except I1's log value | 57529e23 |
| WP2 | migration 0090, its guard, the Rust FK check, `settle`, `forwards_responses` (false for `openai` until WP5), UI, MCP, API docs; tests, `scripts/webrtc_interop.py`, `examples/seed_usage.rs`; Kai's live test in Kai's next landing | §10.2's migration and `settle` tests; nothing re-blessed (the 101 test files change spelling, not expectations) | 66e5c514..bbf91315 |
| WP5 | native passthrough reads usage and tier (the tee, the row at the end, hang-up safe); `forwards_responses` turns on for `openai` with its loop check and the double-need refusal; the review round's key scoping and retrieval (0091) | §10.2's passthrough and routing tests | 5531623e..80f7ba00, built before WP3 |
| WP3 | P1, P2, P3, P4, one commit each, plus the classifier split (§8) | each re-blesses only its own platform cases; the compatible cases never change | 766f5127..1887def7; the relayed 4xx 75b01a10..a5490843 |
| WP4 | the tier: decoder flag, `Completion.service_tier`, `StreamDelta::ServiceTier`, migration 0092, `sheet_for` by tier, `price_note`, the Prices page list of unpriced tiers, `price_set`, the logs, the chat and Responses ingress relay | §10.2's tier tests | 545183d3..14fd4cf3 |
| WP5b | hosted tools and the other native-only parts pass natively and are refused only in the loop; the loop-need + native-only 400; item, file, vector-store and container scoping for client keys (0093); hosted-call fees leave the row unpriced with a note, listed on the Prices page; the hosted-fee budget floor (0094); `SseDecoder` linear; API docs (§6.1) | `tests/it/responses_native/{hosted,hosted_fees,scoping}.rs`; `sse.rs`, `references.rs`, `response_object.rs`, `pricing/hosted.rs` units | 7ec62a88..a4a30c81 between unrelated commits: e60a95ee, 195cbf42, 5a8ef18b, 83e8837e, 2fb0774b, 65db34fe, a4a30c81, and the SSE fixes 7ec62a88, 92661cc5, 0221e0ad |
| WP9 | budget lower bounds (decision 22, §13): the tier floor, the flex refusal, the cut native stream's local count, the cut platform chat stream's local count (§13.6); no migration | §13.5 | dc5c4e61 (tier floor), dfa834a0 (flex refusal), 8087c4a0 (cut native stream), b4274a9c (cut chat stream), then the words and these notes |
| WP10 | one fallback per model and alias, and the quota as a reason (decisions 23, 25–29, §14): migration 0095 (`fallback` on every model and alias table, the global and `inherit` removed), the same-type save check, references rewritten on a rename, the chain with each hop checked, `FallbackReason::{Quota, Unreachable, Disabled}`, the remembered quota state and its re-check, the chat-send and native paths, the two WP9 leftovers (§14.9) | §14.13 | 35670757 (one field), 198e590f (chain), 17f81ec8 (reasons), 0f3c565b (quota memory), 4125e674 (chat send), c290894a (native), 9d9c75da (non-chat priced check), 467669f1 and 2636724c; the review round 68865d4e (writes that change what a stored fallback names), 9acd078b (runtime), 7f175a8e (UI) and the docs commit after them |
| WP7 | the Responses wire for the platform's own sends, with the per-send wire choice (decision 16, §7.2) | §7.2.4; the probe ran 2026-10-25 (gpt-6-luna, all six checks passed) | d860b0db (wire), 9239c455 (body), 67943d4b (readers), f630b59e (errors, summary lesson), 586a175a (recorded items), 1fa61537 (switch on, re-blessed platform cases), then these notes; the review and probe fix round (2026-10-25): fe32ef95 (failed Response, I1), d2400cc2 (Test timeout, I2), c2a87026 (catalog single-flight and stale-while-revalidate, I3), 0ef17339 (I4, I5), 47227d58 (I6), 0ab718d8 (tool image formats, F-B1), 1b3ff859 (assistant replay form, nits), cac18729 (cap below 16, F-B2), adb0443f (incomplete reason, F-R2), 8159c58c (cut stream, F-R3), 19c3f1cf (refusal part, F-R1), b55a7c6b (summary, verbosity, strict, F-R4), ad61f949 (summary lesson, F-R5), d515d8f1 and f0a2b681 (the probe's bodies as fixtures), then these notes |
| WP8 | the seal (decision 17, §7.3): IR, migration 0096, the Chat store, the `/v1/messages` prefix, the Responses ingress, the key check | §7.3.3; the probe run needs explicit approval | the WP7 review's residuals 3c9ccdd2 (catalog, c, d) and 980709f8 (a, b, e–h), then 19261576 (IR), 3e7d276b (decoder, body, assembler), 11c5ae0d (0096, the Chat), df00b0a5 (`/v1/messages`), b3e7172c (`/v1/responses`), then these notes; the review's fix round b591e4c7 (W-N1–3), 810da77e (S-R1, W-N4), 82ecab5c (S-R2), bb2e1b2e (W-R2), 1935ce11 (S-R3), ecc5b75d (W-R1), 179c5c4b (catalog docs), then these notes; the verify round's fixes be9b43cf (V-F1), d05efb79 (V-N1), d0006f73 (V-F2), a3d09e49 (V-F3), 879f3932 (V-F4), d795cd41 (V-N2), then these notes |
| WP6 | the live probe, after separate explicit approval (it spends money on the owner's provider key); its fixtures replace P4's and §5.2's hand-written ones where they differ | §10.3's report | needs explicit approval |

WP0–WP5b are the split and the first features, in the order they were built. Of the rest, WP9
comes first because it closes a budget hole that exists today and WP7's sends then inherit its
floor; WP10 is independent of the wire and lands before WP7 changes what `send_chat` sends; WP8
needs WP7's seals. WP6 waits for a separate go from the owner, since it spends money, and the
probe runs of WP7 and WP8 need the same go.

## 12. Resolved 2026-10-24

The owner answered all seven open questions as recommended; they are decisions 15–21 (Q1 → 15
… Q7 → 21). WP6, the live probe, still needs separate explicit approval, because it spends money on the owner's provider key. Two more owner decisions came the same day (22 and 23, §13 and §14). Their
sections, and the refreshed §7.2 and §7.3, ended with open questions of their own, each with a
recommendation. The owner accepted every recommendation the same day (§7.2.5, §7.3.4, §13.6,
§14.14), said yes to the chat-stream floor in WP9, and answered what a fallback may name
(§14.2). The owner ruled against the draft's one-hop rule: fallbacks chain, with no depth cap and a
cycle refused at save and guarded at use (§14.2). The owner then ruled the first WP10 design's
quota-only alias fallback wrong (decision 25: one fallback setting for every reason) and removed
the global fallback (decision 26); §14 is rewritten around them, and its draft calls and open
questions are listed in §14.14.

## 13. Budget lower bounds for unpriced rows a client can cause (decision 22, WP9)

### 13.1 Where this starts

An unpriced row adds nothing to a budget's spend (usage-analytics cost-policy design §2.3,
billable-units §7), because "one unknown part makes the whole unknown". That is right for the
row's cost, its totals and its rollups. For a budget it is a hole whenever a client can choose
to be unpriced. WP5b closed it for hosted-tool fees: such a row keeps its known token part as
the budgets' lower bound (`Cost::budget_floor_micro`, `pricing.rs:174-210`), and `price_call`
(`proxy/recording.rs:461`) feeds `cost.budget_micro()` to `note_spend`. The row's
`request_logs.budget_floor_micro` and `usage_hourly.budget_floor_micro` (0094) carry it into the
reseed (`store::key_spend_micro` and `total_spend_micro`, `store/usage.rs:745-770`). Nothing else
reads the column.

Decision 22 adds three more cases, and so needs no migration: each writes the same column, and
every reader already sums it. The row stays unpriced in all three: cost NULL, `price_source
unknown`, its note saying why, and its place in the unpriced remainder, the unpriced-tiers list
and the hosted list unchanged. Only budgets see the floor.

### 13.2 A tier gap above Standard floors at Standard

**Which rows.** An `openai` row (`ServedTier::Reported`) whose cost is unknown because of its
tier: `sheet.tier_gap` is set, or the answer names no price tier (`price_tier()` is `None`:
`auto`, or no `service_tier` at all). The floor applies when the tier that served it costs at
least Standard:
- `fast` (and `priority`, which maps onto it), `ultrafast` and `scale` (decision 22 lists scale
  with them; it has no public price);
- `auto` or none reported, *unless the request asked for flex* (then the answer may have been
  flex, which is cheaper, so Standard is no bound).

A served `flex` never floors at Standard. A value lmgw does not know (§5.4's "anything else")
gets no floor either: nothing says it costs at least Standard. *Owner, 2026-10-24 (§13.6): no
floor until §5.4 maps it.*

**What it is.** `price_call` prices the same usage and quantities on the scope's standard sheet
(`snap.sheet_for(.., Some(PriceTier::Standard))`, the same alias, upstream and model the tier
sheet used). When that total is known, it becomes `budget_floor_micro`. When Standard is missing
a unit too, there is no floor, as before. A request that asked for flex, answered at no tier,
with a flex price on file, floors at the flex sheet instead (flex is the cheapest tier there is).

A new `pricing::floor` module holds the rule, so `price_call` only calls it:
`fn tier_floor(served: &ServedTier, requested_flex: bool) -> Option<PriceTier>` says which sheet
(if any) bounds the row. `price_call` prices it and folds it into the cost through a new
`Cost::with_floor(micro)`. This sits beside `Cost::unpriced()`, which keeps the hosted rule: a
row with a tier gap *and* hosted calls floors at the floor sheet's token part, since the fees
stay unpriced either way.

**The note.** The tier note keeps its opening words (the unpriced-tiers list finds the no-tier
notes by them, `pricing::NO_TIER_NOTE`) and gains a clause at its end: "…; budgets count it at
the standard price, 0.004100 USD, a lower bound". `Priced::price_note` joins it with the hosted
note as now.

*As built (WP9):* `pricing::floor` holds the rule (`tier_floor`, `floor_clause`) and
`Cost::with_floor` carries it; `Cost::unpriced` keeps a floor the cost already had, so a tier gap
beside hosted calls floors at the floor sheet's price of what the call priced. `price_call` reads
the floor only for a row that is unpriced and has something to price, and only when its tier left
it so (`sheet.tier_gap`, or an answer that named no price tier). The clause reads "budgets count
it at the standard price, 0.002800 USD, a lower bound" ("at the flex price" for flex asked and
answered at no tier); it ends the note's head, before a hosted note.

*Corrected after the WP9 review:* flex is the cheapest tier, so a flex price bounds every tier
above it. `tier_floor` returns the sheets in the order they are tried, the tightest bound first
and `Flex` last: `[standard, flex]` for fast, priority, ultrafast, scale, `auto` and none, `[flex]`
for a served `default` and for `auto` or none with flex asked, nothing for a served flex or a
value §5.4 does not map. `price_call` takes the first sheet that prices the row, and
`count_floor_tiers` (13.4) ends with the same tail. So a served `default` whose scope has a flex
row only floors at the flex price, and so does a Fast answer on such a scope.

### 13.3 Flex without a flex price is refused to a budgeted caller

**When.** A send to an `openai` route whose request asks for `service_tier: "flex"` (the IR's
`requested_service_tier()`, `ir.rs:529`; the native body's `service_tier`), by a caller with a
budget, when the route's flex sheet has a tier gap: some unit is priced at another tier and not
at flex (`snap.sheet_for(priced_alias, upstream, model, Some(PriceTier::Flex)).tier_gap`). ~~A
scope with no price at any tier is not refused~~: *replaced by decision 24 (owner, 2026-10-24)*,
see "Decision 24, as built" below: a scope with no price is refused to a budgeted caller too,
whatever the tier.

"A budgeted key" is the request's key with `budget_micro > 0`. The global budget counts as well
(owner, 2026-10-24, §13.6): while a global budget is set, every request is budgeted, the owner's
own and keyless ones included.

**Where.** After the route is final and before the send, because the hold, the outside-VRAM swap
or a candidate walk can change the route, and only the route names the scope:
- chat and `/v1/messages`: in `serve_opened` (`proxy/chat.rs:202`), which also runs again when a
  ladder climb, a candidate re-pick or a fallback after a quota (WP10) re-routes the request;
- `/v1/responses`: in `handle_responses` (`responses.rs:95`), after `path::choose`, on both
  paths;
- in-process turns (`proxy/in_process.rs`), at each turn's open, for the internal identities
  that have budgets.

One function serves all three: `policy::check_priced(snap, key, alias, route, class, requested) ->
Result<(), GatewayError>` (`policy.rs`, beside `check_identity`). The budget check before the
gate (`policy.rs:469`) is unchanged.

**The answer.** `GatewayError::KeyBudgetTierUnpriced { scope, tier, model, upstream, on_file }`,
`403`, code `key_budget`, the code of the budget refusal it belongs to (a 403 is not retried by
SDKs, and a missing price does not clear during a retry window: usage-analytics §4.2). Message:
"key 'ci' has a budget, and 'gpt-5.6-sol' on upstream 'OpenAI' has no flex price on file (a
standard one is), so the spend of a flex answer could not be counted: enter a flex price on the
Prices page, or send service_tier default". On `/v1/messages` it is a `permission_error`, as
`key_budget` is. Its row is a refusal row (`record_refusal`) and names the route.

*As built (WP9), with deviations:*
- **Its own code, `budget_unpriced`** (WP9 named it `budget_tier_unpriced`; renamed with decision
  24), not `key_budget`: a client tells "a price is missing" from "the budget is spent" by the
  code, and WP9's scope called for a new code the API docs list. Still `403`,
  `permission_error` on both wires, and in `REFUSAL_KINDS`, so the rollup files it under
  "refused". The variant is `GatewayError::BudgetUnpriced` (below). The tiers on file are in
  `pricing::on_file_words`, the words the tier note uses.
- `policy::check_priced` (`policy/unpriced.rs`) holds the rule; `proxy::priced_refusal` looks up the key
  the row would be charged to (its id, else its name, else the internal identity, as `price_call`
  resolves it), so a refusal and a row name the same key.
- **Where:** `serve_opened` checks before the gate's per-send half, and the refusal is the error
  row it writes anyway, on the route, under the route's headers (a hold swap's `x-lmgw-fallback`
  included); `handle_responses` checks after `path::choose` on both paths and writes the row with
  the route; each in-process turn checks at its open (`sample_once_noting`, `stream_once_on`) as
  the turn's own refusal, so a re-routed turn checks its new route.
- **`/v1/messages`** runs the check in `serve_opened` but cannot ask for flex: the Anthropic
  ingress keeps no passthrough keys, and Anthropic's own `service_tier` has another vocabulary
  (§5.7). Its test is left out; there is nothing it could send.
- **API docs:** the chat and Responses request schemas describe `service_tier` and the refusal,
  and the OpenAI error envelope's `code` lists `budget_unpriced`.

*Decision 24, as built (owner, 2026-10-24, after the WP9 review).* The flex refusal is now one
case of a general one. The variant is `GatewayError::BudgetUnpriced { scope, target, missing,
or_default }` (`target` names model and upstream in one field, so the variant stays no larger
than the others).
- **The rule.** A caller with a budget (its key's, or the global one) on a route that is not
  local is refused when the scope does not price what its request class is billed in
  (`policy::unpriced::Need`, billable-units §2, §4). *Tightened (the owner's decision, 2026-10-24): a budget is enforceable only where pricing is known.* A model turn (chat, `/v1/messages`,
  `/v1/responses`, legacy completions, in-process turns, the dashboard Chat) needs both a token
  input and a token output price; an encoder call (embeddings, rerank) needs an input price; a
  per-request fee alone satisfies only an image call (generated images or a per-request fee; an
  image answer's token usage is not read, billable-units §4.8) and an audio call (any of tokens,
  input audio, input characters or a per-request fee). A manual 0 is a price. The first version
  of the rule let any one priced unit pass. The tiers looked at are the one asked for and those whose price
  bounds it from below (`pricing::count_floor_tiers`, 13.2): a request asking for priority passes
  on a standard price, one asking for flex only on a flex price, one asking for none on a
  standard or a flex price; a route without tiers is looked at as Standard.
- **The message** names the scope, the model and its upstream, the tier needed and the price
  that is missing, the tiers that do have the whole of it, and where to enter it: "key 'ci' has
  a budget, and 'gpt-5.6-sol' on upstream 'OpenAI' has no flex input and output token price on
  file (a standard one is), so its spend could not be counted: enter one on the Prices page or
  with lmgw__price_set (a manual price of 0 is fine for a free upstream, such as one on your
  LAN), or send service_tier default". The last clause only for flex asked where another tier is
  priced. An image call with only token rows says "(token prices are not read for image
  calls)" where the others say which tiers are on file.
- **Self-hosted upstreams count as cloud.** A LAN or otherwise self-hosted `openai_compatible`
  upstream is not a local model of lmgw's own: under a budget it is refused `budget_unpriced`
  until a price is entered on its scope, and a manual price of 0 is the way to say it is free.
- **Hang-ups on non-`openai` cloud rows** (review round 2). The unary read is detached for
  every cloud row, not only `openai`, so a client that hangs up still leaves the answer's usage.
  Where no usage comes (a cut stream, a whole answer without usage, a stream whose 2xx never came)
  the budgets' floor is an estimate at ~4 characters a token (`proxy::floor_count`;
  `LocalCount::estimated`), the row's cost staying unknown and its note saying "an estimate".
  The embedding work lmgw does itself (ingest, re-embed, probes) is checked in
  `embed_in_process` where its route is final, so an unpriced cloud encoder is refused to the
  gateway's own jobs under a global budget too. The realtime chat alias's check is at the first
  turn, as an error event, not before the upgrade.
- **Where.** The model turns are checked where their route is final, at the sites the flex check
  had: `serve_opened`, `handle_responses` on both paths, each in-process turn's open
  (`sample_once_noting`, `stream_once_on`), and the dashboard Chat's send. The media routes,
  embeddings and legacy completions have no such point, and are checked at their policy check on
  the route their alias resolves to (`proxy::alias_priced_refusal`, from `policy_or_refuse`,
  `policy_checked`, `policy_checked_call` for every class but a model turn, and
  `policy_refusal_priced` in the embeddings and legacy handlers). A hold swap or another
  fallback that sends such a request elsewhere is not checked on its new route yet; WP10 moves
  the check onto the route the gate hands back (§14.9).
- Tests: `tests/it/budget_floors/units.rs` (the rule per class), `embed.rs` (gateway-owned
  embedding work), `compat.rs` (non-`openai` hang-ups), and `tests/it/budget_floors/flex.rs` (the flex case, an unpriced model refused to a key and
  under the global budget, priced scopes passing, no budget passing, a local
  model passing, embeddings and legacy completions refused).

### 13.4 A native stream cut before its terminal event floors from a local count

**Which rows.** A native `/v1/responses` stream on an `openai` row that ended before its terminal
event: `Meter::finished()` false (`responses/native.rs:734`), whatever ended it (the client left,
the upstream ended or failed mid-stream, a gap over the timeout, a transport error). Its usage is
unknown, and OpenAI keeps nothing to fetch (§6).

**The count.** Taken when the row is written, and only for such a row, on a blocking thread
(`spawn_blocking`):
- **prompt:** the text of the body as the forward sent it (`Forward.body`): `instructions`;
  `input` as a string, or each item's text (message `input_text`/`output_text` parts,
  `function_call` name and arguments, `function_call_output` text, `reasoning` summary text);
  each function tool's name, description and `parameters` JSON; the `text.format` schema JSON.
  Joined by newlines, one pass of `tiktoken_count(upstream_model, ..)`
  (`egress/openai_compatible.rs:202`, the platform's count). Images, files, `encrypted_content`,
  hosted tool definitions and a `previous_response_id` chain count as nothing, which keeps the
  number below the real one;
- **output:** the text of the deltas lmgw relayed. The `Meter` skips delta events unparsed today
  (`responses/native.rs:649-666`). It gains `relayed: String` and reads the `delta` of
  `response.output_text.delta`, `response.refusal.delta`,
  `response.function_call_arguments.delta` and `response.reasoning_summary_text.delta`. It
  counts once at the end, never per delta, because counting fragments one by one overcounts.
  Reasoning tokens are never relayed, so the real output is higher.

**The price.** The floor sheet is `standard`, or `flex` when the request asked for flex and a
flex price is on file (a budgeted request without one was refused, 13.3). The prompt is priced at
the sheet's cache-read rate (the plain input rate when none is on file): the cut answer may have
read its prompt from cache, and a floor priced at the plain rate could then exceed the real
cost. In code this is `TokenUsage { prompt: Some(p), cached_in: Some(p), completion: Some(o),
.. }` on that sheet. *Owner, 2026-10-24 (§13.6):* the cache-read rate; decision 22 named the
local count, not the rate.

**The row.** Its token columns stay NULL: a local count is not what the upstream reported, and
the row never shows a guess as usage. The count goes into the note instead: "stream ended before
the upstream reported usage; budgets count a local count of 1834 prompt and 212 output tokens,
0.001100 USD, a lower bound". Hosted calls that passed keep their note after it, as now.

`proxy::Reported` (`proxy/recording.rs:1244`) gains `local_count: Option<LocalCount { prompt,
output }>`, which only the native forward sets, and `price_call` takes it beside the tier. A
flagged `openai_compatible` row's cut stream gets no floor yet: lmgw knows no tokenizer for
whatever model an aggregator serves.

*As built (WP9):* `responses/native/count.rs` (`prompt_text`, `DELTA_EVENTS`); the `Meter`
keeps `relayed`; `egress::openai::{local_count, count_locally}` (the second on `spawn_blocking`)
count it; `proxy::Reported.local_count` carries it to `price_call`, which uses a count only for a
row with no usage. The count is priced on `pricing::count_floor_tiers`: the tier the answer
named, when it named one, then `tier_floor`'s bound; a cut Responses stream names none, so that is
Standard, or flex when flex was asked and is priced. The note's clause names the tier: "stream
ended before the upstream reported usage; budgets count a local count of 7 prompt and 1 output
tokens at the standard price, 0.000012 USD, a lower bound" (a client that left: "the client left
before the upstream reported usage; …"). Hosted calls that passed keep their note after it.

*Corrected after the WP9 review:* the count is taken whenever an `openai` row has no reported
usage, not only when its stream ended before the terminal event. A terminal event whose `usage`
is null (a `response.failed`, an `incomplete` one) is counted too, its error `incomplete` when it
had none of its own and its note "the upstream's terminal event reported no usage; budgets count
…". So is a whole native body without usage (prompt plus the output items' text, `count::output_text`)
and a whole platform chat answer without usage (`completion_text`), both under "the upstream's
answer reported no usage; budgets count …".

*Corrected after the WP9 review:* a stop on an `openai` row is counted, not estimated. An
in-process turn its consumer stopped (`stream_once_on`) and a dashboard Chat turn the page
stopped no longer write the ~4-characters-a-token estimate as the row's tokens and real cost
(rows, rollups and run meters all read it): they write no tokens and no cost, and the budgets
count the local count of the request's text and the output relayed (`stop::stopped_cost`), the
note reading "the caller stopped it before the upstream reported usage; budgets count …". A stop
after the request went out and before the upstream answered counts the prompt alone
(`stop::unanswered_cost`). Every other row (local, compatible, Anthropic, Gemini) keeps the
estimate as its usage and cost, as before. The dashboard Chat also runs the flex check of 13.3 at
its send and counts a platform stream that ended without usage (a failure mid-stream) as an
in-process turn does; its reader keeps the output text for that on `openai` rows only
(`stopped::Read::counting`). A non-streamed in-process call (`sample_once`) without usage on an
`openai` row is counted the same way.

*After the WP9 review:* a client that hangs up before the platform answered no longer leaves
the budgets nothing. A non-streamed `/v1/chat/completions` or `/v1/messages` on an `openai` row
sends and reads its answer in a task of its own (`proxy/chat/unary.rs`), which borrows the
request's open row (`Unanswered::lend`); when nobody waits for the answer any more, the task
writes the `499` `client_disconnected` with the answer's full usage and cost, counted as answered
(`Quantities::answered()`). Every other row reads inline, as before, so a local model's slot is
still freed by the hang-up. A stream on an `openai` row whose client leaves after the request went
out and before the upstream's 2xx writes the plain `499` with its prompt counted locally as the
floor (`Unanswered::count_if_sent`), and so does a native `/v1/responses` forward whose client
left before the upstream answered (whole or streamed): the native forward still ends on the
hang-up as §6 says, so its row has the prompt's count, not the answer's usage. The note reads "the
client left before the upstream answered; the upstream reported no usage; budgets count …".

*After the WP9 review:* a local count also feeds the key's tokens-per-minute window
(`price_call`'s `note_tokens`, prompt plus output) when it stood in for usage, so a key that cuts
its streams does not slip past its `tpm_limit`.

*After the WP9 review:* the text is kept only where a count can be taken. `drive_upstream_until`
keeps `relayed` only when asked (`keep_text`, an `openai` row's stream), the native `Meter` parses
the delta events only on an `openai` row (`Meter::counting`) and passes them over unparsed
elsewhere, and the prompt's text is read only when a count is taken: the public chat handler
keeps its IR in an `Arc`, which a platform stream's relay holds and reads at its end only if the
usage never came (`egress::openai::count_chat`).

*Corrected after the WP9 review:* reasoning summary text is out of the count on both sides, as
reasoning is on the chat path: a `reasoning` input item counts as nothing, and
`response.reasoning_summary_text.delta` is not among `DELTA_EVENTS` (a summary is not the
reasoning tokens it sums up, so counting it bounds nothing). A chat stream's `relayed` keeps its
text and tool-argument deltas, no `ReasoningDelta`. Tool `parameters` and the schema are counted
as `Value::to_string()` writes them, which may come out a few tokens above OpenAI's own rendering
of a tool or a schema; the overshoot is small beside what the count leaves out (images, files,
reasoning tokens).

**The platform's chat streams** (§13.6, the owner's yes): `StreamOutcome.relayed` keeps the
text, reasoning and tool-argument deltas `drive_upstream_until` relayed. When a stream on an
`openai` row ends without usage (the public relay in `stream_chat`, an in-process turn in
`stream_once_on`), the request's text and that output are counted the same way and the row
written through `record_counted` / `record_in_process_counted`. A chat chunk names the tier, so
a cut chat stream is usually priced at the tier it was served at. *After the WP9 review:* a
stream whose client left after the content no longer loses its usage chunk: the relay reads on
past the finish chunk (§6, "Hang-ups and ends"), on `openai` and `openai_compatible` rows, so
such a row has the real usage and is priced. The local count is for a stream cut before its
finish chunk (on `openai` rows). Deviation: the prompt is read
from the IR as the platform's chat body sends it (`egress::openai::chat_prompt_text`: the system
text, each tool, the schema, the messages' text, tool calls and tool results, no reasoning
parts), not by rendering `chat_body`, which would copy every image of every platform stream for
a count that is almost never taken. *Since WP8* a turn can send sealed reasoning back (§7.3),
which OpenAI bills as input; its content cannot be counted locally, so this floor leaves it out
and stays a lower bound, below the real input by the encrypted items' tokens (WP8 review S-R4). An in-process turn its consumer stopped already carries the
~4-characters-a-token estimate of `stop.rs` as usage and is priced on it, as before; only a turn
that failed mid-stream is counted.

### 13.5 Surfaces and tests

- The words a3eb6232 put on the budget fields (`lmgw-api-types/src/usage.rs`), beside the
  queries (`web/api_usage.rs`), in the unpriced tooltip (`lmgw-ui/src/pages/usage/key_line.rs`)
  and on `price_call` say "unpriced only for a hosted-tool fee". They become the three kinds:
  hosted-tool fees, a tier above Standard with no price, and a cut native stream.
- billable-units §7's "Since 2026-10-24" paragraph names the three kinds.
- API docs: the new refusal under `key_budget` on every route that takes `service_tier`.

Tests (`tests/it/budgets/` beside the hosted-floor tests, `pricing/floor.rs` units):
- each served tier against a standard-only scope (fast, priority, ultrafast, scale, auto, none:
  floored; flex and an unknown value: not), and auto or none with flex requested (flex sheet, or
  none);
- a tier gap plus hosted calls (the token part at the floor sheet);
- flex refused for a budgeted key on chat, `/v1/messages`, native and loop `/v1/responses`, and
  after a reroute; not refused without a budget, with a flex row, or on a scope with no price;
- a cut native stream, by the client and by the upstream: the floor matches a hand-computed
  tiktoken count at the cache-read rate, the token columns stay NULL, the note says it, and the
  key's spend moves by it; a compatible row's cut stream has no floor;
- the reseed after a restart counts every kind.

*As built (WP9):* the words are the three kinds now: `KeyRow::spent_micro` and
`global_spent_micro` (`lmgw-api-types/src/usage.rs`), the comment beside the key query
(`web/api_usage.rs`), the unpriced tooltip (`key_line.rs`), `store::key_spend_micro`, the row
writer and `price_call`. billable-units §7's paragraph names the three kinds and the refusal.
Tests: `tests/it/budget_floors/{tier,flex,cut_chat}.rs`, `tests/it/responses_native/budget_floor.rs`
(the flex refusal on both paths, the cut native stream by the upstream and by its client, the
compatible row), `proxy/recording/tests.rs` (a tier gap beside hosted calls), and the units in
`pricing/floor.rs`, `responses/native/count.rs` and `egress/openai/count.rs`. The reseed is
checked per kind through `store::total_spend_micro` / `key_spend_micro`, which read the rollup a
restart reseeds from. "After a reroute" is the hold swap (a held local model whose fallback is a
platform alias with no flex price): a ladder climb and a candidate re-pick run the same
`serve_opened` again and have no test of their own. The existing tests that pinned the old notes
(`usage_service_tier`, `responses_native::{tier,hosted_fees}`) now expect the clause.

### 13.6 Questions (WP9), decided

*Owner, 2026-10-24:* each answered as recommended; the chat-stream floor is in WP9.

- **Does the global budget count as "budgeted" for the flex refusal?** Decision 22 says "a budgeted
  key". The global budget has the same hole for every request, the owner's own included. *Decided
  (owner, 2026-10-24):* yes, both. A global budget answers what the month is costing in total,
  and an unpriced flex request escapes it the same way.
- **Prompt at the cache-read rate** (13.4). *Decided (owner, 2026-10-24):* yes; otherwise a cached
  prompt makes the "lower bound" up to ten times the real cost.
- **The platform's chat streams cut before their usage chunk** have the same hole: OpenAI's chat
  wire reports usage only in the last chunk (`stream_options.include_usage`), so a cut
  `/v1/chat/completions` stream or in-process turn on an `openai` row is unpriced with no floor
  today (`proxy/chat_stream.rs`, the `canceled` row). Decision 22 names only native Responses.
  *Decided (owner, 2026-10-24):* the same floor there, in WP9 as a commit of its own (the IR
  rendered by `chat_body` and counted with tiktoken, the
  `TextDelta`/`ReasoningDelta`/`ToolCallArgsDelta` text relayed), and WP7's Responses-wire sends
  inherit it. The owner said yes, in WP9 as its own commit.
- **Flex served when nobody asked for it** (a project whose default is flex answers `auto` as
  `flex`) and no flex price: no floor and no up-front refusal, since the request did not ask.
  *Decided (owner, 2026-10-24):* leave it unfloored and listed in "Unpriced tiers" as today. The
  platform sends `auto` explicitly, and WP6 shows what a no-tier request is answered with.
- **A tier value lmgw does not know:** *Decided (owner, 2026-10-24):* no floor until §5.4 maps it.

## 14. One fallback per model and alias; an upstream quota is one more reason (decisions 23, 25, 26; WP10)

*Rewritten 2026-10-24.* The first version of this section gave a cloud alias a new, quota-only
`models.fallback` ("When this alias's upstream is out of quota, answer with…") beside the local
rows' hold fallback and the candidate aliases' fallback. *Owner, 2026-10-24:* one fallback
setting serves every reason and the multi-setting design was rejected (decision 25), and later
the same day the global fallback setting was removed (decision 26). This version designs one fallback per model and per
alias, used for every reason it cannot answer. The reason is recorded on the answer, never
configured. What the first version designed for the quota and that does not depend on where the
fallback is configured (the error-time retry, the native split, the remembered quota state, the
chain header, the rows, the budgets) stands, adjusted to the one field.

### 14.1 Where this starts

Lines at `main` 1061a550.

**Three places hold a fallback today, under two spellings:**
- the four local classes: `hold_fallback_mode` (`inherit` | `none` | `alias`, default
  `inherit`) and `hold_fallback` (the name, read only under `alias`) on `local_models`,
  `aux_models` and `audio_models` (`migrations/0024_hold_fallback.sql:13-20`) and `image_models`
  (`0033_image.sql:40-41`), carried through the `aux_models` rebuild
  (`0079_aux_decision_fields.sql:50-51`). Rust: `HoldFallbackMode` (`config/models.rs:23`) and the
  fields at `config/models.rs:95`, `:274`, `:377`, `:474`;
- candidate aliases: `fallback_mode` (the same three values, with a CHECK) and `fallback`
  (`0044_candidate_aliases.sql:27-29`; `CandidateAlias::fallback_mode`, `config/routes.rs:280`);
- `settings.hold.fallback_alias` (`config/settings_classes.rs:77`), the global that `inherit`
  reads. It has two readers: a chat row (`Snapshot::hold_fallback_for`,
  `config/snapshot.rs:812-817`) and a candidate alias, unconditionally (`Snapshot::alias_fallback`,
  `:318-322`). An aux, audio or image row on `inherit` has no fallback (`:818`): a different
  embedding model corrupts a vector index (decision of 2026-09-04, gpu-hold design §2), and the
  global is a chat alias.

A plain cloud alias (`ModelAlias`, `config/routes.rs:232`, the `models` table,
`0001_init.sql:17-26`) has no fallback, and neither has a passthrough id.

**The lookups.** Every fallback goes through `Snapshot::request_fallback`
(`config/snapshot.rs:346`): an enabled candidate alias's own (`alias_fallback`, `:314`), otherwise
the row's (`fallback_route`, `:256`, over `hold_fallback_for`, `:780`). Each ends in
`Snapshot::usable_fallback` (`:277`), which refuses a candidate alias (`:278-283`), a name that
does not resolve, and a local target ("is itself a local model", `:285-287`). Its save-time twin
is `ops::validate_fallback_alias` (`ops/settings_patch.rs:159`), called by every setter that takes
a fallback (`ops/local_model.rs:307`, `ops/aux_model.rs:474`, `ops/candidate_alias.rs:99`, the
settings patch at `ops/settings_patch.rs:336` and `web/api_settings.rs:785`). A decision row's
fallback must also answer `/v1/systemone` (`ops/aux_model/fallback.rs:20`). Five more readers
only show the fallback: `vram/on_cpu.rs:65`, `web/chat_voice/resolve.rs:479`,
`web/chat_voice/audio_input.rs:354`, `web/decision_lab/models.rs:179`,
`candidates/derive.rs:234` and `ops/candidate_alias.rs:388`.

**The reasons** (`gate::FallbackReason`, `gate/open.rs:218-247`): the hold (`Hold`, the swap at
resolve, `Snapshot::resolve_for_request`, `config/snapshot.rs:208`, stamped at
`gate/open.rs:584-590`), a benchmark's lease (`Benchmark`, the same swap), outside VRAM
(`ExternalVram`, at admission, `admit_or_fall_back`, `gate/open.rs:722`, `:802`), a background
guest that is deferred (`Background`, `gate/guest.rs:157-174`, and the candidate walk,
`gate/candidate/walk.rs:461`) and an unavailable candidate primary (`Unavailable`, `walk.rs:489`).
Each answers for a local model, one hop deep: a fallback is never local, so a fallback is never
asked for a fallback of its own.

**Names.** A fallback is stored as written, trimmed (`ops::opt`, `ops/common.rs:80`), and resolved
at use through `Snapshot::resolve` (`config/snapshot.rs:151`), the client's own lookup: an enabled
plain alias, then a local public name, then a passthrough prefix. Candidate aliases are keyed
lowercased (`store/snapshot.rs:69`) and looked up lowercased (`config/snapshot.rs:280`, `:301`).
Plain aliases are not: `Snapshot.aliases`' doc says "alias (lowercased)"
(`config/snapshot.rs:16`), but the loader inserts `a.alias.clone()` (`store/snapshot.rs:65`) and
`resolve` calls `self.aliases.get(alias)` (`config/snapshot.rs:159`), so a plain alias resolves
case-sensitively, as written. `models.alias` is `UNIQUE` under SQLite's binary collation
(`0001_init.sql:19`), while `model_set` finds the row to update with `eq_ignore_ascii_case`
(`ops/routing.rs:293`). So a fallback `GPT` for the alias `gpt` is refused at save today ("does
not resolve"), and two plain aliases may differ only in case.

**What WP10 builds on.** One error-time re-send exists: `proxy::reasoning_fit::send_chat`
(`proxy/reasoning_fit/send.rs:61`) re-sends a cloud request whose reasoning-off the provider
refused, each refused attempt its own row (`record_refused`, `:247`). One error-time re-route
exists: `gate::Sent::Rerouted` (`gate/send.rs:98`), "nothing was sent anywhere yet, and the request
goes elsewhere", which every chat site serves again on the new route (`proxy/chat.rs:373-388`,
`proxy/chat_stream.rs:92`, `proxy/in_process.rs:445`, `:880`). Only the platform egress maps a
quota: `egress/openai/errors.rs:85`, `:278`, `:287` give `GatewayError::UpstreamLimited { limit:
Quota, .. }` (`error.rs:368`). So the quota reason acts on `openai` rows; other protocols' quota
errors are not mapped yet (14.14).

### 14.2 The one setting

*Owner, 2026-10-24* (decisions 25 and 26 and the rulings that stand from the first version):
- one fallback per model and per alias, used for every reason the model or alias cannot answer;
  the reason is recorded, never configured;
- no global default: `settings.hold.fallback_alias` and the `inherit` mode are removed, and every
  model and alias has only its own fallback, or none;
- a passthrough model has no fallback; an owner who wants one creates an alias for it;
- a fallback names another model (a local model id, a passthrough id such as `kilo/openai/<m>`)
  or an alias;
- chains have no depth cap; only a cycle is refused, at save, naming the loop, with a visited-set
  guard at use;
- a configured fallback is always used, with no content-based exceptions; privacy is choosing a
  model or alias without a fallback;
- the hold means no VRAM for lmgw.

**The field.** One nullable column, `fallback TEXT NULL`, on every table whose rows a client can
name and that can have a fallback: `local_models`, `aux_models`, `audio_models`, `image_models`,
`candidate_aliases` and `models` (plain aliases). NULL is "no fallback". There is no mode column:
with `inherit` gone two states are left, and one nullable column holds both. 0024 needed two
columns for a third state ("inherit" against "explicitly none" against "not touched by this
patch", `0024_hold_fallback.sql:9-12`); with `inherit` gone, "not touched" is a patch that leaves
the field out and "none" is `clear: "fallback"`, the `clear` list every patch already has. In
Rust, `HoldFallbackMode` is deleted and each of `LocalModel`, `AuxModel`, `AudioModel`,
`ImageModel`, `CandidateAlias` and `ModelAlias` carries `fallback: Option<String>`. The name is
`fallback` everywhere, the candidate aliases' column name: the `hold_` prefix said the reason, which
is exactly what the field no longer says. *Draft call* (one column and its name).

A patch sets it with `fallback: "<name>"` and removes it with `clear: "fallback"`. An empty
`fallback: ""` is refused with a message naming `clear` (an empty string is "not supplied" in
every patch, `ops::opt`, so accepting it as "none" would be the one place it is not). *Draft call.*

**What it may name** (owner, 2026-10-24): any name a client could send as `model` that names one
fixed route, resolved as a client's `model` is (`Snapshot::resolve`): an enabled plain alias first,
then a local public name of any class (`gpt-oss`, `embed/bge`, `audio/kokoro`, `image/flux`), then
a passthrough id on an `expose_all` upstream. So a name that is both an alias and a passthrough id
means the alias, as it does for a client. The answer is priced as a request naming that model
would be (`Route::priced_alias`, `config/routes.rs:345`, is the fallback's name). A local model is
allowed as a fallback now, on every kind (rows of every class, candidate aliases and plain
aliases alike): the refusal "is itself a local model" (`config/snapshot.rs:285-287`) goes. What it
protected, a fallback that puts the GPU back to work under the hold, is protected by the per-hop
check (14.3): a local hop under the hold is never started.

Not allowed:
- **itself** (A → A), refused at save as the shortest loop;
- **a candidate alias** as a fallback target: it has no static route, and which of its candidates
  answers is decided per request. It is refused as today ("is itself a candidate alias"), not
  yet allowed (14.14, owner 2026-10-25: allowed in a follow-up);
- **a model of another type** (decision 29, owner 2026-10-25): a chat model falls back to a chat
  model, an embedder to an embedder, a text-to-speech model to a text-to-speech model, and so on
  (the save check below, item 4).

**Names and case.** A fallback resolves exactly as a client's `model` does: case-sensitively for a
plain alias and a local public name, as `resolve` does today, and case-insensitively for the
candidate-alias refusal. At save the name is stored trimmed and as written, which is the target's
own spelling, since only that spelling resolves. The doc comment of `Snapshot.aliases`
(`config/snapshot.rs:16`) is corrected to "alias, as written (case-sensitive)" in step 1: the code
was always case-sensitive, the comment was wrong. Making plain aliases case-insensitive (one
collation for `models.alias`, the lookup and `model_set`'s find) is its own small change after
WP10 (14.14, owner 2026-10-25), not part of WP10. Along a chain and in the cycle check, hops are compared by identity, not by
text: `HopKey` is `Alias(id)`, `Candidate(id)`, `Local(class, model_id)` or `Passthrough(upstream
id, model)`, so a renamed prefix or a case variant cannot hide a loop.

**Save-time checks**, one function for all six setters, `ops::fallback::validate(state, owner,
name) -> Result<Vec<String>, String>` (a new child module, `ops/fallback.rs`; the notes
are the `Ok` side), replacing `validate_fallback_alias`:
1. the name resolves (`Snapshot::resolve`), else 400 "fallback 'x' does not resolve". A name that
   resolves only to a disabled row or alias, or to an alias whose upstream is disabled, is a hop
   too (`disabled`, 14.3): it is saved with a note saying it is disabled now and that a request
   reaching it goes on to its own fallback;
2. it is not a candidate alias, else 400 "fallback 'x' is a candidate alias, which picks a model
   per request; name one of its candidates or a plain alias";
3. no cycle: lmgw walks from the owner through each hop's own `fallback` (any kind, local rows
   included) and refuses a walk that comes back to a `HopKey` already on it, 400 "fallback 'C'
   would close a loop: A → B → C → A". A walk that reaches a name that does not resolve stops
   there: the use-time check names it;
4. **the same model type** (decision 29, owner 2026-10-25: a fallback must be the same model type; between chat models a modality difference is allowed with a warning).
   A model's type is the `task` `/v1/models` publishes for it (`ModelCapabilities.task`,
   `capabilities::exposed`), the one place lmgw says what a model is: `chat`, `embedding`,
   `rerank`, `decision`, an image model (`image_generation` and `image_edit` are one type: an
   image row that edits also generates), `video_generation`, each audio task on its own (`tts`,
   `asr`, `vdes`, `align`, and every other audio.cpp task: `gen`, `clon`, `vc`, `svc`, `s2s`,
   `sep`, `vad`, `diar`, `spk`, `midi`), and `realtime`. The owner's type comes from the row it is
   set on (a chat row and a candidate alias are `chat`, an aux row its kind, an audio row its
   task, an image row an image model, a plain alias what its upstream catalog and its own
   capability override say), the fallback's from `capabilities::exposed::exposed_entry` on its
   name. Two types that differ are refused, 400 naming both: "fallback 'whisper' is a
   speech-to-text model and 'kokoro' is a text-to-speech model: a fallback must be the same type
   of model". Between two chat models the facets lmgw tracks (`candidates::Facet`: vision, audio
   input, tool calls, reasoning, structured output, read with `candidates::facets::supports`)
   may differ: the save goes through and its answer carries a warning naming what the fallback
   lacks and what it adds ("fallback 'mini' lacks vision, audio input: requests that use them
   reach it degraded"). A decision row's fallback must also answer `/v1/systemone`
   (`ops/aux_model/fallback.rs`, unchanged but for the field). *Draft call:* when
   lmgw cannot know a type at save — a fallback or plain alias whose upstream catalog does not
   list the model or cannot be read right now, so `/v1/models` publishes no capabilities for it
   — the save goes through with a warning that says the type could not be checked and that the
   per-request endpoint check (14.3, "cannot serve this endpoint") is the guard. Refusing would
   make a correct fallback unsaveable whenever a provider's catalog is down or silent, on a
   check lmgw cannot make; accepting silently would hide that nothing was checked. A catalog
   that lists the model bare (OpenAI's own `/v1/models` states no task and no modalities, its
   embedding models included) is unknown too unless the name gives a task
   (`capabilities::task::by_name`) or the alias's capability override states one: the `chat`
   `/v1/models` publishes for it is a routing default, not a fact to refuse a save on;
5. a note, not a refusal, when the owner is a cloud alias and the fallback is on the same upstream
   row: "it shares the upstream, so it may share its quota (quotas can differ per model). Another
   upstream row with the same provider key shares the quota as well, and lmgw cannot see that"
   (owner, 2026-10-24: may share, since quotas can differ per model).

The walk runs on every write that changes a name or a fallback: the six setters' `create` and
`update` (a rename included), the dashboard's alias routes (`web/api.rs:1325`, `:1372`, which write
aliases beside `ops::model_set`) and the prefix settings (`router.public_prefix`, the aux,
decision, audio and image prefixes), since a prefix change renames every local name of its class.

*As built (2026-10-25, review round):* a write is also checked for the **other** rows it touches.
A setter checks the fallback it writes, but a write can change what another row's stored fallback
points at: a create under a name a dangling fallback holds, a rename onto such a name, an alias
retargeted to a model of another type, an aux kind or an audio task changed, a class prefix or an
upstream's `expose_prefix` changed, a row or alias enabled or disabled (an enabled alias shadows a
local name of its spelling), an upstream that starts or stops exposing its catalog. So every such
write (the six setters' create, update, enable, disable and duplicate, the dashboard's alias and
upstream routes, `upstream_set`, the settings save of a prefix) runs as a **dry run** first: the
write inside a `begin_write` transaction, the names read back from it as the write would leave
them (`store::routing_after`), the transaction rolled back (`ops/fallback/referrers.rs`). Every
fallback owner whose stored name then reaches another hop than before, or reaches a hop the write
touched, is checked there: the write is refused, 400 naming each row, when it would give one a
fallback of another type, a candidate alias or a loop ("this save would break another model's
fallback: embedding model 'bge' falls back to 'x', which would be a chat model while it is an
embedding model. A fallback must be the same type of model and must not close a loop: change or
clear that fallback first"). What stood before the write — a fallback of another type that 0095
kept, a loop a hand edit wrote — is not the write's and does not refuse it. A delete is never
refused (5g): its answer names the rows it leaves without a working fallback.

*As built (2026-10-25, verify round):* these writes are **serialized** from the snapshot their
checks read to their commit and the snapshot reload after it (`AppState::routing_write`, an
in-process async lock the commit's detached task carries to its end). Two such saves racing could
otherwise each pass their dry run against the routing before the other and commit together what
neither would have allowed, and a write committed while another one's checks read catalogs would
be judged as that one's. Only these writes wait on it, for as long as the one before them takes,
with no limit of its own; reads, requests and every other write never do. The settings save takes
it before the settings lock. Every write's detached task carries the lock, a delete's as well as a
create's or an update's (*changed 2026-10-25, verify round 2*): a client that leaves mid-write
never lets it go before that write's snapshot is reloaded, so the next save's checks never read
the snapshot from before a commit already made. The task lets it go once the new snapshot is
published, before what follows the reload: the MCP reconcile, a deleted model's podman stop and
rm, an upstream's quota memory and the notes of the answer. The dry run's view of an upstream the
write changes (its row differs, or the dry run inserted it) reads that configuration's catalog
fresh and **never stores it** (`catalog::uncached_for`): the cache, keyed by the row's id, holds
the live configuration's, which the before side reads as every request does. A retarget that
changes a referrer's type is refused whether the cache is warm or cold, and a refused dry run
leaves nothing of the refused configuration behind. That fresh read runs under the lock: it is one
models-list call bounded by that upstream's own timeout (`timeout_ms`, the bound its sends have;
none at 0) as every catalog read is (`catalog::fetch_models`), and a cold cache on the before side
adds one more such read. *Owner, 2026-10-25:* the fixed 10 seconds every catalog read had is
replaced by the upstream's own `timeout_ms`, on every catalog read (the dashboard's upstream Test
included), with no constant of lmgw's own left (no hidden limits). *Owner, 2026-10-25 (the WP7 fix
round):* a request waits for a models list only when none is cached. A stale list answers at once
and its refresh runs in the background; nothing cached, or a cached failure past `ERR_TTL` (60 s),
is waited for, and requests that miss at once share one read (`catalog::read_once`, a task of its
own whose answer is cached whoever still waits, and none that an `invalidate` dropped). The dry
run's fresh read (`uncached_for`) and the `peek_only` / `cached_only` reads keep their ways, and
`/v1/models` reads its expose-all upstreams' catalogs at once. A
read that fails or times out leaves the type unknown, which is not checked (the draft call above
on a type lmgw cannot know). A settings save creates the image class's LoRA and upscaler directories only
once it went through, so one the prefix dry run refuses leaves none.

**A rename never breaks a chain** (decision 27, owner 2026-10-25). A rename of a plain alias, a
candidate alias or a local row of any class, and a change of a class prefix, rewrite every other
row's and alias's `fallback` that names what was renamed, in the same `begin_write` transaction as
the rename. "Names it" is by identity: a stored fallback is rewritten when it resolves, in the
snapshot before the write, to the `HopKey` the write renames, and it becomes that hop's new
public name. A stored name that resolves to something else (an alias that shadows a local name)
or to nothing is left as it is. The answer lists each rewritten row ("fallbacks rewritten: chat
model 'qwen3' now names 'local/gpt-oss-20b'"). Deleting a model or alias that another row names
as its fallback is not refused; its answer names the rows left without a working fallback
("confirm, then do"). *Draft call* (the delete). *As built (review round):* an
upstream's new `expose_prefix` renames its passthrough ids, and the fallbacks that name one follow
it as a class prefix's do (`kilo/openai/x` becomes `kc/openai/x`). A bare `expose_all` upstream
(no prefix) answers every name nothing else does, so a dangling fallback meant for something else
resolves to it as well: its new prefix rewrites only a name its catalog lists, and with its
catalog unreadable it leaves such a name and the answer says so (verify round). An upstream
delete, which takes
its plain aliases with it, names the rows whose fallback was one of them or one of its passthrough
ids. A rewrite only touches a row that still holds the name it was computed from (`AND fallback =
?old`), so a fallback another write changed while the rename's checks read catalogs stays as that
write left it. A duplicate copies the row's fallback only when a save would take it, and says so
when it leaves it off.

**Re-validated at use**, never trusted from save time, as today (`config/snapshot.rs:203-207`):
rows and aliases change in between.

### 14.3 Each hop is checked: the chain

A request that reaches a fallback is served on it as if the client had named it, and the hop is
checked like any request: can it answer now? If it cannot, its own fallback applies, and the
reason is recorded for that hop. This is one walk for every reason, and it replaces today's one-hop
lookups.

**When a hop cannot answer now**, by reason:
- **`hold` / `benchmark`:** a local hop on the GPU while the hold is on or a benchmark holds the
  card (`Snapshot::gpu_block_for`). It is never started: the hold means no VRAM for lmgw (owner,
  2026-10-24). A row on the CPU (`backend: cpu`) is not held and answers, as today. Decided at
  resolve, from the snapshot.
- **`external_vram`:** a local hop that is not loaded and does not fit because VRAM outside lmgw's
  control is short (§4.7's verdict, `vram::external_armed`). Decided at admission, so a local hop
  that fits starts, and one that is already loaded answers.
- **`background`:** a request whose origin is `Origin::Background` (`x-lmgw-background: on`) is a
  guest at every hop: a local hop is admitted only if that disturbs nobody (`gate/guest.rs`), else
  it is deferred and its fallback applies. A cloud hop takes a guest as any request.
- **`unavailable`:** a non-background candidate alias whose primary cannot be used and that has no
  loaded alternate (`walk.rs:489`). Only the first hop can be a candidate alias (14.2), so this
  reason is only ever the first hop's.
- **`quota`:** a cloud hop that answered `insufficient_quota` (at the send, 14.4), or whose upstream
  is remembered out of quota (14.6): such a hop is skipped before anything is sent, with no row.
  A local hop reached on a quota is admitted normally, with every local rule above: under the hold
  it cannot answer, and its own fallback applies (reason `hold`).
- **`unreachable`** (decision 28): a cloud hop whose upstream could not be connected to at the send
  (the connect failed, so nothing reached the upstream and nothing reached the client). Caught where
  the quota is caught (14.4), on the same request classes, and not remembered: the next request
  tries the upstream again. A timeout, a reset after the request went out, and a stream cut after
  its first byte are not `unreachable`: the request may have run, and nothing is ever retried once a
  byte went to the client.
- **`disabled`** (decision 28): a hop that names a disabled row or alias, or an alias whose upstream
  row is disabled. Decided at resolve, from the snapshot, for every request class. The rule only
  changes what was an error: a name that resolves today (an enabled alias first, then a local
  public name, then a passthrough id) resolves as it does, and only a name that would otherwise not
  resolve, or resolves to an alias whose upstream is disabled, is looked up among the disabled rows
  and aliases (a disabled plain alias, a disabled candidate alias as the first hop, a disabled local
  row of any class under its public name; a chat row must still be `public` to be named at all). With
  a fallback, it answers (reason `disabled`); without one, the request gets today's answer (unknown
  model, or the alias's upstream error), and a fallback hop without one ends the walk ("(fallback 'B'
  is disabled)"). A disabled passthrough upstream has no fallback to give: a passthrough model has
  none.

This reverses one answer the first version recorded: "a local model as the quota fallback is
admitted with no fallback of its own (`gpu_hold` under the hold)". With one fallback per model, the
local model's own fallback is the one that applies whatever the reason, so under the hold the
request goes on to it, and only a local hop without one answers `gpu_hold` (now naming the chain).
This follows from decision 25; the owner confirmed the reading on 2026-10-25: "Your
interpretation is correct, the local models own fallback applies."

**A hop ends the walk**, with the error the last hop that could not answer would give and a clause
naming the chain (14.8):
- its fallback is none;
- its fallback does not resolve ("(fallback 'x' does not resolve)");
- its fallback is a candidate alias written past the save check ("(fallback 'x' is a candidate
  alias)");
- its fallback is already in the request's visited set ("(fallback 'A' was already tried in this
  request: A → B → C → A)"). The guard is for a loop the save check could not see (a hand edit,
  two writes racing, a rename); the request answers an error and never loops;
- its fallback is one this endpoint cannot use (the site's `RouteCheck` on the fallback's route,
  `gate::fallback_serves`, `gate/open.rs:993`): "(fallback 'x' cannot serve this endpoint)", as
  `gate::held` words it today (`gate/open.rs:887-902`). The walk does not skip past such a hop to
  its fallback: a chat alias as an embedder's fallback is a configuration to fix, not a state that
  clears. *Draft call.*

At admission (`external_vram`) a walk that ends is not an error: today a model whose fallback is
none or unusable waits for room (`gate/open.rs:946-957`), and a walk that ends at a local hop
waits on that hop, as a request naming it would. *Draft call.*

*As built (2026-10-25, verify round):* a request that leaves a local model it holds a claim on — a
climb that could not happen, a candidate alias picking again, the hold coming on mid-request —
**keeps** that claim, and its
fallback's admission counts it as freeable (`vram::leaving`, `runtime::registry::Freeable`).
Admitting the fallback beside the request's own claim would otherwise queue the request behind
itself whenever the fallback fits only once the model it leaves goes, until the queue timeout (or
for as long as a stream with no queue timeout lasts). Eviction may take the model being left for
it only when that claim is the model's one user: one claim in flight (this one), no ladder send
counted on its container, and no turn on the hold that carries the claim. Each send is a turn on
its hold from the start of its fit until its response ends, or until it leaves the hold for
another model (`LocalHold::turn`, *changed 2026-10-25, verify round 2*: it was counted from the
send, so a sibling still counting its prompt or waiting for pool room was not seen), and the
count goes up under the registry's lock, the one an eviction's stop decides in, so a turn that
starts while its model is judged is either seen or finds the model already stopping. A turn
still counted when a request leaves is therefore another call's: a claim several model calls
share (an agent run's classify stage) or a second request on the model keeps it, as every claim
in flight does; a guest's admission still takes only a guest's model. On the in-process paths
(`proxy/in_process.rs`, the synthesized `/v1/responses` loop's among them) the loop keeps its claim for its next
turn while the rerouted turn's whole call to the fallback runs, so that call runs inside the
claim it left as well, not only its admission (*changed 2026-10-25, verify round 2*): a ladder
fallback that must climb into the room only the left model frees, or a fallback that hands on
again, may evict it under the same rule. The claims a request left are a stack: A → B → C admits
C inside both A's and B's, and either frees its own model. A loop's next turn finds its model
where it was: still loaded when nothing evicted it, and re-admitted by its own rule when its
fallback did. The review round's first answer, giving the claim back before the fallback's
admission, read a claim that other sends shared as gone and sent a candidate alias's next turn to
`candidate_lost` with its model still loaded.

**The visited set** holds `HopKey`s, the requested name's first. The `Origin` and the site's
`RouteCheck` travel to every hop. Policy (scope and budget) is not checked again per hop (14.9);
the priced check runs on the route that answers (14.9).

**The code.**
- `Snapshot::fallback_of(key: &HopKey) -> Option<&str>`, the one lookup of a model's or alias's
  configured fallback, by identity: a candidate alias's, a plain alias's, a local row's (any
  class). It replaces `hold_fallback_for`, `alias_fallback`, `fallback_route` and
  `request_fallback`. A candidate alias's candidates' own row fallbacks are still never consulted
  through the alias (candidate-aliases design §4.1): the alias is the hop, not its candidate.
- `Snapshot::hop(name) -> Hop { Resolved { key, route } | Unresolved(why) }`, with
  `usable_fallback`'s candidate refusal and without its local refusal; the route comes back
  marked as the fallback it is (`Route::fallback`), as today.
- `gate::chain::Chain { visited: Vec<HopKey>, hops: Vec<(String, FallbackReason)> }`, carried by
  `Routed` and `Opened` beside `GateHeaders`, and the source of `x-lmgw-fallback-chain` (14.5).
- **Resolve** (`gate::resolve`, `gate/open.rs:555`): `Snapshot::resolve_for_request` becomes a loop
  over hops while the hop is local and blocked (hold, benchmark), or cloud and remembered out of
  quota with a fallback. `held_container_fallback` (`vram/on_cpu.rs:58`) takes the same loop.
- **Admission** (`admit_or_fall_back`, `gate/open.rs:722`): an `ExternalVram` verdict hands the
  request to the fallback hop; a cloud hop answers, a local hop goes through admission again, with
  the visited set. The guest path (`gate/guest.rs:147`, `defer`) does the same for `Background`,
  keeping `Origin::Background`. The candidate walk's `fallback_answer` (`walk.rs:544`) opens its
  fallback as a hop. The ladder's climb hook (`vram/climb.rs:673`, `:713`) reads the hop through
  `gate::candidate::request_fallback`, which reads `fallback_of`.
- **The send** (14.4): `gate::open_hop(state, name, check, origin, chain)` opens a named hop with
  every rule above, for a fallback chosen after the gate (a quota at the send). It is
  `gate::resolve` + admission started from a hop with a carried chain, and `gate::open` becomes
  `open_hop` with an empty chain. It replaces the first version's `gate::open_no_fallback`.
- **The readers that only show** a fallback (14.1's six) read the first hop through
  `fallback_of`, and where they show a verdict (the chat voice's "which fallback would hear or
  speak", the decision lab's head line, `kept_on_cpu`) they walk the static part of the chain:
  hold, benchmark and remembered quota, which a snapshot and `state.upstream_quota` decide without
  admitting anything.

A one-hop chain onto a cloud model answers exactly as today on every path. What is new is a local
target, a chain, a fallback on a plain alias, and the quota.

### 14.4 Where the quota is caught

**Chat sends** (`/v1/chat/completions`, `/v1/messages`, the synthesized Responses loop's turns
and every in-process caller) all send through `send_chat`. After `send_gated_marked` hands back
`Sent::Upstream(resp)` on a cloud route (`hold.is_none()`), and before the reasoning-off retry,
a new `quota.rs` beside `send.rs`:
1. A `429` is read whole (`resp.bytes()`) and classified as the send site would classify it
   (`egress::chat_error`, as the off retry does at `send.rs:141`).
2. Anything but `UpstreamLimited { limit: Quota }` is rebuilt (`gate::rebuild`) and handed on
   unchanged: a rate limit and flex's 429 reach the client as today (decision 23).
3. A quota marks the upstream exhausted (14.6) and reads the route's hop's fallback
   (`fallback_of`): the hop is `route.priced_alias(requested)`'s, the requested name or the
   fallback already answering.
4. With a fallback: the attempt's row is written (14.7) and the fallback is opened through
   `gate::open_hop` with the request's `RouteCheck`, `Origin` and chain; the answer is
   `Sent::Rerouted(Ok(opened))`, its headers carrying the chain with the hop's reason `quota`.
   `send_chat` gains the `RouteCheck`, `Origin` and `Chain` its callers already hold from
   `gate::resolve`. A hop that ends the walk (14.3) answers as 14.8 says.
5. With none: the quota error stands (14.8).

**Unreachable at the send** (decision 28) follows the same steps. A cloud send whose connection
could not be made ends in a new `GatewayError::Unreachable` (`vram::send_built` on a route with no
hold, reqwest's `is_connect`; a local container's connect failure stays `Transport`, which the
dead-container recovery reads). It is a `502` as `Transport` is, with the kind `unreachable`. With
a fallback on the hop, the attempt's row is written (14.7) and the fallback is opened as in step
4, reason `unreachable`; with none, the error stands as today, with the chain clause (14.8).

**The memory at the send.** Before `send_gated_marked` sends, `send_chat` also asks
`state.upstream_quota` (14.6) about the route's upstream: while it is set and the hop has a
fallback, the request is rerouted as in step 4 with no send and no refused row. The resolve-time
skip (14.3) covers a request that resolves after the state was set; this covers a route resolved
before it, above all the in-process runners, which keep their route for a whole run.

The sites serve `Rerouted` as they serve a candidate re-pick: chat runs `serve_opened` again, so
the new route gets its own defaults, reasoning fit, structured-output check and decision 24's
priced check. The in-process runners continue on the fallback's route for the rest of the run.

*As built (2026-10-25, review round):*
- **The unary hand-over is the handler's.** The unary chat read on a cloud row runs detached so
  its row and budget survive a hang-up (§13.4). The hand-over it decides is not made there: the
  detached send ends in the hop's own error and leaves the hand-over behind
  (`reasoning_fit::Handover::deferred`), and the handler, which a hang-up cancels, opens the
  fallback and writes the attempt's row (`open_deferred`), as `Native::HandOn` does for the native
  forward. A client that left before the quota answered starts, admits or evicts nothing for it;
  the attempt's row is the quota (or the `502`) with "(the client left before it was handed to
  'a''s fallback)", and the quota is remembered all the same.
- **A run's turns hand on as the run.** A synthesized `/v1/responses` run's turns carry the run's
  request (`PerRoute::handover`): the `/v1/responses` route check, the chain the run was routed
  along (a turn the gate rerouted carries that turn's), and its origin — a background run's turns
  hand on as a guest, so a local fallback is admitted only where it disturbs nobody. The owner's
  origin with `RouteCheck::None` and no chain is for lmgw's own callers (the dashboard's Chat, its
  tools, sampling).
- **A failed hand-over's row** (a later hop gave the error) is on the hop that gave it, with its
  reason, not on the cloud route the request left.
- **A realtime response's chat call hands on as the session's request** (verify round): the
  `/v1/realtime` route check, as the owner's, along the chain its route was opened with, and a
  call the gate re-routes carries that route's chain on (`realtime/responder/hand.rs`).
- **A client that leaves during the hand-over keeps the attempt's row** (verify round). The walk
  admits, evicts for and cold-starts a local fallback in the handler, and a hang-up cancels the
  handler there, before the row is written. The row is held while the walk decides
  (`reasoning_fit::quota::left::AttemptGuard`, `responses::native::HeldRow`): dropped then, it is
  written in a task of its own, "…; handed to fallback 'b' (quota); the client left before it
  answered", on the unary chat, the stream and the native forward. A walk that ends still writes
  only the hop's error.

**Native `/v1/responses`** does not go through `send_chat`. `Forward::run`
(`responses/native.rs:216`) reads a non-2xx answer whole before it relays anything. It gains the
same steps 1–3, and `native_passthrough` (`responses/native.rs:70`) returns
`Native::Answered(Response)` or `Native::Quota { error, fallback }` instead of a bare `Response`.
`handle_responses` (`responses.rs:95`) is split at its policy check into the routing half and
`serve_routed(routed)`. On `Native::Quota` with a fallback it opens the hop
(`gate::open_hop`) and calls `serve_routed` once more on its `Routed` (reason `quota`).
`path::choose` then decides native or loop for the fallback's upstream as if the client had named
it. When the fallback cannot take the request (a native-only part on a fallback that takes the
loop, a `previous_response_id` only OpenAI holds), the quota error stands, with a clause saying why
the fallback did not answer, as `gate::held` words it ("(fallback 'x' cannot serve this
endpoint)"). Decision 24's priced check (`proxy::priced_refusal`, §13.3), which `handle_responses`
runs after `path::choose` today, moves into `serve_routed` with the split, so a native quota
fallback onto a platform row is checked on its own route and cannot skip it. The remembered state
is asked before the forward too, as at the chat send. A connect error of the forward is handed on
the same way (`Native::Quota` carries the reason, `quota` or `unreachable`). *As built (review
round):* `Native::HandOn` carries the sent attempt's row, and the handler writes it once the walk
handed the request on (to the first hop it reached, or a later hop's own error) — a walk that ends
leaves the one row, the hop's error with its clause. A native quota with no fallback names the
chain it was tried along ("'b' has no fallback configured (tried: a → b)"), as the chat send's does.

**Streaming: nothing has reached the client.** The chat stream checks the upstream status before
it spawns the relay or writes the first frame (`proxy/chat_stream.rs:80-97`), and only a 2xx
becomes `Turn::Answered`, whose body carries the client's headers. The native forward checks it
before `relay`. So an HTTP-level quota error always comes before any byte, on unary and streamed
requests alike, and a connect error comes before any byte by its nature. A quota that arrived
inside a 2xx stream (an `error` or `response.failed` event
after bytes were relayed) is not fallen back: what was sent cannot be taken back, and the row keeps
the event's error. Whether OpenAI ever does that is for WP6.

**The other request classes** (embeddings, audio, images, legacy `/v1/completions`) on an `openai`
row keep the quota error in WP10, and an unreachable upstream keeps its `502`; their fallback for
the GPU reasons and for `disabled` (both at resolve) is unchanged (14.3). Their quota error says
"the fallback answers an upstream quota for chat and Responses requests only yet" (owner,
2026-10-24: WP10b once chat has run for a while).

### 14.5 The reason and its headers

`FallbackReason::Quota`, `as_str` `"quota"`, `FallbackReason::Unreachable` (`"unreachable"`) and
`FallbackReason::Disabled` (`"disabled"`, decision 28), and the same texts in
`request_logs.fallback_reason` (TEXT with no CHECK, 0042, so no migration). The enum's doc changes from "Why a fallback is
answering instead of the requested local model" (`gate/open.rs:215`) to "…instead of the requested
model or alias". `quota`, `unreachable` and `disabled` join `x-lmgw-fallback-reason`'s `HeaderSchema::Enum` in
`LMGW_HEADERS`
(`openapi/headers.rs`, beside `hold`, `external_vram`, `background`, `unavailable`, `benchmark`)
with a sentence each in its description, and `x-lmgw-fallback`'s description ("The alias that answered
instead of the requested local model") becomes "The model or alias that answered instead of the
one requested". Every match on the reason gets an arm: `openapi/headers.rs:339`,
`lmgw-ui/src/pages/traffic.rs:1037` ("fallback · upstream quota", "fallback · upstream
unreachable", "fallback · disabled"), `proxy/transcribe/clip.rs:64-67`,
`web/chat_voice/audio_input.rs:333`, `voice_lab/facts.rs:199` (not a hold) and
`mcp/media/image.rs:140`.

**Along a chain** (A requested, A and B out of quota, C answers):
- `x-lmgw-fallback: C`, what actually answered, its meaning unchanged;
- `x-lmgw-fallback-reason: quota`, the reason of the last hop, the one the answer is there for;
- `x-lmgw-fallback-chain` on every answer a fallback served: each name the request was routed to
  after the one asked for, in order, with each hop's reason, as an RFC 8941 structured-field list:
  `"B";reason=quota, "C";reason=quota`. Reasons mix along a chain: a held local model whose
  fallback A is a cloud alias out of quota reads `"A";reason=hold, "B";reason=quota`, and a held
  local model whose fallback is another held local model reads `"m2";reason=hold,
  "cloud";reason=hold`. With one hop it repeats what the two other headers say, so a client reads
  one header whatever the chain's length. A name is an sf-string, `"` and `\` escaped with a
  backslash; a name with a byte outside 0x20–0x7E cannot be one, and its entry is the reason alone
  (`"";reason=quota`), as `x-lmgw-rung` leaves out a `gguf=` it cannot write, so the list keeps its
  length. It gets its own `HeaderSchema::Text` row in `LMGW_HEADERS` with
  `Scope::Routes(GATED_ROUTES)`, the scope `x-lmgw-fallback` has, and its rows in the API docs;
- a streamed `/v1/responses` loop cannot carry a later turn's chain: its headers left with the
  first event, so the header names the hops up to the first turn, and a later turn's fallback is
  on its row (14.7).

### 14.6 The remembered quota state

A quota does not clear in seconds ("retrying billing, spend, or quota errors won't restore API
access", error-codes guide), so lmgw remembers it per upstream row, the scope of a provider key's
credit:
- `state.upstream_quota: QuotaMemory` (in memory, like `state.reasoning_learned`): upstream id →
  `{ since, code, message, next_check }`. Set by every quota answer. Cleared by any 2xx from that
  upstream, by an edit of the upstream row (the key may have changed), and by the owner (below). A
  restart forgets it; the first request after one learns it again, and an unpaid request costs
  nothing.
- **While it is set**, a hop on that upstream that has a fallback is skipped to it, reason
  `quota`, before anything is sent (at resolve, 14.3, and at the send, 14.4). The skipped hop has
  no row; the chain header names it.
- **The memory expires.** Once `next_check` has passed, it no longer skips a hop on its own, so a
  state nobody clears holds traffic on the fallback for one interval at most before the upstream is
  asked again.
- **The re-check.** Once `next_check` has passed, the next such request goes to the upstream
  again, and only that one: the others keep going to the fallback until it answers. A 2xx clears
  the state. A quota sets `next_check` again, and that request falls back as in 14.4. The interval
  is the upstream's `Retry-After` when the answer carried one, otherwise
  `settings.quota_recheck_seconds`, default 300, a visible field under Settings → Usage & cost
  (owner, 2026-10-24). *As built (review round):* neither has a bound of lmgw's own: a
  `Retry-After` or `retry-after-ms` past what a `Duration` holds is the longest one, an interval
  past what the clock can add is the farthest instant it can represent, and the unix `next_check`
  saturates, so a `Retry-After` of 10^20 s or a setting of `u64::MAX` never panics. "Any 2xx" is
  any class's: embeddings, rerank, System One, audio, images and legacy completions clear it as the
  chat send and the native forward do.
- **A model or alias without a fallback** still sends to the upstream while the state is set, so
  its client gets OpenAI's own answer and the state learns when the credit is back.

**Surfaced:**
- the Upstreams row: a badge "Out of quota since 14:02 (credit_balance_exhausted). Models and
  aliases on it that have a fallback are answered by their fallback until it answers again; the
  next request goes to it again at 14:07" (a time on another day carries its date), with a "Check
  now" action that clears the state;
- `UpstreamView.quota` (`lmgw-api-types/src/upstreams.rs`), `lmgw__upstreams` and `lmgw__status`;
- a new op `upstream_quota_clear` (MCP tool, op table, OpenAPI and API-docs rows), which "Check
  now" calls;
- one WARN when the state is set ("upstream 'OpenAI' answered insufficient_quota
  (credit_balance_exhausted): models with a fallback answer with it until a check succeeds"), one
  INFO when it clears, nothing in between.

### 14.7 Request rows

As the off retry does it (`send.rs:52-55`, "each refused attempt is its own request row"):
- **a hop that was sent and answered a quota:** `record_refused` with the request's `RowAs`, on
  the route that refused. Its status is the one lmgw would have answered (`429`, or `402` on
  `/v1/messages`), its error kind `insufficient_quota`, and its message is OpenAI's words plus
  "; handed to fallback 'B' (quota)". Its `fallback_reason` is whatever brought the request to that
  route (NULL for the name the client asked for), its `priced_alias` that hop's. No usage, no cost:
  OpenAI does not charge a refused request;
- **a hop whose upstream was unreachable** (decision 28): its own row as a quota's, status `502`,
  error kind `unreachable`, its message the connect error plus "; handed to fallback 'B'
  (unreachable)". Nothing reached the upstream, so no usage and no cost;
- **a hop that was skipped** (a local hop under the hold, a remembered quota, a deferred guest, a
  disabled row) was not sent and has no row; the chain header names it;
- **the answer:** one row, as any fallback row today: `requested_alias` as asked, the answering
  hop's upstream and model, `fallback_reason` the last hop's reason, priced under the answering
  hop (`priced_alias`).

The rows are not linked by a column yet. They share the requested name, the key and a start a few
milliseconds apart, as the off retry's rows do.

*As built (verify round):* a sent hop's row is written once the walk handed the request on, and is
written all the same when its client leaves while the walk admits or starts a local fallback,
ending "; the client left before it answered" (14.4). The answer has no row then: nothing reached
the client.

### 14.8 With no fallback, or a chain that ends

The client gets what it gets today for the last hop that could not answer, with one clause naming
the chain.
- **A quota:** `429` with OpenAI's code and type on the OpenAI and Responses wires, `402
  billing_error` on `/v1/messages` (`error.rs:836`, `:863`), the relayed rate headers, and
  "retrying will not help", plus "'gpt-5.6-sol' has no fallback configured" or, along a chain,
  "'C' has no fallback configured (tried: A → B → C)". It names where a fallback would be set and
  says nothing about privacy: a model without a fallback is how privacy is chosen. The remembered
  state is still set and shown (14.6).
- **The hold, a benchmark, a deferred guest:** `503 gpu_hold`, `gpu_benchmark` or `gpu_deferred`
  as today, its existing clause (`" (fallback 'x' …)"`, `config/snapshot.rs:231-238`,
  `gate/guest.rs:155-181`) extended with the chain ("(tried: m1 → m2)").
- **An unreachable upstream, a disabled hop** (decision 28): today's answer for it (the `502`
  transport error, the unknown model, the alias's upstream error) with the same chain clause.

The clause on `UpstreamLimited` is lmgw's words, not the provider's, so it goes into a new field
rather than into `message`: `note: Option<Box<str>>`, rendered after the message on every wire.
`Box<str>` (16 bytes) rather than `String` (24) because `GatewayError` rides in the gate's
refusal shapes, which `clippy.toml` sizes against `large-error-threshold = 224`; the build step
checks clippy on the whole workspace and boxes more of the variant if it trips.

### 14.9 Budgets and policy

Scope and budget are checked once, on the requested name, before the gate. They are not checked
again for a fallback, as under the hold (`gate/open.rs:710-713`: "Policy is untouched by the
swap"). A refused attempt costs nothing and adds nothing to a budget. The answer is priced under
the hop that answered and counts toward the key's and the global budget like any other row.
Decision 24's priced check runs on the route that answers: chat and `/v1/messages` in
`serve_opened`, which runs again on every reroute; native `/v1/responses` in `serve_routed`
(14.4); in-process turns at each turn's open.

**The non-chat classes after a reroute** (the second WP9 leftover, §13.4's "Where"): embeddings,
media and legacy completions are checked at their policy check on the route their alias resolves
to (`proxy::alias_priced_refusal`), which is the route before a hold swap or another fallback. In
WP10 that check moves to the route the gate hands back (after `gate::open` returns `Opened`),
so a held embedder whose fallback is an unpriced cloud encoder is refused `budget_unpriced` under
a budget, as a request naming that encoder would be. *As built (review round):* the check runs on
the route `gate::resolve` hands back — the gate's static walk, which starts nothing: the hold, a
benchmark, a disabled row, and a row switched to the CPU whose container still runs on the GPU
under the hold. A fallback reached at admission (VRAM outside lmgw, a deferred guest) is not
checked there yet.

**The native whole answer after a hang-up** (the first WP9 leftover, §6 "Hang-ups and ends on the
native path"): a native non-stream forward whose client leaves before the body arrived ends the
forward today and leaves a `499` with unknown usage. In WP10 it reads on detached, as the chat
unary read does (§13.4 "Hang-ups on non-`openai` cloud rows"), so the row gets the answer's usage
and the budget its cost. It is in WP10 because 14.4 reworks the same `Forward::run`.

### 14.10 The migration: 0095

`0095_one_fallback.sql`, after a guard (below). For every existing row, the new `fallback` holds
exactly the name the row answered with before, and NULL where it had none.

```sql
-- The global as the loader reads it; NULL when unset, blank or unreadable.
-- (A blob that is not JSON is a default Settings to the loader, `parse_json_or`.)
-- local_models (chat): inherit read the global.
ALTER TABLE local_models ADD COLUMN fallback TEXT;
UPDATE local_models SET fallback = CASE hold_fallback_mode
  WHEN 'alias' THEN CASE WHEN TRIM(hold_fallback) <> '' THEN hold_fallback END
  WHEN 'inherit' THEN (SELECT CASE WHEN TRIM(g) <> '' THEN g END FROM (
      SELECT CASE WHEN json_valid(value) THEN json_extract(value, '$.hold.fallback_alias') END AS g
      FROM settings WHERE key = 'settings'))
  END;
ALTER TABLE local_models DROP COLUMN hold_fallback;
ALTER TABLE local_models DROP COLUMN hold_fallback_mode;

-- aux_models, audio_models, image_models: inherit meant none (config/snapshot.rs:818).
ALTER TABLE aux_models ADD COLUMN fallback TEXT;
UPDATE aux_models SET fallback = CASE WHEN hold_fallback_mode = 'alias'
  AND TRIM(hold_fallback) <> '' THEN hold_fallback END;
ALTER TABLE aux_models DROP COLUMN hold_fallback;
ALTER TABLE aux_models DROP COLUMN hold_fallback_mode;
-- (the same three statements for audio_models and image_models)

-- candidate_aliases: inherit read the global unconditionally; `fallback` exists.
UPDATE candidate_aliases SET fallback = CASE fallback_mode
  WHEN 'alias' THEN CASE WHEN TRIM(fallback) <> '' THEN fallback END
  WHEN 'inherit' THEN (/* the global, as above */)
  END;
ALTER TABLE candidate_aliases DROP COLUMN fallback_mode;

-- plain aliases had none.
ALTER TABLE models ADD COLUMN fallback TEXT;

-- The global goes, after it was copied.
UPDATE settings SET value = json_remove(value, '$.hold.fallback_alias')
  WHERE key = 'settings' AND json_valid(value);

-- The first start types the cloud fallbacks (below).
INSERT INTO settings (key, value) VALUES ('fallback_type_check', 'pending');
```

Behaviour by old state, every row: `alias` + name → that name; `alias` + blank → none (it was
none, `config/snapshot.rs:811`); `none` → none (a stale name in the column, which only a hand edit
leaves, was never read and is dropped); `inherit` on a chat row or a candidate alias → the global
as it stood, or none when it was unset or blank; `inherit` on an aux, audio or image row → none.
The value is copied untrimmed but only when it is not blank, as `hold_fallback_for` filtered it
(`.filter(|a| !a.trim().is_empty())`) and returned it as stored. A name that does not resolve, or
names a candidate alias, is copied as it is and is refused at use as today, with the same clause.
*As built (review round):* a mode the old loader did not know (anything but `inherit`, `none` and
`alias`) was read as `inherit` (`hold_fallback_mode_from_row`'s `unwrap_or(Inherit)`), and 0095
reads it so: the SQL's `CASE` has `WHEN 'none' THEN NULL ELSE <the global>` on a chat row and a
candidate alias, and the guard names such a row as one that followed the global.

No chain gets longer by the migration: today's fallbacks are never local and plain aliases get
NULL, so every existing chain ends where it ended. The one exception is a fallback that names a
**local** model. The save check refuses one, so it exists only where the target became local after
the save (an alias deleted and a local model given its name, a hand edit). Today it is a named
refusal under the hold ("is itself a local model") and a wait outside it; after 0095 it is a hop
(14.3): under the hold it cannot answer and its own fallback applies, outside the hold it is
admitted. The guard names each such row (below). The migration keeps the name rather than
clearing it, because the row says to fall back to that model and the ruling makes that a valid
setting. *Draft call.*

`DROP COLUMN` needs SQLite 3.35; the bundled SQLite is newer, and 0002 already uses `RENAME
COLUMN`. `candidate_aliases.fallback_mode`'s CHECK is the column's own, which `DROP COLUMN`
allows; no index, view or trigger names any of the dropped columns (checked: no
`CREATE INDEX/VIEW/TRIGGER` in `migrations/` touches them). The migration test proves the drop on
a database built at 0094.

**The guard** (`store/migration_guards/one_fallback.rs`, `one_fallback_notice`, called from
`run_migrations`, `store.rs:158-165`, before the migrator, skipped on a new database and once 0095
has run):
- **refuses the start** when the global as the loader reads it differs from what the SQL above
  would copy: the guard parses the blob with the loader's own `settings_from_row`
  (`store/snapshot.rs`) and compares `hold.fallback_alias` with `json_extract`'s value. They differ
  only for a blob that is JSON but does not deserialize as `Settings` (the loader then reads
  defaults, so no row inherits anything, while SQL would read a name). The refusal names the value
  and says to fix or clear the setting with the old version first. That is the one thing that
  cannot be carried over faithfully, and copying it silently would give rows a fallback they did
  not have. *As built (review round):* a global that is JSON but not a name (a number, a boolean)
  is that refusal too, naming the value ("hold 5 (not a name)"), not a database type error: the
  guard reads `json_extract` as text. The guard imitates the old loader with this build's
  `Settings`, which reads every blob a previous version wrote the same way while no field changed
  type in between (every field added since is `#[serde(default)]`);
- one WARN per row whose fallback names a local model (the exception above): "migration 0095:
  chat model 'a' falls back to the local model 'b', which lmgw refused to use until now; it is
  used from now on, with 'b''s own fallback after it". The guard tells a local name by the
  settings' class prefixes (read with `settings_from_row`) and the four local tables' `model_id`s,
  in its own queries, since no snapshot can be built before the migration;
- one WARN per row whose fallback is a model of another type (decision 29), as far as the
  guard can tell without the network: it runs before the gateway starts, so it types a fallback
  that names a local row (chat, the aux row's kind, the audio row's task, image) and leaves a
  cloud name to the first start after 0095 (below). "migration 0095: embedding model 'bge' falls
  back to 'local/qwen3', a chat model; a fallback must be the same type of model, so the next save
  of 'bge' refuses it until it is changed. It is kept as it is: a request that reaches it is
  refused as one its endpoint cannot serve". The migration keeps the name: a wrong-type fallback
  is a configuration to fix, and the per-request endpoint check (14.3) already refuses it;
- one WARN per row whose fallback comes from the global: "migration 0095: chat model 'qwen3'
  followed the global hold fallback 'cloud'; 'cloud' is its own fallback now, the global setting
  is gone", the same for a candidate alias; one WARN per row whose stale, unread name is dropped
  (mode `none` or `inherit` with a name in the column); one INFO for the global itself ("the
  global hold fallback 'cloud' was copied onto 3 models and 1 candidate alias and removed"). An aux,
  audio or image row on `inherit` changes nothing and is not named.

**The cloud names' types after the start.** 0095 also writes the marker row
`fallback_type_check` = `pending` into the `settings` table (beside the `settings` blob, not in
it). The first start that sees it runs the save-time type
check (14.2, item 4) on every row whose fallback is not a local row, once its snapshot is up and
in the background, fetching upstream catalogs as `/v1/models` would, and logs one WARN per
mismatch in the guard's words (a type it cannot know is not named: the endpoint check guards it).
The marker is removed once the check ran, so a start that stopped before it checks again.

After 0095 a new row starts with no fallback (today a new chat row followed the global): the owner
sets one per model. That is decision 26's consequence, and the WARN lines say so.

### 14.11 Surfaces

No compatibility shims (alpha): every surface moves to `fallback` in the landing, and what breaks
in the owner's own clients is planned (last group).

**Store and config:** migration 0095 and its guard (14.10); `store/local_models.rs`,
`aux_models.rs`, `audio_models.rs`, `image_models.rs`, `candidate_aliases.rs`, `aliases.rs` (the
`NewAlias` field) and `rows.rs`; `config/models.rs` (`HoldFallbackMode` and the four fields);
`config/routes.rs` (`CandidateAlias::fallback_mode` removed, `ModelAlias::fallback` added);
`config/settings_classes.rs:77` (`HoldSettings::fallback_alias` removed, `HoldSettings` keeps
`active`); `config/settings_dto.rs:89`; `config/snapshot.rs` (the lookups, 14.3; the
`aliases` doc comment); `vram/view.rs:110`, `:259` (`VramView.hold_fallback_alias` removed).

**Ops and the gate:** `ops/fallback.rs` (new, 14.2) replacing `validate_fallback_alias`
(`ops/settings_patch.rs:159`); `ops/local_model.rs:285-310` (`resolve_hold_fallback`),
`ops/aux_model.rs:446-480` (`resolve_hold_fallback_text`, used by audio and image as well),
`ops/aux_model/fallback.rs`, `ops/audio_model.rs`, `ops/image_model.rs`,
`ops/candidate_alias.rs:80-105`, `:388`; `ops/routing.rs:241` (`AliasPatch.fallback`) and
`model_set`; `web/api.rs:1325`, `:1372` (the dashboard's alias writes); the settings patch's
`hold_fallback_alias` (`ops/settings_patch.rs:327-336`) and `web/api_settings.rs:418`, `:782-788`
removed; `ops/hold.rs:159`, `:176` (`HoldSet.fallback_alias` removed); `ops/reads.rs`,
`ops/ladder_checks.rs:428`, `ops/container_actions.rs:443`, `ops/audio_tools.rs`; the gate (14.3:
`gate/open.rs`, `gate/guest.rs`, `gate/candidate/{mod,walk}.rs`, `gate/send.rs`, a new
`gate/chain.rs`), `vram/climb.rs`, `vram/on_cpu.rs`; the six display readers (14.1); the test
constructors that spell the fields (`gate/ladder.rs:240`, `ladder.rs:160`, `gate/count.rs:600`,
`bench/plan.rs:531`, `web/aux_models.rs:55`, `web/audio.rs:645`, `web/wiring.rs:150`).

**API types** (`lmgw-api-types`): `models.rs` (`LocalModel` :160-165, `AuxModel` :321-326,
`AudioModel` :428-433, `ImageModel` :510-513 take `fallback`; `default_hold_fallback_mode`
:186-190 goes; `AliasView` :47 gains `fallback`); `model_detail.rs:79`, `:111`, `:159`;
`wiring.rs:112-115`; `candidate_alias.rs:27-28` (`fallback_mode` goes);
`self_admin/models.rs:106-108` (`CandidateAliasEntry.fallback_mode` goes) and `fallback` with
`fallback_usable` in `ModelFacts` (:61, flattened into every entry, so `lmgw__models` shows each
model's and alias's fallback); `settings.rs:204` (`HoldSettingsDto.fallback_alias`), `:376`
(`HoldSet.fallback_alias`); `status.rs:428` (`hold_fallback_alias`); `chat_feed.rs:202`
(`FeedHold.fallback_alias`, the feed's `hold` event and `Hello`); `decision_lab/models.rs:50`
(`hold_fallback` → `fallback`); `upstreams.rs` (`UpstreamView.quota`, 14.6).

**MCP ops** (the self-admin catalog): `lmgw__local_model_set`
(`mcp/selfadmin/catalog/models.rs:285-298`, the `clear` text at :345), `lmgw__aux_model_set`
(:509-530), `lmgw__image_model_set` (:668-720), `lmgw__audio_model_set`
(`catalog/audio.rs:302-311`), `lmgw__candidate_alias_set` (`mcp/selfadmin/candidate_alias.rs:71-73`)
and `lmgw__model_set` (`catalog/routing.rs:122`) each take `fallback` with one description: "The
model or alias that answers when this one cannot: the GPU hold, VRAM outside lmgw, background
traffic deferred, an unavailable candidate, an exhausted upstream quota, an unreachable upstream,
or a disabled row. A local model id, a passthrough id or a plain alias of the same model type;
not a candidate alias. Chains are allowed; a loop is refused. clear: 'fallback' removes it." `lmgw__settings_set` loses `hold_fallback_alias`
(`catalog/runtime.rs:375`); `lmgw__hold_set`'s description (`catalog/runtime.rs:80-87`), the
`lmgw__settings` description (`catalog/reads.rs:140`) and the server instructions
(`mcp/ingress.rs:905-918`) drop the global and say each model has its own fallback. New:
`lmgw__upstream_quota_clear`.

**Errors:** `GatewayError::Unreachable` (14.4), with its arms beside `Transport`'s and its kind
`unreachable` in the in-process error round trip (`proxy/in_process.rs`).

**OpenAPI and the API docs:** the schemas follow the types; `openapi/headers.rs` (the reason enum,
`x-lmgw-fallback`'s description, the chain header); `openapi/planes/decision_lab.rs`; the op table
rows of `upstream_quota_clear`; the API-docs page's header and op rows.

**UI** (`lmgw-ui`, 14.12): `pages/model_editors.rs:62-120` (`HoldFallback`, `hold_signals`,
`hold_body`) → one `Fallback` control, used at :1043 and :2072 (audio, image) and by
`model_editors/aux_models.rs`, `aux_fields.rs`, `aux_save.rs`, `pages/local_edit.rs:1660-1680`,
`pages/candidate_alias_editor.rs:194`, `:244`, `:411`, `:621`, `:637`; `pages/models.rs:94`,
`:119`, `:156` and the plain alias editor (new control); `pages/settings/rows.rs:277` (the
`hold.fallback_alias` row goes) and `pages/settings/tests.rs:10`, `:38`; `shell.rs:263`,
`:405-425` (the titlebar pill); `pages/decision_lab/head.rs:121`; `pages/chat_voice/section.rs`;
`pages/traffic.rs:1037` (the reason's words); the Upstreams row's badge (14.6).

**Docs:** `README.md:112` ("Requests can fall back to a cloud alias" → "to the model's fallback");
`docs/agents.md:2337-2342` (the folder sync: "aliases without a cloud fallback" → "models without a
fallback"); the design notes of 2026-10-24 in the gpu-hold, image-generation, candidate-aliases,
decision-models, client-apps and labs-store designs (added with this section).

**The owner's clients** (planned follow-ups, at their next pin raise): Kai logs the feed's
`hold.fallback_alias` (`kai/src/lmgw_link/feed.rs:609`, tests at `feed/tests.rs:24`, `:247`,
`:746`, `:1123`): the field goes from the log line and the fixtures. kai-mobile shows "which
fallback would hear or speak" from the global (`kai-mobile-core/src/link/actor.rs:999`, `:1108`,
`:1116`, `voice/view.rs:196`, `voice/actor.rs:567`): it reads the per-model answer instead, the
fallback the voice resolution already names (`lmgw-api-types/src/voice_resolved.rs:143`). Until
then kai-mobile's voice view shows no hold fallback, which is the honest state once the global is
gone.

### 14.12 The UI

One **Fallback** control in every model and alias editor: the local chat, aux, audio and image
editors, the candidate-alias editor and the plain-alias editor on the Models page. Its label is
"Fallback"; the plain-alias and the local chat editor give it a card of its own, "When it can't
answer", and the candidate-alias editor one called "When none of its models can answer". Its hint
names the reasons that can apply to that kind of owner: a local row's "Answers whenever this
model can't: while the GPU is held, when VRAM outside lmgw is short, for background requests that
would disturb other work, and while the row is disabled", a plain alias's "when its upstream is out
of quota or can't be reached, and while the alias or its upstream is disabled", a candidate
alias's "and when no candidate is usable".

*As built (2026-10-25, with the review round):* the control is the dashboard's model picker over
the gateway catalog (what `/v1/models` publishes: plain aliases, local models of every class,
passthrough ids of `expose_all` upstreams), "None" first, grouped as the picker groups that
catalog. It is narrowed to the owner's type (decision 29): a model the catalog gives another task
is listed under "Not allowed here", as are the owner itself and every candidate alias; a model whose
task the catalog does not state is offered, and the save checks it; a passthrough id the catalog
does not list is typed as free text. The owner's type is its row's (the class, an aux row's kind,
an audio row's task: every task `/v1/models` publishes is a type of its own), a candidate alias's
chat, and a plain alias's what its capability override states, else its upstream catalog's entry,
else what the gateway publishes for the alias, so an unreachable catalog still narrows it. Under the
picker: a save's refusal of the fallback (in the field's error style), where the fallback leads
("→ cloud-mini → none", "… (a loop: the save refuses it)"), and the saves' notes (the same-upstream
note, the facets a chat fallback lacks or adds, a type that could not be checked, a disabled
target). The notes are kept with the fallback they speak of: they show while the field holds it,
on a second save that says none too, are off screen while another fallback is picked, and show
again when it is picked back (verify round: a save that leaves the fallback unchanged gets no
notes from the server). A local model's create, which opens the saved row's own page, hands its
notes to that page's editor. A refusal for another row's fallback (the referrer check of
§14.2) is said where the editor says its other errors. An editor keeps the row it saved, so a
second save of the same open form is an update; a modal stays open while there is something to
read, and closing it then does not ask about unsaved changes. "None" sends `clear: "fallback"`
whatever the row had when the form opened. A delete's notes, and an upstream delete's or prefix
change's, reach the page as warnings. There is no "Inherit" entry and no link to Settings → GPU →
Hold any more.

The titlebar's GPU pill (`shell.rs:405-425`) says today "Held chat requests go to X" from the
global. It becomes: "Held models answer with their own fallback, where one is set" with a link to
the Models page, and the Models list shows each row's fallback in a column, so "which local models
would refuse under the hold" is one look. *Draft call* (the pill's words and the
column).

### 14.13 Steps and tests

Steps (WP10, one commit each):
1. **The one field.** Migration 0095 and its guard; `fallback` on the six config types and their
   stores; `HoldFallbackMode` and `settings.hold.fallback_alias` removed; `ops/fallback.rs` with
   the save checks, the type check and the cycle walk; the six setters, the dashboard's alias
   routes, the settings patch; the references rewritten on a rename and a prefix change (decision
   27); the API types, MCP descriptions, OpenAPI; the UI control and the removed Settings row;
   the `aliases` doc comment. Behaviour: the same one-hop answers as before for every existing row,
   and a plain alias can now be given a fallback that the GPU reasons never reach (a cloud alias
   is never held).
2. **The chain.** `HopKey`, `Snapshot::fallback_of`, `Snapshot::hop`, `gate/chain.rs`, the loops at
   resolve, admission, the guest path and the candidate walk; the local-target refusal removed;
   the chain header and its docs; the display readers. Behaviour: local fallbacks and chains for
   the GPU reasons.
3. **`FallbackReason::{Quota, Unreachable, Disabled}`** and every match arm, the header docs,
   `UpstreamLimited.note`, `GatewayError::Unreachable`; `disabled` caught at resolve.
4. **`QuotaMemory`**, the skip at resolve and at the send, the re-check and its setting, the badge,
   `upstream_quota_clear`, the log lines.
5. **The chat-send path** (`proxy/reasoning_fit/quota.rs`, `send_chat`'s new arguments,
   `gate::open_hop`), the quota and an unreachable upstream.
6. **The native `/v1/responses` path** (`Native`, `serve_routed`, the priced check moved into it),
   and the detached native whole-answer read (14.9).
7. **The non-chat priced check on the route the gate hands back** (14.9).
8. The other design docs and `README.md`, `docs/agents.md` (already noted in the design docs with
   this section; the README and agents.md lines change with step 2).

Tests:
- **Migration** (`tests/it/migrations/one_fallback.rs`, a database built at 0094 and upgraded
  through `run_migrations`): one row per old state of every table (14.10's list: `alias` + name,
  `alias` + blank, `none` with and without a stale name, `inherit` with the global set, blank and
  unset, on a chat row, a candidate alias and each of aux, audio and image) and the global set to a
  name with surrounding blanks. For each, the resolved fallback through
  `Snapshot::fallback_of` after the upgrade equals what `hold_fallback_for` /
  `alias_fallback` returned before it, computed on the same database before the upgrade by a
  helper that runs today's logic in SQL. The settings blob has no `hold.fallback_alias` after,
  and its other keys are byte-equal. A blob that is not JSON gives NULL everywhere and starts. A
  blob that is JSON but not `Settings` with a global name is refused by the guard, naming it. The
  WARN lines are asserted (one per inheriting row, one per dropped stale name, one per fallback
  naming a local model, which then answers as a hop). The dropped
  columns are gone (`has_column`).
- **Save checks** (`tests/it/fallback_saves.rs`): each setter takes `fallback` and `clear:
  "fallback"`, refuses `""`, itself, a candidate alias and a loop (the message names it, across
  kinds: a local row → an alias → the local row), accepts a local model id and a passthrough id;
  refuses another model type naming both (a chat row → an embedder, an embedder → a chat alias,
  ASR → TTS), saves a chat fallback with other facets with the warning naming them, saves an
  unknown type with its warning; the same-upstream note; a delete's note; a disabled target's
  note.
- **Renames** (`tests/it/fallback_renames.rs`, decision 27): a renamed plain alias, candidate
  alias, chat, aux, audio and image row, and a changed chat, aux, decision, audio and image
  prefix each rewrite every other kind's fallback that named them, and leave a name that resolved
  elsewhere alone; the answer lists them.
- **The chain at the GPU reasons** (`tests/it/vram_admission/fallback_chain.rs`): a held local
  model whose fallback is a held local model whose fallback is a cloud alias (answered by the alias,
  chain header `"m2";reason=hold, "cloud";reason=hold`, one row); a held local model whose fallback
  is a CPU audio row (served locally); a held local chain with no cloud end (`gpu_hold` naming the
  chain); outside VRAM onto a local model that fits (it starts) and onto one that does not (its
  fallback); a guest along a chain (each local hop deferred, the cloud end answers, reason
  `background`); a candidate alias's fallback onto a local model; a loop written past the save
  check (the guard's error, no loop). The existing hold, outside-VRAM, guest and candidate suites
  pass unchanged except for the fields' spelling.
- **The quota** (`tests/it/quota_fallback/`, wiremock): unary and streamed chat, `/v1/messages` and
  native `/v1/responses`, each answered by the fallback with the headers and two rows; a rate limit
  and a flex 429 still reach the client; no fallback (the error and its `note`); a fallback that
  cannot serve the endpoint; a local fallback under the hold that goes on to its own fallback
  (reasons `quota` then `hold`) and one without (`gpu_hold` naming the chain); a chain A → B → C
  answered by C (A's and B's refused rows, C's answer, `x-lmgw-fallback: C` and the chain header);
  a chain with a remembered-exhausted middle hop (skipped, no row, named in the header); the
  remembered skip with one row, at resolve and at the send (an in-process run whose route was
  resolved before the state was set); an unreachable upstream (a closed port) answered by the
  fallback with its own row, and without a fallback the `502` as before; a disabled alias, a
  disabled local row and an alias on a disabled upstream answered by their fallbacks (reason
  `disabled`), and without one today's answer; the re-check after the interval (a 2xx clears it, a quota
  sets it again), `Retry-After` honoured, an upstream edit and "Check now" clearing it; an
  in-process agent run continuing on the fallback; a fallback on the same upstream skipped while
  the state is set; a held embedder whose fallback is an unpriced cloud encoder refused
  `budget_unpriced` under a budget (14.9); a native whole answer whose client left before the body
  (the row has the usage).
- **Units:** the chain header's rendering (escapes, a non-ASCII name), `HopKey` comparison across a
  case variant, `fallback_of` per kind, and `size_of::<GatewayError>()` staying under clippy's
  threshold.

### 14.14 Questions

*Decided by the owner on 2026-10-24* (kept from the first version, and adjusted to the one field):
- **Where the fallback is configured:** on each model and alias, one field (decision 25); not on
  a passthrough model; it may name a local model id, a passthrough id or an alias.
- **One hop or a chain:** a chain, no depth cap, a cycle refused at save naming the loop and
  guarded at use (the owner's ruling against the draft's one-hop rule).
- **The global hold fallback:** removed, with `inherit` (decision 26). The migration writes the
  inherited value into each inheriting row, so no row changes behaviour.
- **The same-upstream note:** kept, and it says "may share its quota".
- **A local model as a fallback:** allowed. The first version's "admitted with no fallback of its
  own" for a local quota fallback is superseded by decision 25 in this section's reading (14.3,
  open item 5).
- **The other request classes:** chat and Responses for the quota in WP10, the rest as WP10b; their
  GPU-reason fallbacks are unchanged.
- **Other protocols' quota errors:** a follow-up mapping each from its provider's documented errors,
  with fixtures, after WP10.
- **The re-check interval:** 300 s by default, visible in Settings, `Retry-After` first.

*Answered by the owner on 2026-10-25* (the questions as they stood, with the owner's answers):
1. **A candidate alias as a fallback target.** Refused in WP10 as today, "not yet": with the
   per-hop gate (14.3) a candidate alias is just another hop that `gate::open_hop` could pick
   for. *Approved:* allowed in a follow-up after WP10, once the chain has run; until then the
   refusal says "not yet".
2. **Renames.** *Approved and built in WP10* (decision 27): a rename of an alias or a row, or a
   prefix change, rewrites every other row's and alias's fallback that names it in the same write
   transaction, so a rename never breaks a chain (14.2, tested per kind in
   `tests/it/fallback_renames.rs`).
3. **More reasons.** *Folded into WP10* (decision 28): `unreachable` (an upstream connect error,
   nothing sent, caught where the quota is caught, before any byte reaches the client) and
   `disabled` (a disabled upstream or a disabled row) are fallback reasons, in the reason enum, the
   headers, the rows and the docs (14.3, 14.4, 14.5, 14.7, 14.8). Nothing is retried once a byte
   went to the client.
4. **Case of plain aliases.** Plain aliases resolve case-sensitively while `model_set` finds them
   case-insensitively and candidate aliases are case-insensitive (14.1). *Approved, not in WP10:*
   one rule, case-insensitive names for every alias kind (a `COLLATE NOCASE` unique index on
   `models.alias`, a refusal at the migration for two aliases that differ only in case, the
   lookup lowercased), as its own small change after WP10.
5. **The draft calls of this section:**
   a. the one nullable column and its name `fallback` (14.2): approved;
   b. `""` refused in favour of `clear` (14.2): approved;
   c. a local hop's own fallback applies on a quota fallback under the hold, reversing the first
      version's "no fallback of its own" (14.3): approved (the owner confirmed this reading on 2026-10-25:
      the local model's own fallback applies);
   d. *changed:* the first draft let any model or alias be named whatever its type. The owner ruled on 2026-10-25 that a fallback must be the same
      model type, with a modality difference between chat models allowed with a warning. So every
      setter refuses a fallback of another model type, the UI picker offers only the owner's type, and between chat models a
      modality difference is saved with a warning (decision 29, 14.2 item 4). A hop the endpoint
      cannot use still ends the walk rather than being skipped (14.3): with the type check at
      save, it is the guard for a hand edit or a type that changed since;
   e. a local hop with no fallback waits as a request naming it would (14.3): approved;
   f. a fallback that names a local model today is kept by the migration and becomes a working
      hop (14.10): approved;
   g. a delete is not refused but names the rows left without a working fallback (14.2):
      approved.

*New draft calls of WP10:*
- **A type lmgw cannot know at save** (14.2 item 4): a fallback or plain alias whose upstream
  catalog does not list the model, or cannot be read right now, has no published capabilities,
  and one listed bare (no task, no modalities, a name that says nothing) has only the routing
  default `chat`.
  The save goes through with a warning that the type could not be checked; the per-request
  endpoint check (14.3) stays the guard. Refusing would make a correct fallback unsaveable
  whenever a provider's catalog is down or silent; accepting silently would hide that nothing
  was checked.
- **`disabled` only where a name fails today** (14.3): a name that resolves today keeps resolving
  as it does; only a name that would otherwise not resolve, or an alias whose upstream is
  disabled, is looked up among the disabled rows. So a disabled alias that shadows nothing keeps
  falling through to a local or passthrough name of the same spelling, as today.
- **`unreachable` on cloud routes only** (14.4): a local container's connect failure is the
  dead-container recovery's (`vram::retry_dead_container`), not a fallback reason.
- **The row of an unreachable attempt** (14.7): written like a quota's, status `502`, so the owner
  sees that an upstream is down even when its fallback answered.
- **The guard's offline type check** (14.10): the guard types only local targets; the first start
  after 0095 types the cloud names.

*Draft calls made while building WP10 (2026-10-25):*
- **"Cannot serve this endpoint" says why** (14.3): the clause carries the site check's own
  refusal, "(fallback 'x' cannot serve this endpoint: model 'x' is …)", so the reader learns what
  the hop lacks without naming it again.
- **A local embedding or rerank row is refused for generation** (14.3): a chat, Messages,
  Responses or legacy completions request that reaches one — named, or along a chain a hand edit
  left — is refused by name before its container starts, as a decision row already is.
- **The quota memory's re-check is the send's** (14.6): resolve only looks (it skips while the
  next check is ahead and takes nothing); the chat send and the native forward take a due
  re-check, so a request resolved and sent once never takes it twice.
- **A disabled hop is passed statically** (14.3): along the chain a disabled fallback with a
  fallback of its own is named in the chain (its reason the one that brought the request there)
  and its fallback gets reason `disabled`. A climb's reroute and the outside-VRAM verdict pass it
  too; a climb's reroute does not name it in the chain header yet.
- **The non-chat priced check runs on the static walk's route** (14.9): the route
  `gate::resolve` hands back (hold, benchmark, disabled, and since the review round a CPU row's
  container still on the GPU under the hold), not yet the one an admission-time reroute (VRAM
  outside lmgw, a deferred guest) of an embedding or media request reaches.
- **lmgw's own in-process callers hand on without a chain behind them** (14.4): the dashboard's
  Chat, its tools and sampling carry `RouteCheck::None` and the owner's origin; the loop guard
  holds within one hand-over, and the chain they report is that hand-over's. *Changed in the review
  round:* a caller that carries a client's request carries that request's instead — a synthesized
  `/v1/responses` run its `/v1/responses` route check, its chain, and its origin (a background
  run's turns hand on as a guest), and since the verify round a realtime response's chat call the
  `/v1/realtime` route check and its route's chain.

*Draft calls of the WP10 review round (2026-10-25):*
- **What stood before a write does not refuse it** (14.2): the referrer check refuses a write that
  would give another row's fallback another type, a candidate alias or a loop; a mismatch or a loop
  that was there before the write (0095 keeps a wrong-type fallback as it was, a hand edit can
  write a loop) is left to that row's next save and the use-time guard, so it does not block every
  unrelated save until it is fixed.
- **A type that cannot be read is not checked for the other rows either** (14.2): an alias whose
  upstream catalog does not list the model, or cannot be read now, has no type; a write that points
  another row's fallback at it goes through, as the owner's own save does with its warning.
- **A request's own claim on the model it leaves is freeable for its fallback** (14.3, *changed in
  the verify round*): the claim is kept, and the fallback's admission may evict that model only
  when the claim is its one user — one claim in flight, no ladder send, no turn on the hold (the
  leaving send's own ends as it leaves; a sibling's counts from its fit). A claim several model
  calls share, or a second request on the model, keeps it. On the in-process paths the
  fallback's whole call runs inside the claim it left, and the claims left stack along a chain
  (verify round 2). (The review round gave the claim back before the admission; that read a
  shared claim as gone.)
- **Routing writes wait for each other** (14.2, verify round): one at a time from the snapshot
  their checks read to their commit and reload, with no limit of lmgw's own on the wait, so a save
  whose checks read a slow catalog holds the next routing save back for as long as that read takes
  (each catalog read bounded by that upstream's own timeout, none at 0; the owner's answer of
  2026-10-25, which replaced the fixed 10 seconds every catalog read had). Since verify round 2 a
  delete's task carries the lock too, and none holds it past its reload. Since the WP7 fix round a
  request waits for a models list only when none is cached (a stale list answers at once and is
  refreshed in the background), and misses at once share one read (owner, 2026-10-25).
- **A client that left before the hand-over is not handed on** (14.4): its attempt's row is the
  hop's error with the clause "(the client left before it was handed to 'x''s fallback)"; no
  fallback row is written, since nothing answered. One that leaves during the hand-over gets the
  attempt's row naming the fallback and "the client left before it answered" (verify round).
- **"Any 2xx" is a billable class's** (14.6): chat, Responses, embeddings, rerank, System One,
  audio, images and legacy completions clear the remembered quota; a token count, `/tokenize`
  and a catalog read do not, since a provider may answer those without credit.

## Later

- An early check of OpenAI's strict-schema subset on `openai` rows that names the offending
  property before the round trip (decision 18).
- The long-context band, the 12-hour cache write and the regional uplift as price dimensions
  (decision 20: a follow-up design after WP4).
- An alias that always asks for a tier (`param_defaults.service_tier`), with the price tier
  resolved from the response as everywhere.
- Context length, output cap and reasoning facts for `openai` rows from a source other than the
  owner's override (OpenAI's catalog gives none).
- Hosted-tool fees as price units (§6.1): per 1k calls of web search and file search, per
  container session for Code Interpreter and Hosted Shell, and the image model's tokens for
  image generation, each with its quantity on the row, so those rows stop being unpriced.
- A reusable `prompt` (`{id, version}`) on the native path is forwarded for every key; it is
  project configuration under the owner's provider key like a file, and whether a client key may
  name one is not decided yet.
- An opt-in per upstream for an owner who gives lmgw its own OpenAI project (owner idea,
  2026-10-24): under it, the project's objects (prompts, files, vector stores, containers,
  skills) count as shared with all client keys. The strict per-key split stays the default.
- Responses-only request features through the platform egress on lmgw's own sends: hosted tools,
  `reasoning.mode: "pro"`, `prompt_cache_options`, `verbosity`.
- A Usage grouping by tier.
- The Batch API and its tier.
- The fallback on an upstream quota for embeddings, audio, images and legacy completions (WP10b,
  §14.4), and quota errors of the other protocols mapped to `UpstreamLimited` (§14.14).
- A column linking a refused attempt's row to the row of the request that answered it (§14.7).
- A candidate alias as a fallback target, and case-insensitive plain aliases (§14.14, approved
  for after WP10).
- Not yet after WP10: the display readers (the chat voice's fallback line, the decision lab's
  head) show a model's first hop past disabled ones, not where a held or out-of-quota chain
  ends; a climb's reroute does not name a disabled hop in the chain header; the non-chat priced
  check does not see an admission-time reroute; an in-process run's quota hand-over has no
  integration test of its own (a background run's unreachable hand-over has, `quota_fallback::runs`;
  the HTTP chat, `/v1/messages` and native Responses paths have the quota's); the race between a
  turn counted and an eviction's stop of its model has no test of its own (the count and the
  stop share the registry's lock, `LocalHold::turn`).
- A budget floor for a flagged `openai_compatible` row's cut native stream, once lmgw knows a
  tokenizer for what it serves (§13.4).

Not a Later item any more: recovering a cut native stream's usage and hosted calls by a `GET`
after the cut. OpenAI does not store a cut response and answers 404 (live, 2026-10-24), so the
idea is dead (§6). Its budget side is §13.4's local count.
