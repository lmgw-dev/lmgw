# Structured output, translated everywhere — design (2026-10-22)

**Status:** approved 2026-10-22 (the decisions below); built the same day.
Choices made while building are in §11, for review; the review's fix round
(2026-10-22) changed §11.1, §11.3 and §11.8 and added §11.17–§11.22. The
provider texts in §9 come from the live probes and source reads listed in
§9.5; every body is a fixture under
`crates/lmgw-core/tests/fixtures/structured_output/` ("fixture" below).

## 1. Summary

A client that wants JSON back asks for it once — `response_format` on
`/v1/chat/completions` and `/v1/completions`, `text.format` on
`/v1/responses`, `output_config.format` on `/v1/messages` — and expects one
of two things: a constrained answer, or a refusal it can act on. The first
such client is a game that tries `json_schema` first and steps down to
`json_object` when the model cannot do the stronger mode. It has to tell
"this model cannot do it" from "your schema is wrong" without matching
provider texts.

Until now neither was possible:

- `response_format` rode in the IR's untyped `passthrough` map. Only the
  OpenAI-shaped egresses re-emitted it; the **Anthropic and Gemini egresses
  dropped it** and answered `200` with free text. `/v1/messages` dropped its
  own `output_config.format` at ingress.
- Every upstream refusal was `code: upstream`, `param: null`, whatever it was
  about.

This design makes structured output a typed IR field that every ingress fills
and every egress translates (§4–§6), refuses early only where lmgw knows the
model cannot (§7), and gives the two refusals a client branches on stable
codes, with the upstream's own message kept (§8, §9).

## 2. Facts this rests on (checked 2026-10-22, `main` c698bd9e)

1. **The IR has no field.** `ChatRequest` (`ir.rs`) carries `response_format`,
   `json_schema` and `grammar` in `passthrough`, captured verbatim by
   `ingress/openai.rs::passthrough_fields`. The OpenAI-shaped body builder
   (`egress/openai_wire/body.rs`) re-emits passthrough after the modelled keys.
2. **`/v1/responses`** renames `text.format` to a chat `response_format` in its
   ingress (`ingress/responses.rs::passthrough_fields`; `name` defaults to
   `"response"`, `strict` to `false`, `description` dropped, any other `type`
   dropped). A native Responses upstream gets the client's body as-is.
3. **`/v1/messages`** ingress sets `passthrough` empty: `output_config.format`
   is lost. `output_config.effort` is read as the reasoning tier.
4. **The Anthropic egress** writes `output_config` only for the effort
   (`apply_reasoning`, which replaces the object). **The Gemini egress** has no
   `responseMimeType`; `sanitize_schema` strips keys from *tool* parameters.
5. **Capabilities.** `structured_output: {json_schema, json_object}` on
   `/v1/models` (api-types `StructuredOutputCaps`). Local llama rows publish
   `true/true` (llama-server builds a grammar for both). OpenAI-protocol
   catalogs: from `supported_parameters` (`structured_outputs` → json_schema,
   `response_format` → json_object), else absent. Anthropic and Gemini
   catalogs: absent, with the comment "egress drops `response_format` today".
   An owner's `capabilities_override` can set either flag.
6. **Candidate aliases** refuse a facet they do not enable in
   `Routed::using` (`gate/open.rs`) with `GatewayError::Unsupported` →
   `400 unsupported`. `structured_output` counts as used for a
   `response_format` of type `json_schema`/`json_object` or llama.cpp's
   top-level `json_schema` (`gate/candidate/uses.rs`).
7. **Errors.** `GatewayError::Upstream` renders `type: api_error` and
   `code: upstream` even for a 400; `to_openai_json` hard-codes `param: null`;
   `to_anthropic_json` has no code slot. The pattern for a classified upstream
   refusal is llama-server's `exceed_context_size_error` →
   `ContextExceeded` (`egress/openai_wire/aux.rs::context_refusal`, ahead of
   the generic mapping). An upstream 400 never triggers a fallback or a
   candidate re-pick. `proxy/reasoning_fit/send.rs` retries a cloud 400/422
   once without the reasoning off — only when the mapped error is still
   `Upstream`.
