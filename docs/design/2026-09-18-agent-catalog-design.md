# Agent catalog — design (2026-09-18)

Successor note to the MCP gateway design (2026-06-29) §21 (the server-side
tool loop), the quickdoc design (2026-08-29) §9c (background jobs) and the
usage-analytics design (2026-09-18) §4.4 (internal identities). Replaces the
Workflows surface documented in `docs/design/ui-rebuild/parity.md` § "Workflows".

## 1. Summary

An **agent** is a manifest: one JSON document that names a model, prompts, the
MCP tools it may reach, a config form and a run shape. Agents live in a
catalog that is stored, imported and exported as data. Adding one never
rebuilds lmgw.

- **Tools come only from the MCP plane.** Registered servers (already runtime
  data, already Podman-isolated, already logged) plus the two built-in labels
  `lmgw` and `docs`. Nothing agent-specific is compiled into the gateway, and
  the current IMAP code leaves the tree entirely.
- **Two run kinds.** `chat` is a Chat-thread preset and needs no new runtime.
  `batch` is the mail workflow's shape generalized: list items, one structured
  model call per item, a human review table, one apply step that writes.
- **Runs are jobs.** Every batch run is a `jobs` row: durable, cancelable,
  on the live feed, with its own token and cost total.
- **The mail workflow becomes the first catalog entry**, driven by Google's
  own local Workspace MCP server (§7). Its behaviour is preserved where it
  matters (nothing is written until Apply, unread stays unread) and its
  password-in-a-KV-key config is retired.

The reason it exists: `web/workflows.rs` is 1.7k lines that fuse IMAP, a
hand-rolled model call, a process-global job map and page-specific DTOs into
one workflow that cannot be copied, exported or joined by a second one
(parity.md: "nothing here is generic/pluggable"). The tool loop, the MCP
executor and the jobs subsystem that a generic version needs all exist; they
were built after the workflow was, and it never caught up.

Principles, in the order they win:

1. Nothing an agent needs is compiled in. If a manifest cannot express it, an
   MCP server does the logic. lmgw grows no scripting language.
   > **Superseded 2026-09-19** by the container runtime spec
   > (`2026-09-19-agent-container-runtime-design.md`): an agent's logic runs in
   > a container the owner builds. lmgw still ships no engine.
2. A manifest never carries a secret. Credentials belong to the server
   registration (env, headers) or to a `secret` config field that is
   write-only and never exported.
3. Every model turn and every tool call is a Logs row, priced and attributed,
   exactly like a `/v1/responses` run.
4. The review table is the gate. Nothing writes before a person has looked.
5. Budgets are visible, never guessed (gateway design §21): a run's tool-call
   and wall-clock bounds come from Settings and are shown on the Run tab.

## 2. What an agent is

### 2.1 The manifest (schema v1)

| field | required | meaning |
|---|---|---|
| `schema_version` | yes | `1`. An unknown version is refused at import with the version named. |
| `id` | yes | `[a-z0-9][a-z0-9-]{0,63}`. The catalog key, the export filename, the jobs key. |
| `name`, `description` | name | Catalog card text. |
| `version` | no | Free text, informational; shown on the card and in the export. |
| `model` | yes | `{ "alias": <template>, "temperature"?, "top_p"?, "top_k"?, "seed"?, "reasoning"? }`. The IR `Params` subset. `alias` is usually `{{config.model}}` so the picker on the Run tab decides. **No `max_tokens`**: the model's context is known and the run uses it. |
| `config.schema` | no | A JSON Schema **subset** (§2.6) that renders the Run tab's form and validates stored values. |
| `tools` | no | `[ { "label", "allowed"?, "install"? } ]`. `label` is a registered server's label or `lmgw`/`docs`. `allowed` narrows to exposed tool names; absent means the whole surface. `install` is an import hint (§5). |
| `run` | yes | `{ "kind": "chat", … }` or `{ "kind": "batch", … }` (§2.4, §2.5). |

Unknown fields anywhere are refused (`deny_unknown_fields`), so a typo in a
manifest is an error naming the key rather than a silently ignored step.

### 2.2 Steps

A step is where an agent touches the outside world. Two forms, chosen per
step:

| form | shape | when |
|---|---|---|
| **direct call** | `{ "tool": "<exposed name>", "args": { … } }` | One tool call maps cleanly. Deterministic, no model involved, args are templated (§2.3). |
| **turn** | `{ "turn": { "tools": [<names>], "system"?, "prompt", "output"? } }` | A procedure is needed (look something up, create what is missing, then write). The model runs the existing tool loop with exactly `tools` attached. |

A turn returns its structured `output` when one is declared, else its final
text. The output is enforced in two stages: the final answer is parsed as JSON
against the schema first; if that fails, **one** further call with no tools
and a `response_format` carrying the schema asks the model to state the result
(§4.4). A JSON-schema grammar and tool-call syntax cannot be active on the
same generation, which is why the finalize call is separate rather than the
whole turn being constrained.

