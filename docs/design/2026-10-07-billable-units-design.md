# Billable units: pricing on seconds, characters, images and requests next to tokens

Draft for the owner's review, 2026-10-07, and approved in that review; its answers are under
Decisions and in §12. It extends the usage-analytics design
([2026-09-18](2026-09-18-usage-analytics-cost-policy-design.md)), whose §9 left "audio and
embedding pricing" out and whose §2.2 kept a `unit` column on `prices` for exactly this day.
Providers bill on more than tokens: minutes of input audio, characters of input text, generated
images, seconds of video, a fee per request or per search. This design makes those units priceable
the way tokens are: a price row per unit, a measured quantity on the request row, the cost summed in
integer micro-units, and **unknown is NULL, never 0** (§2.3 there) for every unit.

**Today** (lines at `main` e38562d):
- Only `per_mtok` rows are read. `Snapshot::prices_for` filters on `p.unit == "per_mtok"`
  (`config/snapshot.rs:117-124`). The only writers are the catalog sync and `ops::price_set`, both
  with the literal `"per_mtok"` (`catalog.rs:361`, `ops/usage_prices.rs:159`).
- The `prices` CHECK already admits `per_second`, `per_char`, `per_image` and `per_request`
  (`migrations/0026_prices.sql:23-24`), but no code path writes or reads them.
- `price_request` multiplies tokens only (`pricing.rs:162-207`). The rule that protects §2.3 is
  `pricing.rs:163-177`: no sheet, or no token count, leaves the request unpriced.
- Cloud transcription and SSE speech rows record the tokens the provider reports, through
  `UsageTap` (`proxy/audio/usage.rs:57-144`). whisper-1's `{"type":"duration","seconds"}` is
  recognised and deliberately dropped (`usage.rs:180-192`, test at `:200-213`).
- In-process synthesis already counts the characters it sent and the audio it got back, and
  writes both to a log line only, because "characters and audio seconds have no column"
  (`proxy/synthesize.rs:26-31`, `:479`, `:505`, `:522-531`; realtime design §11).
- Still unpriced: binary TTS answers, the native `/v1/responses` passthrough
  (`responses.rs:563-580`), streamed legacy `/v1/completions` (`proxy/legacy.rs:440-460`) and
  the cloud image routes (`Answer::Other` at `proxy/image.rs:257` and `:370`).

## Decisions

*Owner, 2026-10-07:*
1. **General billable units, not a per-minute special case.** Tokens stay one unit among
   several, and the token path keeps its exact current behaviour (§3.2).
2. **Price rows carry their unit.** One scope may hold several rows of different units, for
   example tokens plus a per-request fee. A manual row wins over a catalog row for the same scope
   and unit (§2.3).
3. **Request rows record measured quantities next to tokens**, each only when it was measured
   or reported by the provider. **Never estimated** (§4.1).
4. **Cost is the sum over every unit that has both a price and a quantity.** If any priced
   unit's quantity is unknown, the whole request is unpriced: NULL, never 0, never a partial sum
   (§3.2).
5. **No hidden limits.** Any limit that has to exist surfaces to the owner (§9).
6. **Provider billing quirks are out of scope**: minimum increments, how a provider defines a
   character, free tiers. They are named as an extension point and not built (§9.2).
7. **Old rows are never re-priced.** Rows are priced at write time, as today (§5.5).
8. **Rejected**: switching binary TTS calls to SSE to get tokens, and backfilling old Gemini
   rows (see Rejected).

*Taken in the draft; the review let them stand:*
9. **Five units in v1**: `per_mtok`, `per_audio_minute`, `per_mchar`, `per_image`,
   `per_request`. `per_video_second` and `per_web_search` are named and reserved, but not built,
   because no route can carry a video or a hosted search today (§4.6). A unit nothing can measure
   would turn every request on a scope priced in it into NULL.
10. **Each unit's scale is the one providers quote**, chosen so the arithmetic stays a plain
    product in integer micro-units, as per-1M-tokens is (§3.3).
11. **Non-token rows use one new `price` column.** `price_in`/`price_out` keep their token
    meaning, and a CHECK keeps the two shapes apart. This needs a rebuild of `prices` (§5.4).
12. **Typed quantity columns, not a generic quantity/unit pair** (§5.1).
13. **Each unit resolves on its own**, through today's four-step chain (§2.3).
14. **Provider-reported quantities win over lmgw's own measurement** of the same thing (§4.1).
15. **Request-side quantities count only when the upstream answered 2xx.** The characters of a
    refused TTS request were never billed (§4.1).
16. **Quantities are recorded on local rows too, for statistics.** A local row stays
    `free_local` and costs 0 whatever it processed (§4.7, Q1).
17. **Quantities feed no rate limit.** Spend feeds budgets, as it does today (§7).
18. **No new route, op or header.** The surfaces change through existing ones (§8.6).

*Owner, 2026-10-07 (review):* all five questions of §12 confirmed as recommended.
19. **A local row costs a real 0** from the migration on, tokens or not (§3.2 rule 1; Q1).
20. **A fallback row is priced under the alias that answered**, with that alias's upstream and
    upstream model. `requested_alias` stays the name the client asked for. It lands as its own
    commit before WP2 (§2.3, §11; Q2).
21. **One rate per scope in v1** (§9.2; Q3).
22. **No duration measurement for non-WAV uploads in v1** (§4.2; Q4).
23. **Catalog `image` and `web_search` are not synced in v1** and are counted in `not_synced`
    (§6; Q5).
24. **The "Found while reading" items are fixed now**, each where that section says.

## 1. Inventory

### 1.1 What providers bill on

| Unit | Examples | Source |
|---|---|---|
| Minutes of input audio | whisper-1 $0.006/min; gpt-realtime-whisper and gpt-live-transcribe $0.017/min | https://developers.openai.com/api/docs/pricing |
| Tokens, for transcription | gpt-4o-transcribe $2.50/$10 and gpt-4o-mini-transcribe $1.25/$5 per 1M tokens (the per-minute figures shown beside them are estimates) | same page |
| Characters of input text | tts-1 $15 and tts-1-hd $30 per 1M characters | same page |
| Tokens, for speech and images | gpt-4o-mini-tts (text in, audio out); the gpt-image models, with separate text and image input rates | same page |
| Seconds of output video | sora-2 $0.10/s, sora-2-pro $0.30–0.70/s by resolution; Veo 3.1 $0.40/s (720p/1080p), $0.60/s (4k) | https://developers.openai.com/api/docs/models/sora-2, https://ai.google.dev/gemini-api/docs/pricing |
| Per clip or song (per request) | Lyria 3 Clip $0.04, Lyria 3 Pro and 3.5 $0.08 per song | Gemini pricing page |
| Web search calls | OpenAI $10/1k calls plus content tokens; Google Search grounding $14/1k after a free monthly allowance | both pricing pages |
| Per request (catalog) | OpenRouter's `pricing.request`, "fixed cost per API request" | https://openrouter.ai/docs/overview/models |

