# How to write agents for lmgw

This is the reference for authoring an agent: the manifest schema field by
field, the three run kinds, the script shim, the container contract, the HTTP
run ledger, service mode, packaging as an image, and what the token does and
does not buy. Everything here is what the code does on `main`.

**On numbers.** Every number that shapes a run is one of two things: a field you
can see and set (`run.limits.*`, `run.service.*`, `run.item.concurrency`,
Settings → Agents & tools), or a constant this document names outright — the 12-line
stderr excerpt a failure message quotes (the full text is always in the run
log), the 250 ms health-probe poll, the `min(3, n)` classify abort window.
There is no third category: lmgw has no unstated caps, and every bound a run is
under is printed on its Run tab. `0` means "no limit" everywhere except
`run.limits.stop_grace_seconds`, where it is the strictest setting rather than
the loosest.

The design rationale lives in the two specs linked at the end. This document is
the contract.

---

## 1. What an agent is

<!-- source: crates/lmgw-core/src/agents/mod.rs, crates/lmgw-core/src/agents/manifest.rs -->

An agent **is** one JSON document — the manifest. It names a model alias,
prompts, the MCP tool labels it may reach, a config form and a run shape.
Nothing about an agent is compiled into lmgw, so adding one never rebuilds the
gateway. If a manifest cannot express something, an MCP server or a container
image does the logic.

The manifest is stored in the `agents` table as canonical text, alongside three
things that are **not** part of it and are never exported: the stored config
values, the `provenance` record of which image the document came from, and the
`dev_url` override.

### The three run kinds

`run.kind` picks one of three shapes, and it decides everything else about the
document:

| `run.kind` | What it is | What runs it |
|---|---|---|
| `chat` | A Chat-thread preset: alias, system prompt, tool labels | *Open in Chat* materializes a `chat_threads` row; from there it is an ordinary thread |
| `batch` | A pipeline: list → fetch → classify → review table → apply | lmgw's in-process executor (`agent_run` jobs) |
| `container` | An OCI image lmgw starts once per phase | `podman run`, with the image reporting rows back over stdout |

A `batch` agent's apply step may be a `script` — a short ES module lmgw runs in
a stock Node container. That is sugar over the container runtime, not a fourth
kind.

### Where the catalog lives

<!-- source: crates/lmgw-core/src/web/api_agents.rs, crates/lmgw-core/src/mcp/selfadmin.rs, crates/lmgw-core/src/mcp/selfadmin/catalog/agents.rs, crates/lmgw-core/src/mcp/selfadmin/catalog/reads.rs -->

- **Dashboard**: **Agents** in the sidebar (`/agents`). Each card opens a detail
  page with tabs: *Run* (the config form, Start, the review table), *Runs* (past
  runs, reopenable — labelled *Threads* for a `chat` agent), *App* (only for an
  agent that declares `run.service`), and *Definition* (the manifest in a
  textarea with Validate, Save, Export, Duplicate, Copy token, Rotate, Reset and
  Delete).