The result of a direct call is read as data: an MCP `structuredContent`
block, else the first text block parsed as JSON. A tool that returns prose
fails the step with `the tool '<name>' returned text that is not JSON; a
batch step needs a JSON result` rather than the engine guessing at a format.

### 2.3 Templates

`{{path}}` substitution and nothing else. No conditionals, no filters, no
arithmetic.

- Roots: `config` (stored values over schema defaults), `item` (one element
  of the source result), `fetched` (the item step's fetch result), `rows`
  (the reviewed, checked rows handed to apply), `agent` (`id`, `name`),
  `run` (`id`).
- A string that **is exactly one placeholder** takes the referenced value
  with its JSON type: `"maxResults": "{{config.limit}}"` becomes the integer,
  `"rows": "{{rows}}"` becomes the array.
- A string that contains placeholders among other text renders each one:
  strings verbatim, numbers and booleans as JSON text, arrays of scalars
  comma-joined, objects as compact JSON.
- `config.<field>` must name a field of the config schema; an unknown root
  or field is a **validation error at save or import**, not a runtime
  surprise. `item.*` and `fetched.*` cannot be checked ahead (tool payloads
  vary) and render as empty / `null` when absent, except `item.id`, whose
  absence marks the row as errored.

### 2.4 The `batch` run kind

```
"run": {
  "kind": "batch",
  "source": <step>,                 // must yield an array of objects
  "items_path": "/messages",        // optional JSON pointer into the source result
  "item": {
    "id": "{{item.id}}",            // stable row identity; required
    "fetch": <step>?,               // optional; its result is `fetched`
    "columns": { "date": "{{fetched.date}}", "from": "…", "subject": "…" },
    "system": "…", "user": "…",     // the per-item model call
    "output": { "field": "category", "enum_from": "config.categories", "fallback": "Other" }
               | { "schema": <object schema>, "fallback"?: <value> },
    "concurrency": "{{config.concurrency}}"
  },
  "review": { "editable": ["category"] },
  "apply": <step>                   // receives `rows`
}
```

Lifecycle of one run, as the executor drives it:

| stage | what happens | writes? |
|---|---|---|
| `source` | The step runs once. Result → items. | no |
| `fetch` | Per item, bounded by `concurrency`. | no |
| `classify` | Per item, one model call with `system`/`user` rendered against `item` + `fetched`, `temperature` from the manifest, and the output schema as `response_format` (§4.2). The reply is validated against the schema; the `enum_from` form builds a one-field schema with `fallback` appended to the enum exactly once and last. | no |
| `review` | The run is `done`. Rows are in the job's result. The Run tab shows them split into **attention** (output equals `fallback`, or the call failed) and the rest, with `editable` fields as controls and a checkbox per row. | no |
| `apply` | A second job of the same kind. Input: the checked rows with their (possibly overridden) output. The `apply` step runs with `rows` bound. | **yes** |

Semantics carried over from the mail workflow, now generic:

- **Early abort.** If the first `min(3, n)` classify calls all fail with a 5xx
  or transport error before any success, the run fails as "model
  unavailable" instead of tagging every row `fallback`. In-flight calls are
  dropped.
- **Re-run attention rows.** An action on a finished run that re-classifies
  only the attention rows against the *current* config. The user widens the
  enum by editing the config field first; nothing else changes. Rows already
  classified keep their answer verbatim.
- **List only.** `source` + `fetch` + `columns`, no model call. Still
  read-only, still a job, so the table is browsable without spending tokens.
- **One live run per agent.** The job key is `agent:<id>`; a second start
  while one runs returns the running job rather than a duplicate.
- **GPU hold** (gpu-hold design §2): a run is interactive, someone is
  watching it; classify calls fall back like every other in-process path and
  a refusal fails the run loudly.

### 2.5 The `chat` run kind

```
"run": { "kind": "chat", "system": "…" }
```

"Open" creates a `chat_threads` row: the agent's model alias, `system`
rendered against `config`, `mcp_tools` from `tools`, `kind = "chat"`, and a
new nullable `agent_id` column so the thread lists under the agent's Runs
tab. From there it is an ordinary tool-enabled thread (`web/agentchat.rs`):
same sidebar, same persistence, same Logs rows. An agent that attaches `lmgw`
is bound by the `self_admin` gate like any thread that does; attaching the
label cannot widen it.

### 2.6 Config and secrets

The schema subset the form renderer understands, and the only one the
validator accepts:

| schema | renders as | notes |
|---|---|---|
| `string` | text input | `format: "secret"` → write-only; `format: "model_alias"` → picker over `/v1/models`; `format: "multiline"` → textarea |
| `string` + `enum` | the custom `Select` | |
| `integer`, `number` | number input | `minimum` / `maximum` honoured and shown |
| `boolean` | checkbox | |
| `array` of `string` | comma-separated input, stored as an array | the mail categories |

`default`, `title`, `description` and `required` are honoured. Anything else
(`oneOf`, nested objects, arrays of objects) is refused at save with the
field named. The subset can grow; the point is that a manifest never
contains a schema the form cannot draw.

Stored values are validated against the schema on every save (type,
required, enum, bounds), with the error naming the field. The API returns a
secret as `{ "has_value": true }` and never the value; an empty submission
keeps the stored one, the house convention for tokens. Secrets are stored in
the same JSON column as the rest of the config, which is where upstream keys
and MCP env already live.

## 3. Storage

Migration `0031_agents.sql`:

```sql
CREATE TABLE agents (
    id         TEXT PRIMARY KEY,
    manifest   TEXT NOT NULL,                 -- JSON, schema_version inside
    config     TEXT NOT NULL DEFAULT '{}',    -- JSON values, secrets included
    enabled    INTEGER NOT NULL DEFAULT 1,
    source     TEXT NOT NULL DEFAULT 'authored'
               CHECK (source IN ('builtin','imported','authored')),
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
ALTER TABLE chat_threads ADD COLUMN agent_id TEXT;
-- The internal identity the mail workflow logged under becomes the one every
-- agent run logs under (usage-analytics §4.4). Same row, so its history stays.
UPDATE api_keys SET name = 'internal:agents', note = 'Agent runs: model turns and tool calls'
 WHERE name = 'internal:workflow-mail';
```

- **Not on the `Snapshot`.** Nothing on the request hot path reads an agent;
  the API and the executor read the row when asked. Putting it on the
  snapshot would make every manifest edit a config reload for no reader.
- **Runs are `jobs` rows** of kind `agent_run`, key `agent:<id>` (finished
  rows keep the key, so "runs of this agent" is `WHERE kind = 'agent_run'
  AND key = ?`). `input` holds `{ agent_id, phase, rows?, base_job? }`,
  `progress.detail` holds counts and the current stage, `result` holds the
  rows plus the run's usage total (§4.5). Rows are **not** pushed through the
  generic jobs feed: fifty rows with raw model replies on every 500 ms frame
  to every dashboard tab is the wrong pipe. They sit in the executor's
  per-job buffer while running and in `result` after, read by
  `GET /api/agents/runs/{id}` (§5).
- **Built-ins ship embedded** (`rust-embed`, the same mechanism as the SPA
  bundle) and are seeded on `AppState::init` after migrations. The rule: a
  built-in id is inserted with `source = 'builtin'` once, tracked in the KV
  key `agents:seeded`; a deleted built-in is **not** resurrected on the next
  start. A "Restore shipped agents" action on the catalog re-inserts missing
  ones deliberately. Editing a built-in keeps `source = 'builtin'`; "Reset
  to shipped" restores the embedded manifest and keeps the config — minus any
  stored value the shipped schema no longer declares, or no longer declares as
  a secret, which is pruned and reported (container-runtime §5.1, amended
  2026-09-19).
- **The mail KV migrates in the same seed step** (§7.5), then the key is
  deleted.

## 4. Runtime

`crates/lmgw-core/src/agents/`: `manifest.rs` (types, validation, templates),
`batch.rs` (the `JobExecutor`), `chat.rs` (thread materialization), and
`web/api_agents.rs` for the HTTP face. `web/workflows.rs` and
`web/api_workflows.rs` are deleted in WP4.

### 4.1 The executor

`JobKind::AgentRun`, registered like `HfDownload`. `run()` reads the agent
row, validates the manifest again (a row written by a newer build must not
crash an older one; it fails the job with the reason), then drives §2.4 for
the requested `phase`, publishing `JobProgress { done, total, stage }` at
each row and every stage change. Cancellation is checked between items and
before apply; a cancelled classify keeps the rows it has, a cancelled apply
reports which rows were written by reading the apply step's output.

### 4.2 Model calls

The classify call and the turn's finalize call go through the same
in-process path Admin Chat's `TurnRunner` uses (`proxy::stream_once` with a
collecting sink), **not** a raw POST. The structured-output constraint rides
in `ChatRequest.passthrough` as `response_format`, which the OpenAI egress
already re-emits verbatim and llama-server turns into a grammar. That closes
the `TODO(one-path)` in the old `record_llm_call`: the reason the mail code
hand-rolled its request was that passthrough did not exist when it was
written. On a non-OpenAI route passthrough is dropped by the egress; the
reply is then matched against the enum the way `match_category` does today
(a whole line that is a value, else the earliest value mentioned, else
`fallback`), and a general `schema` output on such a route is parsed from the
first JSON object in the text.