The same OpenRouter page lists `pricing.image` as "cost per image **input**". That is a different
quantity from a generated image, and §6 keeps the two apart.

A whisper-1 transcription reports `usage: {"type": "duration", "seconds": number}`; the
token-billed models report `{"type": "tokens", …}`
(https://developers.openai.com/api/reference/resources/audio/subresources/transcriptions/methods/create).
`seconds` is a JSON number, so it may carry a fraction.

### 1.2 Where lmgw sees each request

| Path | Row written by | Counted today | What this design adds |
|---|---|---|---|
| `POST /v1/audio/transcriptions`, `/details`, `/alignments` | `finish_media` (`proxy/audio.rs:152`), `UsageTap` | reported tokens, cloud only | reported seconds; the upload's duration when it is a WAV |
| In-process transcription: Chat attachments, dictation, realtime turns and word checks | `transcribe_labelled` (`proxy/transcribe.rs:222`, `:314-335`) | reported tokens, cloud only | the same |
| `POST /v1/audio/speech` | `finish_media`, through `speech_call` (`proxy/audio/speech.rs:75-179`) | SSE tokens, cloud only | characters as sent |
| In-process synthesis: realtime responses and Chat read-aloud | `Synthesis::finish` (`proxy/synthesize.rs:518-550`) | nothing (binary WAV) | characters as sent, answered clauses |
| `POST /v1/images/generations`, `/edits` | `finish_media` via `finish_image` (`proxy/image.rs:384-391`) | nothing | images in the answer |
| `/v1/tasks/run`, `/v1/tasks/stream` | `finish_media` (`proxy/audio.rs:412-454`) | nothing | answered request |
| Chat, embeddings, rerank, legacy, `/v1/responses` turns | `record` (`proxy/recording.rs:205`), `record_in_process` (`proxy/in_process.rs:70`), `record_free_form` (`recording.rs:1012`) | tokens | answered request |
| Video | none: no `/v1/videos*` route exists (`server.rs:197-225`) | | nothing (§4.6) |
| Hosted web search | none: refused at ingress (`ingress/responses.rs:340-346`, `ingress/anthropic.rs:76-84`) | | nothing (§4.6) |

## 2. Units and prices

### 2.1 The unit set

| `unit` | What is counted | Price is per | Row quantity | v1 |
|---|---|---|---|---|
| `per_mtok` | tokens, with four rates: in, out, cache read, cache write | 1M tokens | `prompt_tokens` and friends | yes, unchanged |
| `per_audio_minute` | duration of **input** audio | 1 minute | `audio_in_ms` | yes |
| `per_mchar` | characters of **input** text | 1M characters | `chars_in` | yes |
| `per_image` | **generated** images | 1 image | `images_out` | yes |
| `per_request` | upstream requests that were answered | 1 request | none (§4.5) | yes |
| `per_video_second` | seconds of generated video | 1 second | `video_out_ms`, later | reserved |
| `per_web_search` | web search calls | 1 call | `web_search_calls`, later | reserved |

The two old placeholders `per_second` and `per_char` are dropped: they named a scale (per second,
per character) that §3.3 replaces, and no lmgw code ever wrote them. `per_image` and
`per_request` keep their names. `per_image` means generated images only. If input images are
ever priced, that unit gets its own name (`per_image_input`), so OpenRouter's `pricing.image`
can never land in `per_image` by accident.

A unit names what is counted and its scale, not the provider's word for it. An owner who meets
"$0.37 per hour" enters `0.0061667` per minute, and the note field says where it came from.

### 2.2 The price row

`prices` keeps its shape and gains one column:

| Column | `per_mtok` row | Any other unit |
|---|---|---|
| `price_in`, `price_out`, `price_cache_read`, `price_cache_write` | as today | NULL |
| `price` (new, REAL) | NULL | the rate, per the unit's scale |

A CHECK enforces the split (§5.4). Usability stays part of *selecting* a row, as in
`snapshot.rs:126-143`. A token row needs `price_in` or `price_out`, as `Prices::is_usable` says
(`pricing.rs:84-86`). Any other row needs `price`. A row that fails this is treated as no row, so a
cleared manual row never shadows a catalog one.

The unique index `(scope_kind, scope_key, source, unit)` already allows one row per unit per
source (`0026_prices.sql:38-40`). Nothing changes there.

### 2.3 Resolution

`Snapshot::prices_for` returns one token sheet today. It becomes `Snapshot::sheet_for(alias,
upstream_id, upstream_model) -> Sheet`, and each unit resolves independently through today's
chain (`snapshot.rs:145-161`):

1. alias, manual;
2. alias, catalog;
3. upstream model, manual;
4. upstream model, catalog.

The alias is the one that answered. A row a fallback answered resolves under the fallback alias,
with that alias's upstream and model (`Route::priced_alias`, Q2), and keeps the requested name in
`requested_alias`.

A local upstream still short-circuits first (`snapshot.rs:107-109`): `Sheet::local()` and no
rows. `prices_for` stays as `sheet_for(..).tokens`, for the one caller that wants tokens alone:
the §2.5 counterfactual (`web/api_usage.rs:849`).

Per unit rather than per scope, on purpose. Suppose a manual alias row overrides the catalog's
token prices. If resolving the alias's manual row made that scope the only one consulted, a catalog
per-request fee for the same model would silently stop applying. That error runs downward. To drop
a fee, the owner sets a manual `0` for that unit, which is an explicit statement and visible in the
editor.

## 3. Arithmetic

### 3.1 Types (`pricing.rs`)

```rust
/// What a request processed besides tokens. `None` = not measured and not reported.
pub struct Quantities {
    pub audio_in_ms: Option<u64>,
    pub chars_in: Option<u64>,
    pub images_out: Option<u64>,
    /// Upstream requests answered for this row: 1 for most rows, the answered clause
    /// count for a synthesis row (§4.5).
    pub requests: Option<u64>,
}

pub struct UnitRate { pub price: f64, pub source: PriceSource }

/// Everything that prices one scope, each unit resolved on its own (§2.3).
pub struct Sheet {
    pub local: bool,
    pub tokens: Option<Prices>,          // today's sheet, unchanged
    pub audio_minute: Option<UnitRate>,
    pub mchar: Option<UnitRate>,
    pub image: Option<UnitRate>,
    pub request: Option<UnitRate>,
}

pub fn price_request(tokens: &TokenUsage, q: &Quantities, sheet: &Sheet) -> Cost;
```

`Cost` keeps `total_micro`, `in_micro`, `out_micro`, `source` and `used: Prices`. It gains
`units_micro: Option<i64>` (the non-token part) and `used_units: UnitRates` (the four non-token
rates as used, each `Option<f64>`). `TokenUsage` and `Prices` do not change.

`PriceUnit` is an enum in `lmgw-api-types` with `as_str`, `parse`, a label and a scale text, so
core, the dashboard and the docs share one table. `PriceRow.unit` becomes `PriceUnit` and gains
`price: Option<f64>` (`config/prices.rs:7-20`).

### 3.2 The rule

1. **Local.** `sheet.local` → `total_micro = Some(0)` and source `free_local`, whatever was
   measured. Today a local row with no token count comes out `unknown`; Q1 makes it 0.
2. **No usable row in any unit** → `Cost::unknown()`, as `pricing.rs:163-165` does today.
3. **Tokens**, when `sheet.tokens` is usable, are priced by today's code, unchanged:
   - with neither `prompt` nor `completion` reported, the token part is *unknown*
     (`pricing.rs:171-177`);
   - otherwise in and out, with the cache-rate fallback (`:179-192`).
4. **Each other unit with a rate** gives `quantity × rate × scale`, rounded to integer micro on
   its own (§3.3), or *unknown* when its quantity is `None`.
5. **Any part unknown** → `total_micro`, `in_micro`, `out_micro` and `units_micro` are all
   `None`, and the source is `unknown`. `used` and `used_units` still snapshot the rates that would
   have applied, as `pricing.rs:172-176` already does for tokens.
6. **Otherwise** `total_micro = in + out + units`, all integers. `in_micro` and `out_micro` are
   NULL when the sheet has no token row. `units_micro` is NULL when it has no other row.

**Source.** `price_source` is `manual` if any row used was manual, otherwise `catalog`. For a
token-only sheet that is `p.source`, as today. Per-unit provenance lives in the price table, and
the row keeps the rates it used.

**The token path is untouched.** A sheet with only a token row gives a `Cost` equal, field for
field, to today's, whatever quantities the row carries. Two things guard that: §10's oracle test,
and keeping today's function as the token part rather than rewriting it.

### 3.3 Scale and rounding

Money stays integer micro-units of the configured currency. A per-1M-tokens price works because
`tokens × price_per_mtok` *is* the micro amount: 1M tokens at 3.00 is 3,000,000 micro. No scale
factor, no tiny decimals typed into a form. The other units follow the same idea: the owner types
the number the provider's sheet shows, and the product needs at most one constant.

| Unit | `cost_micro` of the part |
|---|---|
| `per_mtok` | today's formula (`pricing.rs:186-192`) |
| `per_mchar` | `round(chars_in × price)`: the same identity as tokens |
| `per_audio_minute` | `round(audio_in_ms × price × 1_000_000 / 60_000)` |
| `per_image` | `round(images_out × price × 1_000_000)` |
| `per_request` | `round(requests × price × 1_000_000)` |

Each part is rounded once and the parts are summed as integers, as `in_micro` and `out_micro` are
today (`pricing.rs:191-195`). Rates stay REAL, as the provider quotes them (`0.006`, `15`,
`0.04`). The rejected alternative was to store per character or per second and scale for display.
That puts `1.5e-5` into a form and brings back the float noise that the per-1M convention exists
to avoid.

Goldens (§10):

| Case | Arithmetic | Micro |
|---|---|---|
| whisper-1, 27 s reported, $0.006/min | 27 000 × 0.006 × 10⁶ / 60 000 | 2 700 |
| whisper-1, 27.4 s reported | 27 400 ms → | 2 740 |
| tts-1, 1 234 characters, $15/1M | 1 234 × 15 | 18 510 |
| tts-1-hd, same text, $30/1M | | 37 020 |
| 2 images at $0.04 | | 80 000 |
| 1M in + 100k out at 3/15, plus $0.005 per request | 3 000 000 + 1 500 000 + 5 000 | 4 505 000 |
| the same request, not answered (`requests = None`) | | NULL |

### 3.4 What the row snapshots

`request_logs` gains `cost_units_micro` and one rate column per non-token unit (§5.2), written
from `Cost.units_micro` and `Cost.used_units`. "Why did this transcription cost 0.0027" is then
answerable from the row alone: 27 000 ms at 0.006 per minute. That holds after the price changes,
and it is §2.2's rule: *the row snapshots the numbers, not a foreign key*.

## 4. Measuring quantities, per route

### 4.1 Rules

- **Measured or reported, never estimated.** lmgw counts what it holds: the characters it sends,
  the samples in a WAV, the images in an answer. Or it reads what the provider states. A duration
  is never derived from a byte count and a bitrate, a character count is never derived from a
  token count, and an image count never comes from the request's `n`.

  The chat path's stopped-call estimate of prompt tokens (`proxy/stop.rs:111-125`) is token
  behaviour and stays as it is. No unit gets an equivalent.
- **Request-side quantities count only on a 2xx answer.** `audio_send` and `image_send` return a
  response only for a 2xx (`proxy/audio.rs:89-95`, `proxy/image.rs:201-204`), and
  `MediaOutcome` exists only then. A quantity measured on the request rides in it, so a refused
  request never carries one. A relay that broke after its 2xx headers keeps its quantities: the
  provider accepted the work.
- **Reported wins over measured.** When whisper-1 says 27.4 s and the WAV holds 27 391 ms, the
  row records 27 400. The provider bills on its own figure, and the row explains a bill.
  `seconds` converts to milliseconds by rounding, as the realtime facts already do
  (`realtime/transcribe.rs:331`).
- **Unknown when in doubt.** If a stop leaves it unknown whether the provider accepted a
  request, that quantity is `None`, and on a scope priced in it the row is NULL (§4.3).

### 4.2 Transcription

- **Reported seconds.** `UsageTap` and `of_answer` return a `Reported { usage, audio_in_ms,
  images_out }` instead of a bare `Usage`. Next to `tokens()` (`usage.rs:180-192`), a
  `duration()` reads `{"type":"duration","seconds"}`.

  Token reading stays cloud-only, as e38562d decided (`usage.rs:81-83`, `:149-151`). The whisper
  test at `usage.rs:200-213` changes from "dropped" to "read as `audio_in_ms`".
- **Measured duration.** `multipart_send` (`proxy/audio.rs:768`) reads the `file` field's bytes
  with `wav_duration_ms` (`realtime/audio/pcm.rs:256-265`). For a PCM16 or float32 WAV it gives
  the length from the header and data size, decoding nothing. Anything else gives `None`. The
  result goes into the new `MediaOutcome.measured`.
- **Merge.** `finish_media` (`proxy/audio.rs:224-279`) and `transcribe_labelled`
  (`proxy/transcribe.rs:314-335`) merge reported over measured and hand the result to the row.
- **Realtime** needs no code of its own. A turn and a word check upload the 16 kHz WAV that lmgw
  built (`realtime/transcribe.rs:474`, `:489`) through `transcribe_turn`, so every ASR row
  carries its exact duration. Chat dictation and attachments take the same path. A WebM, Ogg or
  MP3 recording sent to a provider that reports nothing stays `None` (Q4).
- The JSON variant (`{"audio": "<path>"}`) names a file on the container, which lmgw never
  holds. It is local only, and its duration is whatever the answer reports.

### 4.3 Speech

- **HTTP.** `speech_call` counts `out["input"]` after `shape_on` (`speech.rs:133`) as
  `chars().count()` and puts it in `MediaOutcome.measured`. It counts what the TTS reads, not
  what the client sent: shaping strips tags for a route that does not take them. That matches
  the count `Synthesis::speak` already makes (`synthesize.rs:479`).
- **In-process** (`Synthesis`). Today `speak` adds a clause's characters once its whole WAV is in
  hand (`synthesize.rs:505`). Instead, it counts when the clause's answer is a 2xx, after the
  `send` arm of the select at `:482-486`, together with `requests += 1`.

  If a stop interrupts a clause that was sent and has no answer yet, the response's quantities
  become `None`: the provider may or may not be working on it. `finish` passes the two
  quantities. The log line (`:522-531`) stays, and the doc comment at `:26-31`, which says these
  have no column, is rewritten.
- **What this prices.** tts-1 and tts-1-hd, which bill per character, are now priced on both
  paths, binary WAV included. A binary answer from a token-billed model such as gpt-4o-mini-tts
  records its characters but still has no tokens, so its row stays NULL (§4.8).

### 4.4 Images

`Answer::Image` replaces `Answer::Other` on both image routes (`proxy/image.rs:257`, `:370`).
The tap counts images in the answer:
- **A JSON body** goes through a small streaming scanner. It tracks depth and string/escape state
  and the key at depth 1, and counts the elements of the top-level `data` array. Memory is O(1),
  so a 20 MB base64 answer is never buffered (the reason the tap reads no JSON on these routes
  today, `usage.rs:17-22`). An empty `data` is a measured 0. A body with no top-level `data`, or
  one that ends mid-document, gives `None`.
- **An event stream** (`stream: true`) counts the events that each carry one final image.
  `image_generation.partial_image` events never count. WP4 takes the final events' names from
  OpenAI's API reference and pins them in a test. A stream in which no final event is recognised
  leaves `images_out` at `None`, not 0.

The count is read on local sd-server answers too (§4.7). A gpt-image answer's `usage` is not
read: its input has separate text and image rates, and `per_mtok` has one input rate (§4.8).

### 4.5 Requests

`per_request` needs no measurement of its own, only a reliable "the upstream answered". The three
row writers (`record`, `record_in_process`, `record_free_form`) fill `requests` with `Some(1)` when
the caller left it `None` and the row has a route, a 2xx status, and an `error_kind` other than
`canceled`. Otherwise it stays `None`.

`canceled` is excluded because a stop before the answer also writes a 200 row
(`proxy/stop.rs:103-109`). Callers that know better say so:
- `finish_media`: `Some(1)`, because its 2xx headers arrived, even if the client went away later;
- `Synthesis::finish`: the answered clause count, because a response's one row is several
  upstream requests (realtime design §11);
- a chat stream that was stopped after its first upstream event: `Some(1)`. The WP2 audit names
  each relay that knows this.

No column stores the count. The fee is in `cost_units_micro`, and the rate in
`price_per_request` (§5.2).

### 4.6 Video and web search: no path today

No `/v1/videos*` route exists. sd.cpp's `--video-frames` goes through `/v1/images/generations`
on a local row (`capabilities/task.rs:75`), which is free. Hosted tools are refused before any
route is chosen: on `/v1/responses` by `parse_request` (`ingress/responses.rs:340-346`), which also
runs ahead of the native passthrough (`responses.rs:87-90`), and on `/v1/messages`
(`ingress/anthropic.rs:76-84`).

The two units therefore stay reserved. Each one arrives later as one enum variant, one column and
one hook, together with the route that can carry it.

### 4.7 Local rows

A local route resolves `free_local` before any price row is read (`snapshot.rs:107-109`), and it
keeps doing so. Quantities are recorded anyway, because the Usage page's "minutes transcribed"
should include the owner's local ASR, and a quantity column means "what this request processed",
not "what someone billed".

lmgw's own measurements (characters, WAV duration, image count) apply to every row. What the
provider reports is read on cloud routes only, as today.

### 4.8 What stays token-only (follow-ups)

| Gap | Why units do not close it | Follow-up |
|---|---|---|
| Binary speech from a token-billed model (gpt-4o-mini-tts) | characters are recorded, but its price is per token | none planned. The SSE switch is rejected |
| Native `/v1/responses` passthrough | tokens NULL (`responses.rs:563-580`) | read `usage` from the relayed body the way `UsageTap` does. Also where web-search calls would be counted |
| Streamed legacy `/v1/completions` | tokens NULL (`proxy/legacy.rs:440-460`) | read the final chunk's usage |
| gpt-image token usage | text and image input bill at different rates | a token-sheet extension, not a unit |

## 5. Schema

The migration is **0070**, one file: `0070_billable_units.sql`. 0069 is taken by
`0069_agent_created_by.sql`, which landed from another branch in parallel.

### 5.1 Typed columns, not a quantity/unit pair

The alternative was a child table `request_quantities(request_id, unit, quantity, price,
cost_micro)`, plus an hourly twin, or a JSON column. It loses on every path this feature has:
- **The rollup.** `usage_hourly` is one additive `col = col + excluded.col` upsert in the same
  transaction as the row (§3.2 there; `store/request_logs.rs:314-405`). A child rollup would add
  a second upsert per row per unit, which is the latency histogram's shape
  (`0028_usage_rollup.sql:64-89`). The histogram needs that shape because its bucket index varies.
  Units do not.
- **Rebuild and queries.** `rebuild_usage` (`store/usage.rs:349-491`) and every `/api/usage/*`
  query stay flat `SUM`s. The CSV stays one row per rollup cell.
- **The set is closed.** Every unit needs its own measurement hook (§4), so adding one is a code
  change anyway, and one `ALTER TABLE` beside it costs nothing. A generic pair would accept
  quantities nothing can price.
- **A row stays self-describing**, like the token and timing columns of 0027 and 0030.

### 5.2 `request_logs`

All nullable; NULL means not measured, never zero:

```sql
ALTER TABLE request_logs ADD COLUMN audio_in_ms            INTEGER;
ALTER TABLE request_logs ADD COLUMN chars_in               INTEGER;
ALTER TABLE request_logs ADD COLUMN images_out             INTEGER;
ALTER TABLE request_logs ADD COLUMN cost_units_micro       INTEGER; -- the non-token part (§3.2)
-- The rates as used (§3.4), each named `price_` + its unit:
ALTER TABLE request_logs ADD COLUMN price_per_audio_minute REAL;
ALTER TABLE request_logs ADD COLUMN price_per_mchar        REAL;
ALTER TABLE request_logs ADD COLUMN price_per_image        REAL;
ALTER TABLE request_logs ADD COLUMN price_per_request      REAL;
```

These reach `NewRequestLog` and `RequestLogRow` (`store/request_logs.rs:113-162`, `:11-60`).
The quantities, but not the rates, also reach the `request` SSE frame (`RequestSummary`,
`telemetry.rs:16`) and `dto::RequestRow` (`lmgw-api-types/src/status.rs:469`), as the cache
split did (§5 rev there).

### 5.3 `usage_hourly` and the remainder

```sql
ALTER TABLE usage_hourly ADD COLUMN audio_in_ms              INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_hourly ADD COLUMN chars_in                 INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_hourly ADD COLUMN images_out               INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_hourly ADD COLUMN cost_unknown_audio_in_ms INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_hourly ADD COLUMN cost_unknown_chars_in    INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_hourly ADD COLUMN cost_unknown_images_out  INTEGER NOT NULL DEFAULT 0;
```

- A quantity adds its measured value, and an unmeasured one adds 0. That is how `tokens_in` sums
  reported tokens (`request_logs.rs:336-337`). So a quantity total means "measured", and the page
  says so (§8.3).
- The remainder (§2.3 there) carries the quantities of every row it counts, beside
  `cost_unknown_tokens`. The page can then say "3 unpriced requests (4.5 min audio)" instead of
  "(0 tokens)".
- The remainder predicate (`request_logs.rs:332-335`) counts "spent" as tokens **or any
  quantity** > 0. The rebuild's CASE (`store/usage.rs:394-402`) gets the same change, so a
  rebuild reproduces the write path (§8b there).
- `usage_latency_hourly` does not change.

### 5.4 `prices`: the rebuild and its guard

SQLite cannot alter a CHECK in place, so `prices` is rebuilt. It has no foreign keys and nothing
references it, so the file runs in sqlx's normal transaction (unlike 0058's `-- no-transaction`).

