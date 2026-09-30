# Usage analytics, cost & policy — design (2026-09-18)

Successor note to the gateway design (2026-06-09) §10 (observability) and §13
(auth), and to the quickdoc design (2026-08-29) §11, whose eval-history shape
this borrows: *a measurement is only worth keeping if what it was measured
under is kept with it.*

## 1. Summary

Three layers that only work together, plus the page that reads them:

- **Accounting** — every request gets a **priced** log row: the money it cost,
  the price that was used, and (for local models) the GPU time it took. Prices
  are already parsed from upstream catalogs and republished on `/v1/models`
  (`catalog.rs`, `capabilities/exposed.rs`); nothing has ever multiplied one by
  a token count.
- **History** — hourly **rollups** that outlive the raw rows. `request_logs`
  is a 30-day / 200k-row tail (`retention_days`, `retention_max_rows`) that a
  pruner eats; a dashboard reading it directly would quietly lose a month of
  history the first time the gateway got busy.
- **Policy** — an API key gains a **scope**, a **budget** and a **rate limit**,
  enforced on the request path, with denials that name the setting they hit —
  the `gpu_hold` / `body_limit` pattern.
- **Usage** — one page: what was spent, on what, how fast it answered, how much
  of it never left the house.

The reason it exists: lmgw can tell you what is happening *now* — Overview
tiles, the Traffic tail, the SSE bus — and nothing whatsoever about last
Tuesday. The owner's real questions are all retrospective ("what did this month
cost", "is the local model actually carrying the load", "what got slower after
the image bump", "which key is that"), and today every one of them is answered
by reading a log tail by eye, until it is pruned. An API key is a boolean: any
key that works, works for everything, forever, at any rate.

Scope note: this is a **single-owner** desktop gateway. There is no tenancy,
no chargeback, no invoicing. "Policy" here means *the owner fencing off their
own keys from their own mistakes* — an agent in a loop, a script left running,
an unattended corpus ingest against a cloud embedder.

## 2. What a request is worth

### 2.1 The row

`request_logs` gains, all nullable:

| column | meaning |
|---|---|
| `key_id` | FK to `api_keys` — the *identity*, next to today's `client_key` name, so a renamed or deleted key does not orphan its history |
| `cost_micro` | total cost in **micro-units of the configured currency** (integer; floats do not add up) |
| `cost_in_micro`, `cost_out_micro` | the split, because the input/output ratio is the whole story of a cache-heavy or a reasoning-heavy workload |
| `price_in`, `price_out`, `price_cache_read`, `price_cache_write` | the per-Mtok numbers **as used**, snapshotted |
| `price_source` | `catalog` \| `manual` \| `free_local` \| `unknown` |
| `cached_in_tokens`, `cache_write_tokens`, `reasoning_tokens` | the usage detail the IR does not carry yet (§2.3) |
| `class` | `chat` \| `aux` \| `audio` \| `tool` — a `/v1/embeddings` call and a chat turn are not the same unit and must not share a tile |
| `gpu_ms`, `prefill_ms`, `decode_ms`, `decode_tok_s`, `kv_reused`, `draft_n`, `draft_accepted` | llama.cpp `Timings` (§2.4) |

### 2.2 Prices

A `prices` table, one row per priced thing:

```
(scope_kind: alias | upstream_model, scope_key, unit,
 price_in, price_out, price_cache_read, price_cache_write,
 source: catalog | manual, note, updated_at)
```

*(rev, during implementation)* No per-row `currency` and no `effective_from`.
A per-row currency with no conversion is an invitation to add rows that cannot
be summed, so the currency is one **setting** — the label every amount is shown
under. `effective_from` would imply the table is a price *history* that
something reads back through; it is not, because the row that was charged
snapshots its own numbers (below). What replaced it is `updated_at`, which
answers the only question anyone actually asks of it.

- **`unit` exists from day one, even though v1 only implements `per_mtok`.**
  Audio is priced per second or per character and embeddings per Mtok of input
  only; a price table without a unit column has to be migrated the day the
  first TTS bill arrives, and every stored row before it is ambiguous.