Every call logs with `ingress_proto = "agent"` (new `AGENT_PROTO`), which
`internal_identity` maps to `internal:agents`. The `"workflow"` arm goes.

### 4.3 Tool calls

`mcp::exec::resolve` with one `McpToolSpec` per `tools[]` entry
(`allowed_tools` from `allowed`, `require_approval: Never`; the review table
is the approval), executed by `McpExecutor` with `ingress_proto =
"agent-tool"` (new `AGENT_TOOL_PROTO`). The owner's per-tool disable switch
applies by name, so a tool switched off on the MCP page is refused here with
the same reason. A label that does not resolve fails the run naming the
label and the available ones, the message `resolve` already produces.

### 4.4 The turn step

`agent::run` with `RunConfig { tools, budget, parallel_tool_calls: false }`.
`budget` is the Responses Setting pair (`responses_max_tool_calls`,
`responses_timeout_seconds`), the same knob Admin Chat borrows and for the
same reason: one visible bound for "a tool loop", not a second one that
drifts. The Run tab prints both values next to the Apply button. A run that
exhausts either ends `Incomplete` with the reason named, and the apply job
fails with that message rather than reporting success.

The prompt the turn receives is `system` (rendered), then `prompt`
(rendered) with `rows` bound, so `{{rows}}` inlines the reviewed rows as
JSON. `output` is enforced as §2.2 describes.