```sql
CREATE TABLE prices_new (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    scope_kind  TEXT NOT NULL CHECK (scope_kind IN ('alias','upstream_model')),
    scope_key   TEXT NOT NULL,
    unit        TEXT NOT NULL DEFAULT 'per_mtok'
                CHECK (unit IN ('per_mtok','per_audio_minute','per_mchar','per_image','per_request')),
    price_in REAL, price_out REAL, price_cache_read REAL, price_cache_write REAL,
    price       REAL,
    source      TEXT NOT NULL DEFAULT 'manual' CHECK (source IN ('catalog','manual')),
    note        TEXT,
    updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
    CHECK (CASE WHEN unit = 'per_mtok' THEN price IS NULL
                ELSE price_in IS NULL AND price_out IS NULL
                     AND price_cache_read IS NULL AND price_cache_write IS NULL END)
);
-- copy every row with its id and updated_at; carry sqlite_sequence as 0058:73-88 does;
-- DROP, RENAME, recreate idx_prices_scope.
```

- Every existing row is `per_mtok`, so it copies unchanged and satisfies both CHECKs. The default
  stays `per_mtok`, so a raw insert without a unit, such as the test at
  `tests/it/realtime_response.rs:629-635`, keeps working.
- **The guard.** `store/migration_guards/billable_units.rs` refuses the upgrade, naming the rows,
  if any row has `unit IN ('per_second','per_char')`. It is registered like 0058's notice
  (`store/migration_guards.rs:11-12`). Those rows cannot be mapped: they used a different scale
  for an undefined quantity. Only hand-written SQL could have produced one, and like 0023's guard
  (`migration_guards.rs:30-35`) this one exists for honesty about what the migration can lose. It
  is not expected to trip.