- Catalog-advertised prices sync in on the same refresh that already builds the
  model list. A **manual** row always wins over a catalog row for the same
  scope — an upstream that publishes nothing (Gemini publishes no pricing at
  all) is priced by hand, and a scope with neither is priced not at all.
- **The row snapshots the numbers, not a foreign key.** Providers re-price;
  OpenRouter re-prices weekly. A dashboard that recomputes history against
  today's price sheet silently rewrites a number the owner already read and
  reasoned about. What was charged is what was charged.

### 2.3 Unknown is not zero

The single rule this whole feature stands on:

> A request whose price is not known gets `cost_micro = NULL` and
> `price_source = 'unknown'`. **Never `0`.**

and its consequence, which is a UI obligation, not a nicety:

> Every total, tile, chart and export that sums cost **states its unpriced
> remainder** — "€12.84 · 412 requests (1.2M tokens) unpriced" — and a chart
> whose slice is unpriced draws it in the neutral hatch, not as a zero-height
> segment.

A zero for an unknown is the one failure mode that makes an analytics page
worse than no analytics page: it reads as authoritative and is wrong downward.
Local models are a *different* thing and get `price_source = 'free_local'`,
which is a real zero in money terms (§2.5 is where their cost actually lives).

`Usage` in the IR is `{prompt_tokens, completion_tokens}` today, so a
cache-heavy Anthropic conversation is currently unpriceable at any accuracy:
cache reads bill at 0.1× and cache writes at 1.25×, and both are folded into
`prompt_tokens` on the wire shape lmgw keeps. **Work package 1 is extending the
IR**, not the schema — `cached_input_tokens`, `cache_write_tokens`,
`reasoning_tokens`, parsed per provider (`prompt_tokens_details.cached_tokens`,
`cache_creation_input_tokens` / `cache_read_input_tokens`, Gemini's
`cachedContentTokenCount`) and left `None` where a provider says nothing. A
`None` cache split is priced at the plain input rate and the row says so via
`price_source`; it is not guessed.

### 2.4 Local models cost time, not money

Local rows are `free_local`, cost `0` — and that is exactly the number that
makes a self-hoster's dashboard useless, because it makes the half of the
workload they actually care about invisible. So local rows carry the llama.cpp
`Timings` block instead, which `proxy.rs` already receives and currently only
forwards to the Chat tab's live stats panel and then drops:

- `prefill_ms` / `decode_ms` / `decode_tok_s` — **the regression detector**.
  "Did the new quant get slower" and "did the llama.cpp image bump change
  anything" are one chart once these are stored per request.
- `kv_reused` (`cache_n / prompt_n`) — how much of the prompt never got
  recomputed. A prefix-cache hit ratio falling off a cliff is a real incident
  with no current symptom other than "feels slow".
- `draft_n` / `draft_accepted` — speculative-decoding acceptance rate, per
  model. The number that says whether an MTP drafter is earning its VRAM.

Optionally, and **off by default**: Settings → *electricity price* turns
resident GPU-seconds into an energy estimate. It is shown in its own column,
never summed into the money total, and labelled an estimate, because
attributing a shared card's draw to one of four concurrent requests is not
something this gateway can honestly do.

### 2.5 The counterfactual

One derived number, prominent, and carefully worded: **what the locally served
tokens would have cost at a named reference alias's price.** Not "saved" —
a counterfactual, because nobody would have run all of it against Claude. The
reference alias is configured (Settings → *compare local against*), the label
names it inline, and with no reference configured the panel says so instead of
picking one.

It is the single most motivating number a self-hosted gateway can show, and it
is also the easiest place in this whole design to tell a flattering lie, which
is why the wording is specified here rather than left to the page.

## 3. Rollups — the part that survives

### 3.1 Shape