8. **llama.cpp (source, official master of 2026-09-26 and ik_llama.cpp).**
   `response_format` `json_object` takes an optional `schema` (an extension);
   `json_schema` reads `json_schema.schema`; `text` or no type is plain;
   any other type is `400 response_format type must be one of "text" or
   "json_object", but got: …`. A schema the GBNF converter cannot take throws
   `std::invalid_argument("JSON schema conversion failed:\n…")` →
   `400 invalid_request_error` (`common/json-schema-to-grammar.cpp`). The
   newer PEG chat path builds the schema with `common_chat_schema_from_json`,
   whose `fail()` throws `std::runtime_error("JSON schema error at <path>:
   <msg>")` (`common/json-schema.cpp`), which the server's exception wrapper
   answers as `500 server_error`. llama-server never refuses a JSON mode as
   unsupported.
   *Live correction (image official-master b11714, probe 2026-10-24):* the
   server writes schema paths as `#/properties/word` and a root error as
   `#`. On the chat route every schema error is `500 server_error` `JSON
   schema error at #/properties/word: …`, including an unrecognised type
   (`JSON schema error at #: unrecognized type nonsense`) and an unresolved
   `$ref`. "JSON schema conversion failed" appears only on legacy
   `/v1/completions` and `/completion` with a top-level `json_schema`, as
   `400 invalid_request_error` `Field 'json_schema': "json_schema": JSON schema
   conversion failed:\nJSON schema error at #: …`.
   *Fix round (2026-10-22), source of mainline `95887577` (the 2026-09-26
   master the image is built from) and ik_llama.cpp `15e7ecac`
   (2026-10-21):* both servers' `oaicompat_chat_params_parse` read
   `response_format.type: json_schema` from `json_schema.schema`
   (mainline `tools/server/server-common.cpp` 1185–1199, ik
   `examples/server/server-common.cpp` 675–688), and both refuse a
   top-level `json_schema` next to a `grammar` before reading
   `response_format` (`Cannot use both json_schema and grammar`, mainline
   1179–1183, ik 669–673). Their legacy `/v1/completions` reads no
   `response_format` at all: mainline hands the raw body to
   `handle_completions_impl` (`server-context.cpp`, `post_completions_oai`),
   ik passes it through the one-argument `oaicompat_chat_params_parse`
   (`server-common.cpp` 587–636); the reader after that knows only the
   top-level `json_schema` and `grammar`, the grammar winning when both are
   there (mainline `server-schema.cpp` 251–285, ik `server-context.cpp`
   1385–1414).
9. **Anthropic** structured outputs are GA as
   `output_config: {format: {type: "json_schema", schema}}`, no beta header.
   There is no JSON-object mode. Objects must say `additionalProperties:
   false`; recursive schemas, numeric and string-length constraints are not
   supported. `models.list` publishes `capabilities.structured_outputs.
   supported` per model. The older top-level `output_format` is deprecated.
10. **Gemini** constrains output with `generationConfig.responseMimeType:
    "application/json"`, plus `responseJsonSchema` (JSON Schema) or
    `responseSchema` (an OpenAPI subset). `models.list` says nothing about it.

## The owner's decisions (2026-10-22)

1. **Codes** on the OpenAI-shaped body: `400`, `type: invalid_request_error`,
   `param` set — `"response_format"` on `/v1/chat/completions` and
   `/v1/completions`, `"text.format"` on `/v1/responses`:
   - `response_format_unsupported` — the model or its upstream cannot do the
     requested type;
   - `invalid_json_schema` — the upstream rejected the schema itself; the
     upstream's message stays in `message`;
   - anything lmgw cannot classify keeps the generic code (`upstream`) with
     the upstream's message. Never guess.

   `param` becomes part of the OpenAI error body generally (other errors keep
   `null`). On Anthropic-shaped routes the body keeps its shape
   (`invalid_request_error`, message); it has no code slot.
2. **Early refusal only when lmgw knows:** when the routed model's flag for the
   requested type is explicitly `false` (`structured_output.json_schema` for
   `json_schema`, `.json_object` for `json_object`), lmgw answers
   `response_format_unsupported` before any upstream call — for plain model
   ids and plain aliases too. An absent flag is unknown: the request goes, the
   upstream decides. The candidate-alias facet refusal uses the same code and
   param. The owner's capability override is the escape hatch.
3. **Translate natively** — lmgw's core task as a gateway is to translate. A
   feature an upstream has natively is translated in egress; refused only where
   no equivalent exists; never dropped silently. Structured output becomes a
   typed IR field so every ingress reaches every egress. Capability flags
   publish what lmgw now honours: `json_object: false` on Anthropic rows;
   `json_schema` absent unless a real per-model source exists (no
   hand-maintained model lists).
4. **Upstream error classification**, centralised per upstream, unit-tested on
   captured bodies: llama.cpp's `JSON schema conversion failed` →
   `invalid_json_schema`; OpenAI's `Invalid schema for response_format` →
   `invalid_json_schema` and its "not supported with this model" →
   `response_format_unsupported`; Anthropic and Gemini by the texts captured.
   Unknown stays generic. These never trigger a fallback, a re-pick or the
   reasoning-off retry — on the streamed path's pre-stream errors as well.
5. **Live probes allowed**, the cheapest model per upstream that can provoke
   the bodies, about 20 requests in all, `max_tokens` ≤ 32, no personal
   content, through a dev copy of the gateway.