### 5.5 History, backfill and rebuild

- **Old rows are never re-priced** (decision 7). The new columns are NULL on old rows. History
  keeps the cost it was written with, including the local audio and image rows that §3.2's
  local rule (Q1) prices at 0 from the migration on.
- **Quantities start at the migration.** Raw rows from before it have none, so nothing can be
  backfilled. The rollup's DEFAULT 0 there means "not recorded". `/api/usage/series` returns
  `units_since`, read from this migration's `installed_on` in `_sqlx_migrations`, a row sqlx
  already writes. The page labels earlier buckets as not recorded instead of drawing zeros.
- **`rebuild_usage`** sums the new raw columns and applies the extended predicate. It runs only
  on an empty rollup (`backfill_usage_if_empty`, `store/usage.rs:497-515`), so it never meets
  pre-migration rollups.

## 6. Catalog sync

The Kilo catalog, which follows the OpenRouter shape, advertises more than `prompt` and
`completion`. Its own fixture carries `input_cache_read`, `input_cache_write` and `web_search:
"0.01"` (`tests/fixtures/catalogs/kilo_anthropic_variants.json:24-31`). OpenRouter documents
`request`, `image`, `web_search`, `internal_reasoning`, the two cache fields and conditional
`overrides`. `Pricing` keeps only the first two (`catalog.rs:97-105`, `:564-569`).