```
usage_hourly(bucket_utc, key_id, alias, upstream_id, class, outcome,
             requests, tokens_in, tokens_out, tokens_cached, tokens_cache_write,
             tokens_reasoning,
             cost_micro, cost_unknown_requests, cost_unknown_tokens,
             ttfb_hist BLOB, total_hist BLOB, ttfb_sum, total_sum,
             total_min, total_max, gpu_ms, decode_tokens, decode_ms)
PRIMARY KEY (bucket_utc, key_id, alias, upstream_id, class, outcome)
```

- `outcome` is `ok | client_error | upstream_error | refused` — four classes,
  not a status code, because the cardinality of the key is what keeps this
  table small enough to never need its own retention.
- **Latency is a histogram, not a mean.** Percentiles do not average: a
  dashboard that rolls up p95 by taking the mean of hourly p95s is reporting a
  number that exists nowhere in the data. Sums and min/max ride along for the
  cheap questions.

  *(rev, during implementation)* The histogram is **not** a BLOB of packed
  counts on the rollup row, as sketched here. A BLOB has to be read, decoded,
  incremented and written back, which makes every logged request a
  read-modify-write inside the same transaction as its insert — two concurrent
  requests in one bucket then race and one loses its log row to a `BUSY`. It is
  a companion table, `usage_latency_hourly`, one sparse row per occupied
  bucket, incremented by a pure `count = count + 1` upsert: atomic in SQL, no
  read, and summable by the query planner. Buckets are **4 per octave**
  (`idx = round(4·log2(ms))`), so a reported percentile is accurate to about
  ±9% rather than the ±100% a doubling ladder gives.

  Only **successful** requests are recorded. A refusal's time-to-first-byte is
  not a latency signal, and letting refusals into the distribution makes a p50
  improve precisely when a gateway starts failing fast.
- `decode_tokens` / `decode_ms` roll up as a ratio pair so tok/s can be
  aggregated correctly (Σtokens / Σms), which a mean of per-request rates
  cannot.

### 3.2 Written on the same path as the row

The rollup upsert happens in the **same transaction** as the `request_logs`
insert. Not a periodic batch job over recent rows: a batch job's window and
the pruner's window are two clocks that eventually disagree, and the failure
mode is silent under-counting of exactly the busiest hour. One write path, one
truth, and the rollup is correct even if the process dies in the next
millisecond.

Migration backfills from whatever raw rows still exist, marks the backfilled
range, and the UI labels history before the install date as partial rather
than drawing a cliff the owner will read as an outage.

### 3.3 Retention

`retention_days` / `retention_max_rows` keep meaning exactly what they mean
today, and they now prune **only the raw rows**. Rollups get their own setting
(`usage_retention_months`, default `0` = keep forever) and the Settings copy
says the trade in one line: raw rows are the *detail* (you can still open the
request), rollups are the *history* (you can still see the month). A desktop
gateway at a busy 50k requests/day writes on the order of a few hundred rollup
rows a day; keeping ten years of that costs less than one day of raw logs.

### 3.4 Time zones

Buckets are **UTC**; the page renders in local time and labels the range with
the zone it used. A "day" that silently means UTC-day to the query and
local-day to the reader is wrong twice a year by an hour and wrong every day
for anyone not on UTC. Day/week/month grouping happens in the query, from the
hour buckets, with the offset applied there.

## 4. Policy

### 4.1 What a key carries

`api_keys` gains `scope_mode` (`all` | `allow` | `deny`), `scope_patterns`
(newline-delimited globs over aliases — the dashboard-textarea syntax the
self-admin tools already use), `budget_micro` + `budget_period`
(`day` | `month` | `total`), `rpm_limit`, `tpm_limit`, `concurrency_limit`,
`expires_at`, and `note`.

A **global** budget lives in Settings beside them, because the owner's actual
question is "what is this month costing me", not "what is key #3 costing me".

### 4.2 Where it is enforced, and what it returns

| Check | Where | Status | `code` |
|---|---|---|---|
| Key expired / disabled | auth middleware | 401 | `key_expired` |
| An `internal:*` identity presented as a credential | auth middleware | 401 | `auth` |
| Alias out of scope | after resolve, before dispatch | **403** | `key_scope` |
| Requests or tokens per minute | auth middleware, rolling window in memory | **429** + `Retry-After` | `key_rate` |
| Concurrency | auth middleware, guard released on drop | **429** + `Retry-After` | `key_rate` *(rev)* |
| Budget exhausted (key or global) | after resolve, before dispatch | **403** | `key_budget` |

