# Agent container runtime — design (2026-09-19)

Successor to the agent catalog design (2026-09-18). That spec stands except
where this one says otherwise: the manifest, the config-schema subset (§2.6),
templates (§2.3), the review gate, the jobs wiring, the storage row and the
whole import/export/API shape are unchanged and are referenced here rather
than restated. Read it first.

## 1. Summary

The catalog's principle 1 — "nothing an agent needs is compiled in, lmgw grows
no scripting language" — made the apply step a **model turn** (catalog §4.4),
because a turn was the only way to express a procedure without an engine. The
first real run showed what that costs: eight sequential model calls at ~7k
prompt tokens each to do one deterministic thing (list labels, create the
missing ones, one `gws__gmail_batchModify` per label), non-deterministic,
injection-exposed, and a reasoning model looping on the final answer. The
writes landed correctly; the *reporting* of them did not.

The fix is not a smaller DSL and not an embedded engine. **An agent's logic
runs in a Podman container that the owner builds** — any language, Node with
full imports as the expected default. lmgw defines the interface and the
entrypoint contract; the container owns the logic and, optionally, its own UI.

- **lmgw owns identity, money, logs, secrets and the shell. The agent owns
  logic and presentation.** That is the balance principle and every decision
  below follows from it.
- **The contract is layered, and every layer past the first is optional.**
  (1) one token per agent and a base URL; (2) a run ledger the container
  writes rows, logs, progress and an output into, rendered by the *existing*
  run surface; (3) service mode — the container also serves HTTP and lmgw
  reverse-proxies it same-origin under the agent's page; (4) package — the
  agent ships as an OCI image with its manifest inside it.
- **Inline `script` stays, as sugar.** A manifest may carry a JS module; lmgw
  runs it in a stock Node image with an embedded shim. It is layer 1+2 with
  the boilerplate removed, not a second mechanism.
- **`apply.turn` is retired.** No model in an apply step, ever — as a
  *warning* that disables Start, not a validation error, because an error
  would make the manifests that have one unreadable and unfixable (§4.3).

Podman becomes a hard requirement for agent runs, as it already is for local
models. A box without it gets a named error on Start, not a mystery.

## 2. Principles

**0. lmgw owns identity, money, logs, secrets and the shell; the agent owns
logic and presentation.** New, and above the rest: anything that must be
budgeted, attributed, audited or revoked is lmgw's; anything that is "what
this agent actually does" is the container's.

Against the catalog spec's five: **1 is replaced** — logic lives in the
agent's container, and lmgw still ships no engine (`script` is sugar over a
stock Node image, §4.2). **2 (no secret in a manifest) and 3 (every turn and
tool call is a Logs row) are unchanged**, the latter now true of container
traffic too, which arrives on `/v1` and `/mcp` under the agent's own key.
**4 (the review table is the gate) is unchanged**, and a container is invoked
once per phase and stateless across it. **5 is extended**: the container's
memory, CPU, PID and deadline bounds are visible manifest fields with their
defaults printed on the Run tab, like the tool-call and wall-clock bounds
already are.

**What the agent token is not.** It is an attribution, scope and budget
handle — not a confinement boundary. `crate::web::routes()` carries no auth
layer (`server.rs:76` merges it bare; only the `/v1` nest and `/mcp` get
`auth_mw`, `server.rs:63–69`), so on this install a container has the same
reach into `/api/op/*` as any other local process. Closing the admin plane is
a pre-existing lmgw question and out of scope here. What the token *does*
buy is real and is the point: the `/mcp` tool allow-list, the model-alias
scope, the budget and rate limits, and per-agent attribution in Logs.

**What that posture forbids, said once here** (WP5 review). Because the
dashboard plane is unauthenticated and `CorsLayer::permissive()`, anything lmgw
proxies on that plane is reachable by anything that can reach lmgw, on lmgw's
own origin. That is accepted for **the agent's own container** — the owner
installed it, lmgw started it, and it holds the agent's token (§3.3). It is
**not** accepted for a host lmgw did not start: `dev_url` is therefore
**loopback only** (§3.4), never a LAN address, because a LAN `dev_url` would
turn this gateway into an open reverse proxy for another machine. Widening it
would need a visible, explicitly opted-into setting and is the owner's call, not
a default — and the use case does not ask for it, since a `trunk serve` runs on
the box the dashboard is open on. (The loopback-only default itself was approved on 2026-09-19.)

## 3. The contract

### 3.1 Layer 1 — identity

**One token per agent, not per run.** A run is a job row that may be retried,
reopened and re-applied; a credential whose lifetime is a job is a credential
that has to be minted, delivered and reaped four times for one mailbox. The
agent is the thing that has an allow list and a budget, so the agent is the
thing that has a key.

**Where it lives: a third `ApiKeyKind` variant, not a new table** (schema in
§5). `api_keys` already carries everything a scoped, budgeted, revocable
credential needs — `KeyPolicy` (`scope_mode`, `scope_patterns`,
`budget_micro`, `budget_period`, `rpm_limit`, `tpm_limit`,
`concurrency_limit`, `expires_at`), the `request_logs.key_id` link, the hourly
usage rollup and the dashboard's key page. A separate table would re-implement
all five.

`ApiKeyKind::Agent` is **authenticable**, unlike `Internal`. The invariant
"internal identities never authenticate" is untouched verbatim:
`policy::admit`'s first branch still refuses `ApiKeyKind::Internal` with
`GatewayError::Unauthorized`, and `Snapshot::verify_api_key` still matches on
a real `key_hash` — an internal row's is the empty string, which
`hash_api_key` never produces.

| property | value |
|---|---|
| name | `agent:<agent id>` — reads like `internal:agents` does in Logs and on the usage page |
| kind | `agent` |
| key_hash | `hash_api_key("lmgw-agent-<32 random bytes as 64 hex>")` — still the lookup key, so `Snapshot::verify_api_key` is untouched. Hex, not base64url: no base64 crate is in the tree and one dependency for one encoding is not worth it (WP1 amendment, 2026-09-19) |
| key_plain | the same token **in plaintext**, in a new nullable column, non-NULL only for `kind = 'agent'` |
| policy.scope_mode / scope_patterns | derived, see below |
| policy.budget_* / rpm / tpm / concurrency | owner-set on the agent's detail page; defaults `0` (none), shown as "no budget" |
| agent_id | the catalog row's id; `ON DELETE` is manual — `agent_delete` deletes the key row too |

**Why a plaintext column, stated plainly.** `api_keys` stores a hash and
nothing else (`0001_init.sql:42`), so "shown once at creation" would leave
lmgw unable to hand the token to the container on the *second* run, and the
`dev_url` workflow unable to hand it to a process lmgw did not start. The
posture is the one `mcp_servers` already takes for `env` and `headers` —
*"JSON [[name,value]] (secrets; 0600 DB)"*, `0011_mcp_servers.sql:15,21` — a
0600 SQLite file on the owner's own machine. The UI offers **Copy token**, not
reveal-once. Rotation regenerates both columns in one write.

**Where the token exists, exhaustively:** `api_keys.key_plain`, the run's
`secrets.json` (§6.2), and the response body of `agent_token_get` — which,
being an op, sits on the bare `/api` plane and is therefore readable by
anything local (§2). Never in an export, a manifest, `provenance`, a log line
or an env var.

**CORS: the two token ops check `Origin`** (final-review amendment,
2026-09-19). "Readable by anything local" is the posture, and it stays the
posture — but `/api` runs under `CorsLayer::permissive()`, which makes "local"
include *a page on any origin loaded in a browser on this box*: a
credential-less `fetch` to `POST /api/op/agent_token_get` would read the
response, and `GET /api/agents` supplies the ids to ask for. So exactly
`agent_token_get` and `agent_token_rotate` — the two ops that hand out a
durable credential — refuse a request whose `Origin` is not the gateway's own
with a structured `403 cross_origin_refused`, using the same `origin_allowed`
rule §3.3 and `/mcp` already apply. A request with **no** `Origin` (curl, a
container, a script) is unaffected: that is what "anything local" means, and
only a browser sends one.

One further escape hatch, since §3.2 streams stderr verbatim into the run log:
a container that prints its own secrets file leaks its token into its own
output. lmgw closes its side of that (final-review amendment, 2026-09-19): the
agent's own token is replaced with `<agent token>` in every run-log line, in
the `podman logs` tail the App tab / `AgentDetail.service.log_tail` /
`lmgw__agent_get` / a failed start's 503 body carry, and in `StartError.log`,
before any of it is stored or returned. It is a substring replace and it
covers republication by lmgw only — a container that prints the token in
pieces, encodes it, or posts it somewhere is doing something no gateway-side
redaction can reach.

Created on demand, never at import: `agent_token_get` mints the token when
none exists (that is what Copy token calls), and the container runner calls
`token::ensure` at every start (WP2). An agent that is never run and never
copied never mints a credential.

**Disable is the kill switch.** `agents.enabled` is mirrored onto the key
row's `enabled`, so `verify_api_key` drops a disabled agent's token for free
and `auth_mw` answers `401 agent_disabled` on every plane; so nothing a leftover
container does after Disable spends or attributes (WP1 amendment; the spec
was silent, decided during implementation; approved 2026-09-19).

**Scope is derived, and recomputed.** `scope_patterns` is the set of current
values of every `format: "model_alias"` config field, plus a literal
`model.alias` when it is not a template. Recomputed at each container start,
at every ledger open, in `agent_config_set`, and in every manifest write
(`agent_set`, `agent_reset`, `agents_restore`), because a model picker is
exactly the field an owner changes between runs and a replaced manifest can
change which fields exist. If the manifest declares no model-alias field, or
every one of them is empty — **which is today's shipped mail-labeler, whose
`model` field has no `default`** — the policy is `scope_mode: all`, and the
Definition tab says *"token: any model"*. A derived allow-list that silently
came out empty would refuse every call the agent makes, which is the failure
this rule exists to avoid.

**Honoured regardless of `auth_enabled`.** Verified: `server.rs:419` only
looks at `Authorization` when `snap.settings.auth_enabled` is true, and the
default is off. `auth_mw` gains a step *before* that branch — if the presented
bearer resolves to a `kind = 'agent'` row, set `ctx.client_key`, `ctx.key_id`
and a new `ctx.agent: Option<AgentIdentity>`, run `policy.admit`, and continue.
If nothing matched, the existing `auth_enabled` branch runs unchanged. An
agent token is a *capability*, not a gateway credential; honouring it only
when the owner happens to have switched auth on would make its scope
decorative.

**`/mcp` filters by the token.** The aggregate plane (`mcp/ingress.rs`,
`mcp_post` at 493) is behind `auth_mw` and `origin_allowed` (ingress.rs:589,
944), so it is **ungated by default** rather than ungated — `auth_enabled` is
off out of the box, and then `auth_mw` lets everything through. With
`ctx.agent` set by the rule above, which happens whether or not
`auth_enabled` is on:

- `AggregatePlane::list_tools` runs a new `retain_allowed(&agent, &mut tools)`
  after the existing `retain_enabled`, keeping only names in the agent's allow
  list (the union of the manifest's `tools[].allowed`, or a whole label's
  current surface when `allowed` is absent, resolved through
  `agents::ToolSurface`).
- `AggregatePlane::call_tool` re-checks the same list before routing and
  refuses with `CallError::ToolNotFound("<name> — agent '<id>' may call: …")`.
  Re-checked because a list is a snapshot and a call is not, the same reason
  `disabled()` is evaluated per call.
- Without `ctx.agent`, both are exactly as today. The reserved `lmgw__*`
  namespace stays off `/mcp` for everyone, agent or not.

**Run attribution: `X-Lmgw-Run: <job id>`.** Read in `auth_mw` into the ctx.
A header rather than a token claim, because one token serves every run, and a
header works identically on the JSONL transport's sibling HTTP calls, in
service mode, and from a container lmgw did not start. **Only a request that
carries an agent token attributes**; without `ctx.agent` the header is
ignored with a log line, so no unauthenticated local process can bill a run
(WP1 review finding). For an agent caller the value is checked against that
agent's live jobs: an unknown or foreign run id is ignored with a log line,
never a refusal — a mis-stamped request is still a request the owner made.

Cost per run is stored in the job's `result` alongside `usage`,
`cost_micro`, `model_calls`, `tool_calls`, exactly as catalog §4.5 does. The
in-process executor keeps folding into its `Meter`; requests that arrive over
HTTP with `X-Lmgw-Run` are already priced individually when they complete, so
`agents::RunMeters` on `AppState` sums those per-request costs and
`RunMeters::fold_into` merges the sum into the result at close. `model_calls`
counts requests that reached an upstream and got a response, not refused ones,
so the two paths mean the same thing (WP1 amendment). **No `run_id` column on `request_logs`**: the
job row is already the durable home of a run's totals and a second one would
drift. A gateway restart mid-run loses the partial meter, but
`AppState::init`'s `fail_orphaned_jobs` already fails that run, so there is no
surviving run to attribute it to.

**Telemetry.** A container's own traffic arrives on the public `/v1` ingress
with a real key, so it logs under `ingress_proto = "openai"`/`"anthropic"` and
`internal_identity` correctly returns `None` — the `api_keys` row *is* the
identity. `AGENT_PROTO` and `AGENT_TOOL_PROTO` keep their meaning for the
in-process classify calls and direct tool steps, and `internal:agents` keeps
its history; both are `api_keys` rows, so the usage page sums either.
`counts_in_token_stats` needs no change: a container's `tools/call` arrives
under `"mcp"`, which it already excludes.