| Field | Synced into | Rule |
|---|---|---|
| `prompt`, `completion` | `per_mtok` | unchanged (`catalog.rs:238-253`) |
| `request` | `per_request` | only for a model whose token row is usable. A non-zero value writes or updates a row. `"0"` writes no new row, but updates an existing catalog row of that scope to 0, so a fee the provider dropped does not linger |
| `image` | nothing | per **input** image, not `per_image` (§2.1; Q5) |
| `web_search` | nothing | `per_web_search` is reserved (§4.6) |
| `input_cache_read`, `input_cache_write` | the `per_mtok` row's cache rates | synced ahead of WP3 (Found while reading) |
| `internal_reasoning`, `overrides` | nothing | token-sheet changes, not units |

**Tied to the token row.** Suppose `prompt` fails to parse and the model writes nothing today.
Writing its `request` fee alone would turn an *unpriced* model, which is visible on the worklist,
into one priced at the fee alone: a silent number that is wrong downward. A request-only sheet is
legitimate (a per-clip music model), but only as a manual row.

**Nothing skipped is silent.** `UpstreamPriceSync` (`catalog.rs:300-311`) gains `not_synced:
{field: models}`, for example `{"web_search": 37, "image": 12}`. `lmgw__prices_sync` and the
Prices card show it beside "N models advertise no price". The skipped fields are a limit of this
design, so they surface rather than vanish (decision 5).

`catalog_prices` keeps its finite, non-negative rule (`catalog.rs:239-244`) for every field.