**Budget is a 403, deliberately not a 429.** The gpu-hold spec already
established the reasoning for its own refusal: a 429 is the signal every SDK
reads as "retry with backoff", and a budget that resets on the first of the
month will not clear during any retry window. A rate limit *will* clear in
seconds, so it keeps its 429 and carries a `Retry-After` that is a real number.

### 4.3 The overshoot, stated rather than engineered away

A request's cost is knowable only after the response. Enforcement is therefore
"spend **so far** ≥ budget → refuse the **next** request", and a single request
can cross the line. That bound is one request, it is documented, and the UI
shows the overshoot (`€10.40 of €10.00 — over by €0.40`) rather than clamping
the display to the budget.

The alternative — reserving `max_tokens × output price` up front — refuses
requests that would have fit, on a number the client mostly does not send, and
makes the *reserved* figure the one the dashboard would have to explain. An
honest overshoot beats a clever pre-authorisation.

### 4.4 Internal consumers are identities too

Admin Chat, dashboard Chat, quickdoc ingest and re-embed, golden-query
generation, the mail workflow, MCP sampling and `/v1/responses` tool loops all
spend real money on cloud aliases today and appear in the logs as "no key".
Each gets a **synthetic key identity** (`internal:quickdoc-ingest`,
`internal:admin-chat`, …): not authenticable, not listable as a credential,
but budgetable and — more to the point — *visible*. An unattended corpus
re-embed against a cloud embedder is precisely the thing that should not be
able to hide inside an unattributed lump.

## 5. Query surface

One endpoint backs every chart, so a chart cannot invent its own arithmetic:

```
GET /api/usage/series?from&to&bucket=hour|day|week|month
                     &group_by=alias|key|upstream|class|none
                     &alias=&key_id=&class=&upstream_id=&tz=<minutes east>&limit=6
```

*(rev, as built)* One response carries every measure rather than one `metric`
at a time: the tiles, the spend chart and the token chart all read the same
cells, and asking five times for five columns of one aggregate would be five
chances for them to disagree. It also carries `buckets` (**every** bucket in
the window, including empty ones — a chart that drops a quiet day lies about
its own x-axis), the `previous` window's totals for the deltas, the window's
four percentiles, and `latency`, the per-bucket percentiles.

`limit` folds the tail into a real `Other` series server-side (the dataviz rule:
never a ninth generated hue). Plus:

- `GET /api/usage/top?dim=…` — the ranked tables.
- `GET /api/usage/heat?…` — weekday × hour cells, in local time.
- `GET /api/usage/local?…` — the local/cloud split, the §2.5 counterfactual, and
  the local throughput / KV-reuse / draft-acceptance facts.
- `GET /api/usage/keys` — policy + spend-to-date per key, the Keys table's row.
- `GET /api/usage/prices` — the sheets, and the served aliases that have none.
- `GET /api/usage/errors?…` — refusals and errors **by kind**, from the raw
  rows *(rev: not in the original sketch)*. The rollup keeps a four-way
  `outcome` and not the kind, because the cardinality of its key is what keeps
  it retention-free; "which refusal am I hitting" is a recent-operations
  question and the raw rows cover exactly the retention window. The response
  states that range, so the card cannot imply it reaches as far back as its
  neighbours.
- `POST /api/op/key_set` / `price_set` / `price_delete` / `prices_sync` — edits,
  through `ops.rs` like everything else.
- `GET /api/usage/export.csv?…` — the rollup rows, **streamed to the caller**
  *(rev: the sketch said "written to a local file path the UI shows"; a
  download the browser owns is simpler and writes nothing on the gateway's
  disk)*. Nothing is uploaded anywhere, ever.