**Delivered, never in env.** `podman inspect` prints a container's
environment, and so does `podman ps --format`. The token goes in the secrets
file (§6.2) with the config secrets; env carries only the non-secret
addressing below.

| env var | value | set for |
|---|---|---|
| `LMGW_BASE_URL` | `http://host.containers.internal:<port>` — no trailing slash. `<port>` is the port of `settings.bind_addr` (**default `127.0.0.1:8787`**); never hardcoded | every phase |
| `LMGW_API_BASE` | `$LMGW_BASE_URL/v1` — point any OpenAI or Anthropic client here | every phase |
| `LMGW_MCP_URL` | `$LMGW_BASE_URL/mcp` | every phase |
| `LMGW_LEDGER_URL` | `$LMGW_BASE_URL/api/agents/runs/<run>/events` | run/apply |
| `LMGW_AGENT` | the catalog id | always |
| `LMGW_RUN` | the job id | run/apply |
| `LMGW_PHASE` | `run` or `apply` | run/apply |
| `LMGW_INPUT` | `/lmgw/input.json` | run/apply |
| `LMGW_SECRETS` | `/lmgw/secrets.json` | always |
| `LMGW_DEADLINE_SECONDS` | `limits.deadline_seconds`, so a container can wind down before SIGTERM instead of being surprised by it | run/apply |
| `LMGW_APP_BASE` | `/agents/<id>/app/` — the path the SPA must be built for | service mode |
| `LMGW_PORT` | `service.port`, echoed so the entrypoint has one source | service mode |

~~The default rootless pasta network already maps the host loopback, so
`host.containers.internal` resolves and reaches a `127.0.0.1`-bound gateway
with no extra flags.~~ **Wrong, corrected in WP3** against a container that
actually called back (the WP2 check only read `$LMGW_BASE_URL` out of the
env). `host.containers.internal` resolves to pasta's `169.254.1.2`, and a
gateway bound to `127.0.0.1` is *not* on that address: the connection is
**refused**. `--map-guest-addr` does not change that. So `LMGW_BASE_URL`
depends on what the gateway bound, and the runner renders both halves from
`settings.bind_addr` in one place (`container::gateway_access`):

| `bind_addr` | `LMGW_BASE_URL` | argv |
|---|---|---|
| `0.0.0.0:<p>`, or a real address | `http://host.containers.internal:<p>` | — |
| `127.0.0.1:<p>` / `::1` / `localhost` — lmgw's default and every desktop install | `http://127.0.0.1:<p>` | `--network=pasta:-T,<p>` |

pasta's `-T` forwards exactly that one port from the container's loopback to
the host's, so nothing else on the host is reachable from the container and
the argv says which case the run is in. Measured on podman 5.8.4, rootless.
Two caveats, both said in the run log rather than left to be found: `-T,<port>`
also **occupies that port on the container's own loopback**, so an image that
wanted to listen there cannot; and it forwards to the host's **IPv4** loopback
only, so a gateway bound to `[::1]` alone is unreachable from a container
however the URL is written — `container::loopback_note` writes one line naming
that when the bind address is a v6 loopback.

### 3.2 Layer 2 — the run ledger

A container reports through a ledger of four event types. The generic run
surface — `RunBuffer` live rows, the review table, cost per run, the jobs feed
— renders them exactly as it renders an in-process run, because they *become*
`agents::batch::Row` values and a `JobProgress`.

**Events** (the same four on both transports):

| type | fields | effect |
|---|---|---|
| `row` | `id` (required, non-empty), `columns?` (object of scalars), `output?` (any JSON), `prompt?`, `raw?`, `error?`, `attention?` | upsert into the run's rows by `id`; fields absent from the event are left alone, so a second `row` event may add `output` to a row that only had columns. Missing/empty `id` → the event is rejected with a run-log line naming it (catalog §2.3: no stable identity, nothing to apply against) |
| `log` | `level` (`info`\|`warn`\|`error`, default `info`), `message` | one run-log line |
| `progress` | `done` (u64), `total` (u64 or null), `stage` (string) | one `JobProgress`; `detail` is filled by lmgw (agent id, phase, attention and error counts, plus `log_lines`) exactly as `Run::detail` does today |
| `output` | `output` (any JSON) | the phase's terminal value. Last one wins; validated against the step's `output` schema at close — the schema lives on the container run spec, so WP1 stores it unvalidated and WP2 adds the check |

Unknown `type` → a run-log line naming it, not a failure. Same for a `row`
carrying a column key the manifest did not declare: it is appended to the
table after the declared ones in first-seen order and noted once in the run
log, and the extra keys are listed as `extra_columns` in the job result.
Surface it, never drop it silently. The run log itself is `AgentRunDetail.log`
(live from the ledger, from `result.log` after), rendered in the Run tab.

**Transport A — JSONL on stdout (the fast path).** For a container lmgw
started: one JSON object per line on stdout, `stderr` verbatim into the run
log, exit code mapped below. No HTTP, no token needed for the ledger itself,
no ordering question — the pipe is the ordering.

| exit | outcome |
|---|---|
| `0` | `JobOutcome::Done(result)`. If the phase declares an `output` schema and no `output` event arrived, the job **fails** with "the container exited successfully but emitted no output; `<phase>.output` declares a schema" |
| non-zero, run not cancelled | job `failed`: "the container exited with status `<n>`" plus the last 12 stderr lines, the same excerpt shape `Registry::log_excerpt` produces |
| non-zero after a cancel | `JobOutcome::CanceledWith(result)` — the rows and output recorded so far are kept, because Cancel must not be destructive (jobs/mod.rs says exactly this) |
| deadline exceeded | the cancel sequence runs, then the job `failed`: "the run exceeded its deadline of `<n>`s (run.limits.deadline_seconds)" |
| podman could not run at all | job `failed`: "podman is required for container agents and could not be run: `<io error>`" |

A stdout line that is not JSON is a run-log line verbatim. A library that
prints a banner must not be able to kill a run.

**Transport B — HTTP.** For service mode, for a container started outside
lmgw, and for the dev `url` override.

| route | body | returns |
|---|---|---|
| `POST /api/agents/{id}/runs` | `{ "phase": "run"\|"apply", "rows"?: [...] }` | `{ "run": <job id>, "deadline_seconds": <n> }`. Opens a job exactly as `agents::batch::start` does, so "one live run per agent" still comes free from the `(kind, key)` index; a second open while any run of the agent is live is `409 already_running` naming that run id. A `chat` agent or a disabled one is refused as `agent_run` refuses it |
| `POST /api/agents/runs/{run}/events` | one event object, a JSON array of them, or `application/x-ndjson` | `{ "ok": true, "applied": <n>, "rejected": [...] }`. A non-UTF-8 body is `400 invalid_utf8`. The routes drop axum's silent 2 MiB default body limit like the rest of `/api/agents` |
| `POST /api/agents/runs/{run}/close` | `{ "status": "done"\|"failed"\|"canceled", "detail"?: "…", "output"?: … }` | `{ "ok": true }`. `failed` sets the job error to `detail`; `canceled` maps to `CanceledWith` |