## 7. Policy, budgets and run meters

- **Tokens per minute** stays tokens (`proxy/recording.rs:316-321`). Requests per minute and
  concurrency count rows and calls, whatever the unit.
- **No quantity limits.** A "minutes per hour" or "characters per minute" cap would be a new
  policy surface nobody asked for. Budgets already bound the money, and `rpm_limit` bounds the
  call rate.
- **Budgets** see unit costs with no change. `note_spend` takes `total_micro`
  (`recording.rs:336-348`), and the reseed reads `usage_hourly.cost_micro`
  (`store/usage.rs:676-690`). Audio rows that were unpriced, which "audio budgets bite on only
  through per-call counts" (realtime design §11), now count toward a budget once priced.
- **Agent run meters.** HTTP runs sum each row's cost (`agents/mod.rs`, `RunMeters`), so unit
  costs arrive by themselves. The in-process batch meter sums its calls' row costs the same way
  (`agents/batch/meter.rs`, `metered.rs`): each row is priced on the route that answered it, so a
  turn the gate re-routed (a ladder's fallback, a candidate alias's next pick) is never priced on
  the route the run started on. Both decide what an unpriced row does by the rollup's own
  remainder rule (`NewRequestLog::row_cost`, `pricing::CostTotal`): a NULL row that did work
  (answered, stopped, or spent tokens or a quantity) makes the run's total NULL (Found while
  reading), and one that did none adds nothing. Quantities in run totals come later.

## 8. Surfaces

### 8.1 The price editor (`lmgw-ui/src/pages/usage/prices.rs`)

- **A unit selector on a new row.** It defaults to the model's task where one is known: `asr` →
  `per_audio_minute`, `tts` → `per_mchar`, `image_generation`/`image_edit` → `per_image`, and
  everything else `per_mtok` (task names: `capabilities/task.rs:47-84`). The "Price…" button on an
  unpriced line opens with the same default.
- **Fixed when editing.** The upsert key includes the unit (`store/prices.rs:30-41`), so changing
  an existing row's unit would leave the old row in place. The editor posts `id` already, and the
  op ignores it.
- **One box for a non-token unit**, labelled with its scale ("per minute of input audio", in the
  currency). The four token boxes stay for `per_mtok`. The note under them (`prices.rs:410-414`)
  becomes per-unit, and `price_field`'s validation (`:433-449`) is unchanged.
- **Every row shows its unit.** The band head's "mixed units" (`prices.rs:631-645`) goes, so
  several rows of one scope read as parts that add up. The editor already posts `"unit":
  "per_mtok"` (`prices.rs:184`); the op now reads it.

### 8.2 MCP tools and the op

- **`lmgw__price_set`** (`mcp/selfadmin/catalog/prices.rs:22-63`, dispatched at
  `mcp/selfadmin.rs:572-585`):
  - gains `unit` (enum, default `per_mtok`, so existing callers are unchanged) and `price`;
  - for `per_mtok` it requires `price_in` or `price_out` and refuses `price`; for any other unit
    it requires `price` and refuses the four token fields. Each error names the unit's scale;
  - its description stops saying "Prices are per 1M tokens".
- **The `price_set` op** (`web/api.rs:894-909`) reads the same two fields, and `ops::price_set`
  (`ops/usage_prices.rs:121-169`) validates once for both planes.
- **`lmgw__prices`** (`ops/usage_prices.rs:88-114`) returns `price` beside `unit` on each sheet
  and rewrites its hint.

  `unpriced_models` keeps its meaning, extended over units: a model is listed when *no* unit
  resolves a usable row (§2.3), and a local model never is (`:36-65`). A scope that has rows
  whose quantity its route never measures (a token row on a binary-TTS alias) still writes NULL
  rows. Those surface in the per-alias remainder of `lmgw__usage` and the Usage tables, not on
  this list, because "priced but unmeasured" is a fact about traffic, not about the sheet.
- **`lmgw__prices_sync`** reports `not_synced` (§6) and its description names `per_request`.

### 8.3 The Usage page

- **Spend stays one number.** Every unit's cost is money in the one configured currency (§9
  there), so the spend tile, spend chart and budget line sum it with nothing new. A unit explains
  money and never converts into another unit: no "equivalent tokens".
- **Quantity tiles.** "Audio transcribed" (minutes), "Text spoken" (characters) and "Images
  generated" join the tile row (`pages/usage/tile_row.rs`). Each appears when its sum in the window
  or the previous window is non-zero; a chat-only gateway shows none. Each is labelled
  *measured*, and *since <date>* when the window starts before `units_since` (§5.5).
- **The remainder line** names the quantities it holds. `money_with_unpriced`
  (`ops/usage_prices.rs:184-198`) and its UI twin append only the non-zero ones: "… · 3 unpriced
  requests (0 tokens, 4.5 min audio)".
- **The "where it went" table** (§6.1 item 6 there) gains a *measured* column with each series'
  non-zero quantities. The share bars stay cost and tokens.
- The wire cell (`lmgw-api-types/src/usage.rs:13-54`) gains the six rollup fields. The rule from
  §8b there applies: a column is carried end to end or not at all. That means `cell_from_row`
  (`store/usage.rs:123`), the series and top queries, `cell_to_dto`, `merge_into` and
  `fold_tail_to_other` (`web/api_usage.rs:331`, `:360`, `:405`), and `fold_other`
  (`ops/usage_prices.rs:231-266`).

### 8.4 Traffic and the live frame

A row shows its measured quantities, and "—" where none was measured, never 0. The detail shows
each priced part as quantity × rate from the snapshot columns (§3.4). `pages/traffic.rs` reads the
three new `RequestRow` fields.

### 8.5 CSV export and `lmgw__usage`

- `CSV_HEADER` (`web/api_usage.rs:1069-1074`) gains the six rollup columns **at the end**, so a
  spreadsheet that reads columns by position keeps working.
- `usage_cell_view` (`ops/usage_prices.rs:203-225`) gains `audio_in_ms`, `chars_in`,
  `images_out` and their `unpriced_*` counterparts, and its `cost` string uses the extended
  remainder.

### 8.6 The API reference

The repo rule, that a new route, op or `x-lmgw-*` header ships with its doc entry, is enforced by
the drift tests:
- `every_capability_row_is_documented_or_excluded` and its converse
  (`tests/it/openapi_coverage.rs:97`, `:124`);
- `op_names_match_the_dispatcher_arms` and `every_listed_op_is_documented_and_vice_versa`
  (`tests/it/openapi_ops.rs:211`, `:350`);
- `every_x_lmgw_literal_is_in_the_header_table` (`tests/it/openapi_headers.rs:18`).