> **2026-09-18, from WP4's review.** `{{rows}}` binds
> `[{ id, <output fields…> }]` — the identity and the reviewed answer, **not
> the review columns**. A column is rendered from whatever the source and
> fetch steps returned, which for the mail agent is a stranger's `From` and
> `Subject`, and the apply turn is the only step in a run with write tools
> attached: text an outsider chose does not go into the prompt that can call
> `batchModify`. Columns stay on the row for the review table and the job
> result. A future manifest that genuinely needs one in its apply prompt adds
> an explicit declaration (an `apply.columns` list, say) — it does not widen
> this default for every agent.

### 4.5 Cost per run

The executor sees every `Completion.usage` and every tool call it makes. It
sums prompt, completion and cached tokens and prices them with the same
function the log row uses, and stores `{ usage, cost_micro, model_calls,
tool_calls }` in the job result. The Runs tab shows it per run. Per-agent
attribution in `request_logs` (a column) is out of scope (§10); the identity
row gives "all agents", the job row gives "this run", and that is enough to
answer "what did the mail agent cost this month" by summing the Runs tab.

### 4.6 Failure semantics

| failure | outcome |
|---|---|
| Manifest invalid at run time | job `failed`, message names the field |
| `source` step errors, or returns non-JSON / non-array | job `failed`, message names the tool |
| A `fetch` or classify call errors for one item | row kept with `error`, output = `fallback`, counted as attention |
| Early-abort condition (§2.4) | job `failed` "model unavailable: <first error>" |
| Apply turn errors, stops on budget, or its output fails validation | apply job `failed`; the classify job and its rows are untouched, so Apply can be retried |
| Tool disabled by owner mid-run | that call is refused with the owner's reason; item rows record it, an apply turn sees it as a tool error |

**Amended 2026-09-19** (after the first real apply run): a failed step no
longer loses what it did. Tool calls and usage are folded into the run as they
happen, so a turn that fails after successful tool calls ends the job `failed`
*with* a result carrying every call record and the priced usage
(`JobOutcome::FailedWith`). A cancel reaches an in-flight model or tool call
within ~250 ms (`agent::Cancel`), drops the upstream stream, and records any
call that was already sent as an abandoned call rather than as "nothing
written". The 600 s deadline is no longer the only thing that ends a runaway
call.

## 5. Import, export, API

**Export** `GET /api/agents/{id}/export` → download `<id>.agent.json`: the
manifest plus `exported_at` and `lmgw_version`. Config values are **not**
included by default, since config is deployment state; `?include_config=1`
adds the non-secret values as `config_values`. Secret fields are never
written, with or without the flag, and the export says so in a
`config_omitted` list so the receiver knows what to fill in.

**Import** `POST /api/agents/import?replace=&validate_only=` with the file as
the body (paste uses the same endpoint). In order:

1. `schema_version` known, `id` well-formed, manifest deserializes with no
   unknown fields.
2. Templates resolve against the config schema (§2.3); steps and output
   schemas are well-formed; `run.kind` is known.
3. `tools[]`: each label is a registered server or a built-in label, and
   each `allowed` name is one the server lists **now**. A missing label or
   tool is a **warning**, not a refusal: the agent imports, its card shows
   the gap, the Run tab disables Start with the reason, and the MCP page
   link is prefilled from `install` when present. An MCP server is
   registered before its image is pulled; an agent is imported before its
   server is wired, for the same reason.
4. Existing id: refused unless `replace=1`; replacing keeps the stored
   config, since a manifest update is not a reason to lose the taxonomy.

