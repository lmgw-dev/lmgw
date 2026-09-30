# Principals, agent origins and host mounts — design (2026-09-22)

Successor to the agent container runtime design (2026-09-19). That spec
stands except where this one says otherwise: the manifest, the run ledger,
the package format, the run kinds and the review gate are unchanged and are
referenced rather than restated. Read it first, then `docs/agents.md`.

This document is three parts that land in order, each its own commit series
on `main`, each leaving the app working:

1. **One principal, one gate** — every credential is a row in `api_keys`,
   one middleware resolves cookie or bearer to a principal on every plane,
   every route declares the capability it needs. The dashboard plane stops
   being open to whoever can reach `bind_addr`.
2. **Agent origins** — a service agent's UI is served on its own host name,
   `<id>.localhost:<port>`, dispatched on the `Host` header. The base-path
   machinery under `/agents/<id>/app/` is deleted, and the browser's origin
   isolation does the security work between the dashboard and an agent's UI.
3. **Host mounts** — a config field of `format: "directory"` or `"file"`
   whose value is a host path the owner picks through a native dialog, bind-
   mounted into the agent's container. The manifest names a slot; only the
   owner names a path.

Part 3 must not land without part 1 (a confined container could otherwise
bind the home directory to itself), and part 2 must land before the first
service agent is built against the current base path.

---

## 1. Summary

**What is wrong today.** Four credentials, three mechanisms, two gates. Client
API keys on `/v1`, honoured only when *Require API key* is on. Agent tokens,
already `api_keys` rows, honoured always. The self-admin token, a plain string
in Settings compared by hand in the MCP ingress. And the dashboard plane —
`/api`, `/api/op/*`, `/chat/api`, `/audio-lab/api`, `/image-lab/api`,
`/agents/<id>/app`, `/api/events` — carries **no middleware at all**; its trust
is "can reach `bind_addr`". That plane is what a confined agent container
reaches through `pasta -T`, and `docs/agents.md` §9 admits it: a container can
call any op the dashboard can. It cannot reach the host filesystem only
because no op hands one out. Part 3 hands one out.

**What this does.**

- *Principals.* `api_keys.kind` gains `owner`. Two owner rows are seeded:
  `owner:dashboard` (the session the shell and the browser present) and
  `owner:self-admin` (the credential the MCP self-admin plane accepts, migrated
  from the Settings string). One middleware at the router root resolves
  bearer-or-cookie into a `Principal` and stores it on the request. Each route
  is wrapped in a `require(Cap)` layer. The vocabulary is five words:
  `Public`, `Inference`, `Ledger`, `AgentSelf`, `Admin`. Anonymous holds
  `Inference` only while *Require API key* is off, and never holds `Admin`.
- *The cookie.* The dashboard authenticates with `lmgw_session`, an `HttpOnly`,
  `SameSite=Strict` cookie carrying the `owner:dashboard` key. The Tauri shell
  opens the window at a single-use login link; the process log prints a
  durable login link once per start for a headless owner to click. Cookie-
  authenticated requests must be same-origin, checked against the request's
  own `Host`, not a hardcoded loopback list.
- *Origins.* A service agent's UI lives at `http://<id>.<suffix>:<port>/`,
  suffix `localhost` by default. A root layer dispatches on `Host`. Everything
  that existed only because the UI was mounted under a path — `LMGW_APP_BASE`,
  `X-Forwarded-Prefix`, the `Location` rewrite, the dev-server prefix
  stripping, the 308, the dot-segment refusal, "the app can set cookies on the
  dashboard's origin" — is deleted. The MCP face stays a path on the main
  origin; that asymmetry is deliberate and stated (§4.8).
- *Mounts.* `format: "directory"` and `format: "file"` on a `string` config
  field, with an `access` keyword (`ro` default, `rw`). The value is a host
  path, stored like any other config value, stripped from every export, never
  allowed as a `default`. At start it becomes `-v <path>:/lmgw/mounts/<field>:<access>,z`,
  and the container sees the container path in `input.json` and in its own
  agent row, never the host one. The picker is the Tauri dialog plugin invoked
  from the web UI; without the shell it is a text field.

**What does not change.** The `/v1` and `/mcp` planes keep their semantics for
clients: a bearer that worked yesterday works tomorrow unless its own policy
refuses it, `auth_enabled` means what it meant. Agent tokens, scope
derivation, budgets and the ledger routes are untouched. The manifest's closed
keyword set grows by one format pair and one keyword. The container's
`LMGW_*` environment loses one variable and gains one.

**What this does not fix, said once here and again in §3.10.** While *Require
API key* is off, a container that simply omits its token is the anonymous
principal on `/v1` and `/mcp`, with no scope, no allow-list and no budget.
That is today's behaviour and part 1 leaves it; the agent card names it.

---

## 2. Principles

1. **One table, one gate, one vocabulary.** A credential is an `api_keys` row.
   A request has exactly one principal, resolved once, at the root. A route
   needs exactly one capability, declared where the route is built. There is
   no second path that "also checks" — the three ledger routes that today
   authenticate by hand read the principal the root resolved instead.
2. **The browser does the isolation.** Two pages on the same origin cannot be
   kept apart by any header, cookie or storage trick; the only isolation a
   browser offers is a different host name. So an agent's UI gets one. Cookies
   ignore ports and `SameSite` treats one host on two ports as one site, which
   is why it is a host name and not a port.
3. **A manifest can name a slot, never a host path.** A `default` on a mount
   field is refused at load, a template sees the container path, the
   container's own view of its row sees the container path, and an export
   strips the value. An imported agent can ask for "your notes folder"; only
   the owner says which.
4. **Nothing hidden.** Every bound mount is printed in the run log and the
   start summary, host path to container path with its access mode, and the
   fact that binding relabels the folder is on the field itself. Every refusal
   names the capability or the rule. Every constant in this document is either
   a visible setting or named here and surfaced where it applies.
5. **The threat model is stated.** After part 1 the trust boundary is the
   principal, not `bind_addr`. It defends against a foreign web page in a
   browser on this box, and against a confined container. It does not defend
   against a process running as the owner's own user, which can read the data
   directory; no local scheme does, and this one does not pretend to.

---

## 3. Part 1 — One principal, one gate

<!-- source today: crates/lmgw-core/src/server.rs (auth_mw, build_router, hold_until_body_end), crates/lmgw-core/src/agents/token.rs (presented, resolve), crates/lmgw-core/src/policy.rs (admit), crates/lmgw-core/src/mcp/ingress.rs (admin_gate, origin_allowed), crates/lmgw-core/src/web/api.rs (cross_origin_refusal, CREDENTIAL_OPS), crates/lmgw-core/src/web/api_agents.rs (bearer_agent, owned_run, detail_inner, degraded_detail) -->

### 3.1 Principals

```rust
pub enum Principal {
    Anonymous,
    Key { id: i64, name: String, kind: KeyKind /* Client | Agent | Owner */, agent_id: Option<String> },
}
```

Resolved from the `api_keys` table:

| `kind` | Name pattern | Plaintext stored | Authenticates | Holds |
|---|---|---|---|---|
| `key` | free | no (shown once) | bearer only | `Inference` (with its policy row) |
| `agent` | `agent:<id>` | yes | bearer only | `Inference` (scoped), `Ledger` (own runs), `AgentSelf` (own id) |
| `owner` | `owner:dashboard`, `owner:self-admin` | yes | bearer or cookie | everything |
| `internal` | `internal:<x>` | — (`key_hash = ''`) | **never** | — |

`internal` rows are attribution identities, not credentials. The resolver
skips them by kind before it compares hashes; `policy::admit`'s existing
refusal of `Internal` stays as the second backstop.

Two owner rows are seeded (§6). More may be created later from the Keys page
(`key_create { kind: "owner" }`) — for a second browser, a script, a LAN
device — and each is its own thing to rotate or disable. Every enabled owner
row holds every capability but `Ledger`, which is agent-only (§3.2); the rows
differ only in *which one leaked*.

`owner:dashboard` is the door: it cannot be disabled or deleted, and rotating
it re-logs the page that rotated it (§3.12). `owner:self-admin` can be
disabled, which is how "the self-admin plane is closed" is expressed after the
Settings string is gone (§3.7).

### 3.2 Capabilities

```rust
pub enum Cap { Public, Inference, Ledger, AgentSelf, Admin }
```