## 3. The shape, end to end

```
client dialect ──ingress──▶ ChatRequest.response_format ──egress──▶ upstream dialect
  response_format            ResponseFormat::{Text,                   response_format (OpenAI, llama.cpp)
  json_schema (llama.cpp)      JsonObject, JsonSchema, Other}         output_config.format (Anthropic)
  text.format                                                         generationConfig.responseMimeType
  output_config.format                                                  + responseJsonSchema (Gemini)
```

Between the two, the gate refuses a type the routed model is known not to do
(§7), and on the way back each egress's classifier turns its upstream's
structured-output refusals into the two codes (§9). A native `/v1/responses`
upstream gets the client's body as it came; only the gate and the classifier
apply there.

## 4. The IR field

`ChatRequest.response_format: Option<ResponseFormat>` (`ir/structured.rs`),
`None` when the client asked for nothing.

| Variant | Meaning | Fields |
| --- | --- | --- |
| `Text` | plain text, stated explicitly (`{"type":"text"}`) | `extra` |
| `JsonObject` | any JSON object (OpenAI JSON mode) | `schema`: llama.cpp's extension, an optional schema on `json_object`; `extra` |
| `JsonSchema` | output constrained to a JSON Schema | `name`, `description`, `schema`, `strict` — each optional, as OpenAI's `json_schema` object has them; `extra` (the `json_schema` object's other keys), `format_extra` (the format object's), `bare` (no `json_schema` object was sent) — §11.17 |
| `Other` | a `type` lmgw does not model (e.g. a server's own `structural_tag`) | the client's object verbatim |

`ResponseFormat::kind()` is the type a capability flag answers for:
`json_schema` for `JsonSchema`, `json_object` for `JsonObject`, nothing for
`Text` and `Other` (no flag speaks for them, so no early refusal).
`ResponseFormat::ask()` (`FormatAsk`) is what an upstream's error may be
read as (§11.8): nothing for `Text`, a type refusal for `Other`, both kinds
for the JSON types.

The field is a request field like `tools`: `response_format` and a consumed
top-level `json_schema` no longer ride in `passthrough`.

## 5. Ingress

| Route | Read from | Into |
| --- | --- | --- |
| `/v1/chat/completions` | `response_format` | `text` → `Text`; `json_object` (+ `schema`) → `JsonObject`; `json_schema` → `JsonSchema` from its `json_schema` object, fields as sent; any other type → `Other` |
| `/v1/chat/completions` | top-level `json_schema` (llama.cpp's own field), when `response_format` is absent, `null` or `{"type": "text"}` (§11.18) | `JsonSchema {name: "response", schema}`; next to a non-empty `grammar`: `400 bad_request` (§11.1) |
| `/v1/responses` | `text.format` | `text` → `Text`; `json_object` → `JsonObject`; `json_schema` → `JsonSchema` from its flat fields (`name` defaults to `"response"` and `strict` to `false`, as before; `description` now kept); any other type → `Other`: a native Responses upstream gets the body as sent, the synthesized loop refuses it (`400 response_format_unsupported`, `param: text.format`) |
| `/v1/messages` | `output_config.format`, else the deprecated top-level `output_format` | `json_schema` → `JsonSchema {name: "response", schema, strict: true}`; any other type → `400 response_format_unsupported` (`invalid_request_error` in Anthropic's body); not an object or no `type` → `400 bad_request` (§11.19) |
| `/v1/completions` | the body | not parsed to the IR; the gate reads the type for §7 and the classifier for §9; a llama.cpp row gets the format as its top-level `json_schema` (§11.22), every other row the body as it came |

A `response_format` that is not an object, or has no `type`, is a `400
bad_request` — there is nothing to translate.

## 6. Egress

| Upstream | `JsonSchema` | `JsonObject` | `Text` | `Other` |
| --- | --- | --- | --- | --- |
| OpenAI-compatible (`openai`) and llama.cpp (`llama_cpp`) | `response_format: {type: json_schema, json_schema: {name?, description?, schema?, strict?}}` — exactly the fields the IR has, every `extra` key back in its place, no `json_schema` object where none was sent (§11.17) | `{type: json_object}` (+ `schema` when the client sent llama.cpp's extension, + `extra`) | `{type: text}` (+ `extra`) | the client's object verbatim |
| Anthropic | `output_config.format: {type: json_schema, schema}`, merged into the `output_config` the effort writes | refused: `response_format_unsupported` (no JSON-object mode) | nothing (the default) | refused: `response_format_unsupported` |
| Gemini | `generationConfig.responseMimeType: application/json` + `responseJsonSchema: schema` | `responseMimeType: application/json` (+ `responseJsonSchema` for llama.cpp's `schema`) | nothing (the default) | refused: `response_format_unsupported` |

On the OpenAI-shaped route nothing changes for a client of that dialect: the
body carries exactly what it sent (the egress corpus's
`structured_output_unmodelled_keys` and `structured_output_bare_json_schema`
cases hold the bytes). The `extra` keys have no place on Anthropic or Gemini
and are not sent there. The refusals in the table are the egress's
backstop (`build_chat` returns them before anything is sent); the gate's
early refusal (§7) answers first wherever the flag already says so.

## 7. Early refusal

`Routed::serves_format(state, kind)` (`gate/structured.rs`), after
`Routed::using` and before admission, so nothing is started for a request
that would be refused:

- `kind` is `ResponseFormat::kind()` (chat dialects) or the type in a legacy
  completions body. No kind, no check.
- The model judged is the one that would answer at that point: the hold's
  fallback when the hold swapped the route, else the name the client sent.
  Its flags are what `GET /v1/models` publishes for it
  (`capabilities::exposed::exposed_entry`), the owner's override merged in,
  read from the cached catalogs only (§11.21). A candidate alias's flags come
  from its cached pick: `json_schema` is the enabled facet, `json_object`
  what every routable candidate says, absent when none is routable (§11.20).
- `Some(false)` for the asked type refuses with `response_format_unsupported`;
  `Some(true)` and absent let the request go.

A candidate alias whose enabled facets lack `structured_output` already
refuses in `using`; that refusal now carries the same code and param when
`structured_output` is the only missing facet (§11.20).

**The escape hatch** is the owner's `capabilities_override` on the row or
alias: a wrong `false` is corrected there, and the request goes.

## 8. Codes and shapes per route

| Route | Status | `type` | `code` | `param` |
| --- | --- | --- | --- | --- |
| `/v1/chat/completions` | 400 | `invalid_request_error` | `response_format_unsupported` / `invalid_json_schema` | `response_format` |
| `/v1/completions` | 400 | `invalid_request_error` | same | the field the client sent: `json_schema` or `response_format` (§11.24) |
| `/v1/responses` | 400 | `invalid_request_error` | same | `text.format` |
| `/v1/messages` | 400 | `invalid_request_error` (Anthropic body: `{type: error, error: {type, message}}`) | — | — |

`message` is lmgw's own sentence for the early refusal and the egress
backstop, and the upstream's own message for a classified upstream refusal.
Every other error's `param` stays `null`. The request log records the code as
`error_kind`; both are client errors in the usage rollup, not "lmgw refused".

## 9. Upstream classification

One `structured.rs` per egress (`egress/openai_wire/structured.rs`,
`egress/anthropic/structured.rs`, `egress/gemini/structured.rs`) holds both
directions: the request mapping of §6 and the classifier
`structured_refusal(status, body) -> Option<GatewayError>`. The chat send
sites call `egress::chat_error`, which asks the classifier first and keeps
its answer only where what the request asked lets it stand (§11.8): a schema
refusal for a JSON type, a type refusal for any type but `text`; the generic
`map_error` otherwise. The reasoning-off retry reads the same mapping, so a
classified refusal is never retried; re-picks and fallbacks never looked at
a 400 (tested: a guest's and an owner's candidate alias with a loaded
alternate and a fallback, `structured_output::candidates`).

### 9.1 OpenAI-compatible and llama.cpp

One classifier for both OpenAI-shaped egresses: a generic row in front of
llama-server gets llama.cpp's texts.

| Upstream says | Code | Fixture |
| --- | --- | --- |
| llama.cpp `400`, message containing `JSON schema conversion failed` (legacy `/v1/completions` and `/completion` with a top-level `json_schema`; the message starts `Field 'json_schema': "json_schema": `) | `invalid_json_schema` | `llama_conversion_failed` (live 2026-10-24, §2.8) |
| llama.cpp `500 server_error`, message starting `JSON schema error at ` (path written `#/properties/word`, root `#`) | `invalid_json_schema` | `llama_schema_error` (live 2026-10-24, §2.8) |
| llama.cpp `400`, message starting `response_format type must be one of` | `response_format_unsupported` | `llama_unknown_type` (source, §2.8) |
| OpenAI `400`, `param: response_format.type` (`text.format.type` on Responses), `code: invalid_value` — `Invalid value: '<type>'. Supported values are: …` | `response_format_unsupported` | `openai_unknown_type` (fix round) |
| OpenAI `400`, message starting `Invalid schema for response_format` — chat (`param: response_format`, `code: null`) and Responses (`param: text.format.schema`, `code: invalid_json_schema`) alike | `invalid_json_schema` | `openai_invalid_schema`, `openai_responses_invalid_schema` |
| An OpenRouter-shaped aggregator relaying that refusal (its own `type`, an object `param`, the message verbatim) | `invalid_json_schema` | `aggregator_openai_invalid_schema` |
| OpenAI `400`, `param: null`, message `Invalid parameter: 'response_format' of type '<type>' is not supported with this model.` — read only next to the field's name, in `param` or quoted in the message | `response_format_unsupported` | `openai_json_schema_unsupported`, `openai_json_object_unsupported` |

Since 2026-10-24 (openai egress design §8, WP3) this table is the classifier of the
`openai_compatible` and `llama_cpp` egresses only. The OpenAI platform's egress (`openai`) reads
the OpenAI rows in OpenAI's own shape alone (`structured::openai_refusal`: a `400
invalid_request_error` with `param` and `code` exact), so the aggregator and llama.cpp rows stay
generic there.

The three llama.cpp fixtures are written from the source, not captured: a
probe of the current llama-server image is still open (it needs the
GPU).

### 9.2 Anthropic

| Upstream says | Code | Fixture |
| --- | --- | --- |
| `400 invalid_request_error`, message starting `output_config.format.schema:` (or the deprecated `output_format.schema:`) | `invalid_json_schema` | `anthropic_invalid_type` (`Invalid JSON Schema in output format: 'nonsense' is not valid under any of the given schemas`), `anthropic_additional_properties_true` (`For 'object' type, 'additionalProperties: true' is not supported. Please set 'additionalProperties' to false`) |

No model without structured output could be provoked: every model the
catalog lists publishes `structured_outputs.supported: true`. The early
refusal reads that flag (§7, §10); no Anthropic text is read as
`response_format_unsupported` yet. `minLength` was accepted (`200`), although
Anthropic's documentation lists string-length constraints as unsupported.

### 9.3 Gemini

| Upstream says | Code | Fixture |
| --- | --- | --- |
| `400 INVALID_ARGUMENT`, message naming the place in the schema: ` at top-level` or ` at properties.<path>` | `invalid_json_schema` | `gemini_unrecognized_type` (`unrecognized type 'nonsense' at top-level`), `gemini_required_not_a_list` (`value at top-level must be a list`), `gemini_unrecognized_type_nested` (`unrecognized type 'nonsense' at properties.a`) |

A failure elsewhere in a schema (under `items`, `anyOf`, …) was not captured
and stays generic. No model without JSON mode could be provoked (Gemma 4 on
AI Studio took `json_object`, `200`, both sizes). Function tools next to a
JSON schema were taken too (fix round): `gemini-3.5-flash-lite` and
`gemma-4-26b-a4b-it` answered `200`; the 2.x models, whose refusal of the
pair is the known one, are retired on AI Studio (`404`). So no text is read
for it; were it refused, the refusal would stay visible (`upstream` with
the provider's message) — the format is never dropped to let the tools
through. The synthesized `/v1/responses` loop on such a model sends both,
like the chat route (`gemma-4-26b-a4b-it`: `200`, the JSON answer).

### 9.4 Unknown

Everything else keeps `upstream` (or whatever the generic mapping makes of
it), message intact: a retired model's `404` (`gemini_model_gone`), and an
aggregator's own `502 Provider returned error` whose provider text ("JSON
Schema mode is not supported with stream mode") exists only inside
`metadata.raw` (`aggregator_provider_error`) — reading that nested text would
be guessing at one provider behind one aggregator.

### 9.5 Probes

Live, 2026-10-22, through a dev copy of the gateway running this build (its
own data dir and container prefix; cloud aliases only, nothing local started):
20 upstream requests, `max_tokens` 16, the prompt "Reply with ok.". Grouped
by what was asked; ×2 marks a request run again because the first run did
not log raw bodies.

| Route, model | Format | Answer |
| --- | --- | --- |
| chat, `openai/gpt-4.1-nano` (×2) | json_schema strict without `additionalProperties: false` | `400` → `invalid_json_schema` |
| chat, `openai/gpt-3.5-turbo` (×2) | a valid json_schema | `400` → `response_format_unsupported` |
| chat, `openai/gpt-4` | json_object | `400` → `response_format_unsupported` |
| `/v1/responses` native, `openai/gpt-4.1-nano` (×2) | text.format as above | `400` → `invalid_json_schema`, `param: text.format` |
| chat, `anthropic/claude-haiku-4-5` | a valid json_schema | `200`, `{"ok":true}`: `output_config.format` works |
| `/v1/messages`, `anthropic/claude-haiku-4-5` | output_config.format with `minLength` | `200` |
| chat, `anthropic/claude-haiku-4-5` | json_object | early `400 response_format_unsupported`; nothing sent (not counted) |
| chat, `anthropic/claude-haiku-4-5` | `{"type": "nonsense"}`; `additionalProperties: true` | `400` → `invalid_json_schema` (2 requests) |
| chat, `aistudio/gemini-2.5-flash-lite` | a valid schema; nonsense | `404`, the model is retired — stays `upstream` (2 requests) |
| chat, `aistudio/gemma-4-26b-a4b-it` | json_object | `200` |
| chat, `aistudio/gemini-3.5-flash-lite` | a valid json_schema | `200`: `responseJsonSchema` works |
| chat, `aistudio/gemini-3.5-flash-lite` | nonsense type; `required` a string; a nested nonsense type | `400` → `invalid_json_schema` (3 requests) |
| chat, `kilo/openai/gpt-4.1-nano` | as the first row | `400` → `invalid_json_schema` |
| chat, `kilo/meta-llama/llama-3.2-1b-instruct` | a valid json_schema | `502` — stays `upstream` (§9.4) |

Fix round, 2026-10-22, the same way (the copy's raw-body log needs
`RUST_LOG=…,so_probe=warn`): 7 upstream requests, `max_tokens` 16.

| Route, model | Format | Answer |
| --- | --- | --- |
| chat, `openai/gpt-4.1-nano` (×2, the first without the raw body) | `{"type": "regex"}` | `400 invalid_value`, `param: response_format.type` → `response_format_unsupported` |
| chat, `aistudio/gemini-3.5-flash-lite` | tools + json_schema | `200` |
| chat, `aistudio/gemini-2.5-flash` | tools + json_schema | `404`, retired — stays `upstream` |
| chat, `aistudio/gemma-4-26b-a4b-it` | tools + json_schema | `200`, `{"ok": true}` |
| chat, `aistudio/gemma-4-31b-it` | json_object | `200` |
| `/v1/responses` synthesized, `aistudio/gemma-4-26b-a4b-it` | tools + text.format json_schema | `200`, `{"ok": true}` |

## 10. Capability flags

| Row | `json_schema` | `json_object` |
| --- | --- | --- |
| Local llama.cpp | `true` | `true` (unchanged) |
| Cloud OpenAI-protocol | from `supported_parameters` (unchanged) | from `supported_parameters` (unchanged) |
| Anthropic | `capabilities.structured_outputs.supported` from `models.list` when published, else absent | `false` — lmgw has nothing to translate it to |
| Gemini | absent (the catalog says nothing) | absent |
| Candidate alias | the enabled `structured_output` facet | `false` when the facet is off; else every routable candidate's `true`, absent when nothing is routable (§11.20) |

## 11. Implementation decisions (for review)

1. **llama.cpp's top-level `json_schema`** is read into the field when
   `response_format` asks for nothing more than text (§11.18), and every
   OpenAI-shaped egress then writes it as `response_format: {type:
   json_schema, json_schema: {name: "response", schema}}`. llama-server and
   ik_llama.cpp read both spellings the same way (§2.8, verified in both
   sources in the fix round, so one rule serves both); an OpenAI-compatible
   cloud now understands it too. With a JSON `response_format` beside it,
   that is the field and the top-level key stays in `passthrough`, so
   llama-server applies its own precedence. *Changed in the fix round:* both
   servers refuse the top-level key next to a `grammar`, a check the
   rewritten spelling no longer meets, so lmgw refuses the pair at ingress
   (`400 bad_request`, llama's own words `Cannot use both json_schema and
   grammar`) — where llama-server answered it as a `500`. A `grammar` next
   to a JSON `response_format` goes as before; that pair is the server's
   business.
2. **llama.cpp's `schema` on `json_object`** is modelled
   (`JsonObject.schema`): forwarded on OpenAI-shaped egress, honoured as
   `responseJsonSchema` on Gemini. It still counts as `json_object` for the
   flags.
3. **An unknown `response_format.type`** on the chat route is `Other`, sent
   verbatim to OpenAI-shaped upstreams (a server may define its own type),
   refused on Anthropic and Gemini. *Changed in the fix round:* the
   upstream's refusal of it is read as `response_format_unsupported` too
   (llama-server's and OpenAI's texts, §9.1, §11.8), so the code is the same
   whichever upstream refuses. On `/v1/responses` it reaches a native
   Responses upstream in the client's body and is refused by the synthesized
   loop, which has no chat spelling to send it as; `/v1/messages` refuses it
   at ingress (Anthropic defines `json_schema` only).
4. **Names and strictness.** Anthropic's format has no name; the IR gets
   `"response"`, the default `/v1/responses` already used, because OpenAI
   requires one. Its strictness is always on (constrained decoding), so the
   IR records `strict: true`, OpenAI's spelling of the same guarantee.
   Anthropic and Gemini have no `name`, `description` or `strict`; those are
   not sent there. Both providers constrain the output in any case, so a
   client that asked for `strict: false` gets a stricter answer, never a
   looser one. The other way round, OpenAI's strict mode also wants every
   property in `required`, which Anthropic does not: a `/v1/messages`
   schema with an optional property sent to OpenAI comes back as OpenAI's
   `invalid_json_schema`, visibly, rather than as an unconstrained answer.
5. **No schema rewriting.** A response schema goes as the client wrote it.
   Gemini gets `responseJsonSchema` (JSON Schema) rather than the
   OpenAPI-subset `responseSchema`, so nothing has to be translated in the
   schema itself; the tool-parameter `sanitize_schema` stays tool-only. A
   keyword a provider rejects is the provider's `invalid_json_schema`, with
   its message.
6. **Early refusal on client routes only.** The public chat routes,
   `/v1/responses` (native and synthesized) and `/v1/completions` run it.
   In-process callers (the Chat, agents, realtime, MCP sampling) do not; they
   get the egress backstop and the classified upstream answer.
7. **Which model is judged early**: the one that answers at resolve — the
   hold's fallback, else the requested name. A fallback swapped in later (the
   outside-VRAM verdict, a ladder climb, a candidate's alias fallback) is not
   judged again; its egress backstop and its upstream decide.
8. **Classification by what was asked** (`FormatAsk`; *changed in the fix
   round*). llama.cpp's converter error also covers tool-parameter schemas,
   so the schema texts are read only for a JSON type; without one such an
   error stays generic. With both tools and a format, a tool-schema failure
   reads as `invalid_json_schema` — the message says which. The "type
   unsupported" texts are read for any type but `text`, an unmodelled one
   included: it was refused as `upstream` on OpenAI-shaped upstreams while
   Anthropic, Gemini and the synthesized Responses loop answered
   `response_format_unsupported`. `/v1/completions` and a native Responses
   upstream read the type from the raw body the same way.
9. **Two variants, `ResponseFormatUnsupported` and `InvalidJsonSchema`**, each
   carrying its message. Not `Upstream`, so the reasoning-off retry
   (`elsewhere`), the candidate re-pick (`repicks`) and every fallback path
   leave them alone by construction.
10. **The upstream message verbatim.** No `upstream error (400):` prefix on a
    classified refusal: the client reads the provider's own sentence; the
    request log records the same message under the code.
11. **llama.cpp's `500 JSON schema error at …`** becomes a `400
    invalid_json_schema`: the schema, not the server, failed.
12. **The deprecated `output_format`** on `/v1/messages` is read too
    (`output_config.format` wins): Anthropic still accepts it, and dropping it
    would be the bug this design removes.
13. **`apply_reasoning` merges into `output_config`** instead of replacing it,
    so an effort tier next to a client's format keeps both — on
    `/v1/messages/count_tokens`'s passthrough body as well.
14. **Agents' steps use the field.** The batch steps that ask for a schema now
    set `response_format` on the IR. On Anthropic and Gemini routes the schema
    is now sent natively (the answer is constrained) where before it was
    dropped and the reply matched loosely; a schema the provider rejects
    fails that step with `invalid_json_schema`.
15. **The synthesized `/v1/responses` loop** answers a unary request whose
    turn met a structured-output refusal with the error envelope (code,
    `param: text.format`), as a native Responses upstream answers the same
    request — not with the failed response object every other failure of the
    loop answered with (*changed 2026-11-05*, api-compat design §9
    finding 10: every failure of a unary run answers the envelope now). A
    streamed run is already answering: it ends with `response.failed`, whose
    `error.code` is the code instead of the loop's `upstream_error`.
16. **What the classifiers read**: a provider text only where the probes or
    the source showed it (§9.1–§9.3), by its stable part — a prefix, the
    field it names, the place in the schema — never a whole sentence and
    never a nested aggregator payload.
17. **Keys lmgw does not model are kept** (fix round). `from_openai` used to
    drop every unknown key of the format and of its `json_schema` object, and
    wrote `"json_schema": {}` where none was sent. Each typed variant now
    keeps them in `extra` (`format_extra` for the outer object of
    `json_schema`), a `null` where a modelled key stands included, and
    `bare` records a missing `json_schema` object; the OpenAI-shaped
    egresses write them all back, so the object an OpenAI or llama.cpp
    client sent arrives as sent. An empty `json_schema: {}` comes back as
    `{}`, a `null` one as `null`.
18. **`{"type": "text"}` beside a top-level `json_schema`** (fix round) is the
    schema: llama-server reads that `response_format` as no format and
    applies the top-level key, so the IR does too — the facet check, the
    early refusal, the classifier and the Anthropic and Gemini translations
    all see the schema. A `null` `response_format` is read the same way; it
    used to hide the schema from all of them while llama-server applied it.
19. **A malformed `output_config.format`** (not an object, or no string
    `type`) is a `400 bad_request`, as a malformed `response_format` is; only
    a well-formed type lmgw does not translate is
    `response_format_unsupported`. The message names the type in plain words
    (it printed Rust's `Some("json_object")` before).
20. **Candidate aliases.** The facet refusal is `response_format_unsupported`
    only when structured output is the one facet missing; with vision or
    tools missing too it stays `unsupported`, since a client stepping down
    its format would be refused all the same. `json_object` is computed once
    in the alias's derivation (every routable candidate's flag) and read by
    both `/v1/models` and the early refusal; with nothing routable it is
    absent, not `false`: such a request meets `candidate_unavailable`, the
    real cause, rather than a format refusal.
21. **Cached catalogs only.** The early refusal reads every catalog through
    `catalog::cached_only`: a stale entry answers, and a catalog not read yet
    is read in the background while this request goes as unknown — it used
    to fetch the provider's model list synchronously after the cache's time
    to live, per JSON request. A candidate alias reads its cached pick
    instead of a fresh derivation per request. On a cold catalog the owner's
    override on a cloud alias is not seen either (it merges onto the
    catalog's entry, as on `/v1/models`); the egress backstop and the
    upstream answer that first request.
22. **llama.cpp's legacy `/v1/completions`** reads only the top-level
    `json_schema` (§2.8), so a `response_format` sent there was dropped
    without a word. For a `llama_cpp` row it is now rewritten into the
    schema the chat route would apply (`json_schema.schema`; `json_object`'s
    own `schema`, else a top-level one, else `{}`; `text` is no format),
    `egress::llama_cpp::legacy_format`. What has no spelling is refused with
    nothing sent: another type (`response_format_unsupported`) and a JSON
    type next to a `grammar` (`bad_request`; the route would apply the
    grammar and ignore the schema). The refusal is a gate stage before
    admission (`Routed::takes_body`), so a local row is not loaded for it;
    a route admission swaps in is judged again where the body is sent. Any
    other row gets the body as it came.
23. **The `grammar` pair, closed (2026-10-22).** The legacy reader applies a
    top-level `json_schema` only where the body has no `grammar` key at all,
    and the chat path throws (500) on a top-level `json_schema` beside a
    non-empty `grammar`. So: (a) the legacy rewrite drops a `grammar` that is
    `null` or `""` from the outgoing body whenever it carries a top-level
    `json_schema`, instead of letting llama ignore the schema; (b) the legacy
    route refuses a top-level `json_schema` beside a set `grammar` with
    `response_format` absent, `null` or `text` too, not only for a JSON type
    (same 400 `bad_request`); (c) the chat route refuses a non-null
    top-level `json_schema` beside a set `grammar` whatever `response_format`
    says, where a JSON type used to reach llama and come back as a 500
    `upstream`. A streamed legacy request takes the same rewrite (test).
24. **The `param` of a schema refusal on `/v1/completions`** (a maintainer
    change, 2026-10-24; not one of the owner's decisions above, which name
    `response_format` for the route). The first pass of that change stamped
    `json_schema` on every `invalid_json_schema` there, llama.cpp's
    spelling of the field. A client that sent `response_format` was then
    told about a field it never wrote, because lmgw had rewritten it (§11.22).
    The `param` now names the field the body carried when it arrived:
    `json_schema` if it held that top-level key, else `response_format`
    (`proxy/legacy.rs`, `sent_format_param`).

## 12. Not yet

- No Anthropic or Gemini text is read as `response_format_unsupported`: none
  could be provoked (§9.2, §9.3; tools beside a JSON schema were taken in
  the fix round). Their early refusal comes from the flags.
- The llama.cpp fixtures come from the source (§9.1); a probe on the current
  llama-server image is still open (GPU).
- A Gemini schema failure outside `top-level` / `properties.` is generic
  until a body for it is captured (§9.3).
- An aggregator's nested provider text (`metadata.raw`) is not read (§9.4).
- `/v1/models` notes say nothing about structured output yet; the flags are
  the whole statement.
- In-process callers do not run the early refusal (§11.6).
- A fallback swapped in after resolve is not judged early (§11.7).
- Gemini rows publish no flags: its catalog has no per-model source.
- `grammar` and other llama.cpp-only keys still ride `passthrough`; an
  Anthropic or Gemini route does not translate them (no equivalent).
- Mid-stream structured-output errors (after a `200`) are not classified;
  only the pre-stream answer is.
- The legacy `/v1/completions` body is forwarded as it came to an OpenAI
  row; only a llama.cpp row's format is rewritten (§11.22). A `generic` row
  in front of llama-server cannot be told apart and gets the body as sent.
- Keys lmgw does not model are not sent to Anthropic or Gemini (no place for
  them), and a Responses `text.format`'s own unknown keys are not kept for
  the synthesized loop (§11.17).
- On a cold catalog a cloud alias's owner override is not seen by the early
  refusal until the background read lands (§11.21).