- `GET /api/logs` gains `key_id`, `error_kind` and `class`, which is what makes
  §6.1's click-through into Traffic real rather than decorative. Its rows also
  carry the §2.1 cache split (`cached_in_tokens`, `cache_write_tokens`) *(rev:
  not in the original sketch)* — the row is where "why did this one cost 8x its
  neighbour" gets answered, and the token total alone cannot answer it. The
  `request` SSE frame carries the same two, so the live tail and a reloaded
  page show one story.
- `lmgw__usage`, `lmgw__prices`, `lmgw__prices_sync`, `lmgw__price_set`,
  `lmgw__price_delete` — self-admin tools over the same handlers, flat scalar
  arguments, so an agent can answer "what did I spend this month" without a
  second implementation of the question.

Live: the existing SSE bus gains nothing new. The page subscribes to the
`Request` frames it already publishes and grows the **current** bucket in
place; everything older is immutable and never refetched.

## 6. The Usage page

Mockup: `docs/design/ui-rebuild/usage-mockup.html` (open it directly; it is the
contract for the chart tokens the way `design-sample.html` is for the rest).

### 6.1 Layout

One **filter row** above everything it scopes — range, bucket, group-by, class,
key — never per-card filters. Then:

1. **Tile row** — spend (hero, with delta vs the previous equal period and a
   12-point sparkline), requests, tokens in/out, p95 total, error rate, local
   share. Unpriced remainder sits under the spend tile as a sub-line, always.
2. **Spend over time** — stacked columns by alias, `Other` folded. The one
   chart the page exists for.
3. **Cumulative vs budget** — a line against a solid threshold rule, plus a
   **dashed** projection to period end (dashing means projection here, which is
   why gridlines are never dashed anywhere else).
4. **Tokens in / out** — diverging columns from a zero rule, input above it and
   output below: the ratio is the shape of the workload and a stacked bar hides
   it. Two measures, one rule, **never** two y-scales on one plot.
5. **Latency** — p50 line with a p95 band, from the histograms.
6. **Where it went** — horizontal bars, share of cost *and* of tokens, sorted.
   Never a pie; the two-column form answers "expensive per token?" directly.
7. **Local vs cloud** — one stacked bar plus the §2.5 counterfactual.
8. **Local performance** — decode tok/s per model over time, KV reuse %, draft
   acceptance %. The panel that only a gateway that owns its runtime can draw.
9. **When** — weekday × hour heatmap, single-hue ordinal ramp.
10. **Keys** — the policy table: scope, spend-vs-budget meter, limits, last
    used, requests. Inline edit; the meter's fill carries severity.
11. **Errors** — stacked columns by `error_kind`, click-through.

Every chart click-throughs into Traffic with the filter pre-applied. The Usage
page is a *lens on the same rows*, not a parallel universe with its own
numbers — if a chart and the log table disagree, one of them is a bug, and the
click-through is how that gets noticed.

### 6.2 Rendering

**Inline SVG generated in Leptos. No JavaScript charting library.** The
dashboard is CSR WASM: a JS chart lib means a `wasm-bindgen` bridge for every
datum, a second layout system that knows nothing about the CSS tokens, and a
bundle bigger than the app. The chart set here is small, fixed and mostly
rectangles. A `chart.rs` module (scales, ticks, path building, the hover
layer) plus `.chart-*` tokens in `app.css`, which `design-sample.html` and the
mockup both mirror.

Mark specs are the dataviz ones and are not negotiable per-chart: ≤24px
columns with a 4px rounded data-end, 2px lines, ≥8px markers with a 2px surface
ring, 10%-opacity area washes, hairline **solid** recessive grid, a 2px surface
gap between touching fills.

### 6.3 Series colour

Six categorical slots in fixed order, assigned per entity and **never by rank**
— filtering a series out must not repaint the survivors. Validated with the
dataviz validator against the real card surface in both themes (all six checks
pass; dark worst adjacent ΔE 11.6 deutan, light 11.7):