This design adds no route, op or header (decision 18), so those stay green as they are. What must
change:
- **`args::price_set`** (`openapi/ops/args.rs:128-155`) gains `unit` and `price` together with the
  tool. Otherwise `tool_and_op_arguments_agree_except_listed_divergences`
  (`openapi_ops.rs:430`) fails. The `scope_kind` divergence (`openapi/ops/divergence.rs:57-62`)
  stays the only one.
- **The DTOs** (`UsageCell`, `UsageSeriesResponse.units_since`, `PriceRowView.price`,
  `RequestRow`, `PriceSyncSummary`) reach the document through schemars
  (`openapi/planes/usage.rs:70-95`, `openapi/ops/table/usage.rs:45`). The example check
  (`openapi_coverage.rs:672`) validates them.
- The `price_set` op summary (`openapi/ops/table/usage.rs:9-24`) and the two tool descriptions
  are reworded.

## 9. Limits and extension points

### 9.1 No hidden limits

- The image scanner has no size cap: it keeps O(1) state however large the answer is. The WAV
  reader reads an upload that `body_limit` already admitted, and adds no cap of its own.
- Catalog fields that are not synced are counted and shown (§6).
- The quantity history starts at a stated date (`units_since`, §5.5).
- Rounding happens at two stated points: seconds to milliseconds, and each priced part to micro.
- Quantity tiles hide only when their sum is zero. That is a display rule, and no data is dropped.
- A stopped synthesis or a non-WAV upload without a provider report stays unknown, and NULL on a
  priced scope. It lands in the remainder; it is not estimated and not dropped.

### 9.2 Provider billing quirks (named, not built)

The extension point is a per-price-row mapping from the measured quantity to the billed quantity,
applied before §3.3's product and snapshotted with the rate. Cases it would serve:
- **Minimum increments**: "rounded up to 15 s", "minimum 10 s per request".
- **What a character is**: lmgw counts Unicode scalar values of the text as sent. A provider that
  counts bytes or UTF-16 units, or counts SSML markup, differs.
- **Free tiers and allowances**, for example Google Search grounding's monthly allowance. These
  need period state per scope.
- **Prices that depend on a request field**: image size and quality, and video resolution (Sora
  2 Pro $0.30–0.70/s, Veo 3.1 by resolution). In v1 that is one rate per scope (Q3).
- OpenRouter's conditional `overrides` (§6).

## 10. Tests

All new integration tests are modules of the one `tests/it` binary.

**Units** (`src`):
- **`pricing.rs` goldens:** §3.3's table, each case asserting `total`, `units_micro` and the
  snapshot.
- **No false zero:**
  - a priced unit whose quantity is `None` gives NULL, with `used_units` still recorded;
  - an all-`None` unit row is no row;
  - a token sheet with no token count stays NULL even though every other unit is known;
  - every existing test in `pricing.rs:221-368` passes unchanged.
- **The token oracle:** today's function is kept as the test's oracle. For a grid of
  `TokenUsage` values (None/Some on every field, cache counts above the total) and token-only
  sheets (with and without cache rates, catalog and manual), the new `price_request`, given any
  `Quantities`, equals the oracle field for field.
- **Local:** a local sheet gives `Some(0)` with no token count (Q1).
  `local_routes_are_free_not_unpriced` (`store/usage_tests.rs:130-155`) is extended to the row
  level.
- **`sheet_for`:**
  - manual over catalog per unit;
  - alias over upstream model per unit;
  - a manual token row together with a catalog request row, both applying;
  - a cleared row never shadowing.
- **`store/usage_tests.rs`:**
  - `a_rebuild_reproduces_exactly_what_the_write_path_produced` (`:157`) gains the new columns;
  - the remainder carries quantities;
  - the migration copies `per_mtok` rows with their ids, `updated_at` and sequence;
  - the guard refuses `per_second`/`per_char` rows and names them.
- **`proxy/audio/usage.rs`:**
  - duration usage becomes `audio_in_ms`, fractions included;
  - the image scanner counts correctly with chunk boundaries inside keys and strings, a `"data"`
    inside a string, escaped quotes, nested arrays, and an empty `data`;
  - in an event stream, final-image events count, partial ones do not, and a stream with no
    recognised final event gives `None`.

**Integration** (`tests/it`):
- **`audio_cloud_usage.rs`:**
  - `whisper_duration_usage_is_not_turned_into_tokens` (`:171-186`) keeps its token assertion and
    adds `audio_in_ms = 27000`. With a `per_audio_minute` price of 0.006 the row costs 2 700, and
    without one it stays NULL;
  - `a_binary_speech_answer_stays_unpriced` (`:257-278`) splits in two: still NULL under a token
    price, and `chars × rate` under `per_mchar`;
  - a WAV upload to an upstream that reports nothing is priced from the measured length, and an
    MP3 one stays NULL;
  - an upstream 4xx carries no quantities.
- **`image_backend.rs`:** a cloud answer with two images under `per_image` costs 80 000. A local
  sd-server row records `images_out` and stays `free_local`.
- **`realtime_speech.rs`:** `a_cancel_mid_synthesis_writes_the_one_row_and_stops_the_rest`
  (`:430`) asserts the row's characters and request count, or `None` when a clause was in flight.
- **`realtime_audio_in.rs` and `chat_voice_dictation.rs`:** ASR rows carry the WAV's
  `audio_in_ms`.
- **`catalog_fields.rs`:**
  - the Kilo fixture's `web_search` is not synced and is counted in `not_synced`;
  - `request` is synced beside a usable token row;
  - `request` with an unparsable `prompt` writes nothing;
  - `"0"` updates an existing row and creates none.
- **`key_set.rs`:** `the_unpriced_worklist_sees_the_passthrough_models_usage_shows` (`:388`)
  shows that a unit-only sheet takes the model off the worklist.
- **New `usage_units.rs`:**
  - tokens plus `per_request` on `/v1/chat/completions` through wiremock;
  - a stream canceled before its answer on a `per_request` scope is NULL and counted in the
    remainder;
  - `price_set` validation per unit, on the op and on the tool;
  - the CSV's appended columns;
  - `lmgw__usage`'s remainder string.
- **`openapi_ops.rs:430`** stays green with the new arguments on both planes.

## 11. Work packages

Cut by file ownership. WP1 lands first, because it defines the types and columns everything else
writes. WP2 is next. WP3, WP4 and WP5 then share no file and can run in parallel. Each WP ends
green on `bash ci/check.sh` and commits with explicit paths.