| Cap | Routes | Anonymous | Client | Agent | Owner |
|---|---|---|---|---|---|
| `Public` | `GET /api/version`; the SPA (`/`, `/{*path}` static and `index.html` fallback, the `/ui*` legacy redirects); `/api/session*` (§3.4); the `agent_app_moved` 404 (§4.2) | yes | yes | yes | yes |
| `Inference` | `/v1/*` (json and media groups); `/mcp` and the `GET`/`DELETE` session handlers on `/mcp/admin` (they are the aggregate plane's, §3.7) | **only while `auth_enabled` is off** | yes | yes, scoped and allow-listed as today | yes |
| `Ledger` | `POST /api/agents/{id}/runs`, `POST /api/agents/runs/{job}/events`, `POST /api/agents/runs/{job}/close` | no | no | own runs only (the handlers' existing ownership checks, reading the principal) | no |
| `AgentSelf` | `GET /api/agents/{id}`, `GET /api/agents/{id}/runs`, `GET /api/agents/runs/{job}` — **rendered as the container sees them** (§3.10) | no | no | own id / own runs only | yes |
| `Admin` | every other route: `/api/*` reads and `/api/op/*`, `/api/connect`, `/api/logs`, `/api/jobs`, `/api/vram`, `/api/usage/*`, `/api/settings-full`, `/api/responses*`, `/api/audio/catalog`, `/api/agents/import` and `/export`, `/api/docs/*`, `/chat/api/*`, `/audio-lab/api/*`, `/image-lab/api/*`, `/api/events`, `/api/status`, `/agents/{id}/mcp*`, `POST /mcp/admin` | no | no | no | yes |

The table is a code artifact — a list of `(method, path, cap)` beside the
router — and §10's route-walk test fails on any registered route that is not
in it. `Admin` is the default a new route gets by being listed there; there is
no route that is unlisted.

`Ledger` is deliberately agent-only: a run is written by the agent that owns
it, and an owner has nothing to gain from forging events into one. `AgentSelf`
is deliberately read-only: what an agent's backend legitimately needs is its
own row and its own runs. Writes to its own config from its own UI are an open
item (§12), not a capability handed out on day one.

The agent origin (part 2) is outside this table: it is dispatched before the
main router and serves the container's UI to anyone who can reach the port,
exactly as `/agents/<id>/app/` does today. The container's own login, if it
has one, is its business.

### 3.3 Presenting a credential

**Bearer.** `Authorization: Bearer <key>` or `x-api-key: <key>`, as
`token::presented` accepts today, case-sensitive `Bearer` as today. On
`POST /mcp/admin` only, `x-lmgw-admin-token: <key>` is accepted as a third
spelling, so the MCP client configs that use it today keep working; it is
read by that route's `require` layer, not by `presented`, so it widens nothing
else. A bearer is looked up by hash across every non-internal row.

**Cookie.** `lmgw_session=<key>`, accepted **only for `owner` rows**. A client
or agent key in the cookie resolves to `Anonymous` — it is not an error, it is
not a credential in that position. Attributes, always:

```
Set-Cookie: lmgw_session=<key>; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000
```

No `Domain` (host-only, so `127.0.0.1` and `board.localhost` never share it),
no `Secure` (the gateway is `http`). `HttpOnly` because no script needs to
read it, including the dashboard's own. `SameSite=Strict` is the CSRF defence:
a page on any other site never sends it, and DNS rebinding does not help an
attacker because the cookie jar keys on the host name the page was loaded from.
`Max-Age` is one year, a named constant surfaced on the Keys page beside the
owner rows ("a browser stays logged in for a year, or until Rotate"); the key
is persisted, so a browser session should be too.

**Order.** A bearer header wins over a cookie. Neither present is `Anonymous`.

A bearer that matches **no row at all** resolves to `Anonymous`, and the
capability check decides what that means: on `/v1` with *Require API key*
off it is admitted, exactly as today, because OpenAI-shaped clients always
send *some* key string and the Overview snippet promises "any key value
works"; on `/v1` with the toggle on it is `missing or invalid gateway API
key`; on an `Admin` route it is `session_required`. A bearer that matches a
**disabled** row is a refusal naming that row (`agent '<id>' is disabled…`,
`owner key 'self-admin' is disabled…`, `client key '<name>' is disabled…`),
never a fall-through — a credential the owner switched off must say so rather
than read as a typo. The one exception is a `Public` route, which ignores a
pending disabled-row refusal — a browser carrying a disabled second owner's
cookie still gets the SPA and `GET /api/session` (`authenticated: false`);
every other route answers with the refusal.

**Guessing.** An unmatched bearer or cookie costs one hash and a scan of the
key table, is logged at `debug`, and is not throttled: `policy::admit` counts
only matched keys. The keyspace is 256 bits. This is stated so that nobody
reads a rate limit into the design that is not there.

### 3.4 Login

Four routes, all `Public`, all registered ahead of the SPA catch-all:

| Route | Does |
|---|---|
| `GET /api/session/login?nonce=<n>` or `?token=<key>` | The route applies the §3.6 same-origin rule first, unconditionally: `Sec-Fetch-Site: none` (a typed URL, a bookmark, the shell) and `same-origin` pass, `cross-site` is `403 cross_origin_refused` — a foreign page cannot log the dashboard into a key it chose by navigating it here. Past that: a **nonce** is single-use, minted in-process by `session::mint_login_nonce`, valid 60 s (a named constant, printed with the nonce in the process log at `debug`), and exchanged for the `owner:dashboard` cookie. A **token** is an enabled owner key. Either match: `Set-Cookie` and `302 /`. No match (a dead nonce or a token that matches no enabled owner row): `302 /?login=invalid`, no cookie. |
| `POST /api/session` `{ token }` | The paste fallback. Same-origin rule first (§3.6). Same verification, `Set-Cookie`, `204`; a token matching no enabled owner row is `401 login_invalid`. |
| `GET /api/session` | `{ authenticated, kind, name }` for the current principal — `kind` and `name` are always present, empty for Anonymous. Never a `401`, whatever the request carried. What the SPA asks on load. |
| `DELETE /api/session` | Clears the cookie. `204`. The same-origin rule (§3.6) applies here too, via the cookie the request already carries. |

**The shell** (`src-tauri/src/main.rs`, `show_main_window`) mints a nonce in-
process and opens the window at
`http://127.0.0.1:<port>/api/session/login?nonce=<n>` every time it creates
the window, keeping `127.0.0.1` forced as today. The webview's first history
entry is therefore a dead nonce, not the durable key. Rotation (§3.12) is
handled by the page, not the shell. The tray menu's *Open in Browser* mints
its own fresh nonce at click time and opens the same login link in the system
browser, for the same reason a baked-in URL would go dead.

**The log.** Immediately after `lmgw listening on http://<addr>`
(`server.rs`, the existing line), one more line, once per start:

```
dashboard login: http://127.0.0.1:8001/api/session/login?token=lmgw-owner-…
```

The URL uses `net::primary_base_url(bind_addr)`. This line is the headless
owner's whole login procedure: click it. It is printed by `server::run`, so
the headless example and the shell both print it. It is a deliberate
disclosure of a secret into the owner's own process log, and the only one.

**The trace layer.** `TraceLayer::new_for_http()`'s default span records the
whole URI. WP1 replaces `make_span_with` for the **whole router** with one that
records method and path and never the query. That is a global change and is
listed as such in §11; the login route is only the reason.

**Curl.** Every `curl` against `/api` in `docs/agents.md`, the README and the
owner's notes gains one header: `-H 'Authorization: Bearer <owner key>'`. The
Keys page *Copy* button is where the value comes from (§3.12).

### 3.5 The gate

Two layers, one vocabulary, and a clear split of what each does.

**`principal_mw`** at the router root — `auth_mw` renamed and moved, not a
second middleware — does **resolution only**: parses the reasoning headers as
today, resolves the principal (§3.3), reads `X-Lmgw-Run` for an agent
principal, and inserts `RequestCtx { principal, .. }`. `ctx.client_key`,
`ctx.key_id` and `ctx.agent` remain as derived fields so that telemetry,
pricing and the MCP allow-list do not change in this part. It refuses nothing
except a disabled row.

**`require(cap)`** wraps each handler and does the **decision**: it reads the
principal from the request extensions and asks `principal.holds(cap, &snap)` —
the one function that encodes the §3.2 table, including the `auth_enabled`
rule for `Anonymous`. The `Ledger` and `AgentSelf` handlers refine it (the
layer admits *an* agent; the handler checks *which*). Two things are
`Inference`-specific and live in `require(Inference)` alone, because the
capability is known there and not at the root:

- **`policy::admit`** — expiry, rpm, tpm, concurrency — runs for a key
  principal on `Inference` routes only, holding its `ConcurrencyGuard` to the
  end of a streamed body through the existing `hold_until_body_end`. It does
  **not** run on `Ledger`, `AgentSelf` or `Admin` routes: an agent flushing a
  hundred ledger events a minute is not spending its request budget, and a
  dashboard read never was. Scope and budget stay post-body in
  `policy::check_alias`, as today.
- **The refusal dialect** — `require(Inference)` renders refusals through
  `policy_refusal` in the client's dialect (OpenAI or Anthropic by header), as
  `auth_mw` does today; every other `require` renders the flat `ApiError`
  (`{ "code", "message" }`) the SPA's `decode` already parses.

Where one path carries two capabilities — `/api/agents/{id}/runs` is
`get(runs)` (`AgentSelf`) and `post(run_open)` (`Ledger`); `/mcp/admin` is
`post(admin_post)` (`Admin`) beside the aggregate plane's `get`/`delete`
(`Inference`) — `require` is attached per handler with `Handler::layer`,
never per path with `route_layer`. Everywhere else a group-level `route_layer`
is fine, and both run after routing, so an unknown path is still a 404 rather
than a 401.

When `x-lmgw-admin-token` (§3.3) matches a row, it replaces the whole request
context: an agent bearer presented alongside it is not attributed or metered.

The `agent:<id>` MCP transport attaches the dashboard bearer only when the
row's URL is lmgw's own `/agents/<id>/mcp` — the address is checked at dial
time against the gateway's reachable hosts, not the column — and
`mcp_server_set` refuses a `url` on a row that carries an `agent_id`: the URL
is derived from the service and `bind_addr`, never set. The refusal is the
front door, the dial-time check the backstop; without both, an `Admin`
caller (a model-driven `/mcp/admin` client at `full` is one) could repoint
the row and receive the door key (§10 Part 2, §4.8).

The two hand-rolled checks are deleted, not kept alongside: `bearer_agent` in
`api_agents.rs` becomes "the principal is an agent with this id", and
`admin_gate` in `mcp/ingress.rs` becomes `require(Admin)` on the POST plus the
mode check (§3.7).

### 3.6 Same-origin rule for cookie requests

A request whose principal came from the **cookie** must be same-origin with
the gateway:

- If `Origin` is present, its `scheme://host[:port]` must equal the request's
  own `Host` with `http://` in front (the only scheme the listener speaks).
  Otherwise `403 cross_origin_refused`.
- Else if `Sec-Fetch-Site` is present and is not `same-origin` or `none`,
  `403 cross_origin_refused`.
- Else allowed.

The last branch is not reachable from a browser making a state-changing
request — `Origin` is mandatory on a cross-origin `POST` and `Sec-Fetch-Site`
is sent by every current engine — so its residue is non-browser clients, which
hold no cookie to begin with.

This catches the one case `SameSite=Strict` does not: a page on the *same
site* but another port (`http://127.0.0.1:9999`), which a local process could
serve. It is checked against the request's own `Host`, so a LAN-bound gateway
opened from a LAN address passes — unlike today's `origin_allowed`, which is a
hardcoded loopback list and refuses the owner's own dashboard on a LAN bind.
Bearer-authenticated requests skip this rule: `curl` sends no `Origin`, and a
bearer is not something a foreign page can attach. `POST /api/session` and
the login `GET` apply the rule unconditionally (§3.4), since at that moment
there is no cookie yet.

`cross_origin_refusal` on `CREDENTIAL_OPS` in `web/api.rs` is deleted; this
rule covers every cookie-authenticated request, and a bearer-authenticated
`agent_token_get` was never the threat. `origin_allowed` in the MCP ingress
(the DNS-rebinding guard on `/mcp` and `/mcp/admin`) is touched only by part
2 (§4.2, to admit agent origins); its LAN quirk is an open item (§12).

### 3.7 The self-admin plane, folded in

`POST /mcp/admin` requires `Admin`. The mode gate — `settings.self_admin` in
`Off | ReadOnly | Full`, filtering the tool list and refusing writes — is
unchanged and is still what an owner reaches for to limit what a model-driven
client may do. The `GET` and `DELETE` on the same path are the aggregate
plane's session handlers and keep `Inference` (§3.5).

`Settings::self_admin_token` is **removed**. At startup, once (§6): a
non-blank value becomes the plaintext of `owner:self-admin`, enabled, and the
setting is blanked; a blank value mints a fresh `owner:self-admin`,
**disabled**. That preserves today's meaning exactly — a plane that was closed
stays closed until the owner opens it, now by enabling the row on the Keys
page instead of pasting a string into Settings. The existing MCP client
config, which presents that same string, keeps working without an edit.

Today a blank token makes `/mcp/admin` answer `404` ("closed"). After this
part, a request with no credential answers `401 session_required` and one
with `owner:self-admin` disabled answers `401` naming the disabled row, the
same way a disabled agent's token is named. A closed surface and a locked one
were two words for one thing.

The Settings page loses the token field and the clear checkbox and gains one
line: "The self-admin credential is managed on Usage → Keys." The
`has_self_admin_token` DTO field goes with it.

### 3.8 Anonymous and "Require API key"

`auth_enabled` keeps its name, its checkbox and its meaning for clients:
**off** — the anonymous principal holds `Inference`; **on** — it holds
`Public` only. It never held `Admin` and never will; the dashboard's own calls
carry the cookie in both states. The Overview page's connection snippets keep
reading it to decide whether to print an `Authorization` line.

A matched client key is its principal whether or not the toggle is on — its
policy (expiry, rpm, tpm, concurrency) applies and its usage is attributed —
which is a deliberate change: before, the toggle being off skipped key
resolution entirely, so a valid key in the header read the same as none. Only
an absent or unmatched key is `Anonymous`.

### 3.9 Refusals

Every code below is the flat `ApiError` body (`{ "code", "message" }`) except
`missing or invalid gateway API key`, which keeps the client's dialect (§3.5).

| HTTP | `code` | When |
|---|---|---|
| 401 | `session_required` | No principal, and the route needs one. Body: `open the login link printed in the process log, or use the lmgw window`. |
| 401 | `missing or invalid gateway API key` | `Inference` with the toggle on and no matching key (unchanged wording, client dialect). |
| 401 | `agent_disabled` / `owner_key_disabled` / `key_disabled` | The matched row is disabled; the message names it — `key_disabled` is a **client** row (`kind: key`). |
| 403 | `forbidden` | A principal that does not hold the capability. Body names both: `this route needs admin; the request presented an agent token (agent 'board')`. |
| 403 | `cross_origin_refused` | §3.6. |
| 403 | `run_not_owned` / `agent_not_owned` | The `Ledger` / `AgentSelf` handlers' own check, unchanged. |

### 3.10 What changes for containers

A container calling `/api/op/*` with its agent token gets `403 forbidden`; with
nothing, `401 session_required`. What it keeps: `/v1`, `/mcp`, the ledger
routes, and — new — reads of its own row and runs. Nothing shipped does
anything else today (the mail labeler is in-process, the docs librarian is a
chat agent, the script shim uses `/mcp`), so no built-in changes behaviour.

Part 1 confines the container's **process** — what it can reach and do as a
caller. The container's **page** is a different thing: the App tab's
`/agents/<id>/app/` frame is unchanged and still served from the dashboard's
own origin, so a browser with the owner's session open there holds the
`lmgw_session` cookie and full `Admin`, same-origin, until part 2 moves it to
its own origin (§4).

**What the container sees of itself.** `GET /api/agents/{id}` under an
`AgentSelf` principal is rendered by `detail_inner` through a two-value `View
{ Admin, Agent }`, applied last to the finished document: secrets are masked
as today, and — part 1's own change — `dev_url` is `""`. Part 3 adds the
mount substitution in the same place: every mount field's value replaced by
its container path (`/lmgw/mounts/<field>`), and `service.mounts[]` carrying
`inside`, `kind` and `access` but no `host`. `degraded_detail` — the fallback
for a row this build cannot parse, which today returns the stored config
**unmasked** — is routed through the same masking for every principal. Only an
`Admin` principal ever receives a host path from this route. A run's detail
carries nothing to mask for its own agent — `rows`, `result` and `log` are
that run's own output — except free-form diagnostic sentences that may name a
run directory, which are left as they are. **Part 3 narrows that**: a
manifest with a mount field puts a host path into `log` (the mount line) and
sometimes into `error` (a mount refusal) on purpose, not as a stray
diagnostic sentence, so those two get the same substitution as the agent row
— the mount line's container path for `AgentSelf`, a mount refusal rendered
as `<field>: <code>` — and an `Admin` reader keeps the host path in both
(§5.2).

**What part 1 does not close.** While `auth_enabled` is off (the default), a
container that omits its token is `Anonymous`, and `Anonymous` holds
`Inference`: the `/mcp` allow-list applies only to a resolved agent, and scope,
budget and rate limits belong to a matched key. So the token's confinement on
`/v1` and `/mcp` is **advisory** in that state — today as well. Part 1 makes it
visible instead of pretending otherwise: a container agent's Definition tab
shows the warning `token_scope_advisory` — "*Require API key* is off, so this
token's model scope, tool allow-list and budget bind only a container that
presents it; switch it on under Settings to make them binding" — non-blocking,
recomputed per page load. Turning the toggle on is the fix and is one
checkbox; changing the default is an open item (§12).

`docs/agents.md` §9 "What the token does **not** confine" is rewritten around
these two paragraphs: the first bullet (`/api` is open locally, to the
container too) is no longer true, the `bind_addr` sentence becomes "the trust
boundary is the principal; `bind_addr` decides who can *reach* the gateway,
the principal decides what they may do", and the advisory state is named.

### 3.11 CORS and layer order

The root, outermost first:

1. `TraceLayer` — every request on every plane, including agent origins, so
   the trace log stays complete (with the §3.4 span that omits the query).
2. The `Host` dispatch layer (§4.2) — short-circuits agent origins into the
   proxy. **No CORS headers are added on an agent origin**: a foreign page can
   navigate a browser to `http://board.localhost:8001/` (every browser resolves
   `*.localhost` to loopback), but with no `Access-Control-Allow-Origin` it
   cannot *read* the response, and with no dashboard cookie it holds nothing.
3. `CorsLayer::permissive()` — on the main router only, as today. On `/v1` and
   `/mcp` it is what lets a browser-side client call the gateway with a
   bearer. On the dashboard plane it is harmless once the cookie is the gate:
   a cross-site page never has the cookie sent (`SameSite=Strict`), a same-site
   page on another port is refused by §3.6, and a credential-less request
   reaches only `Public` routes. Tightening it per plane is an open item (§12).
4. `principal_mw`, then the routers with their `require` layers.

### 3.12 Keys page and ops

The Keys card (`crates/lmgw-ui/src/pages/usage.rs`) lists owner rows with a
`kind` chip like the others, and the one-year session note (§3.3). Per owner
row: **Copy** and **Rotate**. Client rows are unchanged (shown once at
creation, not copyable). Agent rows keep their existing *Copy token* /
*Rotate* on the agent page.

| Op | Args | Rules |
|---|---|---|
| `key_reveal` | `{ id }` | Owner rows only; returns `{ key }`. Cookie-authenticated callers are same-origin by §3.6 already; no separate origin list. |
| `key_rotate` | `{ id }` | Owner rows only. Replaces hash and plaintext in one write; returns `{ key }`. Rotating `owner:dashboard` invalidates every browser's cookie including the caller's — so the SPA, on a successful rotate of that row, immediately `POST /api/session` with the returned key and carries on; other open tabs land on the login view. Rotating `owner:self-admin` is a plain write; the MCP client config is the owner's to update. |
| `key_create` | `+ kind`, `name` | `kind: "client"`, `"key"`, or absent all mint an ordinary client key. `kind: "owner"` mints `lmgw-owner-<64 hex>`, stores plaintext, returns it; the name is prefixed `owner:` by the server, idempotently — a name that already carries it is not doubled. |
| `key_set` | as today | Owner rows, both refused with the one code `refuse_owner`, distinguished by message: `enabled` is settable except on `owner:dashboard` (`the dashboard key is the door; rotate it instead`); `scope_mode`, `scope_patterns`, `budget_*`, `rpm_limit`, `tpm_limit`, `concurrency_limit`, `expires_at` (`an owner key is not a client; it is not scoped, budgeted or rate-limited`). `note` is editable. |
| `key_delete` | as today | Owner rows: refused for `owner:dashboard`; allowed for the others, including `owner:self-admin` — deleting it is allowed, and the next start re-seeds it disabled, which is §3.7's "closed" (§6). |

The key plaintext format `lmgw-owner-<64 hex>` sits beside `lmgw-<32 hex>`
(client) and `lmgw-agent-<64 hex>` so a leaked string says what it is.

`key_reveal`, `key_rotate`, `key_create`, `key_set` and `key_delete` are
`/api` ops only, deliberately absent from the `lmgw__*` self-admin plane: they
hand out or govern an owner credential to what is, on that plane, a
model-driven client.

---

## 4. Part 2 — Agent origins

<!-- source today: crates/lmgw-core/src/web/agent_proxy.rs, crates/lmgw-core/src/agents/service.rs, crates/lmgw-core/src/web/mod.rs (merge order), crates/lmgw-core/src/web/api_settings.rs (settings_set → resync_all, revalidate_dev_urls), crates/lmgw-core/src/state.rs (init), crates/lmgw-core/src/net.rs (reachable_urls), crates/lmgw-ui/src/pages/agent_detail.rs (AppTab) -->

### 4.1 The origin

A service agent's UI is served at

```
http://<id>.<agent_origin_suffix>:<bind port>/
```

- The label is the **agent id itself**. `manifest::validate_id` already
  restricts an id to `[a-z0-9-]`, at most 64 characters, first character a
  letter or digit. A DNS label is at most 63 characters and does not end in
  `-`; a service-declaring manifest whose id breaks either is refused at write
  with `origin_label_invalid`, naming the rule. No slug, no lossy mapping, no
  collision: ids are unique keys already.
- `settings.agent_origin_suffix`, default `localhost`. Validated on write:
  one or more DNS labels joined by `.`, no port, no scheme, not empty, and not
  `local` (mDNS cannot answer arbitrary names under it) — refused with
  `origin_suffix_shadows_gateway` when it **is** a name the gateway itself
  answers on, or shares a parent domain of two or more labels with one: a
  dashboard at `myhost.lmgw.lan` refuses a suffix of `apps.lmgw.lan`, because a
  page under that suffix could set a cookie for `lmgw.lan` and overwrite —
  never read — the dashboard's session; `agents.lan` is fine. The gateway's own
  names are `net::reachable_urls(bind_addr)`, the machine's host name,
  `<hostname>.local`, and `<hostname>.<search domain>` for each of the
  resolver's search domains. The check runs again at boot; a stored suffix
  that has started shadowing is reported as a warning, not reset.
  The same check runs the other way at manifest write: a service agent whose
  `<id>.<suffix>` equals a name the gateway answers on is refused with
  `origin_shadows_gateway` — which only fires against an own name that is an
  FQDN, since `bind_addr` is always a socket address and can never equal a DNS
  label.
- The port is `bind_addr`'s. The origin answers on the same socket; only the
  `Host` header differs.

**Why `*.localhost`.** Chrome and Firefox resolve any `*.localhost` to
loopback internally. systemd-resolved does the same for every other resolver
client on Fedora, and `nss` on this box confirms it — `board.localhost` →
`127.0.0.1`, `::1` — which is what the WebKitGTK window and `curl` see. A
distribution without resolved needs a hosts entry per agent; the App tab says
so (§4.9). Note the order: `::1` first. A gateway bound to `127.0.0.1` only
gets a refused `::1` connect and an immediate IPv4 retry from every client
that matters; nothing in lmgw needs to change for it, and nothing is.

### 4.2 Dispatch

The `Host` dispatch layer sits second from the outside (§3.11):

1. Read `Host`. Strip a port. Bracketed IPv6 literals and bare addresses never
   match and fall through.
2. If the host ends with `.<suffix>` and the label before it is the id of an
   agent that declares `run.service` (the agent row is read the way the proxy
   reads it by id today), hand the request to the agent proxy for that id and
   return its response. No principal is resolved and no capability required:
   the agent origin is the container's namespace, public to whoever can reach
   the port, exactly as `/agents/<id>/app/` is today.
3. If the host ends with `.<suffix>` and matches no such agent: `404
   not_found` with a JSON body naming the label. Not the SPA.
4. Otherwise fall through to the main router unchanged.

Public to whoever can reach the port now includes **state-changing requests
from any page a browser on this box has open**: a foreign page can navigate to
or form-post at `http://<id>.<suffix>:<port>/`; it cannot read the answer, and
it holds no dashboard cookie, but the request still lands. An app that changes
state on a `POST` needs its own CSRF posture; lmgw does not supply one.

The main router loses `/agents/{id}/app`, `/agents/{id}/app/` and
`/agents/{id}/app/{*rest}`. In their place three `Public` routes, one per
registered pattern, all `any`, answering `404 agent_app_moved` with
`{ code, message, origin }` so an old bookmark says where to go rather than
`session_required`. `origin` is derived from the id alone — string
interpolation, no existence check — so the body does not by itself reveal
whether the id exists; the origin's own `404 not_found` (step 3 above)
likewise uses one message for an unknown id and for an id that exists but
declares no `run.service`, for the same reason. `/agents/{id}/mcp*` stays
(§4.8).

`origin_allowed` in the MCP ingress — the rebinding guard — gains one accepted
shape on `/mcp` only, and only with the gateway's own port: a host of the form
`<id>.<suffix>:<bind port>` for a service agent, so an agent's UI can call
`/mcp` from the browser with a bearer its backend gave it. `/mcp/admin` keeps
the loopback list unchanged (§12).

### 4.3 What the agent origin serves

Every method, every path, every upgrade, forwarded to the container's
published loopback port (or the `dev_url`) with the path and query byte for
byte as they arrived. The container is mounted at `/` on its own origin and
writes its own URLs. There is nothing to rewrite on the way in.

A literal `.` or `..` path segment reaches this face only from a client that
is not a browser: a browser's own URL parser normalises them out of the
address bar before the request ever leaves it, and lmgw's URL parsing
normalises away whatever slips through on the way to the upstream URL — same
rule, applied twice. A **percent-encoded** segment (`%2e%2e`) is not decoded
by either parser, so it passes through byte for byte, same as any other path
segment, for the container to interpret.

### 4.4 What is deleted

From `agent_proxy.rs`, `service.rs`, `api_agents.rs` and the UI:

| Thing | Why it existed | Gone because |
|---|---|---|
| `app_redirect`, the 308 for the bare mount | slash ambiguity of a path mount | an origin has no bare-vs-slash form |
| `split_mount`, `Mount::prefix` | parsing `/agents/<id>/app` out of the path for the UI face | dispatch is by `Host` now; there is no mount left to parse on that face (§4.3). `bad_path` **survives**, on both faces: it still answers when the upstream URL cannot be assembled (an `OPTIONS *` request-target, say), and a narrowed `split_mcp` keeps refusing a literal `.`/`..` segment under it on the MCP face, which still has a real prefix to escape |
| `X-Forwarded-Prefix` | telling the app where it is mounted | it is mounted at `/` |
| `rewrite_location`'s origin-relative case, `base_path`, `strip_base_path`, the dev-server prefix stripping | making the container's `Location: /login` land under the mount | `/login` is right as it is |
| `LMGW_APP_BASE` env, `service.app_base` in `input.json`, `service::app_base()`, `dto::AgentService.app_base` | the base path for an SPA build | replaced by `LMGW_APP_ORIGIN` / `service.origin` / `dto::AgentService.origin` (§4.6) |
| `docs/agents.md` "`LMGW_APP_BASE` and building an SPA for it", the `--base` / `--public-url` flags | building for a sub-path | apps build for `/` |
| the sentence "the app can set cookies on the dashboard's origin" and "an agent's app JS is as trusted as the dashboard" | the same-origin trade | the trade is gone — and with it a hole nobody wrote down: today an app's JS in the App tab iframe is same-origin with the dashboard and can reach `parent.window.__TAURI_INTERNALS__`; Tauri injects that bridge into the main frame only, so a cross-origin frame cannot |
| `NEVER_FORWARD`'s *reason* | the browser attached the dashboard's cookie to the mount | it no longer does; the stripping of `cookie` and `authorization` **stays** as defence in depth, with its comment rewritten |

### 4.5 What survives, unchanged

Raw-path forwarding and query attachment; hop-by-hop stripping in and out;
`content-length` stripping and body re-framing; streaming in both directions;
the in-flight guard and `last_used`; the idle sweep and its lock discipline;
the WebSocket byte tunnel; on-demand start with one `podman run` for N
waiters; the 503-with-log-tail on a failed probe; the eviction on
`agent_service_unreachable`; `dev_url` precedence. The eight things that stop
the container in `docs/agents.md` become nine after this part (§4.10) and ten
after part 3 (§5.7). The error codes keep their names; what changes is that
"the path did not match the mount" is no longer among their causes.

One rewrite case remains and is restated positively: an absolute `Location`
naming the container's **published loopback origin**
(`http://127.0.0.1:<host port>/x`) or the `dev_url` origin is rewritten to the
agent origin (`http://board.localhost:8001/x`), because the browser cannot
reach the former. Every other `Location` — relative, origin-relative, or
absolute elsewhere — passes untouched.

### 4.6 Environment, `input.json`, and the forwarded headers

| Was | Is | Value |
|---|---|---|
| `LMGW_APP_BASE=/agents/board/app/` | `LMGW_APP_ORIGIN=http://board.localhost:8001` | the public origin, for an app that builds absolute URLs (OAuth redirect URIs, share links) |
| `X-Forwarded-Prefix: /agents/board/app` | `X-Forwarded-Host: board.localhost:8001` and `X-Forwarded-Proto: http` | the standard pair; a server-side framework derives its public URL from them |
| `input.json` `service.app_base` | `service.origin` | same string as the env var |
| — | `X-Lmgw-Face: app` (agent origin, upgrades included) / `mcp` (`/agents/<id>/mcp*`) | which face a request came in on; `X-Forwarded-Host` is the same on both, so this is how an app tells lmgw's Admin-gated MCP call from anyone on the port; in `NEVER_FORWARD`, set with `insert` |
| — | `X-Forwarded-For: <peer ip>` | the TCP peer of the connection that reached lmgw (`ConnectInfo`, served by `into_make_service_with_connect_info`), set with `insert`; on the MCP face that is lmgw's own client dialling itself, and that face's trust is the `Admin` gate, not the address |

`LMGW_PORT` is unchanged. `host` is still not forwarded (reqwest sets its
own); the public host travels in `X-Forwarded-Host`.

Because an app is now told to *trust* `X-Forwarded-*`, the proxy must own
them: `NEVER_FORWARD` grows by `x-forwarded-host`, `x-forwarded-proto`,
`x-forwarded-for`, `x-forwarded-prefix` and `forwarded`, so a client — a
container on the gateway port, a foreign page — cannot supply its own, and
lmgw sets its two with `insert`, never `append`. Today
`forwarded_request_headers` copies everything not in the list with `append`,
which would have made a client's value the first of two.

Both `X-Forwarded-Host`/`X-Forwarded-Proto` and the one surviving `Location`
rewrite (§4.5) apply on the MCP face too, not only the UI face: a request
through `/agents/{id}/mcp` carries the same pair, naming the agent's own
authority rather than the main origin's — the proxy code is shared between the
two faces and was never taught to tell them apart for this.

### 4.7 `dev_url`

Rules unchanged — `http(s)`, loopback host, no userinfo, no query or fragment,
not lmgw's own port — with one addition: **no path**. A dev server is an
origin now; `http://127.0.0.1:5173/base` is refused with `a dev_url is an
origin; it cannot carry a path`. `trunk serve` and `vite` run with no
`--public-url` / `--base`. Setting one still stops the running app container.

A row stored before this part may carry a path. `revalidate_dev_urls` runs
today only on a `bind_addr` change; WP2 also calls it at boot, after the seed
and `resync_all` in `AppState::init`, so such a row is cleared and shows
`dev_url_cleared` — the row stores `{ url, why }`, the cleared value kept
beside the reason — rather than proxying to a base nothing strips any more.

`agent_install` and `agent_reimport` call the same `import_inner` `agent_set`
does, but their own signature is `Result<Value, String>`, not
`Result<_, Refusal>`, so a coded refusal's *code* stops there: both render
`origin_label_invalid` / `origin_shadows_gateway` as the plane's ordinary flat
`op_failed` text, the message naming the rule either way.

### 4.8 `provides.mcp` stays a path

`/agents/{id}/mcp` and `/agents/{id}/mcp/{*rest}` stay on the main origin,
and the `agent:<id>` MCP row keeps pointing at
`<primary_base_url>/agents/<id>/mcp`. The consumer of that URL is lmgw's own
MCP client, server-side; a browser never sees it, so origin isolation buys
nothing there, and moving it would drag `reject_self_loop` and the row
lifecycle along for no gain. The asymmetry is deliberate: **the UI face is an
origin, the MCP face is a path.**

Under part 1 that route requires `Admin`. The row stores no credential
(`headers` stays empty, as today, because a row is exported and read back
onto the MCP page). Instead `build_http_transport` attaches
`Authorization: Bearer <owner:dashboard>` at connect time for a row that
carries an `agent_id`, read from the snapshot, so a rotation takes effect on
the next connect. No other MCP row gets this. The proxy strips
`authorization` before the request reaches the container (`NEVER_FORWARD`),
so the container never sees the owner key. The dial goes to the gateway's own
address; a connection to a host's own interface address is delivered locally
and does not leave the machine, whatever `bind_addr` is.

It also gets the same `X-Forwarded-Host`/`X-Forwarded-Proto` pair and
`Location` rewrite as the UI face (§4.6), advertising the agent's own
authority even though nothing on this face — lmgw's own server-side client —
reads them today.

### 4.9 The App tab

The tab shows the origin as a link, the `origin_resolves` verdict, and the
app in an `<iframe src="<origin>?v=<generation>">` as today, plus *Open full
page* (`target="_blank"`, which the shell sends to the system browser; that
works because the agent origin needs no cookie).

`dto::AgentService` gains:

| Field | Value |
|---|---|
| `origin` | `http://board.localhost:8001/` |
| `origin_resolves` | the server ran `tokio::net::lookup_host` on `<id>.<suffix>:<port>` at request time. Its resolver is the same NSS the WebKitGTK window uses; Chrome and Firefox resolve `*.localhost` regardless. |

The agent's own detail route carries neither `bind_addr` nor
`agent_origin_suffix`, so the second standing line below needs a value this
tab does not otherwise have: it reads `GET /api/settings-full` itself, the
same way every other page on this dashboard that needs a gateway setting does,
rather than growing a field on `dto::AgentService` for it.

Three standing lines above the frame, each shown only when it applies:

- `origin_resolves` false: "`board.localhost` does not resolve on this
  machine. Add `127.0.0.1 board.localhost` to `/etc/hosts`, or set an agent
  origin suffix under Settings that your DNS answers for."
- `bind_addr`'s host not loopback and the suffix still `localhost`: "The
  gateway is bound to `<host>`; `*.localhost` only resolves on this machine.
  Set an agent origin suffix with a wildcard record to reach agent UIs from
  elsewhere."
- Always: "The frame is a different site from the dashboard. An app that
  forbids embedding renders blank, and an app that keeps its own session
  cookie may not keep it in here — WebKitGTK blocks third-party cookies by
  default. Open full page is the reliable view."

lmgw does not try to detect either failure — a cross-origin frame's load
failure is opaque by design.

The Start button's op, `agent_service_start`, carries `origin` in its response
on **success only** — a failed start answers the plane's ordinary error text
(a 503 with `log`, or an `op_failed`), with no `origin` field to read.

### 4.10 LAN, the suffix, and what a settings change does

`agent_origin_suffix` exists for one reason: a gateway bound to a real address
and opened from another machine. Set it to a zone with a wildcard record
(`*.lmgw.lan → <host>`) and every agent origin becomes `<id>.lmgw.lan:<port>`.

Origins are computed per request, so nothing needs resyncing on a suffix
change — but every running app container holds `LMGW_APP_ORIGIN` in its
environment. So `settings_set` gains a step beside its existing `resync_all`
and `revalidate_dev_urls`: a change of `agent_origin_suffix` **stops every
running app container and cancels any start in flight**, the ninth reason in
`docs/agents.md`'s table ("it is holding an origin the owner has renamed") —
a start that has not yet published its container would otherwise finish
holding the old origin with nothing left to stop it. A `bind_addr` change
takes effect only on restart, as today; the boot sweep already removes every
leftover service container, so the stale origin dies with the old process.

---

## 5. Part 3 — Host mounts

<!-- source today: crates/lmgw-core/src/agents/manifest.rs (ConfigSchema, RawField, Format, masked_values, without_secrets), crates/lmgw-core/src/agents/container.rs (RunSpecArgs, run_argv, run_plan, input_document, public_config; the :Z measurement at ~249-256), crates/lmgw-core/src/agents/service.rs (start_once), crates/lmgw-core/src/agents/batch.rs (Input, execute), crates/lmgw-core/src/web/api_agents.rs (export_inner, ENVELOPE_KEYS), crates/lmgw-ui/src/widgets/schema_form.rs, crates/lmgw-ui/src/ui_scale.rs (tauri_invoke) -->

### 5.1 Two formats and one keyword

`config.schema.properties.<name>` on a `type: "string"` field:

| Keyword | Value | Notes |
|---|---|---|
| `format` | `"directory"` or `"file"` | Joins `secret`, `model_alias`, `multiline`. |
| `access` | `"ro"` (default) or `"rw"` | **Only** legal with these two formats; anywhere else it is refused naming the property. Declared by the manifest, because the agent knows whether it writes. |

Everything else a `string` field takes still applies (`title`, `description`,
`required`), with two refusals:

- `default` on a mount field is refused at load: `config.schema.properties.<name>:
  a directory or file field cannot have a default — a manifest names a slot,
  never a host path`. This is principle 3 in code.
- `enum` on a mount field is refused: a list of host paths is the same thing.

A mount field is legal only on a `run.kind: "container"` manifest. On `chat`
or `batch` it is a **blocking** warning, `mount_field_without_container`:
"`<name>` is a directory field, but this agent has no container to mount it
into". Script steps in a batch agent are not covered in this part (§12).

### 5.2 The invariant

A manifest can name a slot, never a host path.

- The value is stored in `agents.config` like any other value and is returned
  to an **`Admin`** reader by `GET /api/agents/{id}` — the owner sees their
  own path. An `AgentSelf` reader sees the container path (§3.10).
- **Export strips it.** The export's `include_config=1` writes every non-
  secret value today (`without_secrets`); it now writes through
  `without_secrets_or_mounts`, and the envelope's `config_unbound:
  ["notes"]` names **every declared mount slot** — bound or not on the
  source side, since the value never travels either way — beside
  `config_omitted` so the receiver knows there is a slot to bind.
  `ENVELOPE_KEYS` — the closed list the importer strips before the
  `deny_unknown_fields` manifest parse — gains `config_unbound`, or the new
  export could not be re-imported. **The import report reuses the same field
  name for a different count**: the slots still unbound *after* the import
  finishes. A plain new install therefore lists every mount slot, same as
  the export did — but a `replace=1` against an existing agent that keeps an
  already-bound path lists nothing for that field.
- `agent_duplicate` copies mount values (same box, same paths).
- The container sees `/lmgw/mounts/<field>`, never the host path, in
  `input.json` (§5.6), in its own agent row (§3.10), and in templates (§5.6).
- **The substitution reaches a run's `log` and `error` too, for an
  `AgentSelf` reader.** Reading its own runs, a container gets the run log's
  mount line rendered with the container path, and a mount refusal inside a
  run's `error` rendered as `<field>: <code>` rather than the sentence an
  owner sees — never a host path either way. An `Admin` reader's view of the
  same run keeps the host path in both places (§3.10). The same discipline
  extends to service mode: a failed start answers a **generic** 503 on the
  public agent origin ("the app could not be started" — no `log`, no path),
  because that origin needs no principal (§4.1) and is reachable by anyone
  who can reach the port; the reason, with the log tail and any path in it,
  is on the App tab and in the `agent_service_start` op's own answer, both
  behind the owner's session.

### 5.3 Path rules

Checked wherever a value is **stored** — `agent_config_set`, the per-run
override on `agent_run`, import, and `agent_duplicate` — and again whenever
it is **used** (a phase start, a service start), because the filesystem
changes between the two. A save checks only the fields it actually sets: a
mount field whose folder has since vanished does not block an unrelated
save elsewhere on the same agent — the refusal says to clear the field or
point it at a folder that exists — and the dead mount is refused again the
next time it is used. Within one save, the bindings it sets are checked
against each other as well as against everything already stored, so two
fields bound in the same call cannot violate rule 5 against one another. A
store-time refusal is `400 mount_path_refused` naming the field and the
rule; a use-time refusal fails the start with the same code and never runs
`podman`.

1. Absolute, or refused.
2. Canonicalised (`std::fs::canonicalize`): symlinks resolved. The canonical
   path is what is stored, mounted and printed. A path that does not exist is
   refused at store time (`does not exist`) and at use time
   (`mount_path_missing`, naming the field and the path).
3. The kind matches the format: a `directory` field names a directory, a
   `file` field a regular file.
4. Refused by location, with the reason in the message:
   - `/` and every ancestor of `$HOME`: "too broad to relabel".
   - `$HOME` itself: "relabelling the home directory would break sshd, gpg and
     every other process that reads a label under it".
   - `$HOME/.ssh`, `$HOME/.gnupg`: same reason, by name.
   - The lmgw data directory and everything under it, and its ancestors: "the
     gateway's own database lives here".
   - The lmgw runs root (`$XDG_RUNTIME_DIR/lmgw` or its fallback) and
     everything under it: "run secrets live here".
   - `/proc`, `/sys`, `/dev`, `/run`, `/boot`, `/etc` and everything under
     them: "not a data directory".

   The home, data and runs directories this rule compares against are
   themselves canonicalised before the comparison, so a symlinked `$HOME` or
   data directory cannot be used to slip a path past it. When the gateway
   cannot determine the home directory at all, every mount is refused —
   the home rules do not silently drop out for want of a `$HOME`.
5. **Not nested in, nor containing, another bound mount when either side is
   `rw`.** Across every agent and every field: `ro` over `ro` is allowed, but
   a new path may not lie inside, nor contain, a path bound elsewhere if
   either the new binding or the existing one is `rw` — an `rw` holder inside
   or around a `ro` mount could redirect what that `ro` mount resolves
   through, which is as good as writing to it. Refused with
   `mount_path_nested`, naming the other agent, field and access. The *same*
   path bound twice is allowed whatever the access — that is what the shared
   label in §5.5 is for. The reason for the wider rule is the race below.

**The race, stated.** Between `canonicalize` and podman's own resolution of
the `-v` source there is a window in which a symlink could be swapped in. Who
can write there? A process running as the owner, which is out of the threat
model (principle 5) — and a confined container holding an `rw` mount on
either side of the new binding, which is in it. Rule 5 is what keeps that
container's reach to its own tree: it cannot hold a mount inside a tree
another mount will resolve through, nor sit around a tree another mount
already holds. The residual — podman resolving a path a same-user process
rewrote — is named here and not defended.

There is no allow-list and no "safe" prefix; anything not refused is the
owner's choice.

### 5.4 The picker

The web UI calls the Tauri dialog plugin directly, through the same
`window.__TAURI_INTERNALS__.invoke` bridge `ui_scale.rs` already uses for
zoom: `plugin:dialog|open` with `{ options: { directory: true|false,
multiple: false, title: <field title>, defaultPath?: <current value> } }` —
camelCase, and the `options` wrapper is not decoration: it is the shape
`tauri-plugin-dialog` 2.7.1 actually deserialises, an unwrapped payload is
silently ignored. `defaultPath` is omitted from the object entirely when the
field has no bound value, rather than falling back to `$HOME`. The
`remote-ui.json` capability gains `dialog:allow-open` and nothing else. The
dialog returns a host path string; the UI puts it in the form like any typed
value. An agent UI in the App tab frame cannot reach this bridge: its origin
is `<id>.<suffix>`, and `remote.urls` in the Tauri config lists only
`127.0.0.1` and `localhost` as origins the bridge is injected into — an agent
origin is neither, so `window.__TAURI_INTERNALS__` is simply absent there,
not merely unreachable across a frame boundary.

When `__TAURI_INTERNALS__` is absent — the dashboard open in Firefox, a
headless install — the control is a plain text input with the placeholder
"absolute path on the gateway machine". Nothing in the core knows a dialog
exists; there is no hook to fill in.

### 5.5 Runtime: the mount, the label, `keep-id`

`RunSpecArgs.mounts` (and `Plan.mounts`) widen from `(PathBuf, String)` to

```rust
pub struct Mount { pub host: PathBuf, pub inside: String, pub access: Access /* Ro | Rw */, pub label: Label /* Private | Shared */ }
```

The two lmgw files stay `Ro`, `Private` (`:Z`), as today. A mount field
contributes

```
-v <canonical host path>:/lmgw/mounts/<field>:<ro|rw>,z
```

Every message that has to point at *another* binding — `mount_path_nested`
chief among them — carries a `Slot { agent: String, field: String }` rather
than a bare string, so the message always names both the other agent and the
other field, never just one. `AgentField` — the DTO row each declared config
field is rendered as — types `access` as a plain `String`, not an `Option`:
on a field that is not a mount at all it is simply `""`, so the wire shape
stays flat and a reader does not have to branch on presence to find out a
field has no access mode.

A **service** container has no run log to print a mount line or a keep-id
line into — there is no run — so both go to the **process log** instead, the
same fallback `secrets_dir_fallback` already uses for a service start
(`docs/agents.md` §5). The uid in the keep-id line, `this manifest declares
mounts: the container runs as uid <n> (--userns=keep-id)`, is read from
`/proc/self` — the lmgw process's own uid, since `keep-id` maps the container
to whichever uid lmgw itself is running as — not from a config value or a
`podman` round-trip.

**`:z`, shared, not `:Z`.** `container.rs` already records the measurement:
`:Z` relabels the *source* with a private MCS category, so the second
container that starts against the same source revokes the first one's access
mid-run — which is why the shim is copied per run rather than mounted from
one place. An owner-chosen folder is exactly the case where two containers
meet: a service container plus that agent's phase run, or two agents on one
folder. `:z` relabels to `container_file_t` with no category, narrow to that
path, readable by every container and by the owner.

**The relabel is recursive and permanent**, and that is surfaced where the
owner decides, not only in a log: the field's help line on the Run tab reads
"binding relabels this folder and everything under it for containers
(`container_file_t`); nothing undoes it when the agent stops", the Definition
tab repeats it under *Mounts*, and the run log prints it once per mount. A
relabel that fails (a filesystem without xattrs) fails the `podman run`;
podman's message is the run's failure message, verbatim, as for any other
start failure. A `label=disable` escape hatch is an open item.

**`--userns=keep-id` is a property of the manifest, not of the form.** It is
added to every phase run and service start of a manifest that **declares** at
least one mount field, bound or not, so one image always runs one way: the
container process is the owner's uid inside too, and what it writes is the
owner's. Without it, root-in-container maps to the owner's uid and can read
the owner's files, but an image that runs as `USER 1000` cannot write them and
what it creates lands under a subuid. The run log's start line names it:
`this manifest declares mounts: the container runs as uid <n>
(--userns=keep-id)`. An image that needs to be root at runtime gets `EACCES`
from the kernel, and that is the error the run shows. Existing manifests
declare no mount field and see no change.

The argv, with a mount:

```
podman run --rm --replace --name <prefix>-agent-<slug>-<run>
  --label … --userns=keep-id
  [--memory …] [--cpus …] [--pids-limit …] [--read-only --tmpfs /tmp]
  --cap-drop=ALL --security-opt no-new-privileges --pull=<policy>
  [--network pasta:-T,<port>]
  -e LMGW_…=…
  -v <run dir>/input.json:/lmgw/input.json:ro,Z
  -v <run dir>/secrets.json:/lmgw/secrets.json:ro,Z
  -v /home/alice/Notes:/lmgw/mounts/notes:rw,z
  <image> [args…]
```

### 5.6 `input.json` and templates

`public_config` substitutes the container path for every mount field's value,
and `input.json` gains a `mounts` array so a container can enumerate without
knowing the schema:

```json
{
  "phase": "run",
  "agent": { "id": "notes-desk", "name": "Notes desk" },
  "run": { "id": 512 },
  "config": { "notes": "/lmgw/mounts/notes", "model": "qwen3.8-27b" },
  "mounts": [
    { "field": "notes", "path": "/lmgw/mounts/notes", "kind": "directory", "access": "rw" }
  ]
}
```

The host path is not in the file. The same substitution applies to the
service-mode `input.json`. `mounts` is `[]` when the schema declares no mount
field, and an unbound optional mount field is simply absent from both
`config` and `mounts`.

A template `{{config.notes}}` resolves to `/lmgw/mounts/notes`. It is not
refused in prompts the way a secret is — a container path tells a model
nothing about the host.

The run log, before the start line, one line per mount:

```
mount notes: /home/alice/Notes → /lmgw/mounts/notes (rw, directory; relabelled container_file_t, recursive, permanent)
```

### 5.7 Per run, saved default, apply, service

- **Per run.** `agent_run { values }` already merges the form over the stored
  config for that one run without writing. A mount field rides on it: pick a
  different folder, press Start, the default is untouched.
- **Saved default.** *Save config* → `agent_config_set`, with the path rules
  applied at store time.
- **Apply reuses the run's config.** Today each `agent_run` — run and apply
  alike — merges its own `values` over the *current* stored config, and the job
  row records only the sparse patch. That means an apply can silently use a
  different folder than the run that produced its rows. So: at start, the job's
  `input` gains `effective`, the merged non-secret config the phase actually
  ran with (an `Option`, so rows written before this part deserialise and
  behave as today); and an apply started with `base_job` merges **that run's
  `effective`** over the stored config before applying its own `values`. The
  UI's Apply already sends `base_job` and no `values`, so an apply now runs
  against what the reviewer saw. This is a general fix and is not confined to
  mount fields. **`effective` is written by the `agent_run` op only** — a run
  opened through the ledger's `run_open` (§3.5, the `Ledger`-capability
  `POST /api/agents/{id}/runs`, a container driving its own run) writes none,
  because there is no form-over-stored-config merge on that path to record.
  Inheritance applies to an **apply** start alone: a plain **rerun** of the
  `run` phase does not chain off any earlier job's `effective`, it starts
  fresh from the config stored at that moment, same as before this part.
- **Service.** A service container starts from the stored config. An
  `agent_config_set` that changes a mount field's value **stops a running app
  container** — the tenth reason in `docs/agents.md`'s table: "it is holding a
  mount the owner has re-pointed" — and its response says so on two separate
  keys: `mounts_repointed` names the field(s) that moved, `service_stopped`
  is the boolean that a container was actually torn down as a result (`false`
  when nothing was running to stop). The App tab lists the bound mounts.

### 5.8 UI

- `schema_form.rs` gains a `("string", "directory" | "file")` arm: a read-only
  path display, **Choose…** (dialog or, without the shell, an editable text
  input), **Clear**, a chip `directory · rw` / `file · ro`, and the relabel
  help line (§5.5). Unlike a secret's chip, which reads `has_value` off the
  last save, a mount field's "set / not set" chip reads the **form's own
  draft** — so Choose… and Clear flip it at once, before Save config is ever
  pressed.
- The Definition tab lists mount fields under a *Mounts* heading: title,
  kind, access, required, and the relabel line.
- The Run tab's start summary — the line under Start that already prints the
  limits — adds one line per bound mount, host → container, and the `keep-id`
  note.
- The App tab (§4.9) lists bound mounts.
- The export dialog and the import report show `config_unbound`.

### 5.9 Validation codes

| Code | Blocking | Meaning |
|---|---|---|
| `mount_field_without_container` | yes | a mount field on a `chat` or `batch` manifest |
| `mount_path_refused` | — (HTTP 400 at store; start refused at use) | §5.3 rules 1–4, named |
| `mount_path_nested` | — (same) | §5.3 rule 5, naming the other agent, field and access |
| `mount_path_missing` | start refused | a stored path that no longer exists at use time |
| `mount_unbound` | yes | a `required` mount field with no value — the existing required rule, surfaced with a name that says what to do ("bind `notes` on the Run tab") |
| `token_scope_advisory` | no (part 1) | §3.10 |
| `origin_label_invalid`, `origin_shadows_gateway` | yes (part 2) | §4.1 |

`config.schema.properties.<name>: …` load errors for `default`, `enum` and
`access` misuse are load-time refusals in the existing style, not warning
rows.

---

## 6. Storage and migration

**Migration `0037_owner_keys.sql`.** Rebuilds `api_keys` because SQLite
cannot widen a CHECK in place. It is **not** a copy of `0032`'s shape — that
one deliberately omitted `key_plain` and `agent_id` from its column lists,
which here would drop every agent token's plaintext and then fail its own
CHECK. This one:

- creates `api_keys_new` with `kind IN ('key','internal','agent','owner')`,
  `CHECK ((kind IN ('agent','owner')) = (key_plain IS NOT NULL))` and
  `CHECK ((kind = 'agent') = (agent_id IS NOT NULL))` — the old single
  constraint split so that `owner` keeps a plaintext and has no agent id;
- copies **every** column, `key_plain` and `agent_id` included;
- drops the old table, renames, recreates `CREATE UNIQUE INDEX api_keys_agent`,
  and carries the `sqlite_sequence` watermark as `0032` does.

**Startup seed** (`agents::token::seed_owner_keys`, in `AppState::init`
**before** `resync_all`, and ending with `reload_snapshot()` so that the
shell's `local_url`, the §3.4 log line and the first request all see the
rows; idempotent):

1. `owner:dashboard` absent → mint, enabled, insert. Log `minted the dashboard
   key` once.
2. `owner:self-admin` absent → if `settings.self_admin_token` is non-blank:
   insert with that plaintext and its hash, enabled, then blank the setting
   and log `moved the self-admin token into the keys table`. Else mint,
   **disabled**, insert.
3. Present → nothing.

The same seed runs in the test-only `AppState::init_for_tests`; the real
`init` cannot be exercised in tests because it also reconciles live
containers.

`Settings::self_admin_token` is deleted from the struct after one release in
which it is read for the migration; until then it is read once and blanked.

**Settings** gain `agent_origin_suffix: String` (default `"localhost"`).

**Jobs.** `Input` gains `effective: Option<Map>`; written at start, read by an
apply with `base_job` (§5.7). Existing rows have `None` and behave as today.

**Agents.** No column change. `agents.config` holds mount values as strings.

**Data at rest.** Two more plaintext keys in the database (`owner:*`), beside
the agent tokens that are already there. Principle 5 covers what that does
and does not mean.

---

## 7. API changes

**New routes** — `GET|POST|DELETE /api/session`, `GET /api/session/login`
(§3.4). **Removed routes** — `/agents/{id}/app*` (replaced by the `Public`
`agent_app_moved` 404, §4.2).

**New ops** — `key_reveal`, `key_rotate` (§3.12). **Changed ops** —
`key_create` (+`kind: "owner"`), `key_set` / `key_delete` (owner rules),
`agent_config_set` (path rules, stops a service on a mount change),
`agent_run` (records `effective`; apply merges `base_job`'s), `agent_set` /
import / duplicate (`origin_label_invalid`, `origin_shadows_gateway`,
mount-field load rules), `settings_set` (+`agent_origin_suffix` with
`origin_suffix_shadows_gateway` and the app-container stop, −`self_admin_token`),
`agent_service_start` (response +`origin`).

**DTOs** — `AgentService`: −`app_base`, +`origin`, +`origin_resolves`,
+`mounts: [{ field, host?, inside, kind, access }]` (`host` only in the
`Admin` view). `AgentDetail.config` fields: +`format: "directory"|"file"`,
+`access`; values rendered per view (§3.10). Export envelope:
+`config_unbound`; `ENVELOPE_KEYS` +`config_unbound`. `KeyRow`: `kind` may be
`"owner"`. Settings DTO: −`has_self_admin_token`, +`agent_origin_suffix`.
`ConnectInfo`: unchanged. Warnings: +`token_scope_advisory`.

**Codes** — `session_required`, `forbidden`, `owner_key_disabled`,
`cross_origin_refused` (now on the dashboard plane generally),
`agent_app_moved`, `origin_label_invalid`, `origin_shadows_gateway`,
`origin_suffix_shadows_gateway`, `mount_field_without_container`,
`mount_path_refused`, `mount_path_nested`, `mount_path_missing`,
`mount_unbound`, `token_scope_advisory`, `refuse_owner`.

**Environment** — −`LMGW_APP_BASE`, +`LMGW_APP_ORIGIN`. **`input.json`** —
`service.app_base` → `service.origin`; +`mounts`.

**Every `curl` against `/api`** now needs `Authorization: Bearer <owner key>`.
The MCP self-admin tools (`lmgw__*`) are in-process behind the migrated
credential and need no change.

---

## 8. UI

- **Login view.** `api.rs::decode` maps a `401 session_required` into a
  global `locked` signal; the shell renders a centred card instead of the
  page: "This dashboard needs its session. Open the login link printed in
  the lmgw process log, or paste the key." One input, one button, posting
  `/api/session`. `?login=invalid` on `/` shows the same card with "that
  link's key is not valid — it may have been rotated". The `EventSource` in
  `live.rs` needs no change: the browser attaches the cookie.
- **Keys card.** Owner rows, Copy, Rotate, the `owner:dashboard` rules, the
  one-year session note.
- **Settings.** Self-admin token field and its clear checkbox removed, one
  pointer line added. *Agent origin suffix* field added under Gateway.
- **Definition tab.** `token_scope_advisory` beside the token line; the
  *Mounts* heading.
- **App tab.** §4.9.
- **Schema form, Run tab summary.** §5.8.
- **Tauri capability.** `remote-ui.json` + `dialog:allow-open`.

---

## 9. Documentation to rewrite

`docs/agents.md`: §1 "What lmgw owns and what you own" (add mounts to "what
lmgw owns: the shell"); §2 `config.schema` (two formats, `access`, the two
refusals, the container-kind rule); §2 validation codes (the §5.9 table); §2
`GET /api/agents/{id}` table (`service` fields, `config_unbound`, the agent
view); §5 the `podman run` argv (`--userns=keep-id`, the `-v` line with `:z`),
the `LMGW_*` table (`LMGW_APP_ORIGIN`), `/lmgw/input.json` (`mounts`,
`service.origin`), "Calling back into lmgw" (what `/api` now answers); §7 in
full — "The proxy", "`LMGW_APP_BASE` and building an SPA for it" (deleted),
"`dev_url`" (no path, boot revalidation), "Start, Stop…" (reasons nine and
ten), the App tab and its three lines; §9 "The agent token" and "What the
token does **not** confine" rewritten around the principal and the advisory
state; §11 pre-flight for a container agent (mounts), common failures
(`mount_*`, `session_required`, `origin_*`). Every `curl` example gains the
bearer header.

`README.md` "Agents": the App tab tour, the env-var rows, the base-path
build flags; the self-admin token paragraph; a "Dashboard login" paragraph.

Any external notes that mention `POST /api/op/hold_set` as a release path need
updating: it now needs the bearer. Not this repository's file — noted here so it
is not forgotten.

---

## 10. Testing

**Part 1.**
- Principal resolution: bearer client / agent / owner; cookie owner; cookie
  client → `Anonymous`; cookie agent → `Anonymous`; unmatched bearer →
  `Anonymous`; bearer wins over cookie; `internal` never authenticates however
  its `key_hash` is set; disabled agent and disabled owner name themselves;
  `x-lmgw-admin-token` honoured on `POST /mcp/admin` and nowhere else.
- The route-walk test: enumerate every registered route and method, assert
  each is in the capability table, and for each of the five principal kinds
  assert the status class the table implies. `GET` and `POST` on
  `/api/agents/{id}/runs` and on `/mcp/admin` are asserted separately.
- `policy::admit` runs on `Inference` and not on `Ledger`: an agent key with
  `rpm_limit: 1` posts two ledger events in a minute and both land; its second
  `/v1` call in the minute is `429`.
- `hold_until_body_end`: a streamed `/v1/chat/completions` holds the
  concurrency slot until the body ends (the existing test, still passing).
- Same-origin rule: `Origin` equal / different port / different host;
  `Sec-Fetch-Site` values; bearer skips the rule; the login `GET` allows
  `none` and refuses `cross-site`.
- Login: nonce → cookie once, second use `302 /?login=invalid`, expiry after
  60 s; token → cookie with attributes exactly as §3.3, `302 /`; invalid → no
  cookie; the trace span of any request has no query.
- Rotation: old cookie refused on the next request; `key_rotate` on
  `owner:dashboard` returns the new key; `key_set { enabled:false }` on it is
  refused.
- Migration: every agent row keeps `key_plain` and `agent_id` across `0037`;
  blank token → `owner:self-admin` disabled; non-blank → enabled with that
  plaintext, setting blanked; second start seeds nothing; the snapshot right
  after the seed has the owner rows.
- `/mcp/admin`: no credential → `401`; `owner:self-admin` disabled → `401`
  naming it; `owner:dashboard` → admitted; mode `Off` → empty tool list as
  today; `GET`/`DELETE` unchanged for an `Inference` principal.
- `AgentSelf` view: a bound mount field reads as its container path, `mounts[]`
  has no `host`, `degraded_detail` masks secrets for every principal.
- `token_scope_advisory` present iff `auth_enabled` is off and the agent is a
  container agent.

**Part 2.**
- Host parsing: with port, without, IPv6 literal, uppercase, unknown label
  (`404`, not the SPA), the id of a non-service agent (`404`).
- `origin_label_invalid` (64 chars, trailing `-`), `origin_shadows_gateway`
  (an id equal to the gateway host's first label under a matching suffix),
  `origin_suffix_shadows_gateway`.
- Layer order: an agent-origin response carries no
  `Access-Control-Allow-Origin`; a main-router response still does; both
  appear in the trace.
- `Location` rewrite: published-loopback origin → agent origin; `dev_url`
  origin → agent origin; origin-relative and foreign absolute untouched.
- `validate_dev_url` refuses a path; a stored path-carrying `dev_url` is
  cleared at boot with `dev_url_cleared`.
- Env and `input.json` carry `LMGW_APP_ORIGIN` / `service.origin` and not the
  old names; `X-Forwarded-Host` / `-Proto` present exactly once each and
  lmgw's, a client-supplied `X-Forwarded-Host` dropped; `X-Forwarded-Prefix`
  absent.
- The `agent:<id>` MCP row still stores no header; the transport presents the
  dashboard bearer; the container behind `/agents/{id}/mcp` receives no
  `authorization`; a `/agents/{id}/mcp` request with no principal is `401`.
- `origin_allowed` admits `Origin: http://board.localhost:8001` for a service
  agent `board` and still refuses `http://evil.example`.
- `agent_app_moved` on the old path, without a session.
- A suffix change stops running app containers.

**Part 3.**
- Manifest load: `access` outside a mount format refused; `default` / `enum`
  on a mount field refused; mount field on `chat` → blocking warning.
- Path rules: one test per bullet of §5.3 rule 4, symlink canonicalisation, a
  path that vanishes between store and use (`mount_path_missing`), and rule 5
  in both directions (`mount_path_nested`) with the same-path case allowed.
- Argv: the `-v` line with `rw,z`; `--userns=keep-id` present iff the
  manifest declares a mount field, bound or not; the two lmgw files still
  `ro,Z`.
- `input.json`: container path substituted, `mounts` array shape, host path
  absent from the whole document; template resolves to the container path.
- Export with `include_config=1` carries no mount value and lists it in
  `config_unbound`; that export re-imports.
- Apply merges `base_job`'s `effective`; a run started with an override and
  applied without resending it applies against the override; a job row with
  no `effective` still deserialises.
- A mount change on a service agent stops its container.
- Integration, gated on podman like the existing container tests: a
  `fedora-minimal` image with an `rw` mount writes a file the host then reads,
  owned by the owner's uid; two containers on the same folder both read it.

---

## 11. Work packages (sequential)

Each ends with `ci/check.sh` green and a commit series on `main`.

**WP1 — the gate.** Migration 0037 and the seed (with the snapshot reload
and its place before `resync_all`); `Principal`, `Cap`, `principal_mw`,
`require` with `admit` and the dialect inside `require(Inference)`; the
capability table as a code artifact and the route-walk test; per-handler
`require` on the two mixed paths; the four session routes with nonce and
token; the shell's nonce login; the log line; the global `make_span_with`
without the query; `key_reveal` / `key_rotate` / `key_create` owner kind /
`key_set` rules; `bearer_agent` and `admin_gate` replaced; `x-lmgw-admin-token`
on the admin POST; `self_admin_token` migrated out of Settings; the
`AgentSelf` view in `detail_inner` and the `degraded_detail` masking;
`token_scope_advisory`; the login view, Keys card, Settings and Definition
tab changes in the UI; the MCP client bearer for `agent:<id>` rows (needed
here, not in WP2, because `/agents/{id}/mcp` is gated here); docs §9 and the
`curl` examples.

**WP2 — origins.** `agent_origin_suffix` with its two shadowing checks and
the app-container stop in `settings_set`; the `Host` dispatch layer in its
§3.11 position with CORS moved onto the main router; the proxy
simplification (delete the §4.4 list, keep the §4.5 list, the one rewrite
case); `NEVER_FORWARD` additions and `insert`; `origin_label_invalid`,
`origin_shadows_gateway`; env and `input.json`; `dev_url` path refusal and
boot revalidation; `origin_allowed` admitting agent origins;
`agent_app_moved`; the App tab with `origin_resolves` and the three lines;
docs §7 and README.

**WP3 — mounts.** `Format::{Directory, File}` and `access`; load-time
refusals and the container-kind warning; path rules 1–5 with their tests;
`Mount` struct with access and label, argv, `keep-id` per manifest;
`public_config` substitution and `mounts` in `input.json`; `effective` on
the job and the apply merge; the service-stop reason; `without_secrets_or_mounts`,
`config_unbound` and `ENVELOPE_KEYS` on export; the schema-form control with
the relabel line, Definition and Run tab lines, App tab mounts;
`dialog:allow-open`; docs §2, §5, §11.

Then the example agent, chosen with mounts in hand.

---

## 12. Out of scope and open items

- **`auth_enabled` defaulting to off** leaves a container's token advisory
  (§3.10). Flipping the default for installs that run container agents, or
  refusing a container start while it is off, is a product decision for
  another day; the warning is the floor.
- **Per-plane CORS.** Dropping `permissive()` from the dashboard plane is
  safe after part 1 and would remove the last cross-origin read of `Public`
  routes; it is a one-line change with a small chance of breaking a dev loop,
  so it waits until someone wants it.
- **`origin_allowed`'s loopback list** on `/mcp` and `/mcp/admin` still
  refuses a LAN-bound gateway's own origin. Replacing it with the §3.6 rule is
  straightforward and separate; part 2 only adds agent origins to it.
- **`AgentSelf` writes** — an agent's own UI saving its own config through
  its backend. Wanted for a real settings page inside an agent; not handed
  out until an agent needs it.
- **Mounts for script steps** in batch agents; the shim container could take
  them with no new mechanism.
- **`label=disable`** as a per-mount escape hatch for filesystems without
  xattrs (FAT, some network mounts). The refusal is podman's message until
  then.
- **The mount race** (§5.3) against a same-user process. Passing an `O_PATH`
  descriptor to podman would close it and is worth a spike; it is not
  promised here.
- **A viewer principal** (read-only dashboard for a LAN device) is a `Cap`
  row and a kind; the vocabulary is built to take it.
- **The MCP face on the agent origin** — see §4.8 for why not now.
- **`RequestCtx.key_id`** is removed; the principal carries the id
  (`Principal::key_id`). The remaining cleanup is that pricing still resolves
  the id from `client_key` by name in `proxy::price_call`, rather than reading
  it off the principal directly.