The response is `{ ok, id, warnings: [..], requires: [{ label, registered,
missing_tools }] }`. `validate_only=1` returns the same report and writes
nothing.

`install` hint shape: `{ "kind": "git" | "image" | "url", "ref": "…",
"notes": "…" }`. It is documentation carried with the manifest; lmgw never
fetches anything from it.

**Reads**

| route | returns |
|---|---|
| `GET /api/agents` | cards: id, name, description, kind, model alias, enabled, source, `requires_ok`, last run (job id, status, finished_at) |
| `GET /api/agents/{id}` | manifest, config with secrets masked, the rendered schema, `requires`, the two budget values, the live job if any |
| `GET /api/agents/{id}/runs` | the agent's `agent_run` jobs, newest first |
| `GET /api/agents/runs/{job_id}` | the job, its rows (live buffer or result), the apply output, usage |

**Ops** (`POST /api/op/{name}`, dispatched like the rest): `agent_set`
(manifest in, create or update), `agent_config_set`, `agent_enable`,
`agent_delete`, `agent_duplicate` (new id, same manifest, config copied
minus secrets), `agent_reset` (built-in → shipped manifest), `agents_restore`
(re-seed missing built-ins), `agent_run` (`phase: list | classify | rerun |
apply`, plus `rows` / `base_job` for the last two), `agent_open_chat`.

**Self-admin tools**: `lmgw__agents` and `lmgw__agent_get` (read class;
manifest and masked config), `lmgw__agent_set` (write; the import path,
returns the same report), `lmgw__agent_run` (write; `list` or `classify`
only, apply stays a human action), `lmgw__agent_delete` (write). An agent
holding the admin token can install an agent, which is the point of the
catalog being data.

## 6. UI

Sidebar: **Workflows** becomes **Agents** at `/agents`, still in the "Use"
group, airy density. `/workflows` redirects to `/agents` so old deep links
land.

### 6.1 Catalog `/agents`

```
Agents                                        [ Search…        ] [Import ▾] [New]
──────────────────────────────────────────────────────────────────────────────
┌ Mail labeler ─────────────── batch ┐  ┌ Docs librarian ─────────── chat ┐
│ Classifies unread Gmail into        │  │ Answers from the docs corpora    │
│ labels; nothing is written until    │  │ and files requests for misses.   │
│ you apply.                          │  │                                  │
│ gemma4-e4b · gws                    │  │ claude-sonnet · docs             │
│ last run: done 18 Sep 09:12  ● on   │  │ 3 threads               ● on     │
└─────────────────────────────────────┘  └──────────────────────────────────┘
┌ Receipt filer ────────────── batch ┐
│ ⚠ needs MCP server 'drive'          │      Import ▾  →  From file… / Paste…
│ …                                   │      Restore shipped agents
└─────────────────────────────────────┘
```

A card: name, kind badge, description, model alias and tool labels, last run
or thread count, enabled toggle, and a warning line when `requires_ok` is
false. Empty state explains import and links the MCP page.

### 6.2 Detail `/agents/:id` — three tabs

**Run.** The config form rendered from the schema (§2.6), then the run
surface for the kind. For `batch`:

```
┌ Config ─────────────────────────────────────────────────────────────────┐
│ Model      [ gemma4-e4b            ▾ ]   Limit [ 50 ]  Concurrency [ 4 ]│
│ Categories [ Newsletter, Promotions, Notifications, Social, Personal, … ]│
│ Label prefix [ lmgw ]                                                    │
│                              [ List only ]  [ Dry run · classify unread ]│
└──────────────────────────────────────────────────────────────────────────┘
classifying 23 / 50 ····················· ▓▓▓▓▓▓▓▓▓░░░░░░░░░  [Cancel]
┌ ⚠ Needs attention (4) ───────────────────────────────────────────────────┐
│ ☑ 17 Sep  Fastmail        Your invoice        [ Other ▾ ]  raw: "Billing" │
│ ☑ 17 Sep  no-reply@…      (call failed: 503)  [ Other ▾ ]                 │
│                                   [ Re-run attention rows ]              │
└──────────────────────────────────────────────────────────────────────────┘
┌ Categorized (46) ────────────────────────────────────────────────────────┐
│ ☑ 18 Sep  GitLab          Pipeline passed     [ Notifications ▾ ]        │
│ …                                                                        │
└──────────────────────────────────────────────────────────────────────────┘
Apply writes labels through gws__gmail_batchModify. Budget: 64 tool calls,
300 s (Settings → Responses).                        [ Apply 48 checked ]
```

Subjects open the details modal showing the exact rendered `user` text the
model received, mid-run included. The progress line follows the jobs frame
on `/api/events` and re-reads the rows on every change of `done`. Apply
turns into its own progress line, then a result block listing the turn's
tool calls and its structured output. For `chat`: the form and an **Open in
Chat** button.