| WP | Owns | Delivers | Proven by |
|---|---|---|---|
| WP1 Storage | `migrations/0070_billable_units.sql`, `store/migration_guards/billable_units.rs` and its registration, `store/prices.rs`, `store/rows.rs`, `store/request_logs.rs`, `store/usage.rs`, `store/usage_tests.rs`, `config/prices.rs`, `lmgw-api-types` (`PriceUnit`, the DTO fields of §8) | schema, guard, `PriceRow.price`, new columns written and rebuilt (NULL and 0 until WP2), `units_since` | migration and rebuild tests |
| WP2 Pricing core | `pricing.rs`, `config/snapshot.rs`, `proxy/recording.rs` and `recording/write.rs`, `proxy/in_process.rs`, `telemetry.rs`, `agents/batch.rs` | `Quantities`, `Sheet`, `price_request`, `LogParams.quantities`, the `requests` default and its audit (§4.5), the local zero (Q1), the batch meter's skipped calls (Found while reading) | goldens, oracle, recording tests |
| WP3 Catalog | `catalog.rs`, `tests/it/catalog_fields.rs` | the extra `Pricing` fields, §6's rules, `not_synced` | catalog tests |
| WP4 Measurement | `proxy/audio.rs`, `proxy/audio/usage.rs`, `proxy/audio/speech.rs`, `proxy/image.rs`, `proxy/transcribe.rs`, `proxy/synthesize.rs`; ITs `audio_cloud_usage.rs`, `image_backend.rs`, `realtime_speech.rs` | §4's hooks, `MediaOutcome.measured`, `Answer::Image` | the ITs of §10 |
| WP5 Surfaces | `web/api.rs`, `web/api_usage.rs`, `ops/usage_prices.rs`, `mcp/selfadmin/catalog/prices.rs`, `mcp/selfadmin.rs`, `openapi/ops/args.rs`, `openapi/ops/table/usage.rs`, `lmgw-ui/src/pages/usage/*`, `lmgw-ui/src/pages/traffic.rs`; ITs `key_set.rs`, `usage_units.rs` | §8 | openapi suites, `usage_units.rs`, the UI's own checks |

Ahead of the work packages, each as its own commit: the fallback-alias fix (Q2), before WP2
because it changes which scope a row resolves, and the fixes of "Found while reading", the
catalog ones before WP3.

## 12. Questions for the owner, resolved

The review of 2026-10-07 confirmed all five as recommended (decisions 19–23).

1. **Local rows without tokens: free, not unpriced (recommended: yes).** Confirmed. Today a
   local audio or image row resolves `free_local`, but `price_request` returns `unknown` before
   the source is considered when no token count exists (`pricing.rs:171-177`). The rollup then
   counts the row in the unpriced remainder (`request_logs.rs:332-335`). So the owner's local
   ASR, TTS and image traffic reads as a pricing gap. §3.2 rule 1 makes a local row cost 0 from
   the migration on. History keeps what it was written with.
2. **Price a fallback row under the alias that answered (recommended: yes, as its own commit).**
   Confirmed. `record` priced under the *requested* alias (`recording.rs:213-221`), and
   `prices_for` looked up that alias's rows (`snapshot.rs:145-161`). A cloud alias that answered
   as a fallback therefore never applied its alias-scoped rows. Its catalog rows are
   alias-scoped too, because the model has an alias (`catalog.rs:279-290`). A local ASR alias
   falling back to a cloud whisper alias would have stayed unpriced however the whisper alias
   was priced, and tokens were affected already. Fixed ahead of WP2: `price_call`, behind all
   three row writers, and the batch run meter look up `Route::priced_alias` (§2.3).
3. **Prices that vary with a request field (recommended: one rate per scope in v1).** Confirmed.
   Image size and quality, and video resolution. Price at the tier you use, or split aliases by
   tier. Tiers keyed on request fields stay §9.2's extension point.
4. **Measure non-WAV uploads (recommended: not in v1).** Confirmed. lmgw reads WAV only. A WebM,
   Ogg or MP3 upload to a per-minute provider that reports no duration stays NULL and visible. A
   container demuxer is a new dependency, worth adding only when such a provider is actually in
   use.
5. **Catalog `image` and `web_search` (recommended: not synced in v1, counted in `not_synced`).**
   Confirmed. `image` is per input image, and it is not documented whether that adds to the
   image's prompt tokens or replaces them. No route can carry a search.

## Rejected

- **Switching binary TTS calls to SSE to get token usage.** Whether a model streams SSE speech,
  and what its events carry, is per model. The realtime and Chat paths would need a per-model
  table to keep up with, for one class of model.
- **Backfilling old Gemini rows.** Rows are priced at write time and history is not re-priced
  (decision 7). A backfill would rewrite numbers the owner already read.
- **A generic quantity/unit pair** (child table or JSON column): §5.1.
- **Storing per-character or per-second rates and scaling them for display.** That brings back
  float noise and makes the owner type `0.000015` (§3.3).
- **Reusing `price_in`/`price_out` as the rate of a non-token unit.** Whether input or output
  carries a per-request fee has no answer, and every reader would need a per-unit legend.
- **Estimating a quantity**: a duration from bytes and bitrate, characters from tokens, images
  from `n`. That breaks decision 3.
- **A `mixed` price source.** `PriceSource::parse` reads unknown strings as `unknown`
  (`pricing.rs:53-64`), so a priced row would read as unpriced. "Manual if any" (§3.2) needs no
  new value.
- **Rate limits on quantities**: §7.
- **Syncing a catalog `request` fee without a token row**: §6.

## Found while reading, fixed now

The review had all four fixed now rather than left for later.

- **Run meters summed only the priced calls.** `RunMeters::note_model_call` added a call's cost
  when it was known and skipped it otherwise (`agents/mod.rs:720-722`). A run with one unpriced
  call then reported the sum of the others as its total, which is the partial sum §2.3 rules
  out, although `RunTotals`' doc says `None` means "nobody could price this" (`:693`). Fixed in
  its own commit: the total is `None` from the first unpriced call on, `RunMeters::fold_into`
  keeps to the same rule, and `pricing::sum_micro` states it. The in-process batch meter has the
  same gap one level down: `Usage::add` skips a call that reported no token count before the
  summed usage is priced (`agents/batch.rs`, `Meter`). WP2 owns that file and closes it.
- **Kilo's cache prices were not synced.** The catalog publishes `input_cache_read` and
  `input_cache_write` (fixture lines above), and `catalog_prices` set both to `None`
  (`catalog.rs:250-251`). Cache reads on Kilo-routed models therefore billed at the full input
  rate, an upward error of up to 10× on reads. Fixed ahead of WP3, in `catalog.rs`, in its own
  commit.
- **`sync_prices` skipped `llama_server` and `audio_cpp` but not `sd_cpp`** (`catalog.rs:339`).
  That was harmless, because `prices_for` short-circuits local routes first. The doc at
  `catalog.rs:323-326` named two kinds where three are local. Fixed ahead of WP3, in
  `catalog.rs`, in its own commit.
- **The realtime design's "no new columns in v1" (§11 there) and `synthesize.rs:26-31`** are
  superseded for characters and ASR duration. Fixed in WP4, which rewrites the comment (§4.3).