- **HTTP reads**: `GET /api/agents` (the catalog),
  `GET /api/agents/{id}` (one agent in full — see below),
  `GET /api/agents/{id}/runs` (every run of this agent, newest first, **no
  limit**: the jobs table is already bounded by two visible retention settings),
  `GET /api/agents/runs/{job_id}` (one run with its rows, its log and its
  result — what `lmgw__agent_run`'s `next_step` tells a caller to poll), and
  `GET /api/agents/{id}/export`.
- **HTTP writes**: `POST /api/agents/import` (a file body), the three ledger
  routes (§6), and everything else as `POST /api/op/{name}`: `agent_set`,
  `agent_install`, `agent_config_set`, `agent_enable`, `agent_delete`,
  `agent_duplicate`, `agent_reset`, `agent_open_chat`, `agent_run`,
  `agent_run_cancel`, `agent_token_get`, `agent_token_rotate`, `agent_pull`,
  `agent_reimport`, `agent_dev_url_set`, `agent_service_start`,
  `agent_service_stop`, `agent_service_log`, `agents_restore`.
- **Self-admin tools** (on `/mcp/admin`, behind the `owner:self-admin` key):
  `lmgw__agents` (the catalog, trimmed), `lmgw__agent_get` (manifest + masked
  config), `lmgw__agent_set` (install or replace from a manifest),
  `lmgw__agent_install` (install from an image), `lmgw__agent_run` (`list` and
  `classify` only — apply is a human action), `lmgw__agent_delete`.
  `agent_token_get` and `agent_token_rotate` are deliberately **not** exposed as
  `lmgw__*` tools.
- **Import/export**: an agent is a file. `GET /api/agents/{id}/export` downloads
  `<id>.agent.json`; `POST /api/agents/import` takes one back, with
  `?replace=1` and `?validate_only=1`.

### What `GET /api/agents/{id}` returns

The one read worth knowing, because it is how you inspect everything the
manifest does not carry. `lmgw__agent_get` returns the same document.

| Key | What it is |
|---|---|
| `manifest` | The canonical manifest **as text**, never a parsed object — a round trip through a JSON value would alphabetize the config form. |
| `config` | The stored values, **masked**: every `secret` field is `{ "has_value": … }`, and — read as the agent itself, not the owner — every mount field's value is its container path, `/lmgw/mounts/<field>`, never the host one. |
| `fields` | The config schema converted for a form renderer: name, type, format, title, description, default, enum, min/max, required, `has_value`. |
| `requires_ok` / `requires` | Whether every `tools[]` label resolves, and per label what is missing plus its `install` hint. |
| `warnings` | `[{ code, message, blocks_start }]` — the list in §2. The Start gate is `requires_ok && no blocks_start`. |
| `runtime` | For a container agent (and a batch agent with a `script` apply): image, pull policy, phases, every `run.limits` value, `output_validated`, and whether podman answered. `null` otherwise. |
| `service` | For a `run.service` agent: the manifest's four fields, `provides_mcp`, `origin`, `origin_resolves`, `mounts[]` (`field`, `host` — **`Admin` view only** — `inside`, `kind`, `access`), and what is running right now — `starting`, `running`, `host_port`, `container`, `started_at`, `idle_seconds_now`, `in_flight`, `log_tail` (+ `log_tail_lines`). `null` otherwise. |
| `provenance` | Which image this row was installed from: `image`, `digest`, `manifest_path`, `installed_at`, `pulled_at`. **Absent** for a row that came from a pasted manifest, rather than a record full of empty strings. |
| `dev_url` | The row's dev-server override, `""` when there is none. |
| `token` | `{ name, has_value, scope_note }` — never the token itself. |
| `portability` | `{ portable, notes }`, the same verdict the export carries, but with the `dev_url` named. |
| `batch` | The review surface: columns, editable fields with their options, `has_classify`, `has_apply`, and the `apply_tools` ceiling. |
| `budget` | Settings → Agents & tools: `max_tool_calls` and `timeout_seconds`, which bound a `turn`. |
| `live_job` / `resettable` / `threads` / `currency` / `error` | The run in flight, whether a shipped manifest exists for this id, the chat threads, the display currency, and why the row could not be parsed. |

`config`, `provenance` and `dev_url` are read **only** through here — none of the
three is part of the manifest, and none is ever exported.

A row whose manifest this build cannot parse still answers `200`, with whatever
the raw JSON yields, `error` set, and one warning `manifest_unreadable` that
blocks Start. A `400` would put the Definition editor out of reach of the very
manifest that needs fixing.

### What lmgw owns and what you own

lmgw owns **identity** (one agent token, minted on demand), **money** (the
`X-Lmgw-Run` header folds a call's cost into the run's total), **logs** (stderr
verbatim, non-JSON stdout verbatim, the run log), **secrets** (a `0600` file on
a host tmpfs, bind-mounted read-only) and **the shell** (`podman run`, the
cgroup limits, the cancel sequence, the deadline — and, for a host mount, the
bind, the relabel and `--userns=keep-id`).

You own the logic, and — in service mode — the UI. A mount field splits the
same way: the manifest owns the **slot**, you own the **path** — a manifest
can never set one (§2).

---

## 2. The manifest, field by field

<!-- source: crates/lmgw-core/src/agents/manifest.rs, crates/lmgw-core/src/agents/manifest/tests.rs -->

**Unknown fields are refused everywhere** (`deny_unknown_fields`), at every
level of the manifest's own structure. A typo is an error naming the key, never
a silently ignored step.

**Duplicate keys are refused in the manifest's own fields** and in the two
order-preserving maps — `config.schema.properties` and `run.item.columns` —
where the error names the key (`duplicate key 'x'`). They are **not** refused
inside the raw-JSON regions a manifest carries: a step's `args`, a `turn.output`
or `run.output.<phase>` schema, an item output's `schema`. Those are ordinary
`serde_json::Value`s and a duplicate there resolves **last-wins**, silently.
Do not rely on it either way.

### The four shapes, minimally

The smallest manifest of each kind that loads. Everything else in this section
is optional unless the table says otherwise.

```json
{ "schema_version": 1, "id": "a", "name": "A",
  "model": { "alias": "qwen3.8" },
  "run": { "kind": "chat" } }
```

```json
{ "schema_version": 1, "id": "b", "name": "B",
  "model": { "alias": "qwen3.8" },
  "run": { "kind": "batch",
           "source": { "tool": "t__list" },
           "item": { "id": "{{item.id}}" } } }
```

```json
{ "schema_version": 1, "id": "c", "name": "C",
  "model": { "alias": "unused" },
  "run": { "kind": "container", "image": "localhost/c:1" } }
```

```json
{ "schema_version": 1, "id": "d", "name": "D",
  "model": { "alias": "unused" },
  "run": { "kind": "container", "image": "localhost/d:1",
           "service": { "port": 8080 } } }
```

Required per kind, beyond `schema_version`, `id`, `name` and `model.alias`:

| Kind | Required | Notes |
|---|---|---|
| `chat` | nothing | `run.system` is optional. |
| `batch` | `run.source`, `run.item.id` | A list-only agent. `run.item.user` + `run.item.output` add the classify stage; `run.apply` adds the write. |
| `container` | `run.image` | …unless the manifest declares `run.service`, in which case the app can come from a `dev_url` and the image may be omitted. `run.phases` defaults to `["run"]`. |
| `container` + `service` | `run.service.port` | Everything else in `service` has a default. |

**`model.alias` is required by the schema but is only ever *resolved* when
something asks for a model** — a `turn`, a classify call, or opening a `chat`
thread. A container agent whose image never calls `/v1` runs fine with an alias
this gateway does not serve; nothing looks it up. It must be non-blank, and if
it is a template it must name a declared config field.

Choose it with the **token scope** in mind (§9), because that is the one thing
it always affects: a *literal* alias scopes the agent's token to exactly that
alias, so `"alias": "unused"` on an image that then calls `/v1` gets refused by
name. Three honest choices:

- the image never calls a model → any literal placeholder; the token is scoped
  to a name nothing serves, which is the correct answer;
- the image calls one fixed model → name it literally;
- the owner should choose → `"{{config.model}}"` plus a
  `format: "model_alias"` config field, and the picker sets the scope.

### Top level

| Field | Type | Required | Notes |
|---|---|---|---|
| `schema_version` | integer | yes | Must be `1`. Read off the raw JSON before anything else, so a future document is refused naming its version rather than producing a pile of unknown-field errors. |
| `id` | string | yes | `[a-z0-9][a-z0-9-]{0,63}`. Max 64 characters. `runs`, `import` and `new` are reserved by the `/api/agents` route table. |
| `name` | string | yes | Must not be blank. |
| `description` | string | no | Default `""`; omitted from the canonical output when empty. |
| `version` | string | no | Free text, informational. Shown on the card and in the export. |
| `model` | object | yes | See below. |
| `config` | object | no | `{ "schema": … }` and nothing else. |
| `tools` | array | no | Default `[]`. |
| `run` | object | yes | Tagged by `kind`. |

**The canonical serialization** (`Manifest::to_json`) is what gets stored and
exported. It is *pretty-printed*, and it re-orders anything that is not one of
the two order-preserving maps, so do not expect a byte-compare with your
hand-written file to match — what is stable is the round trip through lmgw:
load → `to_json` → load gives the same manifest, every time.

Two positions keep the order you wrote, because the form and the review table
read top to bottom: **`config.schema.properties`** and **`run.item.columns`**.
Everywhere else a manifest carries raw JSON — a step's `args`, a `turn.output`
or `run.output.<phase>` schema, an item output's `schema` — the keys come back
**sorted**, because `serde_json::Map` is a `BTreeMap` in this build. That is
cosmetic for a schema and for tool arguments; it is why those two maps are
special-cased and nothing else is.

For the same reason, hand a manifest to `agent_set` (and to `lmgw__agent_set`)
as a **JSON string**, not as a JSON object: an object has already been parsed
into a sorted map by the time the op sees it, so the config form comes out
alphabetical. The op accepts it and *says* it re-sorted, rather than refusing.

### `model`

| Field | Type | Notes |
|---|---|---|
| `alias` | string, required | The model alias. Usually `{{config.model}}` so the Run tab's picker decides. Must not be blank. Validated against the config schema with the roots `config`, `agent`, `run`. |
| `temperature` | number | |
| `top_p` | number | |
| `top_k` | integer | |
| `seed` | integer | |
| `reasoning` | object | lmgw's `ReasoningControl`. |

There is **no `max_tokens`**. The model's context length is known exactly from
the model selection and a run uses it; a guessed ceiling here would be an
invisible cap on every answer.

A `chat` agent drops `top_p`, `top_k`, `seed` and `reasoning` when it opens a
thread (a `chat_threads` row has a column for `temperature` only) and the open
reports which ones it dropped as a warning.

### `config.schema`

The form on the Run tab is drawn from this. It is a **subset** of JSON Schema,
and everything outside the subset is refused with the property named
(`config.schema.properties.<name>: …`). All errors are reported, not just the
first.

The schema object itself takes exactly: `type` (must be `"object"`),
`properties`, `required`, `title`, `description`. Property order **is**
preserved here — the form renders top to bottom in the order you wrote — and a
duplicate property name is refused naming the key rather than resolving
last-wins.

Each property takes exactly these keywords:

| Keyword | Applies to | Notes |
|---|---|---|
| `type` | all, required | One of `string`, `integer`, `number`, `boolean`, `array`. |
| `title` | all | The form label. |
| `description` | all | The help line under the field. |
| `default` | all | Type-checked against `type`, `enum`, `minimum`, `maximum` at load. |
| `format` | `string` only | One of `secret`, `model_alias`, `multiline`, `directory`, `file`. |
| `enum` | `string` only | Array of strings; renders a select. Refused on a mount field. |
| `access` | `string` with `format: "directory"` or `"file"` only | `"ro"` (default) or `"rw"`. Refused everywhere else, naming the property. |
| `minimum` / `maximum` | `integer`, `number` only | |
| `items` | `array` only, required there | Must be `{"type": "string"}`. `title`/`description` inside `items` are accepted and ignored. |

The five formats:

- `format: "model_alias"` — a picker over `/v1/models`. **These fields derive
  the agent token's scope** (§9).
- `format: "secret"` — write-only. Never returned by the API, never exported,
  and refused in any model prompt (see the validation rules below).
- `format: "multiline"` — a textarea.
- `format: "directory"` / `format: "file"` — a host path, bind-mounted into
  the container at `/lmgw/mounts/<field>` (§5). The manifest names the slot,
  never the path: `default` and `enum` are refused on a mount field (see the
  validation rules below), and only the owner sets the value. `access` —
  `"ro"` (default) or `"rw"` — is legal only on these two formats. Legal only
  on a `run.kind: "container"` manifest: on `chat` or `batch` it is the
  blocking warning `mount_field_without_container` ("`<name>` is a directory
  field, but this agent has no container to mount it into"). On the Run tab
  the field shows the path, **Choose…** (the shell's native dialog; in a
  plain browser, an editable text input with the placeholder "absolute path
  on the gateway machine"), **Clear**, and a chip — `directory · rw` or
  `file · ro`. Its help line, repeated on the Definition tab: "binding
  relabels this folder and everything under it for containers
  (`container_file_t`); nothing undoes it when the agent stops. With `rw`,
  the container runs as your user and can delete or change anything under
  the mount — `--cap-drop=ALL` does not stop that."

An array field is an array **of strings only**; nothing else is supported.

**Path rules**, checked wherever a mount value is **stored** —
`agent_config_set`, the per-run override on `agent_run`, import, and
`agent_duplicate` — and again whenever it is **used** (a phase start, a
service start), because the filesystem can change between the two. A save
checks only the fields it sets, so a folder that went missing under some
other field does not block an unrelated save; the message says to clear the
field or point it at a folder that exists, and the dead mount is still
refused the next time it is actually used:

- Absolute, or refused.
- Canonicalised — symlinks resolved. The canonical path is what is stored,
  mounted and shown, not necessarily what you typed.
- The kind must match the format: a `directory` field names a directory, a
  `file` field a regular file.
- Refused by location, with the reason in the message: `/` and every ancestor
  of `$HOME`; `$HOME` itself; `$HOME/.ssh` and `$HOME/.gnupg`; the lmgw data
  directory and its ancestors; the lmgw runs root; and `/proc`, `/sys`,
  `/dev`, `/run`, `/boot`, `/etc` and everything under them. The home, data
  and runs directories are compared canonicalised, so a symlinked `$HOME` or
  data directory cannot slip past this rule; when the gateway cannot
  determine the home directory, every mount is refused rather than the home
  rules being silently dropped.
- **Not nested in, nor containing, another bound mount when either side is
  `rw`**, across every agent and every field — `ro` over `ro` is allowed. The
  *same* path bound twice is allowed whatever the access; that is what the
  shared relabel is for (§5).

A store-time refusal is `400 mount_path_refused` or `mount_path_nested`,
naming the field and the rule; a use-time refusal fails the start the same
way and never runs `podman`. There is no allow-list and no "safe" prefix —
anything not refused is the owner's choice.

**How values are stored, masked and stripped:**

- The `agents.config` column holds a plain JSON object of the values.
- A read (`GET /api/agents/{id}`, `lmgw__agent_get`) returns
  `masked_values`: every `secret` field is replaced by `{ "has_value": true }`
  or `{ "has_value": false }`, and any key the schema no longer declares is
  dropped from the view.
- `agent_config_set { id, values, clear? }` is a **sparse patch**: a key the
  submission does not mention keeps its stored value. A `secret` field submitted
  as `null`, `""` or as the `{ "has_value": … }` object it was read back as
  **keeps** the stored secret. `clear` is an explicit array of field names to
  remove, applied after the merge; it may name a field the schema no longer
  declares (that is the only way to get rid of an orphaned value) and the
  response reports those as `cleared_undeclared`.
- An export uses `without_secrets`: every `secret` field and every key the
  schema does not declare is removed, and `config_omitted` in the envelope names
  the secret fields so the receiver knows what to fill in. A mount field's value
  is stripped the same way, named instead in `config_unbound` (§8) — there is a
  slot to bind, not a secret to fill in.
- `agent_duplicate` copies the config **minus secrets**, and copies mount values
  as they stand — same box, same paths.
- The `config` root a template resolves against is `effective_values`: stored
  values over schema defaults.

**Saving is not starting.** The Run tab's form is what a start uses; Save
config is what sets the default it falls back to. `agent_open_chat` and
`agent_run` both take an optional `values` object — the form as it stands at
the click — and merge it over the stored config **for that one thread or run**,
writing nothing back. So picking a different model and pressing Open in Chat
opens a thread on that model and leaves the agent's saved default alone; press
Save config when you want the change to stick.

`values` follows the same rules `agent_config_set` does — sparse, with the
`secret` keep rule intact — with one difference: it is validated as the
*complete* config it is about to be run as, so blanking a required field is
refused naming the field rather than quietly falling back to what was stored.
A caller with no form behind it (`lmgw__agent_run`, curl) sends no `values` and
gets the saved config, which is the only config it could have meant.

### `tools[]`

| Field | Type | Notes |
|---|---|---|
| `label` | string, required | A registered MCP server's `tool_prefix`, **or its `name`** — a server with a prefix answers to both, so either spelling resolves. Or the built-in `lmgw` / `docs`. Must not be blank. |
| `allowed` | array of strings | The exposed tool names this entry narrows to. **Absent means the whole label's current surface.** Entries must not be blank. |
| `install` | object | `{ kind, ref, notes? }` — documentation carried with the manifest. `kind` is `git`, `image` or `url`. `ref` must not be blank. **lmgw never fetches anything from it**; it is a hint for whoever installs the agent. |

A label the gateway has not registered is a **warning, not a refusal**: the
agent saves and its card shows the gap, but Start stays disabled
(`requires_ok` is false). The same holds for a name in `allowed` that the label
does not currently list.

If the manifest declares `run.provides.mcp`, an `allowed` name starting with
this agent's own `<id>__` prefix is **refused**: the agent would be calling
itself out through `/mcp` and back in through its own app proxy.

**The two built-in labels behave differently from a registered server, and
differently again per run kind.**

`lmgw` — an in-process `batch` run resolves it through the self-admin executor,
but only as far as *Self-admin tools* (**Settings → Network & access**) allows: at `read_only`
(the default) only the read tools resolve, and at `off` nothing resolves and the
run is told why. A **container or script** calling `/mcp` cannot reach them at
all, whatever the allow list says: `lmgw__*` lives on `/mcp/admin` behind the
`owner:self-admin` key, and the aggregate plane answers with
*"`<name>` — the lmgw__\* self-admin tools are no longer served on /mcp; they
moved to /mcp/admin, which needs the owner:self-admin key from Usage → Keys"*.
The same holds for a `/v1/responses` run the container starts with its token:
attaching the `lmgw` label there needs an owner credential, and every other
label resolves through the manifest's allow list, just as `/mcp` does.

`docs` — the `docs__*` toolset is on `/mcp` and is reachable by an agent token,
**but only if the manifest declares the `docs` label**. The allow list is the
allow list; nothing is granted for free.

### `run` — `kind: "chat"`

| Field | Type | Notes |
|---|---|---|
| `system` | string | The thread's system prompt, templated. Roots: `config`, `agent`, `run`. A `secret` config field here is refused. |

That is the whole shape. A `chat` agent has no runs; `agent_run` and the ledger
routes refuse it and point at `agent_open_chat`.

### `run` — `kind: "batch"`

| Field | Type | Notes |
|---|---|---|
| `source` | step, required | Runs once. Must yield an array of objects. Roots: `config`, `agent`, `run`. |
| `items_path` | string | A JSON pointer into the source result, e.g. `/messages`. Must start with `/` if non-empty. Absent means the source result itself is the array. |
| `item` | object, required | See below. |
| `review` | object | `{ "editable": [ … ] }` — output fields the reviewer may override before Apply. Each must be a field the item output produces. |
| `apply` | step | The only stage that writes. Receives `rows`. Roots: `config`, `rows`, `agent`, `run`. |
| `limits` | object | Legal on a batch manifest, but **only ever read when the apply step is a `script`** — that is the only part of a batch run that starts a container. A list, a classify or a direct-call/turn apply ignores it entirely. Same fields and defaults as a container's, and the Run tab prints them whenever they can apply. |

`run.item`:

| Field | Type | Notes |
|---|---|---|
| `id` | string, required | The row identity, templated. Roots: `config`, `item`, `agent`, `run`. An item whose `id` renders **empty** marks the row errored and skips its fetch and its classify call. |
| `fetch` | step | Its result becomes the `fetched` root. Roots: `config`, `item`, `agent`, `run`. |
| `columns` | object of string→template | The review-table columns, in author order. Roots: `config`, `item`, `fetched`, `agent`, `run`. Rendered in text mode, so every cell is a string. |
| `system` | string | Classify system prompt. Same roots. Secrets refused. |
| `user` | string | Classify user prompt. Same roots. Secrets refused. |
| `output` | object | The per-item structured output. See below. |
| `concurrency` | integer or template string | Parallel classify (and fetch) calls. Absent means one at a time. A template is rendered against `config`/`agent`/`run`; anything that does not render to a positive integer fails the run naming what it rendered to. |

`run.item.output` has exactly two forms, and they are exclusive:

*enum form* — the model answers with one field taken from a config array:

| Field | Notes |
|---|---|
| `field` | Required. The single output field name. |
| `enum_from` | Required. `config.<field>`, naming an **array** config field. |
| `fallback` | Required, a non-empty string. The answer a failed or unconvinced call gets, and the value that marks a row **needs attention**. Appended to the enum exactly once and last. |

*schema form* — a general object schema:

| Field | Notes |
|---|---|
| `schema` | Required. An object schema: `"type": "object"`, a non-empty `properties`, and `required` (if present) an array of field names. `field` is refused here. |
| `fallback` | Optional. Any JSON; what a failed or unmatched call gets. |

A classify stage needs **both** `user` and `output`. One without the other is
refused. A manifest with neither is a legitimate list-only agent.

### `run` — `kind: "container"`

| Field | Type | Default | Notes |
|---|---|---|---|
| `image` | string | — | The OCI reference. Required **unless** the manifest declares a `service` (then the app can come from a `dev_url`). Blank is refused. `localhost/…` is accepted and flagged on export. |
| `pull` | string | `"never"` | `never`, `missing` or `always`. Always written out in the canonical document — the choice is never implicit. |
| `entrypoint` | string | image's own | Blank is refused; leave the field out instead. |
| `args` | array of strings | `[]` | Appended after the image on the argv. No entry may be empty. |
| `columns` | array of strings | `[]` | The review table's header order. No blanks, no duplicates. |
| `review` | object | — | `{ "editable": [ … ] }`. Requires `apply` in `phases`. For a container every editable field is free-form (there is no enum to offer). |
| `phases` | array of strings | `["run"]` | Subset of `run` and `apply`, no duplicates, not empty. |
| `limits` | object | see below | |
| `service` | object | — | See §7. |
| `provides` | object | — | `{ "mcp": "/path" }`. Requires `service`. |
| `output` | object | — | **Per phase**: `{ "run": <schema>, "apply": <schema> }`. Each key must be a declared phase; each value an object schema. A phase with no key is unvalidated, and the Run tab says so. An empty `output` object is refused. |

### `run.limits`

<!-- source: crates/lmgw-core/src/agents/manifest.rs (Limits), crates/lmgw-core/src/agents/container.rs (run_argv) -->

Applies to `container` and to a `batch` agent's `script` apply step. Every field
is printed on the Run tab's Runtime block, so what the container is started with
is on the page.

| Field | Default | `podman run` flag | `0` means |
|---|---|---|---|
| `memory_mb` | `512` | `--memory <n>m` | no cgroup memory limit (flag omitted) |
| `cpus` | `2.0` | `--cpus <f>` | no CPU quota (flag omitted) |
| `pids` | `256` | `--pids-limit <n>` | no PID limit (flag omitted) |
| `deadline_seconds` | `600` | (lmgw's own timer) | no deadline — the run ends when the container does |
| `stop_grace_seconds` | `10` | `podman stop -t <n>` | **stricter**: SIGKILL at once, no chance to flush |
| `read_only` | `true` | `--read-only --tmpfs /tmp` | (boolean; `false` gives a writable root filesystem and no `/tmp` tmpfs) |

A partial `limits` block keeps the defaults for what it does not mention. A
negative or non-integer bound is refused **naming the field**, e.g.
`run.limits.memory_mb is -1; a limit is a whole number, 0 (no limit) or above`
and `run.limits.cpus is -1; a limit is 0 (no CPU quota) or above`.

There is no size on the `/tmp` tmpfs: its pages are charged to the container's
memory cgroup, so `memory_mb` already bounds it.

### Steps

<!-- source: crates/lmgw-core/src/agents/manifest.rs (Step, validate_step) -->

A *step* appears at `run.source`, `run.item.fetch` and `run.apply`. It is
**exactly one** of three shapes:

| Shape | Fields | What it does |
|---|---|---|
| direct call | `tool`, `args?` | Calls one exposed MCP tool. `args` must be an object; its strings are templated. The result is read as data: `structuredContent` if present, else the first text block parsed as JSON. Prose fails the step. |
| turn | `turn: { tools?, system?, prompt, output? }` | The model drives the tool loop with exactly `turn.tools` attached. `prompt` is required. `turn.output` is the structured-result schema, enforced in two stages. |
| script | `script`, `output?` | An ES module. `script` is a string, or an array of lines joined with `\n`. `args` is refused here. |

Declaring more than one is refused: `a step is exactly one of 'tool' (a direct
call), 'turn' or 'script'; this one declares tool and turn`. Declaring none is
refused too.

`output` on the step itself belongs to a `script` — a turn carries its own
`turn.output`, and a direct call returns the tool's result unchanged. Putting it
anywhere else is refused.

**`apply.turn` loads with a warning and Start disabled.** It is a warning rather
than a parse error so that an existing stored manifest is still editable. The
message is:

> apply may not run a model turn: the apply step writes, and a model deciding
> what to write is neither deterministic nor reviewable. Use a direct tool call,
> a `script`, or `run.kind: container`.

### The templating language

<!-- source: crates/lmgw-core/src/agents/template.rs -->

`{{path}}` substitution, and nothing else. No conditionals, no filters, no
arithmetic, no loops.

> **`run.image`, `run.entrypoint` and `run.args` are not templated.** This is
> the one trap in the manifest. Those three are handed to `podman run` as
> written, and nothing renders them, validates them or complains: a manifest
> with `"image": "localhost/{{config.tag}}"` loads clean — even when
> `config.tag` does not exist — and then fails at Start with podman reporting
> no such image. The same for an entrypoint and for every entry of `args`.
> A container reads its configuration from `/lmgw/input.json`, which carries
> the whole effective config; it does not read it off its own argv. If you want
> a per-install image, edit `run.image` (or install from the image, which sets
> it) rather than reaching for a placeholder.

The six roots:

| Root | Resolves to | Available where |
|---|---|---|
| `config` | Stored values over schema defaults | everywhere |
| `item` | One element of the source result | `run.item.*`, `run.item.fetch` |
| `fetched` | The item's `fetch` result | `run.item.columns`, `run.item.system`, `run.item.user` |
| `rows` | The reviewed rows handed to apply | `run.apply` |
| `agent` | `{ id, name }` — **those two keys only** | everywhere |
| `run` | `{ id }` — **that one key only** | everywhere |

Dotted paths walk objects and index arrays: `{{fetched.headers.from}}`,
`{{rows.0.category}}`.

**Two rendering modes, decided by the shape of the string:**

- A string that is **exactly one placeholder** takes the referenced value *with
  its JSON type*. `"maxResults": "{{config.limit}}"` becomes the integer `50`;
  `"rows": "{{rows}}"` becomes the array. Inner whitespace is fine
  (`{{ config.limit }}`); surrounding text is not — `" {{config.limit}}"` is the
  string `" 50"`.
- A string with placeholders among other text renders each one into text:
  strings verbatim, numbers and booleans as JSON text, arrays of scalars
  comma-joined (`Work, Finance`), objects and nested arrays as compact JSON.
  `null` and an unresolvable path both render as the empty string; in whole-value
  mode an unresolvable path becomes `null` and the key is kept.

An unterminated `{{` is plain text, not a placeholder.

Validation happens at save and at import, per position:

- `{{config.<field>}}` must name a field the config schema declares.
- `{{agent.…}}` accepts `id` and `name` only; `{{run.…}}` accepts `id` only.
- `{{item.…}}`, `{{fetched.…}}` and `{{rows…}}` are never second-guessed — tool
  payloads vary, so a missing path at run time is data, not a crash.
- A root that is not bound at that position is refused naming the ones that are:
  `run.source: '{{rows}}' is not available here (available: config, agent, run)`.
- An unknown root names all six.
- Validation walks into a step's `args`, reporting e.g. `run.source.args.page[1]`.
- …and it never looks at `run.image`, `run.entrypoint` or `run.args`, because
  nothing renders them. A placeholder there is silently literal.

### Validation rules and the codes

**Refused at load** (the manifest does not save):

Every rule above, plus:

- `run.item.user` is set but `run.item.output` is not, and the reverse.
- `run.review.editable` names a field the item output does not produce.
- `run.provides.mcp` without `run.service`; a `provides.mcp` path not starting
  with `/`; an agent id in a reserved namespace (`lmgw`, `docs`) with
  `provides.mcp` set.
- `run.service.port` outside 1–65535; a non-empty `health_path` not starting
  with `/`.
- A `secret` config field in a model prompt (`run.system`, `run.item.system`,
  `run.item.user`, `run.apply.turn.prompt`, `run.apply.turn.system`). A prompt is
  persisted in the clear — a thread keeps its system prompt, a classify call logs
  its request. A secret in a **tool argument** stays allowed: that is what a
  secret config field is for.
- `default` on a mount field: `config.schema.properties.<name>: a directory or
  file field cannot have a default — a manifest names a slot, never a host
  path`. `enum` on a mount field, refused for the same reason: a list of host
  paths is a list of defaults. `access` on a field that is not `directory` or
  `file`, refused naming the property.

**Warnings** are a second channel. They appear on the card, on the Run tab and
in the import report. `blocks_start` is the part that matters — the Start gate is
`requires_ok && no warning with blocks_start`.

| Code | Blocks Start | When |
|---|---|---|
| `apply_turn` | **yes** | The batch apply step is a `turn`. |
| `script_without_output` | no | The apply script declares no `output` schema, so whatever it returns is stored unchecked. |
| `container_without_image` | **yes** | A container agent with no `run.image`. Set one, or point the App tab at a `dev_url`. |
| `local_image_on_import` | no | An imported row whose image is `localhost/…` or has no registry host. This install has to build or retag it. |
| `podman_unavailable` | **yes for a container agent**, no for a scripted batch agent | `podman --version` could not be run. For a scripted batch agent listing and classifying still work and Apply will fail. |
| `image_absent_pull_never` | **yes** | `run.pull` is `never` and `podman image exists` says the image is absent. Only raised when podman *answered*. |
| `secrets_dir_fallback` | no | `XDG_RUNTIME_DIR` is unset, so each run's `secrets.json` is written under `<data_dir>/agents/<prefix>/` — persistent storage — instead of a host tmpfs. |
| `builtin_update_available` | no | A newer built-in manifest ships; this row was edited (or predates the recorded hash), so it is left alone. *Reset to shipped* adopts it. |
| `install_image_mismatch` | no | The agent was installed from image A but its manifest names image B. **The manifest wins** — B is what a run, an apply and the app start. |
| `dev_url_active` | no | The app is served from a dev server; no container is started for it. Runs and applies still use the image. |
| `dev_url_cleared` | no | A stored `dev_url` was cleared when `bind_addr` changed, because it is no longer legal for this gateway. |
| `manifest_unreadable` | **yes** | This build cannot parse the stored manifest. The detail page degrades rather than 400-ing, so the Definition editor stays reachable. |
| `mount_field_without_container` | **yes** | A `directory` or `file` config field on a `chat` or `batch` manifest — mount fields need `run.kind: "container"`. |
| `mount_unbound` | **yes** | A `required` mount field with no value — the existing required rule, surfaced with a name that says what to do ("bind `notes` on the Run tab"). |

Three more mount codes are not warnings — they are refusals at the moment a
value is stored or a container starts, not facts about the saved manifest:

| Code | When | HTTP |
|---|---|---|
| `mount_path_refused` | A path rule above (absolute, canonicalised, kind, refused location) fails. | `400` at store; the start is refused the same way at use. |
| `mount_path_nested` | The nesting rule above fails, naming the other agent, field and access. | `400` at store; the start is refused the same way at use. |
| `mount_path_missing` | A stored path no longer exists at use time. | The start is refused; never raised at store, since the path existed to be stored. |

---

## 3. Batch agents

<!-- source: crates/lmgw-core/src/agents/batch.rs -->

### The pipeline

Five phases exist; a manifest's are derived from what it declares
(`Manifest::phases()`): always `list`; `classify` and `rerun` when
`run.item.user` is set; `apply` when `run.apply` is set.

1. **source** — one step, once. Its result is narrowed by `items_path` and must
   be an array; anything else fails the run naming what it was.
2. **fetch** — per item, bounded by `concurrency`. `run.item.id` is rendered
   first; an empty id errors the row (`this item has no id: '…' rendered empty,
   so nothing can be applied to it`) and marks it attention. `run.item.fetch`
   runs, its result becomes `fetched`, and the `columns` templates are rendered.
3. **classify** — one structured model call per item, bounded by `concurrency`.
   The item output's schema is sent as `response_format`; on an OpenAI-protocol
   route llama-server turns it into a grammar. `strict` is set **only** for the
   one-field object lmgw generates around an enum — your own `schema` is passed
   on as written. The reply is parsed back: the **enum form** reads the answer
   field out of the first JSON object in the reply and matches it
   case-insensitively against the enum; failing that it looks for a line that
   *is* one of the values, then for the earliest value mentioned as a whole word
   (the fallback is excluded from that pass; the candidate is compared
   case-insensitively after stripping surrounding non-alphanumerics), and
   otherwise takes the fallback. The **schema form** takes the **first** balanced
   JSON object in the reply and keeps it only if it matches the schema — a
   second object later in the text is never tried — and otherwise falls back.
4. **review table** — `list` and `classify` write nothing *of lmgw's own* and
   end here. Columns come from `run.item.columns` in author order.
   `review.editable` fields can be overridden, with the enum offered from the
   **stored** config.
5. **apply** — its own job, with `{{rows}}` bound to the checked rows. This is
   the stage the pipeline writes from.

**"Read-only" is a property of the manifest, not a rule lmgw enforces.** Nothing
in the `list` or `classify` path writes, but `source` and `fetch` call whatever
tools you point them at — a manifest whose `source` names a deleting tool
deletes on List. Equally, `apply` is not gated on a human: `agent_run` accepts
`phase: "apply"` with rows supplied verbatim by the caller, and it is only the
dashboard and `lmgw__agent_run` that decline to do it for you
(`lmgw__agent_run` refuses anything but `list` and `classify`). The review gate
is a workflow, and it holds exactly as far as who can reach `/api/op`.

The **column templates render unconditionally**, after the fetch, whatever
happened: a row with an empty id or a failed fetch still gets its cells, and
they come out blank because `fetched` is null. That is what an errored row looks
like in the table — blank cells, a red `error`, `attention` set.

Each row carries: `id`, the source `item` (kept so a re-run can re-fetch),
`columns`, `output`, `prompt` (exactly the user text the model got), `raw` (what
it replied), `error`, `attention`.

`{{rows}}` binds to `[{ id, …output fields }]` — **the review columns are
deliberately not included**. They are rendered from whatever the source and
fetch returned (for the mail agent, a stranger's `From` and `Subject`), and the
apply step is the one place with write tools attached.

**Early abort.** If `min(3, n)` classify calls fail with a status of 500 or
above before any of them succeeds, the run fails with
`model unavailable: <the first error>` rather than tagging fifty rows with the
fallback. One transient 5xx on an otherwise healthy model does not abort a run.

### Attention rows and re-run

A row is `attention` when its output equals the fallback, when the call failed,
when its id rendered empty, or when its fetch failed.

`agent_run { id, phase: "rerun", base_job: <job id> }` re-classifies the
attention rows of an earlier run of the same agent, against the *current*
config — so widening a taxonomy is a config edit and not a manifest edit. Rows
already classified keep their answer verbatim. The attention rows are fetched
again (the fetch result is the body; it was never stored).

Two things to know about the target set:

- **Only attention rows that have an id are re-run.** A row whose `item.id`
  rendered empty is attention *and* has nothing to address, so it is skipped
  every time and stays attention forever. Fix the `id` template, or accept the
  row as permanently flagged.
- The base run only has to be **of this agent** and have rows; a **cancelled**
  run is a perfectly good base, since a cancel keeps the rows it produced. A
  base with no rows at all says `run #N has no rows to re-classify`; a base
  whose rows are all fine says `run #N has no rows needing attention; widen the
  config first, or start a fresh run`.

### Cost

Each phase is its own `jobs` row, so retrying an apply never re-spends the
classification.

The job's `result` has **two shapes**, and which one you get depends on who drove
the run. Both share `usage`, `cost_micro` (**NULL**, never `0`, when nothing
could be priced), `model_calls`, `tool_calls`, `phase`, `agent_id`, `rows`,
`attention` and `errors`.

| Key | In-process batch | Container / script / ledger |
|---|---|---|
| `model` | the alias the run used | — |
| `log` | — (the run log is not in this result) | the run log, as an array of lines |
| `output` | — | present when the run reported one |
| `extra_columns` | — | present when a row carried an undeclared column |
| `applied` | on an apply: `{ output, text, tool_calls }` | on an apply: `{ output, text: "", tool_calls: [] }` |
| `canceled: true` | when it was cancelled | — (the job's own status says so) |
| `container: true` / `ledger: true` | — | whichever transport it was |

**`X-Lmgw-Run` folds a call into that total only when three things hold**: the
caller presented an **agent token**, the id names a run that is **live**, and
that run is **this agent's own**. Anything else — no token, a gateway key, a
finished run, another agent's run, a non-agent job, a non-numeric value — is
ignored with a log line and never refused. A mis-stamped request is still a
request you made; refusing it would turn a bookkeeping slip into an outage.

One live run per agent: `jobs` has a partial unique index on `(kind, key)` with
`key = "agent:<id>"`. `agent_run` reports the running job rather than erroring.

### Worked example: the shipped mail labeler

<!-- source: crates/lmgw-core/builtin-agents/mail-labeler.agent.json -->

This is the complete shipped manifest, annotated after the block.

```json
{
  "schema_version": 1,
  "id": "mail-labeler",
  "name": "Mail labeler",
  "description": "Classifies unread Gmail into labels. Nothing is written until you apply.",
  "version": "3.0.0",
  "model": {
    "alias": "{{config.model}}",
    "temperature": 0.0
  },
  "config": {
    "schema": {
      "type": "object",
      "properties": {
        "model": {
          "type": "string",
          "format": "model_alias",
          "title": "Model",
          "description": "The alias each classify call runs on. One call per message, so a small local model is the usual choice."
        },
        "categories": {
          "type": "array",
          "items": { "type": "string" },
          "title": "Categories",
          "description": "The taxonomy the model must pick from. Widen it here and re-run the attention rows; \"Other\" is added automatically as the catch-all.",
          "default": [
            "Newsletter", "Promotions", "Notifications", "Social", "Personal",
            "Work", "Finance", "Receipts", "Travel", "Spam"
          ]
        },
        "label_prefix": {
          "type": "string",
          "title": "Label prefix",
          "default": "lmgw",
          "description": "Parent label; empty applies bare category names"
        },
        "limit": {
          "type": "integer",
          "title": "Limit",
          "default": 50,
          "minimum": 1,
          "description": "How many unread messages one run lists. It is passed straight to the Gmail search as maxResults; Gmail itself returns at most 500 in one page, so a larger number here cannot list more."
        },
        "concurrency": {
          "type": "integer",
          "title": "Concurrency",
          "default": 4,
          "minimum": 1,
          "description": "Parallel classify calls; match the local server's -np"
        }
      },
      "required": ["model"]
    }
  },
  "tools": [
    {
      "label": "gws",
      "allowed": [
        "gws__gmail_search",
        "gws__gmail_get",
        "gws__gmail_listLabels",
        "gws__gmail_createLabel",
        "gws__gmail_batchModify"
      ],
      "install": {
        "kind": "git",
        "ref": "https://github.com/gemini-cli-extensions/workspace",
        "notes": "Clone and build it, then run the headless login once (npm run auth-utils -- login). Register it on the MCP page as a stdio server: command 'node', args ['workspace-server/dist/index.js'], cwd the clone, tool_prefix 'gws'. These five are every tool the steps name, so nothing else the server offers is in reach of a run. Run dist/index.js directly rather than scripts/start.js — start.js reinstalls on every launch and switches the tool names to dots. Disable gws__gmail_send and gws__gmail_sendDraft on the MCP page so no thread that attaches the whole label can send."
      }
    }
  ],
  "run": {
    "kind": "batch",
    "source": {
      "tool": "gws__gmail_search",
      "args": {
        "query": "is:unread in:inbox",
        "maxResults": "{{config.limit}}"
      }
    },
    "items_path": "/messages",
    "item": {
      "id": "{{item.id}}",
      "fetch": {
        "tool": "gws__gmail_get",
        "args": { "messageId": "{{item.id}}", "format": "full" }
      },
      "columns": {
        "date": "{{fetched.date}}",
        "from": "{{fetched.from}}",
        "subject": "{{fetched.subject}}"
      },
      "system": "You are an email triage assistant. Read the email and pick the single best-fitting category from this list: {{config.categories}}. If none clearly fit, pick Other.",
      "user": "From: {{fetched.from}}\nTo: {{fetched.to}}\nDate: {{fetched.date}}\nSubject: {{fetched.subject}}\n\n{{fetched.body}}",
      "output": {
        "field": "category",
        "enum_from": "config.categories",
        "fallback": "Other"
      },
      "concurrency": "{{config.concurrency}}"
    },
    "review": { "editable": ["category"] },
    "apply": {
      "script": [
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
      "output": {
        "type": "object",
        "properties": {
          "applied": { "type": "integer" },
          "labels": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["applied", "labels"]
      }
    }
  }
}
```

What each part is doing:

- `model.alias` is `{{config.model}}`, so the picker on the Run tab chooses and
  the token's scope follows it. `temperature: 0.0` because classification is not
  a creative task.
- `categories` is the enum source. `enum_from: "config.categories"` builds the
  model's allowed values from the **stored** array, and `fallback: "Other"` is
  appended once and last — so widening the taxonomy on the Run tab and pressing
  *Re-run attention rows* costs no manifest edit.
- `limit` is `maxResults` for the Gmail search, passed through whole-value
  typing so it arrives as an integer. Its description says out loud what the
  real remote ceiling is — a visible limit, not a hidden one.
- `allowed` names exactly the five tools the steps call. The server offers more;
  nothing else is in reach of a run, and the `install.notes` say which two to
  switch off so a Chat thread attaching the whole `gws` label cannot send mail.
- `items_path: "/messages"` narrows the search result to the array.
- `columns` renders three text cells per row; the *fetch* result is what they
  read, so `from`, `subject` and `date` come from the message, not the search.
- `apply` is a **script**, not a turn. It groups by category, creates the
  missing labels, and makes one `gws__gmail_batchModify` call per label with
  `addLabelIds` only. `removeLabelIds` is never constructed, so `UNREAD` cannot
  be touched — the guarantee is the absence of a key, not a sentence in a
  prompt. The apply spends no tokens at all.
- `apply.output` declares the shape the script must return; a script that
  answers with something else fails the job instead of storing it.

---

## 4. Script steps

<!-- source: crates/lmgw-core/assets/agent-shim.mjs, crates/lmgw-core/src/agents/container.rs (execute_script) -->

A `script` step is an ES module lmgw runs inside a container of
**Settings → Agent script image** (default `docker.io/library/node:24-alpine`,
pulled with `--pull=missing`). The entrypoint is `node /lmgw/shim.mjs`; the shim
is embedded in the gateway binary and written per-run beside your module.

Files inside the container, all mounted `ro,Z`:

| Path | Contents |
|---|---|
| `/lmgw/shim.mjs` | The shim (lmgw's). |
| `/lmgw/script.mjs` | Your module, from `run.apply.script`. |
| `/lmgw/input.json` | `{ phase, agent: { id, name }, run: { id }, config, rows }`. |
| `/lmgw/secrets.json` | `{ "token": "<the agent token>", "config": {} }` — **`config` is always empty for a script**. |

The shim reads its paths from `LMGW_INPUT`, `LMGW_SECRETS`, `LMGW_SCRIPT`,
`LMGW_MCP_URL`, `LMGW_PHASE` and `LMGW_RUN`, falling back to the `/lmgw/…`
defaults.

### The module

Export one function per phase; the shim calls the one named by
`input.phase` — for an apply step that is `apply`:

```js
export async function apply(ctx) {
  // …
  return { /* validated against run.apply.output */ };
}
```

If the module exports no function of that name the run fails with
`the script exports no apply function`.

### `ctx`

| Field | Value |
|---|---|
| `ctx.rows` | `input.rows`, or `[]`. Each row is `{ id, …output fields }` — the `Row::for_apply` shape, with the reviewer's edits, and without the rows they unticked. Review **columns are not included**. |
| `ctx.config` | `input.config` — the effective config (stored values over schema defaults) **with every `secret` field removed**. A script cannot see secret config values; a step that needs one is a container, not a script. |
| `ctx.agent` | `{ id, name }`. |
| `ctx.run` | `{ id }`. |
| `ctx.phase` | `"apply"`. |
| `ctx.log(message)` | Emits one `log` event — a line in the run log. |
| `ctx.tools.call(name, args)` | One MCP `tools/call`. |

The token is deliberately **not** on `ctx`. That is tidiness, not a boundary:
the script can read `/lmgw/secrets.json` itself.

### `ctx.tools.call`

JSON-RPC over `POST $LMGW_MCP_URL`. The shim does the `initialize` handshake
once (protocol `2025-11-25`), keeps the `Mcp-Session-Id`, and sends
`content-type`, `accept: application/json, text/event-stream`,
`mcp-protocol-version`, `Authorization: Bearer <token from secrets.json>` and
`X-Lmgw-Run: <run id>` on every request. At the end it `DELETE`s the session,
best effort, with **only** an `mcp-session-id` header — no bearer, no run stamp;
the run is over either way.

The result is read as **data**, in this order:

| Case | Behaviour |
|---|---|
| `result.isError` is truthy | **checked first** — throws with the first text block, or `the tool '<name>' failed` |
| `result.structuredContent` present and non-null | returned as-is |
| else the first `text` content block parses as JSON | the parsed value is returned |
| else | throws `the tool '<name>' returned text that is not JSON; a script step needs a JSON result` |
| JSON-RPC `error` in the body | throws `body.error.message`, or `the tool '<name>' failed` |
| a 2xx body that is not JSON | throws `the gateway's answer for '<name>' is not JSON: <body>` |
| non-2xx HTTP | throws `the tool '<name>' could not be called (HTTP <status>): <body>` |
| handshake fails | throws `the gateway refused the MCP handshake (HTTP <status>): <body>` or `the gateway's MCP handshake returned no Mcp-Session-Id` |
| `LMGW_MCP_URL` unset | throws `LMGW_MCP_URL is not set, so this script has no tool plane to call` |
| empty or non-string name | throws `ctx.tools.call needs the exposed tool name as its first argument` |

`isError` coming first matters: a tool that reports a failure *and* returns
`structuredContent` throws rather than handing you the structured error as
though it were a result.

There is no retry, no timeout and no page size in the shim. What bounds a script
is `run.limits.deadline_seconds` and the token's scope.

### Console, return value, errors, cancel

- **`console` is captured.** `console.log`/`info`/`warn`/`error`/`debug` are
  replaced before your module is imported, and each becomes a `log` event with
  the matching `level`. This is not cosmetic: the ledger reads every stdout line,
  so an uncaptured `console.log` of an object with a `type` key could forge a
  `row` or an `output` event.
- **The return value becomes the phase's output.** If the function returns
  anything other than `undefined`, the shim emits `{"type":"output","output":…}`.
  It is then checked against `run.apply.output` at close; a mismatch fails the
  job with `the run's output does not match run.apply.output: …`, and returning
  nothing when a schema is declared fails it too. Note what "fails" means here:
  the wrong-shaped value **is still stored** in the job's `result.output`, and
  the job is marked failed around it. The check is a verdict on the run, not a
  filter on what gets written — so you can read what the script actually
  returned on the Runs tab and fix it.
- **An error exits 1 with the stack on stderr.** The shim writes
  `e.stack ?? e` to stderr and sets `process.exitCode = 1` (rather than calling
  `process.exit`, which could drop events already written). Non-zero exit fails
  the job, with the last 12 stderr lines quoted in the message; the *full* stderr
  is in the run log.
- **SIGTERM (and SIGINT)** flip a cancel flag. The call already in flight is
  allowed to finish, so a cancel lands *between* two writes and never inside one;
  every later `ctx.tools.call` rejects at once with an error carrying
  `code: "cancelled"`; the process then exits **1** as soon as the last in-flight
  call lands. SIGKILL after `stop_grace_seconds` is the backstop, not the
  mechanism.

### The shim is sugar, not a sandbox

A script runs with the agent's **full token authority**. It can read
`/lmgw/secrets.json` and call `/v1` and `/mcp` itself with that token, inside
whatever the token's scope and the manifest's allow list permit. What confines
it is the container (`--read-only`, `--cap-drop=ALL`,
`--security-opt no-new-privileges`, the cgroup limits) and the token — never the
shim. Read a script before you install an agent that ships one.

It runs under the agent's `run.limits`, printed on the Run tab exactly as a
container agent's are: 512 MiB, 2 CPUs, 256 pids, a 600 s deadline and a
read-only rootfs unless the manifest says otherwise. Only the image and the pull
policy come from Settings instead of from the manifest.

### Worked example: two tool calls and a summary

An apply step that files each reviewed row into a tracker and returns a count.

```json
{
  "apply": {
    "script": [
      "export async function apply(ctx) {",
      "  const board = await ctx.tools.call('tracker__list_boards', {});",
      "  const target = (board.boards ?? []).find(b => b.name === ctx.config.board);",
      "  if (!target) throw new Error(`no board named ${ctx.config.board}`);",
      "  console.log(`filing ${ctx.rows.length} row(s) into ${target.id}`);",
      "  const filed = [];",
      "  for (const row of ctx.rows) {",
      "    const card = await ctx.tools.call('tracker__create_card', {",
      "      boardId: target.id,",
      "      title: row.title,",
      "      label: row.severity",
      "    });",
      "    filed.push(card.id);",
      "    ctx.log(`${row.id} -> ${card.id}`);",
      "  }",
      "  return { filed: filed.length, board: target.id, cards: filed };",
      "}"
    ],
    "output": {
      "type": "object",
      "properties": {
        "filed": { "type": "integer" },
        "board": { "type": "string" },
        "cards": { "type": "array", "items": { "type": "string" } }
      },
      "required": ["filed", "board", "cards"]
    }
  }
}
```

`row.title` and `row.severity` are output fields the classify step produced;
`ctx.config.board` is a config field. Both `tracker__*` names must be reachable
through the manifest's `tools[]`, or the call throws with the allow list quoted.

---

## 5. Container agents

<!-- source: crates/lmgw-core/src/agents/container.rs -->

lmgw starts your image once per phase, hands it two files and a set of
environment variables, reads JSONL on stdout, and turns the exit code into the
job's status.

### The `podman run` lmgw issues

The argv, in order, for a phase run:

```
podman run --rm --replace --name <prefix>-agent-<slug(id)>-<run>
  --label lmgw.instance=<prefix>
  --label lmgw.kind=agent
  --label lmgw.agent=<agent id>
  --label lmgw.run=<job id>
  [--userns=keep-id]
  [--memory <memory_mb>m] [--cpus <cpus>] [--pids-limit <pids>]
  [--read-only --tmpfs /tmp]
  --cap-drop=ALL --security-opt no-new-privileges
  --pull=<never|missing|always>
  [--network pasta:-T,<gateway port>]
  -e LMGW_…=… (one -e per variable)
  -v <host path>:/lmgw/<file>:ro,Z (one -v per file)
  [-v <canonical host path>:/lmgw/mounts/<field>:<ro|rw>,z (one per bound mount field)]
  [--entrypoint <run.entrypoint>]
  <run.image> [run.args…]
```

- `--cap-drop=ALL` and `--security-opt no-new-privileges` are **not
  configurable and always applied**. A manifest cannot ask for a host mount
  directly — only a `format: "directory"` or `"file"` config field can (§2),
  and only the owner supplies the path; otherwise the only host paths in the
  argv are the two files below.
- **`--userns=keep-id`** is added to every phase run and service start of a
  manifest that **declares** at least one mount field — bound or not — so one
  image always runs one way: the container's process is the owner's uid
  inside too, and what it writes is the owner's. Without it, root-in-container
  maps to the owner's uid, which can read the owner's files but not write them
  the way an image running `USER 1000` expects, and what a non-root image
  creates lands under a subuid. An image that needs to be root at runtime gets
  `EACCES` from the kernel — that is the run's failure. The run log's start
  line names it: `this manifest declares mounts: the container runs as uid
  <n> (--userns=keep-id)`. An existing manifest with no mount field sees no
  change. With an `rw` mount bound, this cuts both ways: the container is
  running as your own uid, so it can delete or change anything under that
  mount exactly as you could yourself — `--cap-drop=ALL` does not stop that,
  because it governs Linux capabilities, not ordinary file permission.
- **A bound mount field** contributes
  `-v <canonical host path>:/lmgw/mounts/<field>:<ro|rw>,z` — **`:z`, shared,
  not `:Z`**. lmgw's own files (`input.json`, `secrets.json` and any other
  file lmgw adds) stay `ro,Z`, a private relabel, because nothing else needs
  to see them. An owner-chosen folder is exactly the case
  where two containers meet — a service container plus that agent's phase
  run, or two agents on one folder — and a private `:Z` relabel would revoke
  a sibling container's access mid-run; `:z` relabels to `container_file_t`
  with no category, narrow to that path, readable by every container and by
  the owner. **The relabel is recursive and permanent**: it relabels the
  folder and everything under it, and nothing undoes it when the agent stops.
  A relabel that fails (a filesystem without xattrs) fails the `podman run`;
  podman's own message is the run's failure message, verbatim.
- `--read-only --tmpfs /tmp` is applied unless `run.limits.read_only` is
  `false`.
- A `0` limit **omits its flag entirely**; it never becomes a number lmgw chose.
- `--pull=<policy>` is always on the argv, so the log says which policy the
  start ran under.
- **`run.image`, `run.entrypoint` and `run.args` reach this argv verbatim.**
  They are not templated and not validated (§2): a `{{…}}` in any of them is
  passed to podman as those literal characters. Configure the container through
  `/lmgw/input.json`, not through its argv.
- The container name uses `settings.container_prefix` (the same one the model
  containers use), so a dev instance cannot collide with the real one. A service
  container is `<prefix>-agentsvc-<slug(id)>` with `lmgw.run=service`; a package
  read is `lmgw-pkg-<16 hex>` with `lmgw.run=package`.

At boot, two sweeps run, and they are not the same sweep. The **container**
sweep removes every container labelled `lmgw.kind=agent` +
`lmgw.instance=<prefix>` that is older than this process and whose `lmgw.run` is
not a live job. The **directory** sweep is broader and **runs even when
`podman ps` failed**: it walks every run-directory root this install could have
written to — the current tmpfs leaf, *and every prefix leaf under
`<data_dir>/agents/`*, because a `container_prefix` you have since changed
leaves its whole subtree orphaned — and deletes anything older than this process
whose run is not live. A `service-…` or `pkg-…` directory has no job at all, so
at boot it is a leftover by definition. Under `$XDG_RUNTIME_DIR` only this
instance's own leaf is touched: that tree is shared by every lmgw on the box.

### The `LMGW_*` environment

<!-- source: crates/lmgw-core/src/agents/container.rs (env_for), crates/lmgw-core/src/agents/service.rs -->

Addressing only — **never a secret**, because `podman inspect` prints a
container's environment. In this order:

| Variable | Value | Present |
|---|---|---|
| `LMGW_BASE_URL` | Where to reach this gateway from inside the container | always |
| `LMGW_API_BASE` | `<LMGW_BASE_URL>/v1` | always |
| `LMGW_MCP_URL` | `<LMGW_BASE_URL>/mcp` | always |
| `LMGW_LEDGER_URL` | `<LMGW_BASE_URL>/api/agents/runs/<run>/events` | phase runs only |
| `LMGW_AGENT` | The agent's id | always |
| `LMGW_RUN` | The job id — put it in `X-Lmgw-Run` | phase runs only |
| `LMGW_PHASE` | `run`, `apply` or `service` | always |
| `LMGW_INPUT` | `/lmgw/input.json` | always |
| `LMGW_SECRETS` | `/lmgw/secrets.json` | always |
| `LMGW_DEADLINE_SECONDS` | This run's deadline; `0` is none | phase runs only |
| `LMGW_SCRIPT` | `/lmgw/script.mjs` | script steps only |
| `LMGW_APP_ORIGIN` | `http://<id>.<suffix>:<port>` — the public origin, for an app that builds absolute URLs (OAuth redirect URIs, share links) | service mode only |
| `LMGW_PORT` | `run.service.port` | service mode only |

A service container gets **no** `LMGW_LEDGER_URL`, `LMGW_RUN` or
`LMGW_DEADLINE_SECONDS`: there is no run. Those keys are **absent**, not set to
a zero a container would have to know to disbelieve.

### `/lmgw/input.json` and `/lmgw/secrets.json`

Both are written into a directory on a host tmpfs, every level created at mode
`0700`, each file at mode `0600`, and bind-mounted **`ro,Z`** (a narrow,
per-file SELinux relabel; never a broad path):

| Who | Directory | Lives for |
|---|---|---|
| a phase run (`run`, `apply`, a script step) | `$XDG_RUNTIME_DIR/lmgw/<slug(container_prefix)>/run-<job id>/` | the run; `rm -rf`'d when it ends, on every error path |
| a **service** container | `$XDG_RUNTIME_DIR/lmgw/<slug(container_prefix)>/service-<slug(agent id)>/` | as long as the container does — it is owned by the live entry, so stopping the service takes its `secrets.json` with it |
| a package read | `…/pkg-lmgw-pkg-<rand>/` | three podman invocations |

If `XDG_RUNTIME_DIR` is **unset or empty**, the fallback is
`<data_dir>/agents/<slug(container_prefix)>/` — persistent storage. That raises
`secrets_dir_fallback` on the card and writes a line into the run log:
"your tokens are being written to persistent storage" is not a `tracing::warn!`-
grade fact. A service start has no run log to write into, so it logs to the
process log instead; the card's warning is computed per page load either way.

`input.json` for a phase run:

```json
{
  "phase": "run",
  "agent": { "id": "mail-labeler", "name": "Mail labeler" },
  "run": { "id": 412 },
  "config": { "…": "the effective config, without any secret field" }
}
```

For `phase: "apply"` it also carries `rows`:

```json
{
  "phase": "apply",
  "agent": { "id": "mail-labeler", "name": "Mail labeler" },
  "run": { "id": 413 },
  "config": { "…": "…" },
  "rows": [ { "id": "19a2…", "category": "Receipts" } ]
}
```

`rows` is **absent** for a run phase, not empty — a container can tell "no gate"
from "an empty gate". Each row is `{ id, …output fields }`.

For a service container (no run, no rows):

```json
{
  "phase": "service",
  "agent": { "id": "board", "name": "Board" },
  "config": { "…": "…" },
  "service": { "port": 8080, "origin": "http://board.localhost:8787", "health_path": "/healthz" }
}
```

**A manifest with a mount field gets one substitution and one extra key**,
in every one of the three shapes above: every mount field's value in `config`
becomes its container path, `/lmgw/mounts/<field>`, and a `mounts` array rides
beside `config` so a container can enumerate what is bound without knowing the
schema:

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

The host path is not in the file — not in `config`, not in `mounts`, nowhere.
`mounts` is `[]` when the schema declares no mount field, and an **unbound**
optional mount field is simply absent from both `config` and `mounts` (a
`required` one with no value is `mount_unbound`, and Start stays disabled). A
template `{{config.notes}}` resolves to the container path the same way — it
is not refused in a model prompt the way a secret is, because a container path
tells a model nothing about the host.

The run log prints one line per mount, before the start line:

```
mount notes: /home/alice/Notes → /lmgw/mounts/notes (rw, directory; relabelled container_file_t, recursive, permanent)
```

`secrets.json`:

```json
{
  "token": "lmgw-agent-<64 hex>",
  "config": { "api_token": "…" }
}
```

`config` holds every `format: "secret"` config field that has a value, for a
phase run and for a service alike. For a **script** step it is always `{}`.

### Networking

<!-- source: crates/lmgw-core/src/agents/container.rs (gateway_access, loopback_note) -->

`LMGW_BASE_URL` is derived from `settings.bind_addr`, never hardcoded, and there
are two cases:

- **The gateway bound `0.0.0.0` or a real address.** `LMGW_BASE_URL` is
  `http://host.containers.internal:<port>` and no extra podman flag is needed.
- **The gateway bound loopback** (`127.0.0.1`, `::1`, `localhost` — lmgw's own
  default). `host.containers.internal` gets *connection refused*: a
  loopback-bound socket is not on that address at all. lmgw adds
  `--network pasta:-T,<port>` and sets `LMGW_BASE_URL` to
  `http://127.0.0.1:<port>`, which makes that one port inside the container reach
  the same port on the host — and nothing else.

Two consequences of `-T`, both stated rather than left to be discovered: it
**occupies that port on the container's own loopback**, so your image cannot
listen there; and it forwards to the host's **IPv4** loopback only, so a gateway
bound to `[::1]` alone is unreachable from a container. In that case the run log
carries a line saying so before anything starts.

A `bind_addr` naming no port fails the run: `the bind_addr setting is '…', which
names no port, so a container cannot be told where the gateway is`.

### stdout: the JSONL events

<!-- source: crates/lmgw-core/src/agents/ledger.rs -->

One JSON object per line. Lines are fed to the **same decoder** the HTTP ledger
uses, so an event means one thing on both transports. A line that is a JSON
array is read as the several events it is.

| `type` | Fields | Effect |
|---|---|---|
| `row` | `id` (required, non-empty), `columns?`, `output?`, `prompt?`, `raw?`, `error?`, `attention?` | Upsert by `id`. **A field absent from the event is left alone**, so a second event for the same id adds to the row the first one made. |
| `log` | `level?` (default `"info"`), `message?` | One run-log line, rendered `[<level>] <message>`. A non-string `message` is printed as it arrived. |
| `progress` | `done?` (default `0`), `total?`, `stage?` | One progress frame; lmgw fills `detail`. |
| `output` | `output` | The phase's terminal value. Last one wins. |

Row field details:

- `columns` is an object; a key the manifest's `run.columns` does not declare is
  **kept**, appended to the table after the declared ones in first-seen order,
  and noted **once** in the run log.
- `output` is set to whatever value is there, including `null`.
- `prompt` and `raw` are taken only when they are strings.
- `error` is `null` to clear, a string as-is, anything else stringified.
- `attention` is taken only when it is a boolean.
- **`id` must be a non-empty JSON string.** Absent, empty, `null`, a number
  (`"id": 42`) or an object all read the same way — no id — and the event is
  **rejected into the run log**: `rejected a row event with no id: {…}`. Quote
  your numeric ids.

An unknown `type` is a run-log line, not a failure:
`ignored a ledger event of unknown type 'banner' (known: row, log, progress,
output)`. A line that is **not JSON** becomes a run-log line verbatim, so a
framework's startup banner costs nothing.

### stderr and the exit code

stderr goes into the run log **verbatim**, line by line, with the agent's own
token substituted out. The last 12 lines are the excerpt a failure message
carries; the full text is always in the run log.

| Ending | Job status | Message |
|---|---|---|
| exit `0`, no `run.output.<phase>` schema | done | — |
| exit `0`, output matches the schema | done | — |
| exit `0`, schema declared, **no** `output` event | failed | `the container exited successfully but emitted no output; run.output.<phase> declares a schema` |
| exit `0`, output does not match | failed | `the run's output does not match run.output.<phase>: <detail>` |
| exit `n ≠ 0` | failed | `the container exited with status <n>` + newline + the last 12 stderr lines |
| killed by a signal | failed | `podman run was killed by signal <n>` + the excerpt |
| Cancel pressed | canceled | the rows it produced are kept |
| deadline passed | failed | `the run exceeded its deadline of <n>s (run.limits.deadline_seconds)` |
| podman could not be run | failed | `podman is required for container agents and could not be run: <e>` |

The output check is the same structural subset a turn's output is held to: the
value must be an object, every name in `required` must be present and non-null,
and each declared property's `type` and `enum` must be honoured. It is not a
general JSON Schema validator and does not pretend to be one.

### Phases and the review gate

`run.phases` declares which of `run` and `apply` the image implements
(default `["run"]`), and `LMGW_PHASE` says which one this invocation is.

- The **`run`** phase produces the review table and writes nothing.
- **Apply** is a separate `podman run` of the same image, started only by the
  button, with `input.json` carrying `rows` — the rows the reviewer approved,
  with their edits, and without the ones they unticked. An apply with no checked
  rows is refused before anything starts: `nothing to apply: no rows were
  checked`.
- Starting a phase the manifest does not declare is refused naming both:
  `this agent's image declares the phases run, so it has no 'apply' phase`.

**Every phase run records the non-secret config it actually ran with** —
`effective`, on the job's own `input` — and an apply started with `base_job`
(the button always sends one) merges *that run's* `effective` over the current
stored config before applying its own `values`, rather than the stored config
as it now stands. So an apply runs against exactly what the reviewer saw, not
whatever the config has drifted to since — and a run started with an override
that is applied without resending it applies against the override, not the
saved default. This is general, not confined to mount fields; a job row from
before this existed has no `effective` and behaves as it always did.

### Cancel and the deadline

Cancel and the deadline both run the same **stop ladder**, bounded by the visible
`stop_grace_seconds` and nothing else:

1. `podman stop -t <grace> <name>` (podman's own SIGTERM → grace → SIGKILL),
   then wait `grace` seconds.
2. `podman rm -f <name>`, then wait `grace` seconds.
3. Drop the child process — `kill_on_drop` kills `podman run` itself.

Each rung writes one run-log line naming what it did. The caller stops climbing
the moment the child exits, so a healthy stop never reaches rung two. With
`stop_grace_seconds: 0` every rung fires at once, which is exactly what "SIGKILL
with no chance to flush" already meant.

The **first** cause wins the run's ending: a cancel arriving while the deadline's
ladder is already climbing is logged, not promoted.

The deadline is counted from the container's start for a run lmgw started (and
from the open for a ledger run). `0` is unbounded — the run ends when the
container does — and the run log says so at the start:
`starting <image> as <name> (pull never); no deadline (run.limits.deadline_seconds = 0)`.

### Calling back into lmgw

Read the token out of `/lmgw/secrets.json` and use it as a bearer:

```sh
TOKEN=$(sed -n 's/.*"token"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$LMGW_SECRETS")

curl -sS "$LMGW_API_BASE/chat/completions" \
  -H "Authorization: Bearer $TOKEN" \
  -H "X-Lmgw-Run: $LMGW_RUN" \
  -H 'content-type: application/json' \
  -d '{"model":"qwen3.8","messages":[{"role":"user","content":"hi"}]}'
```

`X-Lmgw-Run` folds the call's tokens, cost and call count into that job's total,
which is what the Runs tab and the cost line show. Only an agent token may
attribute, and only to **its own** live runs; anything else is ignored with a log
line rather than refused.

`/mcp` takes the same bearer plus the MCP handshake (§4 describes the exact
sequence the shim performs; a hand-written client does the same thing).

**`/api` is a narrower door than either.** The same agent token gets `403
forbidden` on `/api/op/*` and everything else that needs `Admin` — editing
another agent, changing Settings, deleting a model are not this container's
to do — and no token at all gets `401 session_required`. What it keeps: `/v1`
and `/mcp` above, the three ledger routes (§6), and reads of its own row and
runs — `GET /api/agents/{id}`, `GET /api/agents/{id}/runs`, `GET
/api/agents/runs/{job}` — rendered in the **agent view**: secrets masked,
`dev_url` empty, and a mount field's value read as its container path
(`/lmgw/mounts/<field>`), never the host one.

### Worked example: a minimal shell container

A `fedora-minimal` image that lists three things, emits a row for each, and
reports an output. The manifest:

```json
{
  "schema_version": 1,
  "id": "disk-report",
  "name": "Disk report",
  "description": "Reports the largest directories under a fixed path.",
  "version": "1.0.0",
  "model": { "alias": "{{config.model}}" },
  "config": {
    "schema": {
      "type": "object",
      "properties": {
        "model": { "type": "string", "format": "model_alias", "title": "Model" },
        "top": { "type": "integer", "title": "How many", "default": 3, "minimum": 1 }
      }
    }
  },
  "run": {
    "kind": "container",
    "image": "localhost/disk-report:1",
    "pull": "never",
    "columns": ["path", "size"],
    "phases": ["run"],
    "limits": { "memory_mb": 128, "cpus": 1, "deadline_seconds": 120 },
    "output": {
      "run": {
        "type": "object",
        "properties": { "counted": { "type": "integer" } },
        "required": ["counted"]
      }
    }
  }
}
```

`report.sh`:

```sh
#!/bin/bash
set -euo pipefail

# jq is not in fedora-minimal; python3 is one microdnf away, but a report this
# small can quote its own JSON.
esc() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }

top=$(sed -n 's/.*"top"[[:space:]]*:[[:space:]]*\([0-9]*\).*/\1/p' "$LMGW_INPUT")
top=${top:-3}

echo "{\"type\":\"log\",\"level\":\"info\",\"message\":\"reporting the top $top\"}"

n=0
while read -r size path; do
  n=$((n + 1))
  echo "{\"type\":\"row\",\"id\":\"$(esc "$path")\",\"columns\":{\"path\":\"$(esc "$path")\",\"size\":\"$(esc "$size")\"}}"
  echo "{\"type\":\"progress\",\"done\":$n,\"total\":$top,\"stage\":\"measuring\"}"
done < <(du -sh /usr/share/* 2>/dev/null | sort -rh | head -n "$top")

echo "{\"type\":\"output\",\"output\":{\"counted\":$n}}"
```

`Containerfile`:

```dockerfile
FROM registry.fedoraproject.org/fedora-minimal:44
COPY report.sh /usr/local/bin/report.sh
RUN chmod +x /usr/local/bin/report.sh
ENTRYPOINT ["/usr/local/bin/report.sh"]
```

The tag is pinned because that is the one on this box; any base with a shell and
`coreutils` works, and lmgw cares about nothing in the image except that it
starts and prints JSONL.

Build and install it. Every `curl` against `/api` needs an owner bearer — the
**Copy** button beside the `owner:dashboard` row on Usage → Keys is where the
value comes from:

```sh
podman build -t localhost/disk-report:1 .
curl -sS -X POST http://127.0.0.1:8787/api/agents/import \
  -H 'Authorization: Bearer <owner key>' \
  --data-binary @disk-report.agent.json
```

Notes that generalise:

- The root filesystem is read-only and `/tmp` is a tmpfs. Write nowhere else.
- One JSON object per line, flushed as it goes. Anything that is not JSON — a
  `set -x` trace, a warning from `du` — lands in the run log verbatim.
- `progress` is optional; without it the job's bar has no total.
- Exiting `0` with no `output` event would fail this run, because
  `run.output.run` declares a schema.

### Worked example: a Node container that calls a tool

```dockerfile
FROM docker.io/library/node:24-alpine
WORKDIR /app
COPY main.mjs /app/main.mjs
COPY agent.json /lmgw/agent.json
ENTRYPOINT ["node", "/app/main.mjs"]
```

`main.mjs`:

```js
import { readFileSync } from "node:fs";

const input = JSON.parse(readFileSync(process.env.LMGW_INPUT, "utf8"));
const secrets = JSON.parse(readFileSync(process.env.LMGW_SECRETS, "utf8"));
const MCP = process.env.LMGW_MCP_URL;
const RUN = process.env.LMGW_RUN;

const emit = (e) => process.stdout.write(`${JSON.stringify(e)}\n`);
const log = (message) => emit({ type: "log", message });

let sessionId = null;
let nextId = 0;

async function rpc(method, params) {
  const headers = {
    "content-type": "application/json",
    accept: "application/json, text/event-stream",
    "mcp-protocol-version": "2025-11-25",
    authorization: `Bearer ${secrets.token}`,
    "x-lmgw-run": String(RUN),
  };
  if (sessionId) headers["mcp-session-id"] = sessionId;
  const res = await fetch(MCP, {
    method: "POST",
    headers,
    body: JSON.stringify({ jsonrpc: "2.0", id: ++nextId, method, params }),
  });
  const text = await res.text();
  if (!res.ok) throw new Error(`${method} failed (HTTP ${res.status}): ${text.trim()}`);
  if (method === "initialize") sessionId = res.headers.get("mcp-session-id");
  const body = JSON.parse(text);
  if (body.error) throw new Error(body.error.message);
  return body.result ?? {};
}

async function call(name, args) {
  if (!sessionId) {
    await rpc("initialize", {
      protocolVersion: "2025-11-25",
      capabilities: {},
      clientInfo: { name: "disk-report", version: "1" },
    });
  }
  const result = await rpc("tools/call", { name, arguments: args ?? {} });
  const firstText = (result.content ?? []).find((b) => b?.type === "text");
  if (result.isError) throw new Error(firstText ? firstText.text : `${name} failed`);
  if (result.structuredContent != null) return result.structuredContent;
  if (firstText) return JSON.parse(firstText.text);
  throw new Error(`${name} answered with no JSON`);
}

try {
  log(`phase ${input.phase} for ${input.agent.id}`);
  // docs__resolve takes { library, query? } and answers { matches: [...] }.
  const found = await call("docs__resolve", { library: input.config.library });
  let n = 0;
  for (const corpus of found.matches ?? []) {
    emit({
      type: "row",
      id: corpus.corpus_id,
      columns: { corpus: corpus.corpus_id, version: corpus.version ?? "" },
    });
    n += 1;
  }
  emit({ type: "output", output: { corpora: n } });
} catch (e) {
  process.stderr.write(`${e.stack ?? e}\n`);
  process.exitCode = 1;
}
```

Its manifest needs `"columns": ["corpus", "version"]` so the two column keys are
declared (an undeclared one is kept and appended, but the header order is
yours), a `library` config field, and `docs__resolve` in reach —
`{"label": "docs"}` for the whole toolset, or
`{"label": "docs", "allowed": ["docs__resolve"]}`. A name outside the allow list
is refused by `/mcp` with the list quoted back:
`unknown tool: docs__query — agent 'disk-report' may call: docs__resolve` (and
`…may call: nothing; its manifest declares no tools` when there is no allow list
at all).

---

## 6. The run ledger over HTTP

<!-- source: crates/lmgw-core/src/web/api_agents.rs (run_open, run_events, run_close), crates/lmgw-core/src/agents/ledger.rs -->

For a process lmgw did **not** start — a cron job, a long-running service, a
laptop script — the ledger turns a posted event stream into the same rows, log
and result an in-process run produces. Three routes, all of them `Ledger` in
the capability table (principals §3.2): the gate admits an **agent token** and
nothing else — not an owner key, not the dashboard session — and the handler is
what says which agent's run it is.

### Getting the token

- Dashboard: **File ▾ → Copy agent token** on the Definition tab.
- Op: `POST /api/op/agent_token_get` with `{"id": "<agent id>"}`. The response
  carries `token`, `name` (`agent:<id>`), `scope_mode`, `scope_patterns` and
  `scope_note`.
- `agent_token_rotate` mints a new one and kills the old immediately.

Both ops are **local-only in the browser sense**, though no longer by a rule of
their own: a call authenticated by the session cookie is held to the same-origin
rule every cookie-authenticated request is held to (§3.6, and the bullet in §9),
so a foreign page asking for a token is refused with `403 cross_origin_refused`.
A caller presenting a bearer and sending **no** `Origin` at all — curl, a
container, a script — is unaffected. Neither op is exposed as an `lmgw__*` tool.

### The three routes

**Open a run.**

```
POST /api/agents/{id}/runs
Authorization: Bearer <agent token>
Content-Type: application/json

{ "phase": "run", "rows": [] }
```

`phase` is `run` (the default when absent) or `apply`. `rows` seeds the table —
an apply's reviewed rows. An empty body is accepted and means
`{"phase":"run","rows":[]}`.

```json
{ "run": 412, "deadline_seconds": 600 }
```

**Post events.**

```
POST /api/agents/runs/{run}/events
Authorization: Bearer <agent token>
```

The body is one JSON object, a JSON array of them, or NDJSON. The four event
types and their rules are exactly the ones in §5 — same decoder.

```json
{ "ok": true, "applied": 3, "rejected": [] }
```

`rejected` carries the run-log line for each event that was understood and
refused (an unknown `type`, a `row` with no `id`, a non-JSON line).

**Close it.**

```
POST /api/agents/runs/{run}/close
Authorization: Bearer <agent token>

{ "status": "done", "detail": null, "output": { "applied": 3 } }
```

`status` is `done`, `failed` or `canceled`. **The default is `done`, and it is a
generous default**: a body that is not JSON at all, a body with no `status`, or
a `status` that is not a string all close the run `done`. Only a *string* status
outside the three is refused (`status 'finished': a run closes done, failed,
canceled`). So `curl -X POST …/close` with no body succeeds and marks the run
done — convenient, and worth knowing before you rely on a malformed body being
caught.

`output` here sets the terminal value just as an `output` event does, and it is
checked against `run.output.<phase>` exactly the same way. `failed` without a
`detail` ends the job with `the run reported failed with no detail`.

```json
{ "ok": true }
```

### The codes

| Code | HTTP | Meaning |
|---|---|---|
| `agent_token_required` | 401 | No bearer, or one that is not an agent token. Message names *Copy token* and `agent_token_get`. |
| `agent_disabled` | 401 | The token is this agent's and the agent is switched off. Disable is the kill switch. |
| `run_not_owned` | 403 | On events/close: a valid agent token, but the run belongs to another agent — **ownership is answered before existence**, so probing cannot enumerate another agent's job ids. On **open** it also fires when the `{id}` in the path is not the token's own agent, including when that id does not exist at all: you may only open a run for yourself, and the 403 does not confirm whether the other agent is real. |
| `not_found` | 404 | No agent run with that job id, anywhere. |
| `run_cancelled` | 409 | Cancel already ran; every later event and the close are refused. |
| `run_closed` | 409 | Already closed by an earlier POST, or the run ended. |
| `run_not_ledger` | 409 | The agent's own run is in flight, but **lmgw is driving it**, so there is no ledger to write to. |
| `already_running` | 409 | On open: one live run per agent. The body carries `{"code","message","run"}` with the id of the run that is already in flight — deliberately not handed over as though it were freshly opened. |
| `invalid_utf8` | 400 | The event body is not valid UTF-8. Not lossily decoded: a replacement character would silently rewrite a row id. |
| `bad_request` | 400 | An unparseable open body, a `phase` that is neither `run` nor `apply`, a `status` that is not one of the three, or a `chat` agent. |
| `op_failed` | 400 on open, 500 on events/close | The gateway could not answer. On open: the agent row could not be loaded or parsed (anything but "no such id", which is `not_found`), the token's scope could not be recomputed, or the job could not be started. On events and close: a read of the jobs table failed. |

**None of the three has a body size limit.** axum's silent 2 MiB default is
disabled on all of them, because a batch is as big as its rows are and a cap
nobody chose answering with an unexplained plain-text 413 is exactly the kind of
hidden bound this gateway does not have.

### The deadline and `no_close`

`run.limits.deadline_seconds` is counted **from the open** — there may be no
container — and is reported in the open response so the caller is told exactly
how long it has. `0` means no deadline; the run log says
`run opened through the ledger; no deadline (run.limits.deadline_seconds = 0)`.

If the deadline passes with the run still open, the job ends **failed** with:

```
no_close: the run reached its deadline of 600s without a close (POST /api/agents/runs/412/close)
```

That is the honest reading of "the process that opened this is gone", and it is
what keeps a ledger run from leaking a live job row forever.

A full session:

```sh
BASE=http://127.0.0.1:8787
TOKEN=$(curl -sS -X POST "$BASE/api/op/agent_token_get" \
  -H 'Authorization: Bearer <owner key>' \
  -H 'content-type: application/json' -d '{"id":"nightly"}' \
  | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')

RUN=$(curl -sS -X POST "$BASE/api/agents/nightly/runs" \
  -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"phase":"run"}' | sed -n 's/.*"run":\([0-9]*\).*/\1/p')

curl -sS -X POST "$BASE/api/agents/runs/$RUN/events" \
  -H "Authorization: Bearer $TOKEN" -H 'content-type: application/x-ndjson' \
  --data-binary $'{"type":"log","message":"started"}\n{"type":"row","id":"a","columns":{"what":"one"}}\n'

curl -sS -X POST "$BASE/api/agents/runs/$RUN/close" \
  -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"status":"done","output":{"seen":1}}'
```

---

## 7. Service mode and the App tab

<!-- source: crates/lmgw-core/src/agents/service.rs, crates/lmgw-core/src/web/agent_proxy.rs -->

A `container` agent that declares `run.service` serves its **own UI** out of the
same image its run and apply phases use, and lmgw reverse-proxies it on its
own origin, dispatched on the `Host` header ahead of the main router — the
browser's own origin isolation is what keeps that UI apart from the dashboard.

### The origin

```
http://<id>.<agent_origin_suffix>:<bind port>/
```

`agent_origin_suffix` is a setting, default `localhost`. No principal is
resolved and no capability required for a request that lands here: the agent
origin is the container's namespace, public to whoever can reach the port,
exactly as `/agents/<id>/app/` was. The old path now answers `404
agent_app_moved` with `{ "origin": "http://<id>.<suffix>:<port>/" }` in the
body, so an old bookmark says where to go.

Public to whoever can reach the port now includes **state-changing requests
from any page a browser on this box has open**: a foreign page can navigate
to or form-post at `http://<id>.<suffix>:<port>/` exactly as it could reach
the old path — it cannot read the answer, and it holds no dashboard cookie,
but the request still lands. An app that changes state on a `POST` needs its
own CSRF posture; lmgw does not supply one. An app that serves anything
private decides whom it serves itself, from the `X-Lmgw-Face` and
`X-Forwarded-For` lmgw sets (see "The proxy" below) — reaching the port is
all the origin asks.

- **The label is the agent id itself.** No slug, no lossy mapping — ids are
  already unique keys. A DNS label is at most 63 characters and does not end
  in `-`; a service-declaring manifest whose id breaks either rule is refused
  at write with `origin_label_invalid`. An id whose `<id>.<suffix>` equals a
  name the gateway itself answers on is refused with `origin_shadows_gateway`.
- **Why `*.localhost`.** Chrome and Firefox resolve any `*.localhost` to
  loopback internally; systemd-resolved does the same for every other
  resolver client on Fedora. A distribution without it needs a hosts entry per
  agent (`127.0.0.1 board.localhost`) — the App tab says so.
- **`agent_origin_suffix` exists for one reason**: a gateway bound to a real
  address and opened from another machine. Set it to a zone with a wildcard
  record (`*.lmgw.lan → <host>`) and every agent origin becomes
  `<id>.lmgw.lan:<port>`. It is validated on write — one or more DNS labels, no
  port, no scheme, not empty, and not `local` (mDNS cannot answer arbitrary
  names under it) — and refused with `origin_suffix_shadows_gateway` when it is
  itself a name the gateway answers on, or shares a parent domain of two or
  more labels with one: a dashboard at `myhost.lmgw.lan` refuses a suffix of
  `apps.lmgw.lan`, because a page under that suffix could set a cookie for
  `lmgw.lan` and overwrite — never read — the dashboard's session; a suffix of
  `agents.lan` is fine. The gateway's own names are its bind address, the host
  name, `<hostname>.local`, and `<hostname>.<search domain>` for each resolver
  search domain. The check runs again at boot; a stored suffix that has
  started shadowing is reported as a warning rather than reset.
- The port is `bind_addr`'s; the origin answers on the same socket, only the
  `Host` header differs.

`/agents/{id}/mcp` is the one exception: it stays a **path** on the main
origin rather than moving here, because its only consumer is lmgw's own
server-side MCP client — a browser never sees that URL, so origin isolation
buys nothing there. See `provides.mcp` below.

### `run.service`

| Field | Type | Default | Notes |
|---|---|---|---|
| `port` | integer, required | — | The in-container port. 1–65535. The host side is ephemeral and picked per start. |
| `health_path` | string | `"/"` | What the start probes. Must start with `/`. **Empty is a choice, not an omission**: it selects a TCP connect to `port` instead of an HTTP GET, for an image whose port speaks something HTTP cannot introduce itself to. |
| `start_timeout_seconds` | integer | `30` | How long the **health probe** is allowed to run. **`0` = no limit** — it probes until the container answers; Stop still interrupts it. It does **not** bound `podman run -d` itself: see below. |
| `idle_seconds` | integer | `300` | Stop the container after this long with no request and none in flight. **`0` = never idle-stop.** |

### On-demand start

The **first proxied request pays for the start**; N concurrent first requests
share exactly one `podman run`. The claim is made under the map lock and the
start runs in a spawned task, so a client that disconnects halfway through a
20-second image start cannot abort it and leave an untracked container.

The container is started `-d` and **not** `--rm`: `podman logs` is the only
account of a container that died during its health probe, and `--rm` would
delete the evidence before the 503 could quote it. It is published on
`-p 127.0.0.1:<ephemeral>:<service.port>` — loopback only, because the proxy is
the only client and a container bound to every interface would put an agent's UI
on the LAN without anyone asking. The chosen host port is in the run log and in
the App tab.

A start is two steps, and `start_timeout_seconds` bounds only the second.
First `podman run -d` runs to completion — **including a cold `--pull=missing`
download, which nothing here bounds**; a pull takes as long as the image is big,
and a clock over it would turn a slow network into a lie about the registry.
Only once podman has returned does the clock start on the probe. So a first
request against a 2 GB image that is not on the box can sit far longer than
`start_timeout_seconds` before it either succeeds or reports a probe timeout.
A Stop interrupts either step.

The probe polls every 250 ms until `start_timeout_seconds` is spent. An HTTP
status **below 500** counts as up — an app whose `/` answers `302 → /login` or
`404` is listening and speaking HTTP, which is the question — and a blank
`health_path` makes it a TCP connect instead. A probe that never succeeds stops
and removes the container before the 503 is written. **On the agent origin
itself that 503 is generic** — "the app could not be started", no `log`, no
path — because that origin is reachable by anyone who can reach the port
(§7); the reason, with the log tail, is on the App tab and in the
`agent_service_start` op's own answer, both owner-authenticated.

Failures the proxy returns:

| Code | HTTP | When |
|---|---|---|
| `agent_service_starting` | 503 | On the public agent origin, generic: "the app could not be started", no `log`, no path. The reason, with `log`, is on the App tab and in the `agent_service_start` op's own answer only. |
| `agent_service_unreachable` | 502 | On an ordinary request: the container was up, passed its probe and has stopped answering — the entry is evicted so the **next** request starts a fresh one. On a **WebSocket upgrade** this is the only unreachable code there is: the upstream refused the upgrade, nothing is evicted, and there is no dev-server variant. |
| `agent_dev_url_unreachable` | 502 | An ordinary request, a `dev_url` is set, and that server did not answer. Nothing is evicted — lmgw did not start it. |
| `agent_service_not_declared` | 404 | The manifest has no `run.service`. |
| `agent_provides_no_mcp` | 404 | `/agents/<id>/mcp` on an agent with no `run.provides.mcp`. |
| `not_found` | 404 | No agent with that id. |
| `agent_unreadable` | 400 | This build cannot parse the stored manifest. |
| `op_failed` | 500 | The agent row could not be read from the database. |
| `upgrade_unavailable` | 400 | An upgrade was asked for on a connection that cannot be upgraded. |
| `bad_path` | 400 | The upstream URL could not be assembled — an `OPTIONS *` request-target, for instance — or, on the MCP face only, a `.` or `..` path segment. |

### The proxy

Every method, every path, every upgrade is forwarded to the container's
published loopback port (or the `dev_url`), with the path and query byte for
byte as they arrived. The container is mounted at `/` on its own origin and
writes its own URLs — build for `/`, not for a sub-path (no `--base`, no
`--public-url`). Both bodies are streamed, never buffered, and there is
nothing to rewrite on the way in.

On the way **in**:

- Hop-by-hop headers are stripped: `connection`, `keep-alive`,
  `transfer-encoding`, `te`, `trailer`, `upgrade`, `proxy-authorization`,
  `proxy-authenticate`.
- `cookie`, `authorization` and `host` are **never forwarded**. The browser no
  longer holds a dashboard cookie against this origin to begin with, but the
  stripping stays as defence in depth. `host` is never forwarded because
  reqwest sets its own; the public host travels in `X-Forwarded-Host` instead.
  The container has its own credential in `secrets.json`.
- `X-Forwarded-Host: <id>.<suffix>:<port>` and `X-Forwarded-Proto: http` are
  set by lmgw (`insert`, never `append`), so a server-side framework can derive
  its public URL from them. A client-supplied `X-Forwarded-Host`,
  `X-Forwarded-Proto`, `X-Forwarded-For`, `Forwarded` or `X-Lmgw-Face` is
  dropped rather than passed through — the app is now told to trust these, so
  lmgw has to own them. There is no `X-Forwarded-Prefix` any more: the app is
  mounted at `/`, and there is nothing to tell it about.
- `X-Lmgw-Face: app` on everything forwarded from the agent origin, WebSocket
  upgrades included, and `X-Lmgw-Face: mcp` on everything forwarded from
  `/agents/<id>/mcp*`. Only lmgw sets it, so an app may rely on `mcp` meaning
  the request passed the `Admin` gate — `X-Forwarded-Host` is the same on both
  faces, and this is the one way to tell them apart.
- `X-Forwarded-For: <ip>`, exactly one address, from the TCP peer of the
  connection that reached lmgw (an IPv4 client of a dual-stack listener is
  reported as IPv4). Only lmgw sets it, so an app may rely on it to tell a
  browser on this machine (loopback) from another machine — and from another
  container on this box, which reaches a gateway bound to `0.0.0.0` through
  `host.containers.internal` and arrives from the host's own address, not
  loopback (one reaching a loopback-bound gateway through pasta's `-T` forward,
  as lmgw's own agent containers do, arrives from `127.0.0.1`). On the MCP
  face the peer is lmgw's own MCP client dialling itself (loopback, or this
  host's own address when the gateway is bound to one): that face's trust is
  the `Admin` gate, not the address.
- The **raw** request path is forwarded byte for byte, percent-encoding intact,
  and the query is attached as a query rather than concatenated.

On the way **out**:

- Hop-by-hop headers and `content-length` are stripped (the body is re-framed as
  a stream).
- One rewrite, and only one: an absolute `Location` naming the container's
  **published loopback origin** (`http://127.0.0.1:<host port>/x`) or the
  `dev_url` origin is rewritten to the agent origin
  (`http://board.localhost:8787/x`), because the browser cannot reach the
  former. Every other `Location` — relative, origin-relative, or absolute
  elsewhere — passes untouched.
- `set-cookie` goes through **as-is** — the cookie lands on the agent's own
  origin, not the dashboard's.
- The status is forwarded verbatim; a 3xx is **not** followed.

### SSE and WebSockets

Both work, because the proxy never buffers. A response body is streamed chunk by
chunk and every chunk touches `last_used`, but that is belt to the braces: **the
load-bearing rule is the in-flight counter.** The guard is claimed before the
request goes out and released only when the body finishes (or the client goes
away and the future is dropped), and the idle sweep skips any service with a
request in flight however stale `last_used` looks — and re-checks both
conditions under the map lock, so a request that claimed its guard a moment ago
cannot be torn down by a decision made before it existed. That is what keeps an
SSE stream open for an hour from being idle-stopped mid-flight.

A WebSocket is a **byte pipe**: `Connection: Upgrade` + `Upgrade: websocket` is
detected, the five `sec-websocket-*` headers are carried through unchanged, and
the two halves are copied raw. Nothing parses a frame — masking, fragmentation,
ping/pong, close and any negotiated extension are between the two endpoints. The
tunnel ends when **either** half sends EOF, and the in-flight guard lives as long
as the tunnel. If the container answers something other than `101`, that answer
is forwarded verbatim.

### `provides.mcp`

`run.provides.mcp` is the path inside the container that speaks MCP; it requires
`run.service` and must start with `/`. Unlike the App tab (above), this face
stays a **path** on the main origin rather than getting one of its own — the
asymmetry is deliberate: the only consumer of `/agents/<id>/mcp` is lmgw's own
MCP client, server-side, and a browser never sees that URL. lmgw keeps an `mcp_servers` row for it
named `agent:<id>`, pointed at its own `/agents/<id>/mcp` proxy (never at the
ephemeral host port, which would be stale on the next start), with
`tool_prefix = <agent id>` and `autostart: false`. `agent:` is a reserved name
prefix on the MCP page for that reason.

Two of its fields are copied **only when the row is created** and then left
alone: `enabled` (from the agent) and `idle_seconds` (from
`service.idle_seconds`). Nothing re-syncs them — a manifest write only ever
rewrites `url`, `tool_prefix` and `agent_id`, so whatever you set on the MCP
page survives. In particular, **disabling the agent does not flip the row's
`enabled`**: the kill switch is elsewhere and is enforced where it cannot be
worked around — `service::ensure` refuses to start a disabled agent's container,
and the token stops authenticating — rather than through a flag the owner is
also allowed to hold. Expect a disabled agent's MCP row to keep saying
"enabled"; it simply has nothing to connect to.

The container's tools therefore appear on `/mcp` as `<agent id>__<tool>`, and a
chat thread attaching that label can call them.

**Listing never wakes the container.** A sleeping service agent's tools are
deliberately absent from the `/mcp` aggregate — listing must not `podman run`
every service on the box — and the MCP page says "sleeping" rather than
"failed". Three things *do* start it: a `tools/call` naming one of its tools
(matched by prefix against the agent rows), a chat thread or a run explicitly
attaching its label, and the App tab. A row with a `dev_url` is **always**
listable, because there is nothing to start.

The row is created, updated and deleted with the agent. Removing
`provides.mcp` from the manifest removes the row; only `url`, `tool_prefix` and
`agent_id` are lmgw's to keep true on an existing row — everything else the owner
set on the MCP page survives a manifest write.

### `dev_url`

<!-- source: crates/lmgw-core/src/agents/service.rs (validate_dev_url), crates/lmgw-core/src/web/api_agents.rs (agent_dev_url_set) -->

While you are building the UI, the App tab's **Dev server** field (or
`agent_dev_url_set { id, url }`) points the agent origin (and the
`/agents/<id>/mcp` mount) at something you are already running:

```sh
trunk serve --port 5173
# or
vite --port 5173

curl -sS -X POST http://127.0.0.1:8787/api/op/agent_dev_url_set \
  -H 'Authorization: Bearer <owner key>' \
  -H 'content-type: application/json' \
  -d '{"id":"board","url":"http://127.0.0.1:5173"}'
```

Rules, all enforced before it is stored:

- **`http` or `https` only.**
- **Loopback only** — `localhost`, `127.0.0.0/8`, `::1`. Not a LAN address: the
  app proxy sits on the dashboard plane, which has no authentication and
  permissive CORS, so pointing it at another host would make lmgw an open
  reverse proxy for that host.
- **No userinfo** — `http://user:pass@127.0.0.1` would store a credential on a
  row that is read back onto the page.
- **No query and no fragment** — the proxy appends the request's own path and
  query.
- **No path.** A dev server is an origin now: `http://127.0.0.1:5173/base` is
  refused with `a dev_url is an origin; it cannot carry a path`. Run `trunk
  serve` / `vite` with no `--base` / `--public-url` flag.
- **Not lmgw's own port**, whatever host spelling is used: that would make the
  agent origin proxy into lmgw's own router until the file descriptors ran out.
- The agent must declare `run.service`; otherwise nothing reads the value.

A trailing slash is stripped.

Setting one **stops a running app container**; clearing it (`{"url": null}`)
goes back to the image and the next request starts the container. `dev_url` is a
row setting, never part of the manifest, and is **never exported**. It overrides
service mode only — runs and applies still start the image, because their dev
loop is a rebuild. `agent_service_start` refuses while it is set.

If a later `bind_addr` change makes a stored `dev_url` illegal (most often
because the gateway moved onto that very port), it is **cleared** and recorded,
and the row then shows `dev_url_cleared` until you answer it. A row stored
before dev servers became origins may still carry a path from an older build;
that one is cleared **at boot** the same way, rather than proxying to a base
nothing strips any more.

### The App tab

The tab shows the origin as a link, the `origin_resolves` verdict — whether
`tokio::net::lookup_host` resolved `<id>.<suffix>:<port>` when the page asked,
using the same resolver the WebKitGTK window and `curl` use — and the app in an
`<iframe src="<origin>?v=<generation>">`, plus **Open full page**
(`target="_blank"`, which the shell sends to the system browser; that works
because the agent origin needs no cookie).

For an agent with mount fields, the tab also lists the bound mounts — host
path to container path, with access.

Three standing lines above the frame, each shown only when it applies:

- `origin_resolves` false: "`board.localhost` does not resolve on this
  machine. Add `127.0.0.1 board.localhost` to `/etc/hosts`, or set an agent
  origin suffix under Settings → Agents & tools that your DNS answers for."
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

### Start, Stop, the log tail, and what stops the container

`agent_service_start { id }` is the same on-demand path a proxied request takes,
pressed by hand; it answers with `container`, `host_port` and `origin`.
`agent_service_stop { id }` runs the stop ladder — `podman stop -t <grace>` then
`podman rm -f`, with no settle between them — and **stopping something that is
not running is success**.

`agent_service_log { id, lines? }` returns the tail. `lines` defaults to 12
(the same excerpt size the rest of the runtime uses, and what the App tab shows)
and **`0` is the whole log**. The count is echoed back with the text. The
agent's own token is substituted out of every tail lmgw republishes.

Ten things stop a running app container, each saying so:

| Action | Why |
|---|---|
| App tab **Stop** / `agent_service_stop` | asked for |
| `agent_token_rotate` | the container is holding a token that stopped working |
| `agent_enable { enabled: false }` | Disable is the kill switch; `ensure` also refuses to start a disabled agent |
| `agent_delete` | the agent and its `agent:<id>` MCP row go too |
| A manifest replace (import, `agent_set`, `agent_reimport`) | it is holding the old image's argv, limits and config |
| Setting a `dev_url` | it was started from the image the override replaces |
| `service.idle_seconds` elapsed with nothing in flight | the idle sweep, on the 5 s status tick |
| A proxied request getting no answer | the proxy evicts the entry itself and returns `502 agent_service_unreachable`, so the next request starts a fresh container instead of 502-ing forever (with `idle_seconds: 0`, for good) |
| A change of the **agent origin suffix** under Settings | it is holding an origin the owner has renamed |
| `agent_config_set` changing a **mount field's** value | it is holding a mount the owner has re-pointed |

A **start in flight** is cancelled by any of the explicit ones, its container
collected, and the waiters get the reason — the idle sweep never cancels a
start.

### Worked example: folder chat, a service agent with a mount

<!-- source: examples/agents/folder-chat/ -->

The shipped **folder-chat** example: chat with a folder of notes, code and
PDFs, indexed into a hidden directory inside the folder itself. Abridged:

```json
{
  "id": "folder-chat",
  "version": "0.1.0",
  "model": { "alias": "{{config.chat_model}}" },
  "config": { "schema": { "properties": {
    "folder": { "type": "string", "format": "directory", "access": "rw" },
    "embed_model": { "type": "string", "format": "model_alias" },
    "chat_model": { "type": "string", "format": "model_alias" },
    "rerank_model": { "type": "string", "format": "model_alias" },
    "allow_remote": { "type": "boolean", "default": false }
  }, "required": ["folder", "embed_model", "chat_model"] } },
  "run": {
    "kind": "container",
    "image": "localhost/folder-chat:0.1.0",
    "service": { "port": 8080, "health_path": "/healthz", "idle_seconds": 1800 },
    "provides": { "mcp": "/mcp" },
    "limits": { "memory_mb": 2048 }
  }
}
```

Every piece above is one of §5's or §7's rules made concrete:

- **`folder`**, `access: "rw"`, is the one mount field — the index lives
  inside the folder it indexes. Any mount field turns on `--userns=keep-id`
  (§5): the process runs as the owner's uid, never root, so the Containerfile
  has no `USER` line and instead makes every file world-readable. Binding it
  relabels it, and everything under it, to `container_file_t`, `:z`,
  recursively and permanently.
- **The origin.** `run.service` makes this a service agent: its UI is
  `http://folder-chat.localhost:<bind port>/`, reverse-proxied on the `Host`
  header — a different site, with no dashboard cookie to inherit.
- **Whom it serves is the app's own job** — the origin asks for no
  credential. folder-chat runs four rules, in order, on every route but
  `/healthz`, each refusal a `403` naming its rule:
  1. **Provenance**: exactly one `X-Forwarded-Host`, equal to its own
     origin's authority (`not_via_lmgw`) — a request straight at the
     container's published port has none.
  2. **Face**: `/mcp` needs `X-Lmgw-Face: mcp` (`not_mcp_face`), every other
     route `X-Lmgw-Face: app` (`not_app_face`). Without it, anyone who can
     reach the gateway port could call `read` through the origin.
  3. **Client address**, app face only: `X-Forwarded-For` must be loopback
     (`remote_client`) unless the owner turned on `allow_remote` ("Serve other
     machines"); a missing or unparseable one is `not_via_lmgw`. Another
     container on this box counts as remote. The MCP face is not
     address-checked — its trust is the `Admin` gate.
  4. **CSRF**, on every method but `GET` and `HEAD`: a custom
     `X-Folder-Chat: 1` header (`csrf_header`; the MCP face is exempt), a JSON
     `Content-Type` (`csrf_content_type`), and an `Origin`, when one is sent,
     equal to its own origin (`csrf_origin`) — none of which a cross-site form
     POST can produce.
- **Framing.** The App tab shows the UI in an iframe, so its CSP's
  `frame-ancestors` is `'self'` plus the dashboard's loopback origins at the
  gateway's port (`http://127.0.0.1:<port>` — what the desktop window loads —
  `http://localhost:<port>`, `http://[::1]:<port>`), not `'none'`. A
  dashboard opened from a LAN address cannot frame it; **Open full page**
  opens it at its own origin instead.
- **Writes stay in one directory.** The agent writes only
  `<folder>/.lmgw-folder-chat/`: the SQLite index and its sidecars, plus a
  `.gitignore` of `*` and a `CACHEDIR.TAG` when it creates the directory.
  The directory is held open from start-up and re-checked before every sync
  and every file (same device and inode, no symlink, every index file a
  plain file with one link); if it was moved or replaced, the sync stops
  with `aborted` kind `index_containment` before writing anything. The index
  keeps the vector of a fixed probe sentence as its embedding model's
  fingerprint, so an alias re-pointed to another model of the same width
  resets the index (cosine under `PROBE_SAME_MODEL_MIN_COSINE`, 0.99) rather
  than mixing two vector spaces.
- **No hidden limits in the app either.** A question's request body is read
  up to the chat model's `context_length` × `MAX_BYTES_PER_TOKEN` (48: the
  budget's 4 characters a token × at most 12 bytes a character in JSON),
  with the budget's `FALLBACK_CONTEXT_TOKENS` (8192) standing in when the
  model reports none; past it, `413 body_too_large` names the numbers, and
  `/api/status` shows the current limit. A reported `max_output_tokens` is
  the answer reserve only when it leaves room for the system prompt and one
  excerpt — a model that reports its whole context there gets
  `DEFAULT_ANSWER_RESERVE_TOKENS` instead, with a note. The one size skip is
  derived from the real cgroup memory limit: a file over
  `LARGE_FILE_MEMORY_FRACTION` (a quarter) of it is skipped as
  `too_large_for_memory`, and a PDF `pdftotext` cannot extract within
  `PDF_EXTRACT_TIMEOUT` as `pdf_timeout`, each naming its numbers. The
  retrieval depths (`k_fts`, `k_vec`, `rrf_k`, `k_rerank`) are in every
  answer's notes, in `/api/status` and in the MCP `search` description.
- **Keeping it local.** File text goes only to the three aliases the owner
  picked. A GPU-hold fallback on one of them routes the next request there
  before the agent can see it: an embedding batch answered by a fallback
  (`x-lmgw-fallback`) stops the sync unstored, a rerank is dropped with a
  note saying the question's excerpts reached the fallback, and a chat
  answer is labelled with the fallback's name. A folder that must stay on
  the machine needs aliases without a cloud fallback.
- **Stopping.** `POST /api/sync/stop` — the Sync card's **Stop sync** button,
  behind the same four rules — stops a running sync where it stands; the run
  ends with an `aborted` event of kind `stopped`, reason "stopped by the
  owner", and the next sync redoes every file it had not finished. On
  SIGTERM the sync is stopped first — the same kind, its own reason — then
  in-flight requests get 5 s, then the index is closed — inside the default
  `stop_grace_seconds` of 10.
- **`provides.mcp: "/mcp"`** puts an `agent:folder-chat` row on the MCP page —
  its two tools appear on lmgw's aggregate `/mcp` as `folder-chat__search`
  and `folder-chat__read`, starting the container on demand.
- **The token's scope is exactly three aliases**: `embed_model`, `chat_model`
  and `rerank_model` are the manifest's only `format: "model_alias"` fields
  (§9); `model.alias` is a template, `"{{config.chat_model}}"`, not a
  literal, so it adds nothing — it is already one of the three.

Build and install it (`examples/agents/folder-chat/build.sh`):

```sh
./build.sh             # cargo build --release, podman build, tag checked
./build.sh --install    # also POSTs /api/op/agent_install
```

Walkthrough, what it writes, what it skips, its limits:
[`examples/agents/folder-chat/README.md`](../examples/agents/folder-chat/README.md).

---

## 8. Packages

<!-- source: crates/lmgw-core/src/agents/package.rs, crates/lmgw-core/src/web/api_agents.rs (agent_install, agent_pull, agent_reimport) -->

An **agent package** is an OCI image carrying its manifest at `/lmgw/agent.json`.
That fixed path is the whole of the format.

### Building one

```dockerfile
FROM docker.io/library/node:24-alpine
WORKDIR /app
COPY main.mjs /app/main.mjs
# The manifest, at the one path lmgw looks at.
COPY agent.json /lmgw/agent.json
ENTRYPOINT ["node", "/app/main.mjs"]
```

```sh
podman build -t localhost/board:1 .
```

The manifest inside should name **itself** as `run.image`:

```json
{
  "schema_version": 1,
  "id": "board",
  "name": "Board",
  "model": { "alias": "{{config.model}}" },
  "config": { "schema": { "type": "object", "properties": {
    "model": { "type": "string", "format": "model_alias", "title": "Model" }
  } } },
  "run": {
    "kind": "container",
    "image": "localhost/board:1",
    "pull": "never",
    "phases": ["run"],
    "service": { "port": 8080, "health_path": "/healthz", "idle_seconds": 300 },
    "provides": { "mcp": "/mcp" }
  }
}
```

### Installing

From the catalog page: **Install from image…**. From the API:

```sh
curl -sS -X POST http://127.0.0.1:8787/api/op/agent_install \
  -H 'Authorization: Bearer <owner key>' \
  -H 'content-type: application/json' \
  -d '{"image":"localhost/board:1","pull":"never","replace":false}'
```

or `lmgw__agent_install { image, pull?, replace?, validate_only? }`.

lmgw reads the manifest **without starting the image**: `podman create
--pull=never …` (labelled like every other agent container so a crash cannot
orphan it), `podman cp <container>:/lmgw/agent.json <run tmpfs>`, `podman rm -f`.
The text then goes through the **same import path** a dropped file takes, so
every validation error and every tool-gap warning apply unchanged.

| Argument | Default | Notes |
|---|---|---|
| `image` | — | Required. Must not start with `-`, be empty, or contain whitespace. |
| `pull` | `"never"` | `never`, `missing`, `always`. |
| `replace` | `false` | Overwriting an existing agent has to be asked for — the id is inside the image, which the caller has not read yet. Its stored config is kept. |
| `validate_only` | `false` | Reports what *would* land. See below — it is not a dry run of the whole thing. |

**`validate_only` skips only the write.** It still applies the pull policy —
under `missing` or `always` it will download the image — and still runs the
`create` → `cp` → `rm` read to get the manifest out. What it skips is inserting
or updating the row and recording the provenance. If you wanted "tell me about
this image without touching the network", pass `pull: "never"` as well (the
default), which reports `image_absent_pull_never` instead of fetching.

The report adds `image`, `digest`, `pulled`, `manifest_path` and a `message`.
**`pulled` means two slightly different things**: under `missing` it is true
when the image was absent and was therefore fetched; under `always` it is true
only when the digest actually moved (or the image was not there before), because
`always` on an unchanged image copies nothing and reporting a download for it
would be lmgw inventing an event. Under `never` it is always false.

Package-specific error codes: `image_ref_invalid`, `image_absent_pull_never`,
`image_pull_failed`, `podman_unavailable`, `package_create_failed`,
`package_no_manifest` (`the image '…' carries no /lmgw/agent.json`),
`package_read_failed`.

**Any** import refusal — an id collision, a validation error, anything — gets
this sentence appended once the image was in fact downloaded during this call:
*"The image '…' was downloaded before this was discovered and is on the box now,
so retrying with replace costs nothing."* It is not specific to the collision
case; it is there so several gigabytes never arrive unannounced.

### Pull policies

`never` is the default everywhere and it is what stops a **Start** or an install
from turning into a multi-gigabyte download nobody asked for. `missing` fetches
when the image is absent; `always` runs `podman pull` every time (and reports
`pulled` only if the digest actually moved). `podman pull` is **not** bounded by
a timeout lmgw invented — a pull takes as long as the image is big.

The one implicit fetch in the system is the **script image**, which runs with
`--pull=missing` because it is lmgw's own choice of runtime rather than an image
you built. It is visible: the policy is on the argv and the run log names the
image.

### Provenance, Pull image, Re-import

The row records where its document came from, in the `provenance` column, shown
on the Run tab's Runtime block:

| Field | Meaning |
|---|---|
| `image` | The reference **as you gave it**, tag and all — not the digest-pinned form. |
| `digest` | `podman image inspect --format '{{.Digest}}'` at the last read. Empty when podman could not say. |
| `manifest_path` | `/lmgw/agent.json`. |
| `installed_at` | RFC 3339, when this row was written from the image. |
| `pulled_at` | RFC 3339, when the digest was last read. |

**Pull image** (`agent_pull { id }`) fetches the image again and says whether the
digest moved — **pressing it is the consent**, so it pulls under every policy,
`never` included. When the digest moved it reads the manifest out of the new
image and compares it with the stored one, answering `changed`,
`manifest_differs` and a `note`. Nothing polls in the background and nothing is
adopted behind your back. A pull deliberately does **not** fill in
`manifest_path` or `installed_at`: a row with a digest and no `installed_at` is
an image that was pulled, not a package the row came from.

**Re-import from image** (`agent_reimport { id }`) adopts the new manifest and
**keeps the config** — the built-in upgrade's shape, for a package. A package
whose manifest carries a *different id* is refused naming both; install it as its
own agent instead.

The image both act on is the **manifest's `run.image`**, because that is what
every phase actually starts; `provenance.image` is the fallback only for a row
that has no `run.image` (a service agent served from a `dev_url`). When the two
disagree you get `install_image_mismatch` and **the manifest wins**. An install
never rewrites the document — doing so would mean the exported manifest no longer
matched the package it came from.

### Export and portability

`GET /api/agents/{id}/export` (add `?include_config=1` for the non-secret config
values) writes the canonical manifest plus an envelope:

| Envelope key | Contents |
|---|---|
| `exported_at` | RFC 3339. |
| `lmgw_version` | The exporting build. |
| `config_omitted` | The `secret` field names that were left out. |
| `config_unbound` | The mount field names that were left out — `["notes"]` — so the receiver knows there is a slot to bind, not a secret to fill in. |
| `config_values` | Only with `include_config=1`, and never a secret or a mount value. |
| `portability` | `{ portable, notes }`. |

All six are stripped on the way back in, so a downloaded file re-imports
unchanged.

**Never exported, with or without the flag**: the agent's **token** (a credential
of *this* gateway), the **provenance** (where *this* box got the package) and the
**`dev_url`** (a path on *this* desk). The export says an agent is being served
from a dev server without naming it; the dashboard, showing you your own row,
names it.

`portability` is said, never enforced. Two things make a file unportable:

- a **`localhost/…` image** — or one with no registry host at all. The test is
  docker's own registry-host rule with one explicit exception bolted on: a
  reference with no `/` is local; otherwise the first path segment is a registry
  host if it contains a `.` or a `:` — **and `localhost` is special-cased as
  local even though it is a legitimate host name**, which is the whole point,
  since it names *this* box. So `localhost/board:1`, `board:1` and
  `acme/board:1` are all local; `ghcr.io/acme/board:1` and
  `registry:5000/board:1` are not.
- a **`dev_url`**, because it is not in the file at all.

A container manifest with no `run.image` adds a third note: the receiver has
nothing to run it with.

---

## 9. Identity, cost and safety

<!-- source: crates/lmgw-core/src/agents/token.rs, crates/lmgw-core/src/mcp/ingress.rs, crates/lmgw-core/src/server.rs -->

### The agent token

**One token per agent, not per run.** It is `lmgw-agent-` followed by 32 random
bytes in hex, stored as a real `api_keys` row of kind `agent` named
`agent:<id>`, so it authenticates `/v1`, `/mcp` and the ledger whether or not
**Require API key** is on — which is the point: `auth_enabled` is off by default
and the agent's authority must not be.

It is minted **the first time something actually needs to hand it over**, and
never at import. Precisely, that is: the start of a container phase run, the
start of a **script** step, a service container start, or `agent_token_get` /
`agent_token_rotate` (*Copy token* / *Rotate*). It is **not** minted by a purely
in-process run — a `batch` agent whose steps are direct tool calls and model
turns, or a `chat` agent, never mints one however often you run it, because
nothing outside the process ever has to authenticate. Such an agent's Definition
tab shows `has_value: false` until you press Copy token.

### Scope

The scope is **derived, not configured**, and recomputed on every write that
could move it: a config save, a manifest replace through `agent_set` **or the
`/api/agents/import` endpoint** (they are one code path), a reset, an enable
toggle, a run start, and the **startup built-in upgrade** when it moves a
shipped manifest forward.

- Every `format: "model_alias"` config field contributes its **current value**.
- `model.alias` contributes itself when it is a literal (not a template).
- Values are trimmed and deduplicated.

If that set is empty — no model-alias field, or all of them blank, which is the
shipped mail labeler's state out of the box — the policy is **`all`** and the
Definition tab prints **"token: any model"**. A derived allow-list that silently
came out empty would refuse every call the agent makes.

Otherwise the policy is `allow` with those aliases, and the page prints
`token: <alias>, <alias>`. Asking for a different alias with that token is
refused by name.

On `/mcp` the token sees only the tools the manifest's `tools[]` resolve to
right now — each entry's `allowed`, or the whole label's current surface when
`allowed` is absent. The list is recomputed per call, not stored: a list is a
snapshot and a call is not. An agent whose row is gone or whose manifest this
build cannot read gets an **empty** list rather than an unfiltered one.

The `api_keys` row also has `budget_micro`, `budget_period`, `rpm_limit`,
`tpm_limit`, `concurrency_limit` and `expires_at` columns, and they **are**
enforced for any key that has them set — including an agent token, which
`server.rs` admits through the policy gate whether or not **require gateway API
keys** is on, precisely so its scope and budget are not decorative.

**Set them on Usage → Keys & policy**, the `agent:<id>` row's **Edit** button.
The dialog writes through `POST /api/op/key_set`, and it splits the row down
the line between what you own and what lmgw derives:

| field | on an agent token |
|---|---|
| budget, period, rpm, tpm, concurrency, expiry, note | yours — editable here, enforced on the next request |
| `scope_mode`, `scope_patterns` | **derived** from the manifest; greyed out here and refused by `key_set` |
| `enabled` | **mirrors the agent row**; greyed out here and refused by `key_set` |

The two derived fields are refused rather than written, because the next
`token::resync` — any agent save, run, import or reset — would take the value
back without telling anyone. Restating the value they already hold is *not*
treated as a change, so a dialog that posts the whole form on every save still
works. Change the model on the agent's page and the scope follows it; use
**Disable** there as the kill switch.

Before 2026-09-19 that dialog posted an op that did not exist (`unknown op
'key_set'`) and nothing was ever written, so an agent token on an older build
really is unbudgeted whatever the page implies.

### Disable, rotate, delete

- **Disable** (`agent_enable { enabled: false }`) disables the key row, so the
  token stops authenticating everywhere at once. On the **ledger routes**, which
  resolve the bearer themselves, the refusal names it:
  `agent '<id>' is disabled; its token is refused until it is enabled again`.
  Elsewhere the answer comes from whichever layer runs first — with **Require
  API key** on, the gateway's own auth middleware rejects the key before the
  agent-aware code sees it, so you get that layer's generic refusal rather than
  the sentence above. Same outcome, different wording. Disable also stops a
  running app container and makes `service::ensure` refuse to start one.
- **Rotate** replaces both the hash and the plaintext in one write. The previous
  token is dead immediately, so a running app container is holding something that
  can now only fail — rotation therefore **stops that container**, and the next
  request to its origin starts it again with the new token. Nothing after the
  write can turn a rotation into an error: a scope write that fails afterwards
  is reported as a `warning` beside the token, not as a refusal.
- **Delete** deletes the key, stops the container, drops the `agent:<id>` MCP row
  and unlinks (but keeps) the agent's chat threads.

### What the token does **not** confine

Be clear about this before you install an agent someone else wrote:

- **`/api` needs a principal, and an agent token is not an owner's.** A
  container calling `/api/op/*` with its agent token gets `403 forbidden`; with
  no token at all, `401 session_required`. **The trust boundary is the
  principal, not `bind_addr`**: `bind_addr` decides who can *reach* the
  gateway, the principal decides what they may do. What the container keeps:
  `/v1`, `/mcp`, the three ledger routes, and — new — reads of its own row and
  runs (`GET /api/agents/{id}`, `GET /api/agents/{id}/runs`, `GET
  /api/agents/runs/{job}`), rendered in the agent view (secrets masked,
  `dev_url` empty, a mount field's value its container path, never the host
  one). A container reading its own runs this way sees the container path in
  the run log's mount line too, and never a host path in a failed start's
  `error` — a mount refusal there reads `<field>: <code>` rather than the
  sentence an owner sees; the owner's own read of the same run keeps the host
  path in both places. What the token still *buys* there is the `/mcp` allow
  list, the model-alias scope, the policy columns (see above) and per-agent
  attribution in Logs. **This confines the container's process, and its page
  too.** The App tab serves the app at its own origin,
  `http://<id>.<suffix>:<port>/` (§7), dispatched on the `Host` header ahead of
  the main router. An agent's app JS is a **different site** from the
  dashboard: it holds none of the `lmgw_session` cookie, and it cannot reach
  the Tauri bridge (`window.__TAURI_INTERNALS__`), which is injected into the
  main frame only. The browser's own origin isolation does the rest — but a
  different site can still be navigated to or form-posted at; see the
  publicness paragraph under §7.
- **That confinement is advisory while *Require API key* (`auth_enabled`) is
  off**, which is the default. A container that simply omits its token
  resolves to the anonymous principal on `/v1` and `/mcp`, and anonymous holds
  `Inference` there — so the token's model scope, tool allow-list and budget
  bind only a container that actually presents it. A container agent's
  Definition tab names it: `token_scope_advisory` — *"**Require API key** is
  off, so this token's model scope, tool allow-list and budget bind only a
  container that presents it; switch it on under Settings → Network & access
  to make them binding"* — non-blocking, recomputed per page load. Switching **Require API
  key** on is the fix, and it is one checkbox.
- **Every cookie-authenticated request is Origin-checked**, not just the
  credential-handing ops. A request whose principal came from the session
  cookie has to carry this gateway's own `Origin` — or, where the browser sent
  none, a `Sec-Fetch-Site` of `same-origin` or `none` (a typed address, a
  bookmark, the lmgw window). Anything else is `403 cross_origin_refused`
  (principals §3.6). That covers the one case `SameSite=Strict` cannot see: a
  page on the *same site* but another port, `http://127.0.0.1:9999`, which any
  local process can serve. It is checked against the request's **own `Host`**,
  so a LAN-bound gateway opened from its LAN address passes. A caller
  presenting a **bearer** skips the rule entirely — curl, a container and a
  script send no `Origin`, and a bearer is not something a foreign page can
  make a browser attach. So the real asymmetry is between **same-origin code
  (the dashboard) and everything else** — a foreign web page, and now an
  agent's own UI too, since it has a host name of its own (§7). An agent's app
  JS holds none of the `lmgw_session` cookie and cannot reach the Tauri bridge;
  it is exactly as trusted as any other site a browser might open, which is
  the point of giving it an origin.
- **A script holds the agent's full authority.** The shim strips nothing that
  matters; `/lmgw/secrets.json` is readable from inside.
- **`lmgw__*` is not reachable with an agent token.** The self-admin plane is
  `/mcp/admin` behind the `owner:self-admin` key on Usage → Keys — disabled
  until you enable it there — and an agent token buys nothing there.
  Presenting one on that plane behaves exactly as presenting none.

### Secrets and redaction

- A secret **never travels in the environment, on the command line, or in a
  label** — `podman inspect` prints all three. It travels in
  `/lmgw/secrets.json`: mode `0600`, in a `0700` directory, mounted `ro,Z`,
  deleted when the run (or the service) ends. That directory is a host **tmpfs**
  — *except* when `XDG_RUNTIME_DIR` is unset or empty, when it falls back to
  `<data_dir>/agents/<slug(container_prefix)>/`, which is **persistent disk**.
  That is the `secrets_dir_fallback` warning, and it is a warning precisely
  because the file is no longer guaranteed to die with the session. It is still
  `0700`/`0600` and still deleted at the end of the run, and the boot sweep
  collects any that a crash left behind — but a token has touched a disk.
- A secret config field is refused in any model prompt at validation time, and
  is stripped from `input.json`, from every API read, from exports and from
  duplicates.
- **The token is redacted out of anything lmgw republishes**: every run-log line
  (including container stderr verbatim), the stored job `result`, the Run tab,
  the App tab's log tail, `lmgw__agent_get`, and the body of a failed start's
  503. It is a substring replace producing `<agent token>`. It does **not** cover
  a container that prints the token in pieces, base64s it, or sends it somewhere
  else — nothing inside the container is lmgw's to police. What it covers is
  every path by which lmgw itself would republish it.

---

## 10. Built-ins and upgrades

<!-- source: crates/lmgw-core/src/agents/seed.rs -->

Built-in manifests ship embedded in the binary (read off disk in a debug build,
so editing one is a restart rather than a rebuild). Two ship today:
`mail-labeler` (batch) and `docs-librarian` (chat).

A built-in id is inserted **once**, tracked in the KV key `agents:seeded` as
`{id: sha256(the manifest text at the moment it was seeded)}`. **A built-in you
delete stays deleted** — an agent that comes back from the dead every morning is
a bug. *Restore shipped agents* (`agents_restore`) re-inserts the missing ones
deliberately, and it is the only path that ignores the seeded set.

On every start, for each shipped manifest:

| Recorded hash | Meaning | What happens |
|---|---|---|
| equals `sha256` of the stored manifest | never edited since seeding | **replaced** with the shipped text, hash updated, **config kept** — or skipped when the stored text already *is* what ships |
| differs | you edited it | left exactly as it is; the card shows `builtin_update_available` beside **Reset to shipped** |
| absent (an install that predates hashes) | unknowable | left alone. If the stored text already equals the shipped text the hash is recorded; otherwise the entry becomes the sentinel `legacy`, which no real hash can equal, and the row stays yours until you press Reset |
| the row is gone | deliberately deleted | still skipped |
| the row is `authored`/`imported` under a shipped id | yours | skipped; the id counts as seeded so this does not re-run every start |

**Reset to shipped keys on the id, not on `source`.** It is offered whenever
`seed::shipped(<id>)` finds an embedded manifest for that id — so it is
available for an *authored* or *imported* agent that happens to occupy a shipped
id too, and pressing it will replace that document with the shipped one. What
`source = 'builtin'` gates is the other half: the automatic upgrade pass and the
`builtin_update_available` notice, both of which skip a row that is not a
built-in. Editing a built-in keeps `source = 'builtin'`, so an edited one stays
inside that rule.

Reset writes the row and the hash **in one transaction**, so the row goes
straight back under the upgrade rule — and it **inserts when the row is gone**,
which makes it the second way back for a deleted built-in, beside *Restore
shipped agents*.

**Config is kept across every replace** — an upgrade, a reset, an import with
`replace=1`, a re-import — *except* for values whose field the new manifest no
longer declares. Those go with the manifest that declared them, in the same
transaction, and are **named** in the report (`dropped_config`), in the warning
and in the startup log. Keeping them would produce a row that fails validation on
every run and every config save — unusable *and* unfixable from the form.

### Forking a built-in

Reset is the way back to the shipped text; forking is the way to keep your
changes under the shipped agent's nose:

```sh
BASE=http://127.0.0.1:8787
AUTH='Authorization: Bearer <owner key>'
curl -sS -H "$AUTH" "$BASE/api/agents/mail-labeler/export" -o my-labeler.agent.json
# change "id" (and usually "name") — the id is the catalog key
sed -i 's/"id": "mail-labeler"/"id": "my-labeler"/' my-labeler.agent.json
curl -sS -X POST -H "$AUTH" "$BASE/api/agents/import" --data-binary @my-labeler.agent.json
```

The fork is `source = imported`, so it is never touched by the built-in upgrade
pass, and the original keeps following the shipped text.

`agent_duplicate { id, new_id, name? }` is the one-call version: same manifest
under the new id, `name` defaulting to `"<name> (copy)"`, `source = authored`,
and the config copied **minus every secret** (the response's `config_omitted`
names them). A copy must not silently inherit a credential.

---

## 11. Checklist and troubleshooting

### Pre-flight: a `chat` agent

- [ ] `run.system` templates resolve against `config`, `agent` and `run` only.
- [ ] No `secret` config field appears in the prompt (it is refused).
- [ ] `model.alias` renders to something the gateway can resolve; every
      `required` config field has a value or a default, or the open is refused
      naming the field.
- [ ] Every `tools[]` label is registered, or you accept that the thread opens
      with fewer tools than you meant.
- [ ] You know `top_p`, `top_k`, `seed` and `reasoning` are dropped — a thread
      has a column for `temperature` only.

### Pre-flight: a `batch` agent

- [ ] The source step yields an **array**; `items_path` is a JSON pointer
      starting with `/` if the array is nested.
- [ ] `run.item.id` renders non-empty for every item, or those rows error by
      design.
- [ ] `run.item.user` and `run.item.output` are both present, or both absent.
- [ ] `review.editable` names only fields the item output produces.
- [ ] The apply step is a direct call or a `script` — **never a `turn`**.
- [ ] A `script` apply declares `output`, or you accept
      `script_without_output`.
- [ ] `tools[].allowed` names exactly the tools the steps call, and nothing
      else.
- [ ] `concurrency` matches what your model server can actually take in
      parallel.
- [ ] Rehearse: **List only** (no tokens) → **classify** (no writes) → apply two
      checked rows → verify at the destination.

### Pre-flight: a `container` agent

- [ ] `podman --version` works for the user lmgw runs as.
- [ ] The image is on the box, or `run.pull` says `missing`/`always`.
- [ ] **No `{{…}}` in `run.image`, `run.entrypoint` or `run.args`** — they are
      not templated and nothing will tell you.
- [ ] `model.alias` is chosen with the token scope in mind: a literal alias is
      the token's whole allow list.
- [ ] `run.phases` matches what the image actually implements; `review` only
      appears with `apply` among them.
- [ ] `run.columns` lists every column you emit, in the order you want them.
- [ ] `run.output.<phase>` matches what you actually emit — exit `0` with no
      `output` fails the job when a schema is declared.
- [ ] The image writes only to `/tmp` (or `read_only: false` is deliberate).
- [ ] The image reads `LMGW_INPUT`/`LMGW_SECRETS` from the environment rather
      than hardcoding paths.
- [ ] It handles SIGTERM within `stop_grace_seconds`.
- [ ] It sends `X-Lmgw-Run: $LMGW_RUN` on every `/v1` and `/mcp` call.
- [ ] For service mode: it listens on `$LMGW_PORT`, answers `health_path`, and
      builds absolute URLs from `LMGW_APP_ORIGIN`. Remember that with a
      loopback-bound gateway, pasta's `-T,<gateway port>` occupies **that** port
      number on the container's own loopback.
- [ ] For a package: `/lmgw/agent.json` is in the image and names the same
      image in `run.image`.
- [ ] Mount fields are bound on the Run tab, not left to a default — a
      manifest cannot set one. Binding relabels the folder for containers,
      recursively and permanently. The container runs as the owner's uid
      inside (`--userns=keep-id`), not root.

### Common failures

| What you see | What it means | What to do |
|---|---|---|
| `manifest schema_version 2 is not supported; this build understands version 1` | The document is from a newer build. | Downgrade the document, or upgrade lmgw. |
| `manifest: unknown field 'retries', expected one of …` | A typo, or a field that does not exist. | Fix the key; unknown fields are refused everywhere. |
| `id 'Mail' must start with a lowercase letter or a digit` / `contains '_'` | The id pattern is `[a-z0-9][a-z0-9-]{0,63}`. | Rename. |
| `id 'runs' is reserved by the /api/agents routes; pick another` | `runs`, `import`, `new`. | Rename. |
| `config.schema.properties.notes: unknown format 'email' (secret, model_alias, multiline)` | Outside the config subset. | Use one of the three, or drop `format`. |
| `config.schema.properties.tags: an array field needs items {"type": "string"}` | Arrays are of string only. | Add `items`. |
| `run.item.user: '{{config.nope}}' names no config field (fields: model, categories, …)` | Template typo. | Use a declared field. |
| `run.source: '{{rows}}' is not available here (available: config, agent, run)` | Wrong root for that position. | See the roots table in §2. |
| `run.item.user: '{{config.api_token}}' puts the secret field 'api_token' into a model prompt…` | Secrets are refused in prompts. | Pass it as a tool argument instead. |
| `run.apply: a step is exactly one of 'tool' (a direct call), 'turn' or 'script'; this one declares tool and script` | Two shapes in one step. | Pick one. |
| `run.limits.memory_mb is -1; a limit is a whole number, 0 (no limit) or above` | A negative bound. | Use `0` for "no limit". |
| `run.image is required for a container agent…` | No image and no `service`. | Set `run.image`, or add a `service` block and a `dev_url`. |
| `run.review is set but run.phases has no 'apply'` | A review gate in front of a phase that does not exist. | Add `"apply"` to `phases`, or drop `review`. |
| `run.output names the phase 'classify', which run.phases does not declare (run, apply)` | `output` is keyed per phase. | Use `run` and/or `apply`. |
| `run.provides.mcp needs run.service` | The MCP route proxies the same container the App tab does. | Add `service`. |
| `tools[0].allowed[0] is 'board__pin', one of this agent's own tools…` | The agent would call itself through the gateway. | Remove it from the allow list. |
| `no MCP server with label 'gws' is registered on this gateway (available: lmgw, docs, …)` | A tool gap — a warning, not a refusal. | Register the server; Start is disabled until you do. |
| `the image 'localhost/board:1' is not on this box and run.pull is 'never', so Start would fail rather than download it` | `image_absent_pull_never`. | `podman build`/`podman pull` it, or change `run.pull`. |
| `podman is required for container agents and could not be run: …` | `podman_unavailable`. | Fix podman for lmgw's user; for a scripted batch agent, list and classify still work. |
| `the container exited successfully but emitted no output; run.output.run declares a schema` | Exit 0 with no `output` event. | Emit one, or remove the schema. |
| `the run's output does not match run.output.apply: 'applied' is missing` | The output failed the structural check. | Match the declared shape. |
| `the run exceeded its deadline of 600s (run.limits.deadline_seconds)` | Wall clock, from the container's start. | Raise it, or `0` for none. |
| `rejected a row event with no id: {…}` (in the run log) | A `row` without a stable identity — including one whose `id` is a **number** rather than a string. | Always send `id`, as a non-empty JSON string. |
| `Error: no such image localhost/{{config.tag}}:1` from podman at Start | `run.image` is not templated; the placeholder went to podman literally. | Put the real reference in `run.image`; configure the container through `/lmgw/input.json`. |
| A row is always in *needs attention* and re-run never touches it | Its `run.item.id` renders empty, so it is attention with nothing to address. | Fix the `id` template. |
| `ignored a ledger event of unknown type 'result' (known: row, log, progress, output)` | A typo in `type`. | Use one of the four. |
| `row column 'spam' is not one the manifest declares; it is kept and shown after the declared columns` | An undeclared column — kept, never dropped. | Add it to `run.columns` for a stable order. |
| `no_close: the run reached its deadline of 600s without a close (POST /api/agents/runs/412/close)` | A ledger run whose opener disappeared. | Close it, or raise the deadline. |
| `409 run_not_ledger: run #412 is in flight but lmgw is driving it, so it takes no ledger events` | You posted events to a run lmgw started. | A container lmgw started reports on stdout, not to the ledger. |
| `403 run_not_owned: that run belongs to a different agent than the token presented` | Wrong token, or a run id that is not yours. | Ownership is checked before existence; it is not an enumeration oracle. |
| `401 session_required` | A dashboard-plane call (`/api`) arrived with no principal — no session cookie, no owner bearer. | Open the login link printed in the process log, or send `-H 'Authorization: Bearer <owner key>'` (Usage → Keys). |
| `403 forbidden` | The principal presented does not hold the route's capability — an agent token on an `Admin` route, for instance. The body names both the capability and what was presented. | Use the credential the route actually needs; an agent token is not an owner key. |
| `403 cross_origin_refused` | A **cookie**-authenticated request arrived from another origin — the rule is general to every such request, not only the token ops (§3.6). Including a page on this host at another port, which `SameSite=Strict` does not catch. | Ask from the dashboard's own origin, or from a client that presents a bearer and sends no `Origin`. |
| `503 agent_service_starting: the app for 'board' could not be started: the container did not answer http://127.0.0.1:41234/healthz within run.service.start_timeout_seconds (30s)` | The health probe never passed; this detail is what the App tab and the `agent_service_start` op show — a request on the public agent origin itself gets the generic form of this 503, with no reason and no path. | Read the `log` on the App tab; raise `start_timeout_seconds`, or `0` to wait as long as it takes. |
| `502 agent_service_unreachable … It was started and passed its health probe, so it has stopped answering since.` | The container died after coming up. | The entry is evicted; the next request starts a fresh one. Check `agent_service_log`. |
| `a dev_url has to be on this box: 'dev.lan' is not loopback …` | The app proxy is unauthenticated. | Use a loopback address. |
| `origin_label_invalid` | The agent id is not a legal DNS label — over 63 characters, or ending in `-`. | Rename the id; the App tab needs it as a host-name label. |
| `origin_shadows_gateway` | `<id>.<suffix>` equals a name the gateway itself answers on. | Rename the id, or change the agent origin suffix under Settings. |
| `origin_suffix_shadows_gateway` | The agent origin suffix set under Settings → Agents & tools is itself a name the gateway answers on, or shares a parent domain of two or more labels with one. | Pick a suffix that does not capture the gateway's own address. |
| `404 agent_app_moved` | An old `/agents/<id>/app/` bookmark; that path is gone (§7). | The body names the origin (`{ "origin": "…" }`) — go there instead. |
| `<id>.localhost` does not resolve | Nothing answers that host name on this machine. | Add `127.0.0.1 <id>.localhost` to `/etc/hosts`, or set an agent origin suffix under Settings → Agents & tools that your DNS answers for. |
| `the image 'localhost/board:1' carries no /lmgw/agent.json` (`package_no_manifest`) | Not an agent package. | `COPY agent.json /lmgw/agent.json`. |
| `an agent with id 'board' already exists; pass replace=1 to overwrite it (its stored config is kept)` | Id collision on import. | `replace=1`, or change the id to fork. |
| `the stored config values x, y are not declared by the new manifest and were removed with the old one` | A replace pruned orphaned config. | Expected; the report names them. |
| `agent '<id>' has an unreadable manifest: …` (`manifest_unreadable`) | The row was written by a newer build, or hand-edited badly. | The detail page still opens; fix it in the Definition editor. |
| `mount_path_refused` | A stored or submitted path fails a path rule (§2) — not absolute, does not exist, wrong kind, or a refused location. | Pick a different path; the message names the rule. |
| `mount_path_nested` | The path lies inside, or contains, another bound mount where either side is `rw`; the message names the other agent, field and access. | Choose a path outside that tree — or bind the *same* path, which is allowed whatever the access. |
| `mount_path_missing` | A stored path no longer exists at start. | Recreate it, or bind a different path. |
| `mount_unbound` | A `required` mount field has no value. | Bind it on the Run tab. |
| `EACCES` writing to `/lmgw/mounts/<field>` | The image needs to run as root at runtime; `--userns=keep-id` maps root-in-container to the owner's uid, which an image expecting to run privileged cannot write as. | Rebuild the image to run as a normal user, or as the owner's uid. |

---

## Design specs

For the reasoning behind all of the above:

- [`docs/design/2026-09-18-agent-catalog-design.md`](design/2026-09-18-agent-catalog-design.md)
  — the manifest, the config subset, the templating language, the batch
  pipeline, the review gate, import/export.
- [`docs/design/2026-09-19-agent-container-runtime-design.md`](design/2026-09-19-agent-container-runtime-design.md)
  — the agent token, the run ledger, the container runtime, script steps,
  service mode, the app proxy and the package format.

Where a spec and the code disagree, the code wins and this document follows the
code.