**Runs.** The agent's jobs newest first: phase, status, rows, attention
count, tokens, cost, duration; any finished classify run reopens its review
table; an apply run shows what it wrote. `chat` agents list their threads.

**Definition.** The manifest in a monospace editor with validate-on-save
and the report from §5 rendered inline (errors block, warnings do not),
plus Export, Duplicate, Reset to shipped (built-ins), Delete.

### 6.3 Components

New: `pages/agents.rs` (catalog), `pages/agent_detail.rs`, `widgets/
schema_form.rs` (the §2.6 renderer). Reused: `Modal`, `Select`, toasts, the
dense table inside the airy shell. `pages/workflows.rs` and the `Mail*` DTOs
in `lmgw-api-types` are deleted; the agent DTOs replace them. Styling stays
on the locked graphite-and-blue palette; the review table is a working
surface, not a print layout.

## 7. The mail agent

### 7.1 Which server

Surveyed 2026-09-18. Building our own was the first draft; it is
unnecessary.

| candidate | tools for our purpose | results | auth | verdict |
|---|---|---|---|---|
| **Google's remote Gmail MCP** (`gmailmcp.googleapis.com/mcp/v1`, Streamable HTTP, Developer Preview since 2026-04) | search threads, get message/thread, list/create labels, label/unlabel message and thread; no batch, no send | presumably structured | OAuth 2.0, pre-registered web client, browser sign-in | Blocked twice: lmgw's remote transport sends static headers only (`mcp/mod.rs` `build_http_transport`), so a bearer token dies after an hour; and the preview requires Developer Preview Program enrollment. Revisit after §10's OAuth item. |
| **Google's local Workspace MCP** (`gemini-cli-extensions/workspace`, Apache-2.0, Node, stdio) | `gmail_search`, `gmail_get`, `gmail_listLabels`, `gmail_createLabel`, `gmail_modify`, `gmail_batchModify`, plus drafts/send/attachments; no read touches `UNREAD` (verified in `GmailService.ts`) | JSON | Google's own OAuth client, secret held in a Google cloud function; encrypted local token store; headless login | **Chosen.** Official, JSON, batch labels, fits the stdio transport as-is. Costs: clone-and-build (not on npm), v0.0.8. |
| GongRzhe `Gmail-MCP-Server` and the `safe-gmail-mcp` fork | batch modify by label id; the original also sends, deletes and edits filters | prose (`ID:/Subject:/From:/Date:` blocks) | own GCP OAuth client, refresh token file | Fallback if the Google local server proves unworkable; the prose output would need the `turn` form for `source`. |
| IMAP + app-password servers (Docker MCP catalog `gmail-mcp`, various `imap-mcp`) | list/search/send; no Gmail labels | mixed | app password | No label tools. Out. |

### 7.2 Registration

One `mcp_servers` row, entered on the MCP page (`McpServerPatch` fields):

| field | value |
|---|---|
| name | `gws` |
| transport | `stdio`, bare subprocess (no `container_image`) for now |
| command / args / cwd | `node` / `scripts/start.js` / the clone directory |
| tool_prefix | `gws` → tools appear as `gws__gmail_search` etc. |
| autostart | off; the agent's first call connects it lazily |
| idle_seconds | default; the server reconnects on demand |

Before the first run, once, on the host: clone, `npm install`, build, then
the headless login the README describes (`npm run auth-utils -- login`),
which stores an encrypted refresh token. Exact tool names and the token
path are confirmed on the first `tools/list`; the manifest below assumes
the default dot-to-underscore normalization the server applies.

Podman isolation is the second step, once the token directory is known:
`container_image` + `extra_run_args` mounting that directory with `:Z`.

**Kill switch**: `tool_set` disables `gws__gmail_send` and
`gws__gmail_sendDraft` by name. The agent never attaches them (`allowed`
narrows to five tools), but a Chat thread that attaches `gws` whole must not
be able to send either.

### 7.3 The manifest (`mail-labeler.agent.json`, embedded)