| slot | dark (`#23272C`) | light (`#FBFCFC`) |
|---|---|---|
| 1 | `#0486D3` | `#0077C7` |
| 2 | `#D57700` | `#CF7100` |
| 3 | `#009D9E` | `#009192` |
| 4 | `#875CC6` | `#7541B8` |
| 5 | `#5B9D2C` | `#4A8B10` |
| 6 | `#CF5291` | `#C43783` |

`Other` is `--text-3`. The heatmap's ordinal ramp is one hue, five steps,
validated with `--ordinal`: `#375F80 #3674A5 #3089CB #3F9FE8 #60B6FA` (dark).

This **replaces `hue_for()` for charts**. The FNV-hash hue is fine for a chip
where one colour sits alone, and unfixable for a chart: it is unstable under
CVD, has no lightness discipline, and collides. Chips keep it; a *legend* is
the chart's identity channel. (Aligning chips to the six slots later is a
separate, cosmetic change.)

Status colours (`--ok`, `--err`, `--amber`) stay reserved for status and are
never a seventh series. The error chart is the one place `--err` is a series
colour, because there it *means* error.

### 6.4 Animation

Motion encodes a data change and nothing else; every rule below is off under
`prefers-reduced-motion: reduce` (the titlebar pulse already sets that
precedent):

- **Entry** — columns grow from the baseline, lines draw left-to-right, ~420 ms
  with a ≤40 ms per-series stagger. Once per mount, not per re-render.
- **Live tick** — the current bucket animates its own height as `Request`
  frames land. This is the only continuously moving thing on the page, and it
  is moving because the data is.
- **Count-up** — tiles interpolate to a new value over ~500 ms; the digits are
  proportional (never `tabular-nums` on a hero figure) and the slot is width-
  reserved so a 9 → 142 does not shift the layout, exactly as `.pulse .rate`
  already does. A delta's colour is direction **times whether up is good** — a
  rising request count is not a red number.
- **Range change** — crossfade, previous render held at reduced opacity. No
  skeleton flash, no layout jump.
- **Hover** — crosshair + tooltip on time charts, per-mark tooltip elsewhere,
  hit targets ≥24px, keyboard focus showing the same. Tooltips *enhance*; the
  table view under every chart is what guarantees no value is gated behind a
  pointer.

## 7. Testing

- **Cost math golden tests** per provider usage shape: OpenAI cached input,
  Anthropic cache create/read tiers, Gemini cached content, a reasoning-token
  response, and an unknown price → `NULL` (asserted as NULL, not 0 — this is
  the test that protects §2.3).
- **Rollup equivalence fuzz**: N random logged requests → assert every
  `usage_hourly` aggregate equals a direct scan of the raw rows, including
  percentiles within one histogram bucket.
- **Retention**: prune raw rows, assert the dashboard's numbers are unchanged;
  assert the pruner never touches rollups.
- **Policy**: scope allow/deny globs; token bucket refill under a clock; the
  budget overshoot bound is exactly one request; a 403 (not 429) for
  `key_budget`; `Retry-After` present and sane for `key_rate`.
- **Time**: hour buckets across a DST transition, grouped to local days.
- **Page**: the series endpoint's `limit` folds to `Other` server-side; a
  filter change does not change a surviving series' colour slot.

## 8. Work packages (sequential)

1. IR `Usage` extension (cache / reasoning tokens) + per-provider parsing;
   persist llama.cpp `Timings` on the row.
2. `prices` table, catalog sync, manual override editor.
3. Cost columns on `request_logs` + pricing at log time; `unknown` never 0.
4. `usage_hourly` + same-transaction upsert + backfill + retention split.
5. `/api/usage/*` + `lmgw-api-types` DTOs.
6. `chart.rs` + `.chart-*` tokens; Usage page v1 (tiles, spend, mix, latency).
7. Key policy schema + enforcement + the five denial codes.
8. Keys table, budget meters, global budget, synthetic internal identities.
9. Local performance panel + the §2.5 counterfactual.
10. Heatmap, errors panel, click-through wiring into Traffic.
11. `lmgw__usage` tool + CSV export.

1–4 are the feature even with no page; 6 is the first thing worth looking at.

## 8b. What was found in review *(added 2026-09-18, after implementation)*