**These three handlers verify the bearer themselves.** They live under
`/api/`, which carries no `auth_mw` (§2), so the check cannot be inherited:
each resolves the bearer through `Snapshot::verify_api_key`, requires
`kind = 'agent'`, and requires the run's `agent_id` to equal the token's.
Missing or unusable → `401 agent_token_required`; valid but another agent's
run → `403 run_not_owned` (ownership is checked before existence, so a foreign
token learns nothing about other agents' runs); a run id no job row has →
`404 not_found`; a run that already ended → `409 run_closed`, or
`409 run_cancelled` when it ended cancelled; a live run that is an in-process
batch run rather than a ledger one → `409 run_not_ledger`; a jobs-table read
that fails → `500 op_failed`, never a silent drop. The one authenticating corner of the `/api` plane,
because a run ledger anything on the box could write to would make the review
table meaningless.

**Lifetime of a ledger-opened run.** `limits.deadline_seconds` runs from the
*open*, not from a container start — there may be no container. Cancel marks
the run cancelled immediately; every later `events` or `close` POST is refused
with `409` and error code `run_cancelled`, which is how a container that lmgw
cannot signal finds out (it either reads the refusal or polls
`GET /api/agents/runs/{run}`). A run that reaches its deadline with no
`close` ends `failed` with reason `no_close` — the honest reading of "the
process that opened this is gone", and the reason a ledger run cannot leak a
live job row forever.

Both transports carry the same events, so a container can start on JSONL and
grow into service mode without changing how it reports.

**The run log is pushed, not polled** (final-review amendment, 2026-09-19).
The Run tab re-reads `AgentRunDetail` when the jobs SSE frame changes, and the
frame is `(id, done, status)` plus this `detail` — so a run that emits only
`log` events and stderr, which is exactly what a container does while it pulls,
prints a banner or reports a diagnostic, changed nothing and left the Run tab
empty for precisely the phase its owner is watching it for. `detail` therefore
carries a **monotonic `log_lines` count**, and both transports report a frame
when the log grows and nothing else has: transport A on a stdout `log` event or
a stderr line, the HTTP ledger from its wait loop. It is a count, not the lines
— fifty lines of container stdout on every frame to every open dashboard tab is
the wrong pipe (§3) — and the endpoint that already serves the log is the right
one. The dashboard keeps **one such re-read in flight at a time**, serving the
newest ask when the previous answer lands; the request rate follows the
server's own response time, which is backpressure rather than an invented
interval. `jobs::note_progress`'s existing 500 ms publish throttle bounds the
frames themselves, so a chatty container costs two a second whatever it does.

### 3.3 Layer 3 — service mode

The manifest declares `service.port`. lmgw starts the container **on demand**
(first proxied request), health-probes it, reverse-proxies it same-origin, and
idle-stops it.

- **Route:** `ANY /agents/{id}/app/{*rest}`, plus `/agents/{id}/app` → 308 to
  the trailing-slash form. Registered in `web::routes()` ahead of
  `ui::routes()`, whose `/{*path}` catch-all would otherwise take it; the SPA
  keeps `/agents/:id` because a two-segment static route beats a wildcard.
  `ui::API_PREFIXES` is **not** touched — adding `agents/` there would 404 the
  SPA's own detail page.
- **Auth posture, stated rather than implied** (WP4 review): both routes are
  merged into the dashboard plane, which carries **no `auth_mw`** — anything
  local can reach them with no credential, exactly as `/api` can — and
  `CorsLayer::permissive()` makes them callable cross-origin from any page the
  owner happens to be visiting. That is the same posture §2 records for the
  rest of the dashboard plane, and it is the posture the same-origin decision
  below is already predicated on; it is written here so that "the app's proxy
  is authenticated" is never assumed.
- **The path is forwarded raw** (WP4 review). axum hands `{*rest}` over
  percent-**decoded**, and re-assembling a URL from a decoded path is four bugs
  in one, all verified against this proxy: `a%3Fevil=1?real=1` injects a query
  and drops the real one, `a%23frag?real=1` drops the query at a `#` that was
  never a fragment, `%2e%2e%2f` walks out of the mount (and, on the `/mcp`
  route, out of the declared `provides.mcp` path), and `a%2Fb` loses the
  encoded slash. So the raw `Uri::path()` is what gets forwarded — the known
  prefix stripped off the front, the remainder appended byte for byte, and the
  query attached with `Url::set_query` rather than concatenated. A **literal**
  `.` or `..` segment is refused with `400 bad_path` rather than resolved: an
  encoded one is an ordinary segment that happens to spell one and goes through
  verbatim.
- **Base path:** `LMGW_APP_BASE = /agents/<id>/app/`; `X-Forwarded-Prefix` is
  set for frameworks that read it. Nothing in the response body is rewritten —
  a proxy that rewrites HTML breaks on the first thing it did not anticipate.
- **Streaming:** the upstream body is passed through as a stream (`reqwest`'s
  `stream` feature is already enabled) wrapped in `Body::from_stream`, so SSE
  arrives chunk by chunk. Hop-by-hop headers are stripped both ways
  (`connection`, `keep-alive`, `transfer-encoding`, `te`, `trailer`,
  `upgrade`, `proxy-authorization`, `proxy-authenticate`), and `content-length`
  on the way back, because this hop re-frames the body as a stream.
- **A 3xx is forwarded, never followed** (WP4 review). The proxy and the health
  probe use `AppState::proxy_http`, a second `reqwest::Client` whose only
  difference from the shared one is `redirect::Policy::none()`. The shared
  client follows up to ten redirects and does not confine them to the origin it
  started on: through a proxy that collapses every 3xx the app returns into
  whatever the last hop said, and turns a `Location:` the container chooses
  into an arbitrary-URL fetch relayed on the dashboard's own origin.
- **`Location` is pointed back at the mount**, and `set-cookie` is not touched.
  An origin-relative `Location` (`/login`) is relative to the *dashboard's*
  root, not to where the app thinks it lives, so it is rewritten to
  `/agents/<id>/app/login`; so is one naming the container's own published
  origin, which the browser cannot reach. An absolute URL somewhere else is the
  app sending its user away and goes through untouched. `set-cookie` goes
  through as-is, which means **the app can set cookies on the dashboard's
  origin** — the same-origin decision below accepted rather than worked around,
  because stripping them would break every session the app keeps and scoping
  them would be guessing.
- **A 502 clears the entry.** A container that dies *after* its health probe
  passed would otherwise be 502'd against for the life of its map entry — with
  `idle_seconds = 0`, for the life of the process. A connect or request error
  (not an upstream 5xx, which is the app's own answer) runs the stop path,
  collects the container and says in the 502 body that the next request starts
  a fresh one.
- **WebSocket:** a raw bidirectional tunnel, and the library question of §12
  is **settled** (WP4): `hyper::upgrade::on` on the inbound axum request,
  `reqwest::Response::upgrade()` on the outbound one — present and ungated in
  reqwest 0.13.4 — and two `tokio::io::copy` halves between them. `hyper` and
  `hyper-util` (feature `tokio`, for `TokioIo`) become direct dependencies;
  both were already in the lock via axum and reqwest, and neither adds a build.
  No `tokio-tungstenite`: this proxy must *not* decode frames — masking,
  fragmentation, ping/pong, close and any negotiated extension are between the
  two endpoints, and decoding them would be a second WebSocket implementation
  to keep correct.
  **The two halves are raced, not `copy_bidirectional`d** (WP4 amendment):
  `copy_bidirectional` returns only when *both* directions have shut down, and
  a half-closed WebSocket is not a thing — a FIN from either endpoint means
  that endpoint is gone. Measured on this box: node's `server.on('upgrade')`
  socket does not close on a half shutdown, so `copy_bidirectional` never
  returned, the in-flight guard was never released, and the container could
  never idle-stop. `select!` over two one-way copies fixes it, and a test
  asserts `in_flight` is back to `0` after a tunnel closes.
- **Same-origin is accepted knowingly.** The agent's browser JS can reach
  `/api/op/*`, which the container's own token cannot — as trusted as the
  dashboard. Correct here (sole user, own agents, none third-party) and the
  first thing that would have to change otherwise: separate origin, iframe
  sandbox, CORS.
- **Idle stop:** `service.idle_seconds`, default `300`, `0` = never. Mirrors
  `McpServer::idle_seconds` including its semantics (`McpManager::reap_idle`:
  `<= 0` is a true no-op warm-keep) and its in-flight guard — a proxied
  request holds a counter the sweep skips, the discipline the `tools/call`
  guard uses so a slow call is never torn down mid-flight. The sweep's two
  conditions (`in_flight == 0`, and `last_used` older than the window) are
  **re-asked under the map lock** at the moment the entry is claimed, not only
  on the snapshot the sweep decided from: a request that took its guard in
  between must not be torn down by a decision made before it existed.
- **Health:** `service.health_path` (default `/`), probed until
  `service.start_timeout_seconds` (default `30`, **`0` = wait as long as the
  container takes**). A start that never answers fails the request with the
  container's log tail. Two WP4 amendments, both widenings:
  **any status below 500 counts as up**, not only a 200 — the question the
  probe asks is "is this process listening and speaking HTTP", and an app whose
  `/` answers `302 → /login` or `404` is up by any reading that matters; and a
  **blank** `health_path` selects a **TCP connect** to `service.port` instead
  of an HTTP GET, for an image whose port speaks something an HTTP request
  cannot introduce itself to. The poll's 250 ms sampling rate is
  `registry::HEALTH_POLL`'s, restated with its reasoning: *a rate, not a
  bound* — the bound is the visible `start_timeout_seconds`, and the remaining
  budget is each probe's own timeout, so there is no second number to explain.
  The log tail is read through the **agent** spawner
  (`container::logs_tail`), not `Registry::logs_tail`: a service container is
  started by the agent seam, and a test that fakes the one must not have to
  fake the other.
- **Port:** `service.port` is in-container; the host side is
  `runtime::registry::ephemeral_port()`, **published on loopback only**
  (`-p 127.0.0.1:<host>:<service.port>`). Measured, not assumed, and the
  mirror image of §3.1's finding: the container's own IP is not reachable from
  the host under rootless pasta, so the host side has to be published — and
  publishing it on every interface would put an agent's UI on the LAN without
  anyone asking. `-p` and `--network=pasta:-T,<gateway port>` coexist
  (verified on podman 5.8.4): the container reaches the gateway's loopback and
  the host reaches the container's port, at the same time. The chosen port is
  in `AgentDetail.service` and in the start's log line.
- **Stopping**, and what a service container is not: it is `-d` and **not**
  `--rm` (WP4 amendment). `podman logs` is the only account of a container that
  died during its health probe, and `--rm` would delete the evidence before the
  503 could quote it. The stop is `container::stop_ladder_inner`, the WP2
  ladder, with **no settle between the rungs** — `podman stop` on a detached
  container returns only once it is down, so there is no child to race and the
  sleeps a foreground run needs would be `2 × stop_grace_seconds` of dead time
  on every idle stop — followed by a `podman rm -f` through the agent spawner,
  which is what removes the stopped object a non-`--rm` container leaves.
- **A start in flight is stoppable** (WP4 review). The `Starting` slot carries
  a cancel channel that the `podman run` and every await in the health probe
  race. A stop claims that slot too: it fires the cancel, the starter collects
  whatever container it had created and answers its waiters with
  `503 agent_service_starting … the start was cancelled`, and the stop reports
  "a start was cancelled". Without it, all five stop paths — the App tab's
  Stop, delete, disable, token rotation and a manifest replace — answered
  "nothing was running" whenever they arrived a second early, and the container
  came up behind them as an orphan. This is also what keeps
  `start_timeout_seconds = 0` honest: unbounded means no per-probe timeout, and
  the cancel is what makes it a setting rather than a wedge. The idle sweep
  never cancels a start — a start is not idle.
- **What else stops it.** Three deliberate acts invalidate a running container
  rather than leaving it to serve something that is no longer true, each
  saying so in its own response: **token rotation** (§12's open question,
  answered — the container is holding a credential that stopped working, and
  the only thing it can do next is fail), a **manifest replace** (it was
  started from the manifest that was just overwritten), and **disable** (the
  kill switch has to reach the app too). `agent_delete` stops it and drops the
  MCP row with it.

**`provides.mcp`.** A manifest may declare `"provides": { "mcp": "/mcp" }`.
lmgw then keeps an `mcp_servers` row for the agent: `name: agent:<id>`,
`transport: http`, `url: <primary base>/agents/<id>/mcp`,
`tool_prefix: <agent id>` (already `[a-z0-9-]` by `manifest::validate_id`, so
`validate_tool_prefix` passes), `autostart: false`, `idle_seconds` inherited
from `service.idle_seconds`, and a new nullable `mcp_servers.agent_id`. A
second proxy route `/agents/{id}/mcp` targets `service.port` + the declared
path and shares the on-demand start and idle-stop logic. **The URL points at
lmgw's own proxy, not at the container's host port**, because that port is
ephemeral per start and an `mcp_servers` row is persistent; the host part
comes from `net::primary_base_url(bind_addr)`, which already resolves a
wildcard bind to loopback. It follows that the row is written **once, when the
manifest is written** (beside `token::resync`, on every path that writes a
catalog row — `agent_set`, the import endpoint, the Definition editor's Save,
`agent_enable`, `agent_reset` and the §5.1 built-in upgrade) and **not on every
start**: nothing in it changes per start, and a row rewritten per start would
reconcile the live MCP connection each time for no reader. `enabled` tracks the
agent's own. A row whose manifest this build cannot parse is **removed** rather
than left stale: an unreadable manifest cannot be asked what it provides.

**An aggregate `tools/list` never starts a container** (WP4 review, and the
decision this whole route turns on). `McpManager::list_tools` connects every
enabled server that is not `Ready`, and for an `agent:<id>` row connecting means
`podman run` — so without this rule every MCP client handshake, every chat turn
carrying tools and every load of the MCP page would start *every* service agent
on the box. An agent row's tools are therefore in the aggregate **only while its
app container is already running**. A start is something that is asked for, and
there are exactly three asks: a `tools/call` naming one of its tools (matched to
the row by prefix, because a sleeping agent has no entry in the reverse map), a
chat thread **explicitly attaching the agent's label** (`exec::resolve` starts it
before it lists), and the App tab. Until then the MCP page shows the row as
**sleeping — the app container is not running**, not as a server that failed.

**Except with a `dev_url`, where the rule has nothing to protect**
(final-review amendment, 2026-09-19). The refusal exists because listing would
*start* something; a row served from a dev server (§3.4) has nothing to start
and nothing to idle-stop, so no `Slot::Ready` ever exists for it and the rule as
written made a dev agent's tools permanently unlistable — a chat thread
attaching its label learned nothing, and the App tab's "attaching the label
starts it" was simply false for the row the owner is actively developing.
`listable_now` is therefore also true when the agent's row carries a `dev_url`,
and the MCP page's detail says **served from a dev server at `<url>`** instead
of "sleeping".

**Stopping the container drops the connection.** Every stop path — the App tab,
delete, disable, rotation, a manifest replace, the idle sweep and the proxy's
502 eviction — goes through `service::stop`, which calls
`McpManager::stop_server` for the agent's row. An `agent:<id>` conn left alive
across a restart holds an `mcp-session-id` the *next* container has never heard
of.

**The row is created once and then only corrected.** On creation it takes the
same defaults the MCP page gives a hand-made row (`ops::DEFAULT_MCP_TIMEOUT_MS`,
the one constant both creation paths read). On every later sync lmgw rewrites
only the three fields it owns — `url`, `tool_prefix`, `agent_id` — and leaves
the rest as the owner set it; a manifest save that reverted someone's timeout or
idle window on the MCP page would be a write nobody asked for. `enabled` is
deliberately *not* among them: the catalog's disable is enforced where it cannot
be worked around, by `service::ensure` refusing to start a disabled agent's
container. And because the url is derived from `bind_addr`, every agent row is
re-derived **at boot and on a settings write that moves `bind_addr`**
(`service::resync_all`) — otherwise a bind address that moved would leave them
pointing at a dead port for good.

Three guard-rail changes this forces:

- `mcp_server_set` **reserves the `agent:` name prefix** — an owner-created
  row named `agent:foo` would be silently adopted (and deleted) by the agent
  lifecycle. Refused with that reason.
- `ops::reject_self_loop` currently refuses any URL on this gateway's
  host+port whose path *ends with* `/mcp`, which would refuse
  `/agents/x/mcp`. Narrow it to the aggregate endpoint exactly (path `/mcp`
  or `/mcp/`). That is the more correct rule anyway: `/agents/x/mcp` proxies
  a container, and aggregating it recurses into nothing.
- `tool_prefix` is still validated against `mcp::RESERVED_NAMESPACES`
  (`lmgw`, `docs`), so an agent may not be given either id.

Chat stays in-process. A `chat` agent that wants tools from its own container
gets them this way — through the MCP plane, like every other tool.

### 3.4 Layer 4 — the package

An agent package is an **OCI image with the manifest inside it** at
`/lmgw/agent.json`.

**How lmgw reads it: `podman create` → `podman cp` → `podman rm`** — the
image's filesystem read through a created-but-never-started container. Three
invocations, no trait change (`CommandRunner::run` already takes argv), and
nothing required *of the image*, which is the constraint that matters: a
`--entrypoint cat` read needs a `cat` in there and executes the image to read
its metadata, and an OCI label cannot hold kilobytes of prompt JSON legibly.

**Install** = an image reference. `agent_install { image, pull, replace,
validate_only }` (`agents/package.rs`):

1. `podman image exists <image>` → if absent, honour `pull` (below).
2. `podman create --pull=never --name lmgw-pkg-<rand> --label lmgw.instance=…
   --label lmgw.kind=agent --label lmgw.run=package -- <image>` (never started).
   `--pull=never` because step 1 has already settled the download question *and
   said so*; a create that could quietly fetch would make the visible policy a
   suggestion. Measured on podman 5.8.4: this succeeds on a `FROM scratch`
   image with no `CMD` at all, so "nothing required of the image" holds.
   **Labelled like every other container this instance makes** (WP5 review): a
   SIGKILL between the create and the `rm -f` would otherwise leave a container
   boot reconciliation cannot see, since §6.4 filters on
   `lmgw.kind=agent` + `lmgw.instance=<prefix>`. `lmgw.run=package` is not a job
   id, which is exactly the "collect it" answer §6.4 gives a run label it cannot
   parse — the same reading `service` gets. The run directory is named
   `pkg-<container>` and `sweep_run_dirs` knows the prefix, for the same reason.
   **`--` before the reference, and a guard in front of it**: `podman image
   exists --help` exits `0`, so a reference that reads as a flag would come back
   "present" and every diagnosis after it would be about the wrong thing. A ref
   starting with `-`, empty, or containing whitespace is refused as
   `image_ref_invalid` before podman is asked anything.
3. `podman cp <name>:/lmgw/agent.json <rundir>/agent.json` → the manifest text.
   **WP5 amendment**: not `cp … -`, which writes a **tar stream** to stdout —
   `CommandRunner::run` buffers stdout into a `String`, so a tar there would be
   lossy UTF-8, and untarring it by hand would be a second archive reader to
   keep correct. The copy goes to a `0700` directory on the run tmpfs
   (`RunDir::create_named`, `pkg-<name>`) and is read back from there; `Drop`
   removes it on every path, panic included.
4. `podman rm -f <name>` — always, including on every error path (`Created`'s
   own `Drop` spawns it if the happy path did not get there).
5. `manifest::load`, then the catalog row is written **through `import_inner`**
   with `source = 'imported'` and the provenance below. Not a second write
   path: the validation, the tool-gap requirements, `local_image_on_import`,
   the token resync and the `agent:<id>` MCP row are all the import's, so an
   install and a dropped file cannot diverge.

A missing `/lmgw/agent.json` fails with "the image `<ref>` carries no
`/lmgw/agent.json`; an lmgw agent package puts its manifest there".

**Errors carry a code** in the ops plane's own `"{message} ({code})"` shape:
`image_absent_pull_never`, `package_no_manifest`, `package_create_failed`,
`image_pull_failed`, `podman_unavailable` — and an invalid manifest is the
import's own error, quoted verbatim rather than paraphrased.

**`replace` defaults to `false`** (WP5), the import *endpoint*'s default rather
than `agent_set`'s: the id being installed is the one **inside** the image,
which the caller has not read yet, so overwriting an existing agent has to be
asked for. `validate_only` reads the image and writes nothing.

**When the manifest inside names a different `run.image` than the reference
that was installed, the manifest wins** (WP5 decision). The document is stored
**verbatim** — rewriting it would mean the exported file no longer matched the
package it came from, and `run.image` is what every phase and the app actually
start (WP2). The install report says the two disagree, and the row carries
`install_image_mismatch` (non-blocking) for as long as they do, so nothing is
silent. `agent_pull` and `agent_reimport` follow the same rule: they act on the
manifest's `run.image`, falling back to `provenance.image` only for a row that
declares none (the `dev_url` case).

**Pull policy is a visible field**, default `never` — the reasoning
`run_throwaway` spells out for `--pull=never`: an image that is not on the box
must not turn an install or a Start into a multi-gigabyte download nobody
asked for. `never` fails fast naming the image; the UI then offers an explicit
**Pull image** action that runs `podman pull`. `missing` and `always` are
selectable and printed next to the image field, so the choice is never
implicit.

**WP5 amendment — `agent_pull` is a synchronous op, not a jobs row.** §8 said
"as a jobs row with progress and a Cancel"; two things argued it down. (a) The
op's *answer* is now §12's image-update comparison — old digest, new digest,
and whether the manifest inside the new image differs — which is a reply to the
press, not a progress bar to poll; a jobs row would scatter that answer across
a poll the Runtime block has no surface for. (b) `podman pull` on a pipe emits
"Copying blob … done" lines, not a byte total, so a `JobProgress{done,total}`
would either stay `None` or be a number lmgw invented — the thing this design
refuses everywhere else. The cost is honest and stated: **a pull of a large
image holds its HTTP request open for as long as podman takes**, the button
says "Working…", and nothing is bounded by a timeout lmgw made up. If a
progress bar is ever wanted, the jobs row is the way and this is the note that
says why it was not built yet.

**`pulled` is a digest comparison, not the policy** (WP5 review): `always` on
an image that has not moved copies nothing, and reporting a download that did
not happen would be lmgw inventing an event. No digest before and one after is
an arrival; the same digest on both sides is `pulled: false`.

**Pressing Pull image pulls under every policy, `never` included** (WP5).
`never` is what stops a *Start* from becoming a download; the button is exactly
the explicit act this section says the UI then has to offer, so the press is
the consent. `always` therefore forces nothing extra — it is already the
policy's own behaviour on the install path.

**Export** = manifest + image reference, config values excluded as today
(`without_secrets`, `config_omitted`). `ENVELOPE_KEYS` grows a fifth entry,
`portability` — `{ portable: bool, notes: [] }`, always written, portable or
not, because a key that appears only on failure is a key nobody looks for. An
image reference starting with `localhost/`, or with no registry host at all, is
local to the machine that built it, so the export carries *"the image
`localhost/mail-labeler:1` is local to the machine that built it; the receiver
must build or retag it"* and the UI shows it before the download. Not a refusal
— exporting to the same box is the common case. Two more notes join it (WP5): a
row with a `dev_url` says the app works here for a reason that is **not in the
file**, and a container manifest with no `run.image` says the receiver has
nothing to run it with. `AgentDetail.portability` is the same value from the
same function, so the dialog and the downloaded file cannot disagree.

Three things on the row are **never** in an export, with or without
`include_config`: the agent's **token** (a credential of *this* gateway),
`provenance` (where *this* box got the package) and `dev_url` (a path on *this*
desk). The last one has a **redaction** to go with it (WP5 review): the
portability note has to exist in the file — "this agent works here because of
something the file does not contain" is the whole point of the line — but it
must not carry the *address*, because an export is a document that gets mailed
around. So `portability(agent, redact_dev_url)` is one function with two
renderings: the file says "a dev server on the exporting machine", and the
detail page, which is showing the owner their own row, names the URL.

**A `pull: never` policy is not a portability concern** and is deliberately not
in the notes: the pull policy travels in the manifest and is the *receiver's*
own decision about downloads on *their* box, not something the exporter's
machine makes untrue.

**Dev override.** A nullable `agents.dev_url` column points service mode's
proxy at an already-running dev server instead of a container, for the
hot-reload loop outside Podman. On the **row, not in the manifest**, and never
exported: a manifest naming `localhost:5173` would ship a broken agent. It
overrides service mode only; run/apply still need an image, because their dev
loop is a rebuild.

WP5, in detail:

- **Validated before it is stored** (`service::validate_dev_url(raw,
  bind_addr)`): http(s), **no userinfo** (lmgw would store that credential on a
  row it reads back onto the page and replay it upstream on every request), no
  query and no fragment (the proxy appends the request's own path and attaches
  its own query, so a stored one would be dropped silently), and the host must
  be **loopback — `localhost`, `127.0.0.0/8`, `[::1]` — and nothing else**
  (WP5 review decision; §2 states the posture it follows from). Not the RFC 1918
  ranges, not `169.254/16`, not ULA or link-local, not `*.local`/`*.lan`/
  `*.internal`/`*.home.arpa`: every one of those would make the unauthenticated,
  CORS-permissive app proxy a reverse proxy for another machine.
  **Nor lmgw itself**: a `dev_url` on `settings.bind_addr`'s port makes
  `/agents/<id>/app/` proxy into the router that serves it, re-entering on every
  hop until the file descriptors run out, so it is refused naming the loop. The
  comparison is on the **port**, deliberately — `127.0.0.1`, `localhost` and
  `::1` all reach the same listener, so matching host strings would wave the
  loop straight through. A trailing slash is trimmed; a path prefix is kept,
  because a dev server mounted under one is a real shape (vite, Tauri) and the
  proxy just concatenates — and an origin-relative `Location` that repeats that
  prefix has it **stripped once** before the mount is prepended, or the next
  request would go to `/base/base/…`.
- **One decision point**: `service::target()` returns `Target::Dev(url)` or
  `Target::Container(live)`, and the proxy asks for *that* rather than for a
  container — so neither mount can forget the override. A `Dev` target holds no
  in-flight guard and is never idle-stopped: lmgw did not start that server and
  does not get to stop it. A dev server that is not answering is a
  `502 agent_dev_url_unreachable` naming it, with nothing started to paper over
  it.
- `ensure_by_id` (the MCP plane's wake-up) returns `Result<(), StartError>` and
  is a **success with nothing done** for a `dev_url` row: a `tools/call` on such
  an agent has to reach the dev server, not fail for want of a container.
- **Setting one stops a running app container** and says so; `agent_service_
  start` refuses with the reason instead of starting one nothing routes to.
  `provides.mcp` is untouched — the row points at lmgw's own stable proxy URL
  either way.
- The row carries a non-blocking `dev_url_active` warning while it is set, so
  the catalog card and the Run tab say the app is not coming from the image.
- **Re-validated when `bind_addr` moves** (final-review amendment,
  2026-09-19). A stored `dev_url` was checked against the bind address *of the
  day it was entered*, and nothing re-asked — so moving `bind_addr` onto a
  stored dev port armed the self-proxy loop above from the other side, silently
  and permanently. The `bind_addr_moved` branch of the settings save (the one
  that already re-points every `agent:<id>` MCP row) now runs
  `validate_dev_url` over every stored value against the new address, **clears**
  the offenders — a dev override is a developer's temporary setting and leaving
  a stored value that is refused on use would be a row claiming a dev server it
  does not have — and says which and why in the save message. Because a bind
  address only takes effect on the next start, the fact is also recorded in the
  KV key `agents:dev_url_cleared` and surfaces after the restart as a
  non-blocking `dev_url_cleared` warning on the row, cleared the next time the
  owner sets or clears that agent's `dev_url`.

## 4. Manifest changes

### 4.1 `run.kind = "container"`

```json
"run": {
  "kind": "container",
  "image": "localhost/mail-labeler:1",
  "pull": "never",
  "entrypoint": null,
  "args": [],
  "columns": ["date", "from", "subject"],
  "review": { "editable": ["category"] },
  "phases": ["run", "apply"],
  "limits": { "memory_mb": 512, "cpus": 2.0, "pids": 256,
              "deadline_seconds": 600, "stop_grace_seconds": 10,
              "read_only": true },
  "service": { "port": 8080, "health_path": "/", "idle_seconds": 300,
               "start_timeout_seconds": 30 },
  "provides": { "mcp": "/mcp" },
  "output": { "type": "object", "properties": { … } }
}
```

| field | required | meaning |
|---|---|---|
| `image` | yes unless `service` + a row `dev_url` | OCI reference. `localhost/…` is accepted and flagged on export |
| `pull` | no, default `never` | `never` \| `missing` \| `always`. Shown, never implicit |
| `entrypoint`, `args` | no | override the image's; `args` is appended after `LMGW_PHASE` is already in env, so an image usually needs neither |
| `columns` | no | review-table header order. Declared here because `Row::columns` is a map and this build's maps are alphabetical — the same reason `batch::review_columns` exists |
| `review` | no | the existing `Review` type, unchanged |
| `phases` | no, default `["run"]` | which phases this image implements. `["run","apply"]` is the review-gated shape |
| `limits.*` | no | table below |
| `service.*` | no | absent = no service mode, no proxy route, no App tab. `port` (required, 1–65535), `health_path` (default `/`; **empty** = TCP connect), `idle_seconds` (default `300`, `0` = never), `start_timeout_seconds` (default `30`, `0` = wait as long as it takes) |
| `provides.mcp` | no | path inside the container; requires `service` |
| `output` | no | per-phase map `{ "run": <schema>, "apply": <schema> }`; each key must be a declared phase (validation error naming the key otherwise); a phase without a key is unvalidated and the Runtime block says so. One schema for both phases made the field unusable for §9's `{applied, labels}` apply shape (WP2 amendment, decided during implementation; approved 2026-09-19) |

**Limits — visible, with defaults printed on the Run tab. `0` always means
"no limit", never a hidden fallback**, and each field's `description` says so
in those words:

| field | default | `0` means | podman |
|---|---|---|---|
| `memory_mb` | `512` | no cgroup memory limit | `--memory <n>m`, flag omitted at `0` |
| `cpus` | `2.0` | no CPU quota | `--cpus <f>`, omitted at `0` |
| `pids` | `256` | no PID limit | `--pids-limit <n>`, omitted at `0` |
| `deadline_seconds` | `600` | unbounded — the run ends when the container does | enforced by lmgw; the cancel sequence then runs |
| `stop_grace_seconds` | `10` | **not a "no limit"** — no grace at all, SIGKILL at once | `podman stop -t 0` |
| `read_only` | `true` | — | `--read-only --tmpfs /tmp` |
| `service.idle_seconds` | `300` | never idle-stop (`McpServer::idle_seconds`' own semantics) | — |
| `service.start_timeout_seconds` | `30` | the first request waits as long as the container takes to answer its health probe | — |

Nothing here has a mandatory ceiling; an owner who sets `0` gets no limit and
the Run tab says "unlimited" rather than printing a number lmgw invented.
`stop_grace_seconds` is the one exception and is labelled as such: `0` there
is *stricter*, not looser — SIGKILL with no chance to flush, which the Run tab
spells out rather than calling it unlimited. No
size on the `/tmp` tmpfs either: tmpfs pages are charged to the container's
memory cgroup, so `memory_mb` already bounds it. Not configurable and always
applied: `--cap-drop=ALL`, `--security-opt no-new-privileges`, the default
network.

### 4.2 `script` — sugar

`manifest::Step` is `{ tool, args, turn }` today. It gains **two** fields:
`script`, and `output` — the schema the script's return value is validated
against, exactly as `Turn.output` is validated for a turn. Without it a
script step would be the only step in the system that can return anything at
all, and §9's converted mail labeler would silently lose the
`{applied, labels}` contract its `turn` had.

```json
"apply": {
  "script": ["export async function apply(ctx) {", "  …", "}"],
  "output": { "type": "object",
    "properties": { "applied": { "type": "integer" },
                    "labels": { "type": "array", "items": { "type": "string" } } },
    "required": ["applied", "labels"] }
}
```

`script` is a JS **module** (ESM) exporting one function per phase; the first
hook is `apply(ctx)`. The value may be a **string** or an **array of lines**
joined with `\n` — an array is what survives hand-editing a manifest in the
Definition tab without a wall of `\n`.

lmgw runs it in a stock Node image: `Settings.agent_script_image`, a visible
field on the Settings page, default `docker.io/library/node:24-alpine`
(verified to exist, built 2026-09-17). Everything else — limits, cancel,
logging, the ledger — is §3.2 and §6 unchanged. That is the point: `script`
is not a second runtime.

**Pull policy `missing` for this image, and only this image** (WP3 decision).
`run.pull` is the owner's choice about an image *they* built; the script image
is lmgw's own choice of runtime, and a first script run failing with "not on
this box" would be a setup step nobody was told about. It stays visible rather
than implicit: `--pull=missing` is in the argv and the run log's first line
names the image and the policy. Blanking the Settings field restores the
default instead of leaving a script step with no image, and says so.

**The shim.** `crates/lmgw-core/assets/agent-shim.mjs`, embedded with
`include_str!`, written content-addressed to
`<data_dir>/agents/shim/<sha256[..8]>.mjs` on first use (so an upgrade can
never serve a stale copy) and bind-mounted read-only at `/lmgw/shim.mjs` with
`:Z`. Entrypoint `node /lmgw/shim.mjs`. Its contract:

*Reads*

- `LMGW_INPUT` → `{ phase, agent: { id, name }, run: { id }, config, rows }`.
  `config` carries no `secret` field — the shim does not strip them, **lmgw
  never writes them**: `input.json` is `container::public_config` and a script's
  `secrets.json` is `secrets_document(…, with_config = false)`, so the values
  never enter the container at all. A script is the deterministic half of an
  agent and has no business holding a credential; a step that needs one is a
  container, not a script.
- `LMGW_SECRETS` → `{ "token": "…" }`. For a script run that is the only
  entry; the token never appears in `ctx`.
- `/lmgw/script.mjs` — the manifest's `script`, written into the run directory
  and mounted read-only. Imported with a dynamic `import()`.

*Builds* `ctx`:

| member | shape |
|---|---|
| `ctx.rows` | `[{ id, …output fields }]` — exactly `Row::for_apply`'s shape, including its 2026-09-18 rule that review **columns are not included** |
| `ctx.config` | `input.config` as it stands — the effective config, which lmgw wrote without its `secret` fields |
| `ctx.tools.call(name, args)` | `POST $LMGW_MCP_URL`, JSON-RPC `{"jsonrpc":"2.0","id":<n>,"method":"tools/call","params":{"name","arguments"}}`, headers `Authorization: Bearer <token>` and `X-Lmgw-Run: <run>`. Returns `result.structuredContent` when present, else the first `content[]` text block parsed as JSON. A result that is neither — prose — **throws**, with catalog §2.2's message: *"the tool '`<name>`' returned text that is not JSON"*. Never handed back verbatim: a script that string-matches on prose is the guessing the batch executor already refuses to do |
| `ctx.log(message)` | one `{"type":"log"}` line on stdout |
| `ctx.agent`, `ctx.run` | `{ id, name }` / `{ id }` |

*Writes*: `ctx.log` lines as they happen; the hook's resolved value as one
`{"type":"output","output":…}` line; exit `0`.

*Error mapping*

| condition | result |
|---|---|
| the module exports no function named for the phase | stderr "the script exports no `<phase>` function", exit `1` |
| `tools/call` returns a JSON-RPC error | throw `Error(<error.message>)` |
| the result has `isError: true` | throw `Error(<first text block>)` |
| the result is prose | throw, with the §2.2 message above |
| an uncaught throw | message + stack on stderr, exit `1` → job `failed` with that message |
| the resolved value fails the step's `output` schema | lmgw fails the job at close, naming the mismatch (§3.2) |

**Cancel.** The shim installs a `SIGTERM` handler that flips a flag; every
`tools.call` *after* it rejects immediately with error code `cancelled`, and
the one already in flight is allowed to finish. So a cancel lands between two
writes, never inside one.

Flipping the flag is not enough on its own, and WP3's review measured why:
installing a handler **suppresses node's default exit**, so a script that is
sleeping rather than calling never notices and sits until SIGKILL (5.19 s,
exit 137). The shim therefore counts in-flight `tools.call` fetches and calls
`process.exit(1)` the moment the flag is up and the count is zero — at once if
nothing was in flight, otherwise as soon as the last write lands. The container
then exits on its own well inside the visible `stop_grace_seconds`, and
`podman stop`'s SIGKILL is the backstop it was meant to be. Partial writes are not guesswork afterwards: every `tools/call`
is a Logs row as it happens (§2, principle 3), so the run log shows exactly
which ones landed before the stop.

Nothing else exists: no row emission (a script *is* the apply step; rows come
from the review gate), no network helper beyond `tools.call`, no filesystem
beyond Node's own against a read-only rootfs. The shim's code is not in this
spec on purpose — the contract is, and the code is WP3's.

### 4.3 Removals and validation

**`apply.turn` is retired as a *warning*, not an error.** `Manifest::errors`
feeds `validate()`, which feeds `manifest::load`, which feeds
`Agent::from_row` — so making it an error would brick every stored manifest
that has one. `list_inner` degrades gracefully (`api_agents.rs:268–278`: the
card still appears, carrying the reason), but `detail_inner` goes through
`load_agent` and turns the failure into a 400, which would put the Definition
editor out of reach of the very manifest that needs editing. So:

- `apply.turn` produces the catalog §5.3 treatment: the agent imports and
  loads, the card and the Run tab show *"apply may not run a model turn: the
  apply step writes, and a model deciding what to write is neither
  deterministic nor reviewable. Use a direct tool call, a `script`, or
  `run.kind: container`."*, and **Start is disabled with that reason**.
- `detail_inner` gains `list_inner`'s degradation: an unreadable or
  warning-carrying row returns a detail document with `error` set instead of a
  400, so the editor is always reachable. This also fixes the pre-existing
  case — a manifest written by a newer build currently 400s its own detail
  page. A warning-carrying row needs nothing special: it parses, so the
  ordinary document already carries its warnings. An **unreadable** one comes
  back with whatever the raw JSON still yields (`name`, `version`,
  `run.kind`), the stored text verbatim for the editor, `requires_ok: false`,
  and one `manifest_unreadable` warning with `blocks_start` (WP3).
- `Turn` survives as a step form for `source` and `fetch`. The per-item
  classify call is untouched: it is not a `turn` and never was.

Other rules:

- A `Step` is **exactly one** of `tool` / `turn` / `script`. Two is an error
  naming both; zero is the existing error. `output` without `script` is an
  error naming the field (a turn carries its own `Turn.output`).
- `run.kind = "container"` with no `image` and no `service` → error.
  `provides.mcp` without `service` → error. `service.port` outside 1–65535 →
  error. A `provides.mcp` `tool_prefix` colliding with
  `RESERVED_NAMESPACES` → error naming it.
- `limits.*` below zero → error. `0` is always legal and always means "no
  limit" (§4.1), except `stop_grace_seconds`, where it means the opposite.

**The warnings channel has to be built; it does not exist yet.** There is no
`Manifest::warnings()`. Catalog §5.3's warnings are entirely
`ToolSurface::check` → `Requirement` → `dto::AgentRequirement`
(`api_agents.rs:88–100`) — a *tool-gap* list with no room for anything else.
So `api_agents` computes a second list beside `requirements`, of the same
shape and rendered in the same place on the card and the Run tab:

```rust
struct AgentWarning { code: &'static str, message: String, blocks_start: bool }
```

| half | computed from | codes |
|---|---|---|
| manifest-derived | the parsed `Manifest`, pure and unit-testable | `apply_turn`, `script_without_output`, `container_without_image`, `local_image_on_import` |
| runtime-derived | the box and the row, alongside `ToolSurface::load` | `podman_unavailable`, `image_absent_pull_never`, `secrets_dir_fallback` (§6.2), `builtin_update_available` (§5.1) |

`requires_ok && warnings.none(blocks_start)` is the new Start gate. Splitting
the two halves is not decoration: the manifest half is a pure function of the
document, so it can be tested without a gateway and can run inside the import
report, while the runtime half changes between two page loads and must not be
baked into a stored row.
- The `Phase` enum gains `Run` (`"run"`). `Phase::ALL` becomes five, so
  `Phase::names()` picks it up for free. `List`/`Classify`/`Rerun` stay
  batch-only; `Apply` is shared. An op asking for a phase the manifest's
  `run.kind` does not have is refused naming both.

## 5. Storage

Migration `0032_agent_containers.sql`. SQLite cannot alter a `CHECK`, so the
`kind IN ('key','internal')` constraint from `0029_key_policy.sql` is replaced
by the create-copy-drop-rename rebuild `0030_usage_fidelity.sql` already uses.
Written out in full because `request_logs.key_id` (`0027`) and
`usage_hourly.key_id` (`0028`) point at these ids, so they must survive the
copy verbatim. No `PRAGMA foreign_keys = OFF/ON` bracket, unlike the rebuilds
in `0002`, `0008`, `0013` and `0018`: nothing in the schema declares
`REFERENCES api_keys` — both `key_id` columns are soft links (`0028`'s is a
plain `INTEGER NOT NULL DEFAULT 0`), so there is no constraint to suspend.

```sql
CREATE TABLE api_keys_new (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,  -- as 0001_init.sql:40; see below
    name              TEXT NOT NULL UNIQUE,
    key_hash          TEXT NOT NULL,
    key_plain         TEXT,                          -- kind='agent' only (§3.1); 0600 DB, like mcp_servers.env
    enabled           INTEGER NOT NULL DEFAULT 1,
    created_at        TEXT NOT NULL DEFAULT (datetime('now')),
    scope_mode        TEXT NOT NULL DEFAULT 'all'
                      CHECK (scope_mode IN ('all','allow','deny')),
    scope_patterns    TEXT NOT NULL DEFAULT '',
    budget_micro      INTEGER NOT NULL DEFAULT 0,
    budget_period     TEXT NOT NULL DEFAULT 'month'
                      CHECK (budget_period IN ('day','month','total')),
    rpm_limit         INTEGER NOT NULL DEFAULT 0,
    tpm_limit         INTEGER NOT NULL DEFAULT 0,
    concurrency_limit INTEGER NOT NULL DEFAULT 0,
    expires_at        TEXT,
    note              TEXT NOT NULL DEFAULT '',
    kind              TEXT NOT NULL DEFAULT 'key'
                      CHECK (kind IN ('key','internal','agent')),
    agent_id          TEXT,
    CHECK (kind <> 'agent' OR (key_plain IS NOT NULL AND agent_id IS NOT NULL)),
    CHECK (kind =  'agent' OR (key_plain IS NULL     AND agent_id IS NULL))
);
INSERT INTO api_keys_new
     (id, name, key_hash, enabled, created_at, scope_mode, scope_patterns,
      budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit,
      expires_at, note, kind)
SELECT id, name, key_hash, enabled, created_at, scope_mode, scope_patterns,
      budget_micro, budget_period, rpm_limit, tpm_limit, concurrency_limit,
      expires_at, note, kind FROM api_keys;
DROP TABLE api_keys;
ALTER TABLE api_keys_new RENAME TO api_keys;
CREATE UNIQUE INDEX api_keys_agent ON api_keys(agent_id) WHERE agent_id IS NOT NULL;
-- AUTOINCREMENT is kept deliberately. Without it SQLite hands out max(id)+1 and
-- therefore **re-uses a deleted row's id** — and `agent_delete` now deletes key
-- rows, so the next key created would silently inherit the deleted agent's
-- `request_logs.key_id` / `usage_hourly.key_id` history. The explicit `id`
-- column in the INSERT above is what preserves the existing ids across the copy;
-- AUTOINCREMENT only governs what comes after.

-- Package provenance and the dev override (§3.4).
ALTER TABLE agents ADD COLUMN provenance TEXT NOT NULL DEFAULT '{}';
ALTER TABLE agents ADD COLUMN dev_url TEXT;

-- An agent-owned MCP registration (§3.3), removed with its agent.
ALTER TABLE mcp_servers ADD COLUMN agent_id TEXT;
```

- `provenance` is one JSON column — `{ image, digest, manifest_path,
  installed_at, pulled_at }` — matching the house convention (`manifest`,
  `config`, `extra_run_args` are all JSON text) and leaving room for the fields
  a package format grows. `digest` comes from
  `podman image inspect --format '{{.Digest}}'` at install; it is present for a
  **locally built** image too, which is what makes "has this tag moved?"
  answerable for `localhost/…`. WP5 added `installed_at` beside `pulled_at`
  because they are two facts: an install sets both, `agent_pull` moves only
  `pulled_at`, so "installed in March, re-pulled yesterday" stays readable
  instead of being lost to one overwritten timestamp. A column this build
  cannot parse is *no* provenance, never an error — it is a record of where
  something came from, and losing it must not take the agent down with it.
  `AgentRow` grows `provenance: String` and `dev_url: Option<String>`, and
  `AgentDetail` grows `provenance` (absent for a row nobody installed from an
  image), `dev_url` and `portability`.
- `agents.source` needs no change: the existing
  `CHECK (source IN ('builtin','imported','authored'))` already covers an
  image install as `imported`.
- Still **not on the `Snapshot`** (catalog §3) — except `api_keys`, which
  already is, so the agent token is on the hot path for free and
  `verify_api_key` needs no query.
- Runs remain `jobs` rows of kind `agents::JOB_KIND` (`"agent_run"`), key
  `agents::job_key(id)` (`agent:<id>`). `Input` gains `ledger: bool`, which
  makes the job wait on the ledger instead of running a manifest; `phase` is
  the new `Phase::Run`, `rows` already carries the reviewed rows for apply.
  `Run::dispatch` refuses `Phase::Run` for the `agent_run` op, naming the
  ledger route (WP1 amendment).

**Config pruning is every replace's job, not just an upgrade's** (WP5 review).
"Replacing keeps the stored config" (catalog §5) has to stop at a value whose
**field the new manifest no longer declares**: `validate_values` refuses an
undeclared key, so such a row fails on every run, every apply and every
`agent_config_set` that touches it — unusable *and* unfixable from the form
that exists to fix it. `store::update_agent_manifest_pruned` does what
`put_builtin_manifest` already did (one transaction, `prune_config_tx` shared
between them) and `AgentImportReport.dropped_config` names what went, so
`agent_set` with `replace`, the import endpoint with `replace=1` and
`agent_reimport` all behave the same way. The safety valve for a row already in
that state, left by an older build: `agent_config_set`'s `clear` accepts a name
the schema does not declare and says it did (`cleared_undeclared`).

### 5.1 Upgrading a shipped built-in

WP3 rewrites the mail labeler's apply step, and today nothing would deliver
it: `seed()` inserts a built-in **once**, records its id in the KV key
`agents::SEEDED_KEY`, and skips it forever after (`seed.rs:93–136`). An owner
running 0.1.58 would keep the `apply.turn` manifest and its new warning until
they noticed the Reset button.

The KV value changes from `["<id>", …]` to `{"<id>": "<sha256 of the embedded
manifest text at the moment it was seeded>"}`. On startup, for each embedded
built-in whose row still has `source = 'builtin'`:

| stored hash | stored manifest | action |
|---|---|---|
| known, equals the recorded hash | never edited since seeding | **replaced** by the new embedded manifest; the hash is updated; config values are kept, exactly as `agent_reset` keeps them |
| known, differs | the owner edited it | left alone; the card shows *"a newer built-in manifest ships with this version"* next to the existing **Reset to shipped** |
| absent (the legacy array form, i.e. every install before this version) | unknowable | left alone with the same notice — lmgw will not guess whether a row it has no hash for was edited. The startup pass **rewrites the array into the map form in the same sweep**, recording `sha256` of each built-in's *current stored text*, so the row is under the rule from here on and an unedited one upgrades on the next ship |
| the row is gone | deliberately deleted | still skipped; a deleted built-in stays deleted (catalog §3) |

**An upgrade prunes config it can no longer hold.** "Config values are kept"
is the promise, but a value whose field the new manifest no longer declares is
not kept, it is a landmine: the next Start fails validation with *"'x' is not a
config field"* on a manifest the owner never chose. `put_builtin_manifest`
therefore drops exactly those keys in the same transaction and hands them back,
and both the startup log and `agent_reset`'s message name them (WP3 review).

**And it prunes a value that stopped being a secret** (final-review amendment,
2026-09-19). Pruning by *name* alone left a second, quieter landmine: a replace
that re-declares a former `format: "secret"` field **without** `format: secret`
keeps the stored value, and every reader downstream decides what to mask by
asking the *current* schema — so `AgentDetail.config`, `lmgw__agent_get`, an
export with config, the Config form and the container's `input.json` would all
start printing a credential that was entered into a masked field. A manifest
replace is not a reveal. The prune rule is therefore by **(name, was-secret)**:
a stored value goes when the new manifest does not declare its key *or* when
the old field was secret and the new one is not, and it is reported in
`dropped_config` exactly like an undeclared key. This applies on every replace
path — the import/`agent_set` replace, `agent_reimport`, `agent_reset` and the
startup built-in upgrade — because they all prune through one function.

**Every writer of a built-in manifest updates the map, in the same
transaction as the row.** `agent_reset` (§9's one-click path) and
`agents_restore` (`seed.rs:308`) both write `{id: sha256(embedded text)}`
alongside the manifest. Without that, a reset row would carry the *new*
manifest against the *old* recorded hash, read as "edited", and stay pinned
out of the upgrade path for good — the exact trap this section exists to
close.

So an existing install gets a one-click **Reset to shipped** on the mail
labeler, and every install from this version forward moves built-ins forward
on its own. The legacy-form row is the one-time cost of not having recorded a
hash the first time.

**The legacy row is pinned, not guessed at** (WP3 review ruling). Recording
`sha256` of the current stored text would have been the same as *claiming* the
row was never edited: the next start would read that hash back, find it
matching, and replace the owner's edit behind their back. So the sweep records
a real hash only when the stored text already equals the embedded text (the row
provably needs nothing); otherwise it records the sentinel **`"legacy"`**,
which is not 64 hex characters and therefore can never equal a `sha256`. A
pinned row is never auto-replaced, at this start or any later one. It still
shows `builtin_update_available` and **Reset to shipped**, and the reset writes
a real hash — which is what puts the row back under the rule.

The card's notice itself is not computed from the hash at all: it is "what is
stored differs from what ships", compared as parsed manifests rather than as
text so a serialization change in an older build does not read as an edit.
That is true for an edited row and a legacy row alike, and false the moment
the startup pass has moved an untouched one forward.

## 6. Runtime

`crates/lmgw-core/src/agents/container.rs`, beside `batch.rs`, driven by the
same `AgentRunExecutor` — one job kind, two run shapes, so every run reaches
the same surface.

### 6.1 The runner

Foreground, one invocation per phase:

```
podman run --rm --replace --name <prefix>-agent-<slug(id)>-<run>
  --label lmgw.instance=<container_prefix> --label lmgw.kind=agent
  --label lmgw.agent=<id> --label lmgw.run=<run>
  --memory 512m --cpus 2.0 --pids-limit 256
  --read-only --tmpfs /tmp
  --cap-drop=ALL --security-opt no-new-privileges
  -e LMGW_BASE_URL=… -e LMGW_AGENT=… -e LMGW_RUN=… -e LMGW_PHASE=… …
  -v <rundir>/input.json:/lmgw/input.json:ro,Z
  -v <rundir>/secrets.json:/lmgw/secrets.json:ro,Z
  [-v <shim>:/lmgw/shim.mjs:ro,Z  -v <rundir>/script.mjs:/lmgw/script.mjs:ro,Z]
  [--entrypoint …] <image> [args…]
```

Name: `runtime::slug(agent_id)` under `settings.container_prefix` — the prefix
the model containers already use, which is what keeps a dev instance from
colliding with the real one (`runtime::container_name`'s own reasoning, and a
collision this project has paid for once).

**A new process seam is needed.** `registry::CommandRunner::run` buffers to
completion (`TokioRunner` calls `tokio::process::Command::output()`), so it
cannot deliver a live row. `agents::container` gets its own small trait — a
spawn yielding `(stdout lines, stderr lines, exit status)` as streams — with a
`TokioSpawner` and a fake for tests: the split `registry::CommandRunner` made,
for the same reason. The registry's trait is left alone; bending it would
force every existing fake to grow streaming. Everything non-streaming reuses
what exists: `Registry::rm_force`, `Registry::logs_tail`,
`Registry::container_exists` (its three-way `Presence` — "absent" and "podman
could not answer" mean opposite things here too), and
`Registry::run_throwaway` for the pull-policy probe.

Measured start cost on this box: **0.28 s** (rootless, Podman 5.8.4, alpine) —
cheap enough that one invocation per phase needs no pooling.

### 6.2 Input and secrets

Per run, a directory on a **host tmpfs**: `$XDG_RUNTIME_DIR/lmgw/<slug(container_prefix)>/run-<run>/`
(the prefix level is what keeps a dev instance's `run-7` off the real one's —
job ids are per database, `$XDG_RUNTIME_DIR` is per user; WP2 amendment),
mode `0700`, created by lmgw and `rm -rf`'d when the run ends (and swept at
boot with the container reconciliation). Files are `0600` and mounted
`:ro,Z` — a narrow, per-run relabel, never a broad path.

| file | contents |
|---|---|
| `input.json` | `{ phase, agent: {id, name}, run: {id}, config, rows }`. `config` is the effective config **without** secrets; `rows` is the reviewed rows (`Row::for_apply` shape) for `apply`, absent otherwise |
| `secrets.json` | `{ token, config: { <secret field>: <value> } }`. For a `script` run, `config` is empty (§4.2) |
| `script.mjs` | the manifest's `script`, when present |

If `XDG_RUNTIME_DIR` is unset, lmgw falls back to
`<data_dir>/agents/<slug(container_prefix)>/run-<run>/` and raises the `secrets_dir_fallback`
`AgentWarning` (§4.3) — on the card and the Run tab, not a log line someone
has to go looking for. "Your tokens are being written to persistent storage" is not a
`tracing::warn!`-grade fact.

### 6.3 Cancel

`jobs::cancel` → the runner runs `podman stop -t <stop_grace_seconds>`
(SIGTERM, grace, SIGKILL — podman's own sequence), then waits for the child.
**Bounded escalation, every step in the run log** (WP2 review finding): if the
child has not exited when `podman stop` returns — the container may not exist
yet because `podman run` is still pulling, or the stop itself failed —
`Registry::rm_force(name)` follows, and if the child is still there the runner
drops it (`kill_on_drop`), so neither a cancel nor a deadline can wedge a job
in `running`; the cancel poll stays armed during the deadline sequence. Each
step is bounded by the visible `stop_grace_seconds`, no other constant.
The run ends as `JobOutcome::CanceledWith(result)` carrying every row, log
line and output recorded up to that moment. This also fixes the bug the first
real run hit: cancellation no longer depends on an in-process loop noticing a
flag between turns, because the *process* is what gets stopped.

The deadline is the same sequence on a timer, and ends `failed` rather than
`canceled`, because nobody asked for it to stop.

**Runs lmgw cannot signal** — ledger-opened (§3.2) and service-mode-driven —
have no child process to stop, so Cancel is a state change instead: the run is
marked cancelled at once and the ledger routes start refusing with
`run_cancelled`. The writer learns on its next `events` POST, which bounds the
lie to one event. Their deadline runs from the open, and expiring with no
`close` ends the run `failed` with reason `no_close`. Boot reconciliation
(§6.4) covers them too: `fail_orphaned_jobs` already closes every run row a
restart interrupted, so a ledger run whose writer died across a restart is
failed by the mechanism that was already there.

### 6.4 Boot reconciliation

On `AppState::init`, after `fail_orphaned_jobs`, spawned rather than awaited
(`podman ps` has no bound on a slow or absent podman, and `lifecycle::boot` is
spawned for the same reason; `reconcile()` is a plain function tests call
directly):
`podman ps -a --filter label=lmgw.kind=agent --filter
label=lmgw.instance=<container_prefix> --format json`. Every container whose
`lmgw.run` label is not a live job row → `Registry::rm_force`. The run
directories under `$XDG_RUNTIME_DIR/lmgw/` are swept by the same rule. This
mirrors `Registry::reconcile`'s policy without sharing its map: an agent
container is never adopted, only collected — there is nothing to adopt, since
the job that owned it is already failed.

**Nothing younger than this process is a leftover** (final-review amendment,
2026-09-19). Because the pass is *spawned*, the router is already serving while
it runs: the first App-tab request can have a `service-*` container and its run
directory up before `podman ps` even answers, and "every agent container whose
`lmgw.run` is not a live job" would collect it seconds after it came up. Both
halves of the sweep therefore skip anything created at or after
`AppState::started_at_utc` — podman's `Created` for a container, the directory
mtime for a run directory. The comparison errs towards keeping (podman's
timestamp has second granularity), which costs at most one leftover collected
on the next boot instead of this one.

**Both roots, and every prefix under the persistent one** (same amendment).
`runs_root` picks its root per boot — the tmpfs when `XDG_RUNTIME_DIR` is set,
`<data_dir>/agents/` when it is not — so sweeping only the current one left a
boot's worth of `secrets.json` on persistent storage forever. The sweep now
covers this instance's leaf under **both** roots, plus **every** prefix leaf
under `<data_dir>/agents/`, which is this install's own directory and therefore
has nobody else's work in it. It deliberately does **not** walk sibling leaves
under `$XDG_RUNTIME_DIR/lmgw/`: that tree is shared by every lmgw on the box —
which is the entire reason `runs_root` scopes itself by prefix — so doing so
would be one instance deleting a live instance's secrets file, the collision
class the prefix exists to remove. Nothing is lost by the exception: a tmpfs
does not outlive the session, so a stale prefix's leftovers there go on their
own. The corollary is written into the
`container_prefix` Settings help: a second lmgw booting with the same prefix
collects the first one's *live* agent containers, so two instances on one box
need two prefixes.

### 6.5 Service mode

`agents/service.rs`. A service container is `-d` (detached) rather than
foreground, published on an ephemeral host port, and held in a small map
beside the registry's: key `agent id`, value `Live { host port, container,
started_at, last_used, in_flight, idle_seconds, stop_grace_seconds, run dir }`.
Start is claimed once and waiters park on a `watch`, the same
one-start-many-waiters shape `Registry::acquire` uses — N concurrent first
requests must produce one `podman run`. A **stop replaces the entry rather than
removing it** (WP4): a request arriving while the ladder is climbing waits for
it and then starts a clean container, instead of racing its own
`podman run --replace` against the `podman rm -f` that is still going. The start itself runs in a **spawned
task** rather than in the requesting future (WP4): a client that disconnects
halfway through a twenty-second image start must not abort it and leave a
container nobody is tracking. Idle sweep and the in-flight guard are §3.3; the
sweep rides the **same 5 s tick `McpManager::reap_idle` rides**, because the
two idle windows mean the same thing and should not drift into being checked
on two cadences. A service container's stdout/stderr are *not* the ledger; they
go to `podman logs` and are read back by `container::logs_tail`.

Names and labels: `<prefix>-agentsvc-<slug(id)>`, one name per agent rather
than per start (§12's "one instance per agent"), and `lmgw.run=service` —
deliberately not a number, so boot reconciliation's "every agent container
whose `lmgw.run` is not a **live job row**" collects a leftover service for
free. The same rule extends to the run directory: `service-<slug(id)>` under
`$XDG_RUNTIME_DIR/lmgw/<prefix>/`, swept at boot like `run-<n>`, and owned by
the `Live` entry so stopping the service deletes its `secrets.json` with it.
`input.json` carries `phase: "service"` and the public config; there are no
rows, no run id, no ledger URL and no deadline — a service has no run to report
to and is bounded by its idle window.

## 7. UI

- **Catalog card** (`pages/agents.rs`): the kind badge gains `container`, and
  a service-mode agent shows a second `app` chip. The existing warning line
  carries the new ones (`podman not available`, `image not present`).
- **Detail** (`pages/agent_detail.rs`), Run tab: under the config form, a
  **Runtime** block — image (read-only, with the digest as a tooltip), pull
  policy and every `limits.*` field with its default printed next to it. The
  **token** is not here: it lives on the **Definition** tab beside Export,
  masked, with **Copy token** and **Rotate** (the bullet below, and what WP1
  shipped) — the Runtime block is what a *run* is under, and the token is a
  property of the agent (first sentence corrected in the final review; it and
  the Rotate bullet had disagreed since WP1). The existing budget line
  ("Budget: 64 tool calls, 300 s") gains the container's memory, CPU and
  deadline, because principle 5 now covers both.
- **App tab**, only when `service` is declared: an `<iframe>` at
  `/agents/<id>/app/` plus a full-page link, a running/stopped badge naming the
  container and its host port, the idle window and the start timeout in words
  (with `0`'s meaning spelled out), the last-request/in-flight line, and
  **Start** and **Stop**. The iframe *is* the Start button — loading it is a
  proxied request and a proxied request starts the container; the explicit
  Start is for getting the container (and its `provides.mcp` tools) up without
  looking at its page; it is disabled while a start is already in flight
  (`service.starting`), since one start per agent is the contract. **The
  container's log tail** is under it — `podman logs --tail`, through the agent
  spawner, labelled with the number of lines it is showing. That number starts
  at `container::STDERR_EXCERPT_LINES` (the one excerpt size the runtime has)
  and is a **visible field the reader can raise**, backed by the op
  `agent_service_log { id, lines }`, with `0` meaning the whole log
  (final-review amendment, 2026-09-19): for a detached service container this
  tail is the only account anyone has of it, so a fixed twelve lines was a cap
  on the only diagnostic there is. A service reports nothing through the
  ledger. When `dev_url` is set the
  badge says so and names the URL (WP5) — `dev: http://127.0.0.1:5173` — Start
  is disabled with that as its title, the in-flight line is hidden (there is no
  container for anything to be in flight against), and a **Dev server** field
  with *Use dev server* / *Clear* sits under the buttons with what it does
  spelled out. The catalog card's `app` chip links straight here
  (`/agents/<id>?tab=app`).
- **Rotate**, beside Copy token on the Definition tab, with a confirm that
  names what is holding the current token *right now* — the live run and the
  running app container (§12's open question, answered in favour of saying so
  **before**, and of stopping the container for you).
- **Run surface unchanged.** The review table, the attention split, the
  details modal, the Runs tab and the cost line all read `batch::Row` and the
  job result, so a container run renders identically to an in-process one.
  `BatchShape` learns the container variant: `has_classify = false`,
  `has_apply = phases.contains("apply")`, and `apply_tools` for a script or
  container step is the union of the manifest's `tools[].allowed` — the
  ceiling the token enforces — labelled "may call" rather than "calls".
- **Definition tab** unchanged, including validate-on-save; the new errors and
  warnings render through the existing report. Above Export, WP5 puts the
  **portability line**: one dim sentence when there is nothing local about the
  agent ("never the token, the provenance or the dev_url"), and an amber "this
  export will not run as-is on another box" with the notes as a list when there
  is.
- **Settings** gains `agent_script_image` next to the other image fields.

## 8. Import, export, API

Additions to catalog §5; everything unlisted is unchanged.

| route / op | change |
|---|---|
| `POST /api/agents/{id}/runs` | new (§3.2) |
| `POST /api/agents/runs/{run}/events` | new (§3.2) |
| `POST /api/agents/runs/{run}/close` | new (§3.2) |
| `ANY /agents/{id}/app/{*rest}`, `ANY /agents/{id}/mcp` | new (§3.3) |
| `agent_install` | new op: `{ image, pull?, replace?, validate_only? }` → reads the manifest from the image (§3.4) and returns the same report `agent_set` does, plus `image`, `digest`, `pulled` and `manifest_path`. `pull` defaults to `never`, `replace` to **false** |
| `agent_pull` | new op: `{ id }` → `podman pull`, then the digest comparison of §12: `{ old_digest, new_digest, recorded_digest, changed, manifest_differs, note }`. **Synchronous, not a jobs row** — WP5 amendment, reasoned in §3.4 |
| `agent_reimport` | new op (WP5): `{ id }` → the install path over an existing row, keeping `config`, the way §5.1's built-in upgrade does — **including its pruning**. A package carrying a different id is refused naming both |
| `agent_token_rotate` | new op: `{ id }` → regenerates `key_hash` + `key_plain`, returns the new plaintext, and **stops a running service container**, naming it in `service_stopped` and in the message (§12, answered) |
| `agent_token_get` | new op: `{ id }` → the plaintext, for Copy token; a read of a column that already holds it, not a reveal-once |
| `agent_dev_url_set` | new op: `{ id, url? }` — `null` clears it |
| `agent_service_stop` | new op: `{ id }` → stop the service container now, **including one that is still starting** (`cancelled_start: true` in the answer). Stopping something that is neither running nor starting is success, not an error: "make sure this is not running" is what the button means |
| `agent_service_start` | new op: `{ id }` → start it without opening its page (WP4 addition: the App tab has two buttons, and the iframe only covers one of them). The same on-demand path a proxied request takes — one start per agent however many callers ask |
| `GET /api/agents/{id}` | adds `runtime` (image, digest, pull, limits, podman availability), `token` (`{ name, has_value }` — the value comes from `agent_token_get`), `service` (declared, running, host port, idle window, start timeout, in-flight, and the container's log tail with the line count it is capped at), `dev_url`, `provenance` (absent when the row came from a pasted manifest) and `portability` (the same value the export file carries) |
| `GET /api/agents/{id}/export` | `ENVELOPE_KEYS` gains `portability` (§3.4); the token, `provenance` and `dev_url` are never written |
| `POST /api/agents/import` | unchanged; a manifest with `apply.turn` imports with a warning and a disabled Start (§4.3). **Every replace now prunes** the stored config keys the new manifest no longer declares and reports them as `dropped_config` (WP5 review, below) |
| `agent_config_set` | `clear` may name a field the schema does **not** declare (WP5 review): a value orphaned by an older build's replace is otherwise unremovable, since `values` merges and `clear` was the only gesture that could drop it. Reported as `cleared_undeclared` |
| `GET /api/agents/{id}` (failure mode) | an unreadable or warning-carrying row returns the document with `error` set, not a 400 — `list_inner`'s degradation, extended to `detail_inner` (§4.3) |

**Self-admin tools**: `lmgw__agent_set` already takes a manifest and therefore
already installs a container agent. Add `lmgw__agent_install` (write class) so
the image path is reachable from a chat agent — same four arguments, same
defaults (`pull: never`, `replace: false`), and its `next_step` points at
`lmgw__agent_get` for the config form the image brought with it. `agent_pull`,
`agent_reimport` and `agent_dev_url_set` are **not** exposed there (WP5): all
three act on this box's own filesystem and network rather than on the catalog
document, and the dashboard is where an owner decides to spend a download or to
point an app at their own dev server. `agent_token_rotate` and
`agent_token_get` are deliberately **not** exposed there — **defence in depth,
not a boundary**: both are ordinary ops on the unauthenticated `/api` plane
(§2), so withholding them from `lmgw__*` raises the bar for a model wandering
through its own tool list without pretending the value is out of reach. Same
posture, and the same honesty, as `SelfAdmin` not being settable through the
self-admin tools.

## 9. The mail labeler converted

The manifest changes in exactly one place: `run.apply` loses its `turn` and
gains a `script`. Source, fetch, columns, the classify prompts, the output
enum and the review block are untouched, so the taxonomy, the details modal
and every stored config value survive.

```json
"apply": { "script": [
  "export async function apply(ctx) {",
  "  const prefix = ctx.config.label_prefix ? ctx.config.label_prefix + '/' : '';",
  "  const listed = await ctx.tools.call('gws__gmail_listLabels', {});",
  "  const byName = new Map((listed.labels ?? []).map(l => [l.name, l.id]));",
  "  const groups = new Map();",
  "  for (const r of ctx.rows) {",
  "    if (!r.category) {",
  "      ctx.log(`skipping ${r.id}: the review row carries no category`);",
  "      continue;",
  "    }",
  "    const name = prefix + r.category;",
  "    if (!groups.has(name)) groups.set(name, []);",
  "    groups.get(name).push(r.id);",
  "  }",
  "  for (const name of groups.keys()) {",
  "    if (!byName.has(name)) {",
  "      const made = await ctx.tools.call('gws__gmail_createLabel', { name });",
  "      byName.set(name, made.id);",
  "      ctx.log(`created label ${name}`);",
  "    }",
  "  }",
  "  let applied = 0;",
  "  for (const [name, messageIds] of groups) {",
  "    await ctx.tools.call('gws__gmail_batchModify',",
  "      { messageIds, addLabelIds: [byName.get(name)] });",
  "    applied += messageIds.length;",
  "    ctx.log(`labelled ${messageIds.length} message(s) ${name}`);",
  "  }",
  "  return { applied, labels: [...groups.keys()] };",
  "}"
],
"output": { "type": "object",
  "properties": { "applied": { "type": "integer" },
                  "labels": { "type": "array", "items": { "type": "string" } } },
  "required": ["applied", "labels"] } }
```

The `output` schema is the one the retired `apply.turn` carried, moved across
unchanged (§4.2) — so the Runs tab renders the same block and a script that
returns the wrong shape fails the job instead of storing it.

`removeLabelIds` is never constructed, so `UNREAD` cannot be touched — the
guarantee moves from a sentence in a system prompt to the absence of a key.
`gws__gmail_modify` is already absent from the shipped `tools[].allowed`; the
five names stay as they are.

What this replaces: ~8 sequential model calls at ~7k prompt tokens, gone. One
`listLabels`, N `createLabel` for labels that do not exist, one `batchModify`
per distinct label. Deterministic, re-runnable, and the reviewer sees the same
review table.

`version` goes to `3.0.0`. An existing install seeded the `2.0.0` row before
hashes were recorded, so §5.1 leaves it alone and shows the "a newer built-in
manifest ships with this version" notice; one **Reset to shipped** adopts the
new manifest, keeps the taxonomy, **and records the hash** — so `3.1.0` and
everything after arrive on their own. A fresh install gets `3.0.0` directly.

## 10. Testing

**Runs in CI, no podman** (the bulk):

- *Manifest* (`agents/manifest.rs` tests): every field of §4.1 refused when
  malformed with the field named; **`apply.turn` loads and warns rather than
  erroring, and `detail_inner` on that row returns a document with `error`
  set, not a 400** — the regression the whole §4.3 decision exists to prevent;
  two of `tool`/`turn`/`script` in one step refused naming both; `output`
  without `script` refused; `script` as a string and as an array parse to the
  same module text; `provides.mcp` without `service` refused; a reserved
  `tool_prefix` refused; a name starting with `agent:` refused by
  `mcp_server_set`; `reject_self_loop` allows `/agents/x/mcp` and still
  refuses `/mcp`; negative limits refused and `0` accepted everywhere as "no
  limit".
- *Built-in upgrade* (§5.1): a matching hash replaces the row and keeps
  config; a differing hash leaves it alone; the legacy array KV form leaves it
  alone **but is rewritten to the map form, so the next ship upgrades it**;
  `agent_reset` and `agents_restore` record the hash, so a reset row upgrades
  next time instead of reading as edited; a deleted built-in stays deleted;
  idempotent across restarts.
- *Warnings* (§4.3): the manifest half is a pure function — `apply.turn`,
  `script` without `output`, a `localhost/` image on import each produce their
  code with `blocks_start` set correctly, with no gateway in the test; the
  Start gate is `requires_ok && no blocking warning`.
- *Ledger decoding*: a table of JSONL lines → the resulting `Vec<Row>`,
  `JobProgress` sequence, run-log lines and terminal outcome. Covers upsert by
  id, a second `row` event adding `output`, an unknown `type`, a non-JSON
  line, an undeclared column, an empty `id`, and each exit-code row of §3.2's
  table. This is the golden test of the whole protocol and needs nothing but a
  string slice.
- *Fake container*: the new spawn seam gets a fake that replays scripted
  stdout/stderr/exit — the same arrangement `tests/it/runtime_registry.rs`'s fake
  `CommandRunner` already uses for podman. Drives the executor end to end:
  happy path, cancel mid-stream keeps rows (`CanceledWith`), deadline, missing
  `output` with a declared schema, stderr into the run log.
- *Token*: `ApiKeyKind::Agent` authenticates with `auth_enabled` **off** and
  on; `ApiKeyKind::Internal` still cannot (`policy::admit`'s backstop); an
  `agent` row round-trips `key_plain` and rotation rewrites both columns;
  scope derivation yields `scope_mode: all` when no model field has a value;
  `/mcp` `tools/list` with an agent token lists only the allow list and
  without one is unchanged; `tools/call` on an unlisted name is refused;
  `X-Lmgw-Run` lands on the run's meter and a foreign run id is ignored.
  Extends `tests/it/key_policy.rs` and `tests/it/mcp_ingress.rs`.
- *Ledger auth*: no bearer → `401 agent_token_required`; another agent's token
  → `403 run_not_owned`; a cancelled run → `409 run_cancelled`; the owning
  token → `200`. The one authenticating corner of the `/api` plane deserves
  its own four assertions.
- *Argv*: the rendered `podman run` argv, pure and asserted token by token the
  way `tests/it/runtime_argv.rs` asserts the model one — including that no `-e`
  carries a secret and that a `0` limit omits its flag entirely.
- *Proxy* (`tests/it/agents_service.rs`, plus `agents/service/tests.rs` for the
  map): the fake spawner **binds the host port its `run -d` argv publishes**, so
  the whole proxy path runs against something real without podman — path
  prefixing, the dashboard's `cookie`/`authorization` never reaching the
  container, hop-by-hop stripping, `X-Forwarded-Prefix`, a status forwarded
  verbatim, an SSE body arriving in chunks (asserted by *when* the first chunk
  lands, not just what arrives), a raw upgrade tunnelled byte for byte, a 404
  under `/agents/x/app/` not falling through to the SPA, and
  `/agents/<id>` still serving `index.html`. The map's own half: one `podman
  run` for N concurrent callers, a start that never answers failing with the
  bound it waited under and the container's log, `idle_seconds = 0` as a true
  warm-keep, the idle stop and the restart after it, and a request in flight
  holding the stop off.
- *Service, real podman* (`the_real_thing_*`, self-skipping): one image built
  in a tempdir over `node:24-alpine` (removed afterwards, image included)
  serving a page, an SSE endpoint, a **real** WebSocket — the handshake's
  `Sec-WebSocket-Accept` computed with node's `crypto`, one masked client text
  frame in and one unmasked server frame out — and a minimal MCP endpoint.
  Asserts the page through `/agents/<id>/app/`, the SSE stream arriving live,
  the WebSocket round trip (and that its guard is released when it closes), the
  container's tool on `/mcp` under the manifest's prefix, the idle stop leaving
  no container behind, and the restart on the next request. A second one proves
  a container that never answers ends as `503 agent_service_starting` quoting
  its own log, with nothing left running.
- *Package* (`agents/package/tests.rs` + `tests/it/agents_package.rs`): a fake
  spawner replaying `create`/`cp`/`rm` returns a manifest, and the **sequence**
  is the assertion — one throwaway container name across all three, `--pull
  =never` on the create, nothing ever started. A missing `/lmgw/agent.json`
  fails with the named message and still removes the container; a create that
  fails removes nothing (there is nothing to remove); the run directory is gone
  afterwards. The pull policy: `never` refuses without a `pull`, `missing`
  pulls only what is absent, `always` pulls regardless, and a digest podman
  cannot report is `None` rather than a placeholder. On top of that, over the
  HTTP plane against a fake podman whose **image table a `pull` can change**:
  an install writes a working row (config form and all) with provenance and
  `local_image_on_import`; an absent image under `never` downloads nothing; a
  non-package image and an invalid manifest each fail with their own error and
  write nothing; a manifest naming a different image installs with the manifest
  winning and both said; `replace` is asked for and keeps the config; a pull
  that moves the tag reports both digests, reads the new manifest and offers
  the re-import, which then adopts it and keeps the config; a moved image that
  is no longer a package says so; a re-import carrying another id is refused.
- *Portability*: the export of a `localhost/` agent names the caveat, carries
  no token, no `provenance` and no `dev_url`, re-imports unchanged, and equals
  `AgentDetail.portability`; an agent with nothing local about it says it is
  portable.
- *Dev override*: `validate_dev_url` as a table of accepted and refused URLs
  (loopback, private ranges, LAN names; a public host, a query, a fragment, a
  non-URL), the trailing slash trimmed and a path prefix kept. Then end to end:
  a **real host HTTP server** spawned in the test stands in for `trunk serve`,
  `/agents/<id>/app/**` and the `/mcp` mount both reach it, `podman run` is
  never called, Start refuses with the reason, setting a `dev_url` stops the
  container that was serving the app, a dev server that is not listening is a
  `502 agent_dev_url_unreachable`, and clearing it starts the container again on
  the next request.
- *Routes*: `tests/it/web_pages.rs` gains the three ledger routes and the two
  proxy routes.

**Needs podman.** Everything real — the Node image with the shim, the start
cost, `host.containers.internal`, `podman stop` grace, reconciliation, a real
`podman cp` — is covered by §11's per-WP done-criteria and is not repeated
here. CI has no container runtime and must not grow one: `NoRuntime` exists
precisely so a test that reaches the runtime by accident fails with a message
instead of starting something on the box.

## 11. Work packages (sequential)

1. **WP1 — identity and ledger.** Migration `0032` (the `api_keys` rebuild
   with `key_plain`, `agent_id`, `provenance`, `dev_url`,
   `mcp_servers.agent_id`); `ApiKeyKind::Agent` with derived, recomputed
   scope; the `auth_mw` pre-branch and `ctx.agent`; `retain_allowed` on
   `AggregatePlane` for both `tools/list` and `tools/call`; `X-Lmgw-Run` into
   the meter; the three ledger routes with their own bearer check;
   `agent_token_rotate` and Copy token. `Limits.deadline_seconds` on the run
   spec and `Phase::Run` land here rather than in WP2, because "deadline from
   open" and `no_close` are unimplementable without them; the other limits
   wait for the runner that applies them.
   *Done when*: the migration preserves every `api_keys.id` a `request_logs`
   row points at; with `auth_enabled = false`, `curl` with an agent token gets
   exactly the manifest's allowed tools from `/mcp` and is refused on any
   other name; an unauthenticated ledger POST is `401 agent_token_required`
   and a foreign one `403 run_not_owned`; a hand-posted event sequence shows
   up in the Run tab's review table of a hand-opened run.
2. **WP2 — the container runner.** `agents/container.rs`, the streaming spawn
   seam and its fake, the JSONL decoder, `run.kind = "container"` with
   the remaining limits, cancel/deadline/reconciliation, run-surface reuse
   (`BatchShape`, `rows_of`, `RunBuffer`), `token::ensure` at container start,
   routing as one branch in `batch::execute` after the shared load/validate/
   enabled preamble (not `Run::dispatch`, which exists only once a model is
   resolved), `service`/`provides` parsed and validated already (so
   `deny_unknown_fields` does not refuse a WP4 manifest), `--pull=<policy>`
   passed through, `Limits` read by hand so a bad value names its field,
   validation of the ledger's `output` against the step schema at close, and
   the UI's Runtime block.
   *Done when*: a hand-built image that prints rows and an output runs to a
   green review table and an applied result, Cancel ends it `canceled` with
   the rows it had, a deadline ends it `failed` with the reason, and a
   leftover container is gone after a restart.
3. **WP3 — script sugar.** The shim (including its SIGTERM behaviour),
   `Settings.agent_script_image`, `script` + `output` on `Step` with their
   validation, **`apply.turn` demoted to a warning** and `detail_inner`'s
   degradation, the §5.1 built-in upgrade rule, and the mail labeler converted
   to §9 and re-embedded at `3.0.0`.
   *Done when*: the existing `2.0.0` row shows the "newer manifest ships"
   notice and one **Reset to shipped** adopts `3.0.0` with the taxonomy
   intact; a real classify run over the live mailbox followed by Apply labels
   every checked row with one `batchModify` per label; `UNREAD` is untouched
   on all of them; the Runs tab shows `{applied, labels}`; and zero model
   calls appear in Logs for the apply job.
4. **WP4 — service mode.** `agents/service.rs` (the live map, the one-start-
   many-waiters claim, the health probe, the stop and the idle sweep) and
   `web/agent_proxy.rs` (the two routes, streaming both ways, header hygiene,
   the WebSocket tunnel), the App tab and the card's `app` chip,
   `provides.mcp` and its `mcp_servers` row, and the three guard-rail changes
   §3.3 names.
   *Done when*: an agent's own SPA loads at `/agents/<id>/app/`, an SSE stream
   from it survives, the container stops after its idle window and restarts on
   the next request, and a chat thread attaching the agent's label sees the
   container's tools.
5. **WP5 — package.** `agent_install` (`create`/`cp`/`rm`), the pull policy
   field and `agent_pull`, provenance and digest, the `portability` export
   line, `dev_url`. **Done** — `agents/package.rs`, the four ops in
   `web/api_agents.rs`, `lmgw__agent_install`, and §12's image-update item
   answered by `agent_pull` + `agent_reimport`.
   *Done when*: `lmgw__agent_install` with a locally built image produces a
   working catalog row with no manifest pasted anywhere, an absent image with
   `pull: never` warns instead of downloading, the export of that agent names
   the `localhost/` caveat, and setting `dev_url` makes the App tab serve a
   `trunk serve` on the host. All four are asserted in
   `tests/it/agents_package.rs`, the first two against real podman
   (`the_real_thing_installs_from_an_image_and_the_row_runs`, which also runs
   the installed row end to end through the WP2 runner, and
   `the_real_thing_refuses_an_absent_image_under_pull_never`).

## 12. Out of scope, and open items

Genuinely open — decide when the work reaches them, do not guess now:

- **Token rotation UX.** There is exactly one `(key_hash, key_plain)` pair
  per agent and rotation replaces both in one write — no grace window, no
  second valid hash. A run in flight keeps the token it was handed in its
  `secrets.json` and starts getting `401`s at once; a service container holds
  a stale token until it is restarted, and nothing restarts it on rotation.
  **That is accepted**: rotation is a deliberate act on a single-owner box,
  and the recovery is a Stop or the run's own failure, both visible.
  **Settled in WP4**, both halves: the UI says so *before* — a confirm naming
  the live run and the running app container — and rotate *does* stop the
  service container, because a container holding a credential that stopped
  working a moment ago can only fail, and trading a visible restart for an
  invisible 401 is the wrong way round. A run in flight is still left alone:
  stopping it would be destructive, and it fails visibly with its own reason.
- ~~**WebSocket proxy library.**~~ **Settled in WP4**: a hyper upgrade on the
  inbound half and `reqwest::Response::upgrade()` on the outbound one, piped
  with two raced `tokio::io::copy` halves. See §3.3.
- **Multiple concurrent service containers.** One map entry per agent assumes
  one instance; whether a second dashboard tab should share it (it does, by
  construction) and what happens on a config change mid-session is unspecified.
- ~~**Image update detection.**~~ **Settled in WP5**, at the minimum that is
  honest: **nothing looks on its own.** No background poll, no startup check, no
  "an update is available" badge derived from a registry lmgw did not ask the
  owner about contacting. The look happens when the owner presses **Pull
  image** — `agent_pull` records the digest before, pulls, records the digest
  after, and reports both. When it **moved**, and only then, the manifest is
  read out of the new image (three more podman invocations, so not for free) and
  compared with the stored one; the answer says whether it differs and offers
  `agent_reimport`, which runs the install path over the existing row and keeps
  the `config` exactly as §5.1's built-in upgrade does. Adopting is never
  automatic, for §5.1's reason: a row the owner may have edited is not
  overwritten behind their back. A moved image that no longer carries
  `/lmgw/agent.json` says so instead of offering a re-import, and one whose
  manifest carries a different id is refused naming both. A pull on a row that
  was **never installed from a package** (an authored manifest that names an
  image) records the digest and nothing else — no `manifest_path`, no
  `installed_at` — and the Package block says "image" rather than "installed
  from", because claiming a document came out of `/lmgw/agent.json` when nothing
  ever looked there would be provenance lmgw made up.
- **Per-run cost as a `request_logs` column.** Deliberately not done (§3.1),
  but if "what did run #712 cost, three months later" becomes a real question
  the job result will not answer it after job retention trims the row.
- **Third-party agents.** Everything about same-origin (§3.3) is wrong for
  them. If it ever happens: separate origin, iframe sandbox, CORS, and the
  browser-JS trust argument has to be rebuilt from scratch.
- Unchanged from the catalog spec's §10: OAuth for remote MCP servers,
  scheduling, approvals inside a run, model-input fidelity for mail.