```json
{
  "schema_version": 1,
  "id": "mail-labeler",
  "name": "Mail labeler",
  "description": "Classifies unread Gmail into labels. Nothing is written until you apply.",
  "version": "2.0.0",
  "model": { "alias": "{{config.model}}", "temperature": 0.0 },
  "config": { "schema": { "type": "object",
    "properties": {
      "model":        { "type": "string", "format": "model_alias", "default": "gemma4-e4b" },
      "categories":   { "type": "array", "items": { "type": "string" },
                        "default": ["Newsletter", "Promotions", "Notifications", "Social",
                                    "Personal", "Work", "Finance", "Receipts", "Travel", "Spam"] },
      "label_prefix": { "type": "string", "default": "lmgw",
                        "description": "Parent label; empty applies bare category names" },
      "limit":        { "type": "integer", "default": 50, "minimum": 1 },
      "concurrency":  { "type": "integer", "default": 4, "minimum": 1,
                        "description": "Parallel classify calls; match the local server's -np" }
    }, "required": ["model"] } },
  "tools": [ { "label": "gws",
    "allowed": ["gws__gmail_search", "gws__gmail_get", "gws__gmail_listLabels",
                "gws__gmail_createLabel", "gws__gmail_modify", "gws__gmail_batchModify"],
    "install": { "kind": "git", "ref": "https://github.com/gemini-cli-extensions/workspace",
                 "notes": "node scripts/start.js; run the headless login once" } } ],
  "run": { "kind": "batch",
    "source": { "tool": "gws__gmail_search",
      "args": { "query": "is:unread in:inbox", "maxResults": "{{config.limit}}" } },
    "items_path": "/messages",
    "item": {
      "id": "{{item.id}}",
      "fetch": { "tool": "gws__gmail_get", "args": { "messageId": "{{item.id}}", "format": "full" } },
      "columns": { "date": "{{fetched.date}}", "from": "{{fetched.from}}", "subject": "{{fetched.subject}}" },
      "system": "You are an email triage assistant. Read the email and pick the single best-fitting category from this list: {{config.categories}}. If none clearly fit, pick Other.",
      "user": "From: {{fetched.from}}\nTo: {{fetched.to}}\nDate: {{fetched.date}}\nSubject: {{fetched.subject}}\n\n{{fetched.body}}",
      "output": { "field": "category", "enum_from": "config.categories", "fallback": "Other" },
      "concurrency": "{{config.concurrency}}" },
    "review": { "editable": ["category"] },
    "apply": { "turn": {
      "tools": ["gws__gmail_listLabels", "gws__gmail_createLabel", "gws__gmail_batchModify"],
      "system": "You apply Gmail labels exactly as instructed. You never remove labels and never touch UNREAD.",
      "prompt": "Label prefix: '{{config.label_prefix}}'. For every row below, the label is '<prefix>/<category>', or just '<category>' when the prefix is empty. List the labels, create any that are missing, then call batchModify once per distinct label with all of its message ids in addLabelIds. Rows: {{rows}}",
      "output": { "type": "object",
        "properties": { "applied": { "type": "integer" },
                        "labels": { "type": "array", "items": { "type": "string" } } },
        "required": ["applied", "labels"] } } } }
}
```

> **Amended 2026-09-19 (container runtime §4.3).** `apply.turn` is no longer a
> supported apply shape: a model deciding what to write is neither
> deterministic nor reviewable. A manifest that still declares it **loads with
> a warning and Start disabled**, rather than being refused, so an existing row
> can be read and fixed. The shipped mail labeler applies through a `script`
> step instead (container-runtime §9); the manifest above is kept here as the
> record of what it was.

### 7.4 What stays the same, what changes

| behaviour | old (IMAP) | new |
|---|---|---|
| Read-only until Apply | `EXAMINE` + `BODY.PEEK[]` | `gmail_get` never modifies; Apply is the only turn with write tools attached |
| Unread stays unread | `X-GM-LABELS.SILENT` never touches `\Seen` | `batchModify` with `addLabelIds` only; the system prompt forbids `removeLabelIds` and the reviewer sees every call |
| Labels | one `UID STORE` per distinct label | list, create-if-missing, one `batchModify` per label |
| Auth | IMAP app password in the KV key | OAuth refresh token held by the server; no credential in lmgw |
| Model input | `extract_content`: decoded headers minus a noise list, full body | `gmail_get format=full` rendered through the `user` template; **the extra signal headers (List-Unsubscribe etc.) are not carried**, an accepted loss |
| Details modal | the exact model input | same, the rendered `user` string |
| "Other" fieldset, widen, re-run | bespoke | §2.4 attention rows + re-run |
| Early abort | bespoke | §2.4 |
| Run survives a reload | no (`Expired`) | yes, a jobs row |
| Structured output | raw POST with `response_format` | passthrough on the in-process path (§4.2) |

### 7.5 Migration and deletion

In the seed step (§3), if the KV key `workflow:mail` exists: `model`,
`categories` (split, deduped, `Other` dropped since `fallback` supplies it),
`label_prefix`, `limit` and `concurrency` become the mail-labeler's config;
`host`, `port`, `username`, `password` are dropped and a log line says so;
the key is deleted. The seed never re-runs (§3).

Deleted in WP4: `web/workflows.rs`, `web/api_workflows.rs`, their routes
and the `mail_` op prefix in `web/api.rs`, the `/api/workflows/mail*` and
`/workflows` entries in `tests/it/web_pages.rs`, `pages/workflows.rs`, the
`Mail*` DTOs, the `"workflow"` identity arm, and the `async-imap` and
`mail-parser` dependencies. `docs/design/ui-rebuild/parity.md` § Workflows is
replaced by a pointer to this spec; the README gains an Agents section.