An adversarial review of the built plane found twelve defects, every one
invisible to a passing test suite. They are fixed; the ones that change what
this document says are:

- **A cached figure is not a ledger.** The per-key period spend is cached so the
  request path never runs a `SUM`, and it is now re-seeded from the rollup once
  a minute. Without that, a request in flight across the seeding instant had its
  increment dropped and was not yet in the `SUM`, so the number the *gate*
  enforced on could sit below the number the *dashboard* displayed until the
  period rolled over, with nothing to say which was live.
- **A concurrency slot must outlive the handler.** `next.run()` returns when the
  response is built; a streamed reply is built in microseconds and drains for
  minutes on a spawned task. The guard lives in the response body.
- **Refusals decided in the middleware are still traffic.** `key_rate` and
  `key_expired` are decided before any handler runs and were logged nowhere —
  making the refusal class most likely to be hit in a loop the one class the
  page could never show.
- **The unpriced remainder means a pricing gap, and only that.** It counts work
  that happened and could not be priced. A refusal or an error that spent no
  tokens is not a hole in the price sheet, and counting those made an agent
  retry-looping into refusals read as one.
- **A budget is blind to unpriced spend**, structurally: it sums `cost_micro`,
  and an alias with no sheet contributes nothing. The refusal path and the
  warning path are connected on the page — the Prices panel lists the unpriced
  aliases and every total states its remainder — not in the code. Documented
  rather than papered over, because the fix is to price the alias.
- **A rebuild must reproduce the write path exactly**, which means storing what
  was counted rather than reconstructing it: `predicted_n` is on the row now,
  because `CAST(rate x ms AS INTEGER)` turned a one-token turn into zero tokens.
  It also means **one clock read per row**: the insert and the rollup upsert
  each called `'now'` in their own statement, and SQLite re-reads the clock per
  statement, so a request committing at `HH:59:59.99` could be logged in hour H
  and counted in hour H+1 — which a later rebuild, reading `ts`, would silently
  move back.
- **A stored column nothing reads is still an invisible number** *(found after
  that review, while tracing where the cache counters actually go)*. Cache
  *writes* — Anthropic's 1.25x tier, the dearest input there is — reached
  `usage_hourly` and stopped there: the wire DTO had no field for them, so the
  export, the charts, the `lmgw__usage` tool and the page never saw the column
  the migration had just added. The same fold that dropped them was also
  dropping `prefill_ms`, which reads as infinitely fast prefill for the whole
  `Other` tail. A tier is carried end to end or it is not carried.
- **A link that silently returns the unfiltered head looks exactly like an
  answer.** `/api/logs` ignored any parameter it did not know, so the Usage
  page's click-throughs could not be built honestly until it grew `key_id`,
  `error_kind` and `class` filters. §6.1's "every chart click-throughs into
  Traffic with the filter pre-applied" was a promise about the *log* endpoint
  as much as about the page.

## 9. Out of scope

- **Multi-currency / FX.** One currency, a label, no conversion.
- **Per-user auth, chargeback, invoices.** One owner.
- **OpenTelemetry traces / `/metrics`.** A separate feature with a separate
  audience; this one is a desktop page for a person.
- **The energy estimate** for local models (§2.4's optional electricity price).
  *(rev)* Not built: the per-request GPU *time* is now recorded exactly (the
  llama.cpp timings), but attributing a shared card's power draw across four
  concurrent requests is not something this gateway can do honestly, and an
  estimate built on an assumed average wattage would be a number that looks
  measured and is not. The timings are there for whoever wants to try.
- **Anomaly detection** ("you are spending more than usual"). Tempting, and
  a thing that cries wolf on a workload with four users' worth of variance
  generated by one person. Revisit once a year of rollups exists to calibrate
  against.
- **GPU residency lane chart** (which model held the card when). Wants a
  residency event log the registry does not write yet; it belongs to the
  runtime spec, not this one.
- **Audio and embedding pricing.** The `unit` column exists so this lands
  without a migration; only `per_mtok` is implemented in v1.