## 8. Testing

- **Manifest** (`agents/manifest.rs`): every row of §2.1 refused when
  malformed with the field named; unknown field refused; unknown
  `schema_version` refused; `config.<unknown>` in a template refused; the
  §2.6 subset accepted and everything outside it refused; `enum_from` builds
  the enum with `fallback` last and once.
- **Templates**: whole-value typing (integer, array), mixed-string rendering
  of each JSON type, absent `item.id` → row error, absent other paths →
  empty.
- **Batch executor** against a fake `ToolExecutor` and a fake `TurnRunner`
  (both exist for `agent.rs`'s tests): the happy path; `structuredContent`
  and JSON-text results both accepted, prose refused naming the tool;
  `items_path`; concurrency bound observed; the early-abort rule (3 failures
  before any success aborts, 2 then a success does not); re-run touches only
  attention rows; cancel between items keeps rows; one live run per agent.
- **Turn step**: final text that validates is taken as-is; text that does
  not triggers exactly one finalize call with `response_format` and no
  tools; a budget stop fails the apply job with the reason.
- **Model call path**: on an OpenAI-protocol route the wiremock upstream
  receives `response_format` with the enum; on an Anthropic route it
  receives none and the reply is enum-matched.
- **Import/export**: round trip is byte-stable modulo `exported_at`; secrets
  never appear in an export with or without `include_config`; a missing
  server label imports with a warning and `requires_ok = false`;
  `validate_only` writes nothing; `replace` keeps config — except for keys the
  new manifest does not declare and keys that stopped being `format: "secret"`,
  which every replace path now prunes and reports in `dropped_config`
  (container-runtime §5.1, amended 2026-09-19).
- **Seed**: idempotent across restarts; a deleted built-in stays deleted;
  the KV migration copies the five fields, drops the four, deletes the key,
  and runs once.
- **Routes**: `tests/it/web_pages.rs` gains `/agents`, `/agents/mail-labeler`
  and the §5 reads; the `mail_*` guard-rail tests are ported to `agent_run`
  (`classify` without a model refused before any tool call; unknown op
  named).
- **Manual, once**: the Workspace server registered, `tools/list` shows the
  five names the manifest expects, a list-only run against the real mailbox,
  a dry run, an apply on two rows, and `UNREAD` still on both.

## 9. Work packages (sequential)

1. **Manifest, store, API.** `agents/manifest.rs` with validation and
   templates; migration 0031; `web/api_agents.rs` reads, ops, import/export;
   the self-admin tools; DTOs. Seed of built-ins (without the mail manifest
   yet). Tests for §8's first two and the import/export items.
2. **Catalog UI.** `/agents`, `/agents/:id` with Run (form only), Runs
   (threads), Definition; the schema form widget; the `chat` kind end to
   end via `agent_open_chat`; `/workflows` redirect; nav rename.
3. **Batch runtime.** `JobKind::AgentRun`, `agents/batch.rs`, the turn step
   with finalize, `AGENT_PROTO`/`AGENT_TOOL_PROTO`, per-run cost; the Run
   tab's progress, review table, apply, re-run; the Runs tab for jobs.
   Tests for the executor, turn and model-call items.
4. **Mail.** Register `gws`, confirm tool names, embed the manifest, the KV
   migration, the manual check in §8, then delete everything in §7.5 and
   update the docs. Disable the two send tools.

## 10. Out of scope, and open items

- **OAuth for remote MCP servers.** Its own spec: per-server client id and
  secret, the MCP authorization flow with a loopback redirect on the
  dashboard, refresh-token storage, token injection on reconnect. Unlocks
  Google's remote Gmail server and the GitHub, Atlassian and Notion remote
  servers alike. Swapping the mail agent to it is then a `tools[]` edit.
- **Scripting.** No Rhai, Lua, JS or WASM steps. The escape hatch is an MCP
  server, where Podman isolation already is.
  **Superseded 2026-09-19** by `2026-09-19-agent-container-runtime-design.md`:
  the escape hatch is the agent's own container, and `script` is sugar over a
  stock Node image — still no engine linked into lmgw.
- **Run kinds beyond `batch` and `chat`.** A multi-stage pipeline or a
  scheduled watcher is a new `run.kind` with its own spec, not a DSL grown
  onto this one.
- **Scheduling.** `lmgw__agent_run` plus any cron covers "classify every
  morning"; Apply stays a click by design.
- **Per-agent cost column in `request_logs`.** The job row carries the run's
  total; the identity row carries all agents. Add the column when a real
  question needs it.
- **Approvals inside a batch run.** The review table is the gate;
  `require_approval` remains a `/v1/responses` concern.
- **Model-input fidelity for mail.** If the dropped signal headers turn out
  to matter for classification, `gmail_get` with `format: "metadata"` and a
  second fetch is the fix, not a code path.
