# The lab store and the Decision lab

The owner asked on 2026-10-16 for a dashboard surface for decision models (`POST /v1/systemone`,
aux kind `decision`, prefix `decide/`; decision-models design 2026-10-12), and approved an
outline the same day. The dashboard's labs exist first to test models and routes; productive use
is a plus. The same day the owner decided that all three labs (Audio, Image, Decision) get
history, storage and export through **one** lab store, designed once. The Decision lab is its
first user because its runs are small JSON. The Audio and Image labs move onto it later, and
that is where image and audio files come in.

This document designs the store for all three labs and the Decision lab on top of it, in four
slices. Slice 1 (the store core and the Decision lab) is specified fully enough to build. Slices
2 to 4 are thinner, and slice 1's schema leaves room for them. The owner took §8's four
recommendations on 2026-10-16 (Decisions 31 to 34) and left the drafts 13 to 30 standing.

**Today** (lines at `main` 2864c312):
- **The labs keep nothing.** The Audio lab's module doc lists "any persistence of the history or
  of presets" as deliberately absent (`lmgw-ui/src/pages/audio_lab.rs:17-20`). The shared
  layout's results head says "newest first · this session only", and its empty text says nothing
  is kept on the server (`pages/lab_frame.rs:415`, `:433-434`). The Audio lab's voice library
  (`/audio-lab/api/refs`) is reference input stored in a models dir, not history.
- **The lab servers dispatch in process** through the real `/v1` handlers. The Image lab calls
  `proxy::handle_image_generation(state, RequestCtx::default(), body)` (`web/image_lab.rs:184-205`,
  `proxy/image.rs:267`). The hold, admission, fallbacks and the log row are real; only the auth
  middleware and the HTTP hop are skipped (`web/image_lab.rs:6-13`). A success is wrapped with
  the request, latency and the `x-lmgw-*` headers. A failure is returned unwrapped, as the
  handler produced it (`web/image_lab.rs:135-168`).
- **`/v1/systemone` has no in-process entry point.** Its handler is the axum function
  `handle(State, Extension<RequestCtx>, Result<Bytes, BytesRejection>)` (`proxy/systemone.rs:64-68`).
  A direct call works, but there is no `handle_*` wrapper like the image route's.
  `SystemOneBody::with_model` rewrites only `model` and keeps every other member's place and
  bytes (`egress/openai_wire/systemone.rs:52`).
- **Order is part of the wire, and `serde_json::Map` sorts.** The map is a `BTreeMap` in this
  build (`lmgw-core/Cargo.toml:197-200`), so the protocol types use `Ordered<V>`
  (`lmgw-api-types/src/systemone.rs:28-34`). But `SystemOneRequest::state` and each question's
  `instructions` are `Value` (`:144`, `:176`): a JSON-object state read into one sorts its
  members. The Image lab's result carries body and answer as `Value`
  (`lmgw-api-types/src/image_lab.rs:327-340`), which for a decision request would sort the
  options.
- **Only the Audio lab has a request override**, JSON merged into the body last through a
  `Value` (`pages/audio_lab.rs:1763-1772`, `pages/audio_spec.rs:575-584`). The Image lab's
  Request panel is read-only (`pages/image_lab.rs:848`).
- **Lab routes are `Cap::Admin`**, in `labs_routes()` (`web/mod.rs:348-411`, merged at
  `:177-179`), each with a `CAPABILITY_TABLE` row (`server.rs:504-519`), their prefixes skipped
  by the SPA's catch-all (`web/ui.rs:28-34`). Since 2026-10-10 the lab APIs are **documented and
  typed** in the admin OpenAPI document (`openapi/planes/audio_lab.rs`, `planes/image_lab.rs`,
  registered at `openapi/registry.rs:227-228`); `openapi/exclusions.rs` keeps only the SPA, the
  `/ui` redirects and the agent proxies (`:16-37`).
- **Token counts.** `/v1/count_tokens` takes `{model, input}` (`proxy/count.rs:170-209`);
  `count_texts` counts several texts on one resolved route (`:235`). A decision row counts like
  any llama-server (`gate/open.rs:66-71`). A cheap count writes no log row; one that failed or
  brought its model up writes one (`proxy/count_admit.rs:21-23`).
- **Header facts.** `modelinfo::decision::Shape` gives the three header shapes (non-causal,
  causal with a head, label logits; `modelinfo/decision.rs:26-52`). The per-type option limits
  (52, 255) are deliberately not coded: they live in llama.cpp's source, not the header
  (`modelinfo/decision.rs:6-10`, `capabilities/notes.rs:426-430`). A running container's `/props`
  gives `n_ctx_slot` and `vision` (`egress/llama_cpp/props.rs:43-58`). The engine defaults are
  `LLAMA_DEFAULT_UBATCH` 512 and `LLAMA_DEFAULT_BATCH` 2048 (`runtime/aux.rs:103`, `:145`).
  Headers are read through `gguf_cache.summarize_cached` (`modelinfo/licence.rs:33-37`).
- **Persistence.** Chat keeps attachments as BLOBs in `lmgw.sqlite`
  (`migrations/0038_chat_archive_attachments.sql:22-33`); knowledge-base originals are files
  under the data dir (`knowledge/originals.rs:29-35`). Every write transaction begins with
  `begin_write` (`BEGIN IMMEDIATE`), and nothing slow goes inside it (`store.rs:81-111`).
  Migrations are embedded (`store.rs:165`); the last is `0082_key_device_tools.sql`.
- **Jobs.** A job is a `jobs` row (`migrations/0019_jobs.sql:12-36`) plus a registered executor
  (`jobs/mod.rs:49-79`, `:322-335`, `:358-371`). A boot marks running jobs failed and resumes
  none (`state.rs:502`). Finished rows are pruned after `jobs_retention_days` (14) or past
  `jobs_max_rows` (500) (`config/settings.rs:104-128`, `server.rs:892-904`). A batch agent run
  keeps its rows in the job's `result` (`agents/batch.rs:9-16`), so they go with it.
- **Batch agents.** Classify is one LLM call per item with the output schema as
  `response_format` (`agents/manifest.rs:590-661`). A row needs attention when its output equals
  the fallback or the call failed (`agents/batch.rs:1697`, `:1704`). The mail labeler classifies
  into `config.categories` with fallback `Other` (`builtin-agents/mail-labeler.agent.json:110-114`).
- **Export.** A chat export is a GET answered with `Content-Disposition: attachment`, started by
  the shared `<a download>` click (`pages/docs.rs:825-842`, `pages/chat_export.rs:1-8`). In the
  Tauri window `on_download` saves it to Downloads and tells the page
  (`src-tauri/src/main.rs:150-155`). Zips are written in `web/chat_export.rs:332-371`; CSV fields
  are quoted by `csv_field` (`web/api_usage.rs:1068`).
- **Size.** The UI's `image_lab.rs` is 1,930 lines and `audio_lab.rs` 2,596. The json ratchet
  allows `web = 27` uses of `json!` and may only fall (`ci/json-ratchet.toml`).

## Decisions

*Owner, 2026-10-16* (the approved outline):
1. **A Decision lab**, nav "Decision lab" after the Image lab, on the labs' shared layout. Its
   server `/decision-lab/api/*` dispatches in process through the real `/v1/systemone` handler,
   so hold, admission, fallbacks and the `systemone` log row are real. No new public route:
   `/v1/systemone` stays the one decision mechanism (decision-models Decision 12).
2. **The model picker** offers local `decide/*` rows and cloud aliases of task `decision`. The
   head line shows what produces the visible errors: decision type, ctx and ubatch, the option
   limit (see §2.2 and Q2), image support, CPU or GPU, container state.
3. **The state editor** has Text and JSON modes, and image attachments only for models that read
   images.
4. **A live token gauge** through the `/v1/count_tokens` mechanism against the real limit, to
   prevent the 500 over the ubatch. The counters stay open on decision rows (decision-models
   Decision 16).
5. **A question builder** for `choice`, `score` and `noul` (ids, reorder, remove), and a
   **request panel** showing the exact body, built by one shared function in `lmgw-api-types`
   (as `ImageGenForm::generation_body` is), with an editable override.
6. **Answer views:** probability bars (`choice`), a level strip with the expected-level marker
   (`score`), a P(yes) meter (`noul`); confidence, latency, `input_tokens`, raw JSON and
   `x-lmgw-fallback`.
7. **Compare** ("Ask on…" several models, one grouped card) and **Perturb** (re-runs with option
   descriptions dropped and with option order reversed).
8. **Built-in examples:** ticket triage, sentiment, "is the user done talking?".
9. **Question sets and labelled set runs** (slice 2) as durable jobs, with accuracy, confusion
   matrix, reliability diagram and a threshold slider (coverage against accuracy at the cut);
   without labels, bulk classification; export as CSV or JSONL.
10. **Batch agents can classify through a question set** (slice 3); "needs attention" becomes
    "below the set's measured threshold"; the mail labeler first; reviewed rows of past agent
    runs export as a labelled set.
11. **Out of scope:** saved sets as MCP tools, which is close to the lmgw-own classify API
    Decision 12 rules out. Revisit after slice 3.
12. **All labs get history, storage and export through one shared lab store.** The Decision lab
    first (slice 1); the Audio and Image labs in slice 4, their artifacts as files.

*Taken in the draft:*

13. **What is kept:** one click is a **group**, each dispatch a **run**, media bytes are
    **artifacts** (files), reusable form parts are **presets**, a set run is a **batch** (§1.1).
14. **Exact bytes.** A run keeps the request text it sent and the response text it got, never
    re-serialized through a `Value`. The shared builder writes the JSON text itself, so a
    JSON-mode state goes out as typed. Raw views pretty-print without reordering (§1.3).
15. **Shared `/labs/api/{lab}/*` routes for the store, per-lab routes for dispatch** (§1.6).
16. **The new routes are documented and typed** in the admin OpenAPI document, each with a
    `CAPABILITY_TABLE` row and none excluded: the 2026-10-10 rule, not the earlier "drop
    everything internal" one (§1.8).
17. **Artifacts are content-addressed files** under `<data_dir>/labs/artifacts/`, written before
    the write transaction and unlinked after it commits; orphans are counted and removable. The
    mechanism is built in slice 1, because a decision request can carry images (§1.4).
18. **No retention rule.** Everything is kept, the count and size are shown, a group can be
    deleted, Clear empties a lab after a confirm (§1.5, Q1).
19. **A group's cells run one at a time, driven by the page**: the group and its plan are stored
    first, then each cell is one request. Each latency is the cell's own, cells appear as they
    land, a stop leaves a partial group that says so (§2.6).
20. **A dispatched cell answers 200 with the stored run**, the upstream's status and body inside
    it verbatim. Only the lab's own failures keep their status. Unlike the Image lab's unwrapped
    failures, because here a failure is part of the record (§2.6).
21. **The override replaces the body**, with only `model` rewritten per compared model. The form
    enforces the documented wire rules; the override can send anything. Perturb is off, with a
    reason, while an override is in effect (§2.4).
22. **Perturbations:** "descriptions dropped" nulls `choice` descriptions and drops `noul`
    criteria; "order reversed" reverses `choice` options. Neither touches `score`, whose levels
    are its descriptions and whose order is the scale (§2.6).
23. **The gauge** counts the state and each question through `count_texts`, compares against
    the limit that applies to the model's header shape, says it is approximate (template not
    counted, images omitted), counts by itself only when the model is loaded or in the cloud,
    and warns without blocking (§2.5).
24. **No per-type option-limit table.** Options are counted against the wire's 255, and
    llama.cpp's 400 names a type's lower limit (§2.2, Q2).
25. **Model facts come from the server** (`/decision-lab/api/models`); the limits are computed in
    a new `modelinfo/decision/limits.rs` beside `Shape` (§2.2).
26. **A `noul` answer's certainty is max(p, 1−p)**, the lab's own measure, labelled as such (the
    wire gives `noul` no confidence). Slice 2's thresholds and reliability use it.
27. **A set run's items and answers live in the lab store; the job only executes**, because job
    rows are pruned. An interrupted set run can be resumed. Concurrency is a visible field,
    default 1 (§3).
28. **CSV is read with the `csv` crate**, a new dependency (§3).
29. **No import, and no op or MCP tool for the store.** Export is a file for the owner.
30. **The mail labeler through a set:** the set's `choice` options are the categories, the state
    is the rendered `user` template, the system prompt is not sent (§4, Q3).

*Owner, 2026-10-16* (§8, approved):

31. **Keep everything** through slice 3, with the size always in view, delete per group and Clear
    per lab; revisit before slice 4 with sizes measured on a dev copy (Q1).
32. **No per-type option limit:** options are counted against the wire's 255, and llama.cpp's 400
    naming a type's own limit is relayed into the cell verbatim; no table of llama.cpp's types (Q2).
33. **Slice 3's categories come from the set's `choice` options** when a set is chosen;
    `config.categories` is not read, and the config form says so (Q3).
34. **Slice 3's labels are the rows the apply job received:** the checked rows with the owner's
    edits; unchecked rows are not labels (Q4).

## 1. The lab store

### 1.1 What is kept

- **Group:** what one click produced: the form as the lab held it, the plan of cells (model ×
  variant), the override if one was in effect, a title. A group with fewer runs than cells was
  stopped, and says so. Slice 2 adds a set-run item's batch, index and expected answers.
- **Run:** one dispatch to one `/v1` route: the route; the model asked and the one that answered
  (`x-lmgw-fallback`, else the one asked) with the fallback reason; the upstream's own `model`;
  the request text as sent and the response text as received, with its content type; the HTTP
  status, and the envelope's code and message on a failure; the `x-lmgw-*` headers; the input
  and output tokens when reported; the latency in the gateway; when it ran.
- **Artifact:** media bytes: an image sent to a decision model; later a generated image, a speech
  clip, an uploaded recording. `input` artifacts belong to the group and are shared by its
  cells; `output` artifacts belong to a run.
- **Preset** (slice 2): a named form part: a question set; later an image prompt, audio settings.
- **Batch** (slice 2): a set run: a preset snapshot, a model, the items and labels, its job.

The request log stays what it is. A run row is not a second request log: it keeps what the log
deliberately does not, bodies, answers and media.

### 1.2 Tables (migration `0083_lab_store.sql`)

0083 is the next number at 2864c312; the build takes whatever is next at merge.

```sql
CREATE TABLE lab_groups (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    lab         TEXT NOT NULL CHECK (lab IN ('decision', 'image', 'audio')),
    title       TEXT NOT NULL,            -- derived once from the form (the state's first line, a prompt)
    form        TEXT NOT NULL,            -- the lab's form as JSON (DecisionForm, …); media as lab-artifact refs
    plan        TEXT NOT NULL,            -- JSON [{model, variant}], the cells in run order
    override    TEXT,                     -- the request override, as typed, when one was in effect
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX idx_lab_groups_lab ON lab_groups (lab, id DESC);

CREATE TABLE lab_runs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id        INTEGER NOT NULL REFERENCES lab_groups(id) ON DELETE CASCADE,
    cell            INTEGER NOT NULL,     -- index into the group's plan
    variant         TEXT NOT NULL,        -- 'as_written', 'no_descriptions', 'reversed'; labs add theirs
    endpoint        TEXT NOT NULL,        -- '/v1/systemone'
    model_asked     TEXT NOT NULL,
    answered_by     TEXT NOT NULL,        -- x-lmgw-fallback, else model_asked
    fallback_reason TEXT,                 -- x-lmgw-fallback-reason
    upstream_model  TEXT,                 -- the answer's own `model`
    http_status     INTEGER NOT NULL,
    error_code      TEXT,                 -- lmgw_code, else the envelope's code or type
    error_message   TEXT,
    request         TEXT NOT NULL,        -- the bytes sent, as text; data URLs as lab-artifact refs
    response        TEXT,                 -- the bytes received when text; NULL when an artifact
    response_type   TEXT,
    headers         TEXT NOT NULL DEFAULT '{}',  -- the x-lmgw-* headers
    input_tokens    INTEGER,
    output_tokens   INTEGER,
    latency_ms      INTEGER NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (group_id, cell)
);

CREATE TABLE lab_artifacts (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id    INTEGER NOT NULL REFERENCES lab_groups(id) ON DELETE CASCADE,
    run_id      INTEGER REFERENCES lab_runs(id) ON DELETE CASCADE,  -- NULL for a group's input
    role        TEXT NOT NULL CHECK (role IN ('input', 'output')),
    ord         INTEGER NOT NULL,
    name        TEXT NOT NULL,            -- the upload's name, or one lmgw gives an output
    mime        TEXT NOT NULL,            -- sniffed from the bytes, as chat attachments are
    size        INTEGER NOT NULL,
    sha256      TEXT NOT NULL,            -- the file is <data_dir>/labs/artifacts/<sha256>
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX idx_lab_artifacts_group ON lab_artifacts (group_id);
CREATE INDEX idx_lab_artifacts_sha ON lab_artifacts (sha256);
```

The `lab` CHECK names all three labs now, so slice 4 needs no table rebuild. Slice 2's migration
adds `lab_presets`, `lab_batches` and three nullable `lab_groups` columns (§3.1); slice 1 depends
on none of them.

*As built (WP1, WP2):* `lab_artifacts` also has `data_url_head TEXT`, an input's data URL head as
typed (§1.3), and `idx_lab_artifacts_run`, for the cascade from a deleted run to its outputs.
`LabArtifactView` carries it as `data_url_head`. `error_code` is `lmgw_code`, else a string
`code`, else `type`, else a numeric `code`: llama.cpp's envelope carries `"code": 500, "type":
"server_error"`, and the number only repeats `http_status`.

### 1.3 Exact bytes

The Decision lab sends the bytes its preview shows, and keeps them:
- The shared builder (§2.4) writes the JSON text from the typed form: ids, options, levels and
  questions in the order entered; a JSON-mode state checked to parse
  (`serde_json::from_str::<IgnoredAny>`) and inserted verbatim; a Text-mode state as a JSON
  string. No `Value` and no `SystemOneRequest` sits between the form and the bytes.
- The stored `request` is those bytes, except that each image data URL becomes
  `lab-artifact:<id>`, a string where a string was (§1.4). The stored `response` is the body as
  received. An override is stored as typed; its per-cell body is `SystemOneBody::with_model`
  over it, the handler's own rewrite.
- Raw views show the stored text. "Pretty" is `lmgw_api_types::ordered_json::pretty`, a new
  printer that parses into an enum whose objects are `Vec`s of members and writes them back
  indented; `serde_json::to_string_pretty` over a `Value` would sort.
- The page reads an answer with `SystemOneResponse` (`Ordered` keeps the option order), never
  with `Value`.

*As built (WP2):* the bytes the container receives are the route's own `with_model` over the
cell's bytes, which writes the top-level members compactly (`{"model":…,"state":…}`) and every
value as sent; the stored `request` keeps the builder's text. An override is stored as typed
except that each data URL among its top-level `images` and `files` strings becomes
`lab-artifact:<id>`, spliced in place by byte range (`web/decision_lab/images.rs`, through
borrowed `RawValue`s), and is spliced back at dispatch. Data URLs anywhere else stay inline.
A data URL is sent as typed, not rebuilt from its sniffed type: its head, everything before the
comma (`data:image/x-receipt;name=a.png;base64`), is kept in `lab_artifacts.data_url_head`, and a
cell writes `<head>,<base64 of the bytes>`. Only a URL that comes back byte for byte that way
becomes an artifact: a canonical base64 payload (no whitespace, padded), and in an override a
JSON string written without escapes. Any other stays inline as typed. The row's `mime` stays the
sniffed type, for serving and the export's extension; an artifact that came as bytes goes out as
`data:<type without parameters>;base64,…`.

*As built (WP1):* `pretty` refuses what RFC 8259 refuses (a raw control character in a string,
an escape it does not name, a leading zero) and nesting deeper than `ordered_json::MAX_DEPTH`
(127, serde_json's own limit of 128), with an `Err` that names the place; the page shows the raw
text then.

### 1.4 Artifacts as files

The owner asked for files. The knowledge-base originals are the precedent
(`knowledge/originals.rs:29-35`), not chat's BLOBs: image-lab output would grow `lmgw.sqlite` by
megabytes per run, and the database is what dev copies and backups read whole.

- **Where:** `<data_dir>/labs/artifacts/<sha256>` (`labs::artifacts::dir`). The name is the
  content hash, so one image sent with fifty groups is one file. The row holds the MIME type,
  sniffed from the bytes as chat attachments are; an export adds the extension.
- **Ingest:** hash, write `<sha256>.tmp-<pid>` and rename it unless the file exists, then insert
  the rows in one `begin_write`. File work never runs inside a write transaction
  (`store.rs:81-94`).
- **Delete:** in one transaction delete the rows and select the hashes no remaining row names;
  commit; unlink those files. Ingest and delete hold one async mutex in `labs::artifacts`, so a
  delete cannot unlink a file an ingest is about to name. The gateway is its data dir's only
  writer.
- **Orphans:** a crash between write and commit, or commit and unlink, leaves a file no row
  names. The usage read counts orphan files and bytes, and one action removes them. Nothing
  sweeps on its own.
- **Reading:** `GET /labs/api/artifacts/{id}` answers the bytes with the row's MIME type,
  `X-Content-Type-Options: nosniff` and `Content-Security-Policy: sandbox`, as chat attachments
  (chat-archive design §2).
- **In a decision request:** the form's images are the group's `input` artifacts, read back into
  data URLs (`data:<mime>;base64,…`) at dispatch and named `lab-artifact:<id>` in the stored
  request. A re-opened form shows them by URL and sends their ids, which the server resolves, so
  a re-ask uploads nothing again.

*As built (WP1):* a file's type is sniffed (`extract::sniff`); the data URL's declared type stands
in only where the bytes are of no kind lmgw recognises. A record that did not commit unlinks the
files it wrote that no row names, under the lock. An answer that is not UTF-8 is kept as an
output artifact named `response`, its `response` column NULL. The sniff runs on the blocking pool
before the lock is taken (a ZIP-headed input is inflated to tell its kind), so only file writes
run under it. A file is synced before its rename and the directory after (best effort); a file
already under a digest is kept only when its length is the bytes', and written again when not.

### 1.5 Writes, size, retention

- **Writes:** each is one `begin_write` in `store/lab_*.rs`, with no network or model call
  inside. A cell's run is recorded after its dispatch returned. If recording fails (a full disk,
  a lock held past the busy timeout), the cell still answers the page: its run has no `id`,
  `store_error` says why, and the cell shows "not kept: …". An answer is never dropped because
  the store failed.
- **Size:** `GET /labs/api/{lab}/usage` answers the counts of groups, runs and artifacts;
  `text_bytes` (the stored request, response, form and plan lengths); `artifact_bytes` (the
  distinct files the lab's rows name); `orphan_files` and `orphan_bytes`. The results head shows
  "1,234 runs · 2.1 MB", its tooltip split into text and files.
- **No retention rule (Q1).** Nothing is deleted by age or count. The owner deletes a group from
  its card or clears a lab from the results head ("Delete all 1,234 runs and 37 files?", then it
  really does). This is the hidden-limits rule applied to storage: everything is kept, the size
  is in view, the owner decides. A rule, if one comes, is a visible setting in Settings →
  Retention beside `chat_purge_days` (`pages/settings.rs:615-646`), 0 meaning keep everything.
- **Paging is not a cap.** The history is read in pages, a transport unit: the answer carries
  the total, the page loads older groups as the list scrolls and shows "N older" until none are
  left. The page size is the client's query parameter; the server sets no maximum.

*As built (WP1):* `LabUsage` also carries `artifact_files`, the distinct files the lab's rows name
(Clear's confirm names files, rows can share one). The orphans are the artifacts directory's: no
row of any lab names them, so every lab's usage reports the same orphans and any lab's
`orphans/delete` removes them all. The history's query is `LabGroupsQuery`. A group's `title` is
the state's first line, at most `TITLE_CHARS` (120) characters, then an ellipsis: a label, the
form keeps the whole state.

### 1.6 Routes

The store's routes are shared, `/labs/api/{lab}/…`, where `{lab}` is a `LabKind` (`decision`,
`image`, `audio`) and anything else a 404. History, delete, usage, export and artifacts are the
same operations for three labs: one implementation, one plane of DocRoutes and one UI client
(`pages/lab_history`) serve all three, and slice 4 adds no store route. Dispatch stays per lab,
beside `/audio-lab/api/*` and `/image-lab/api/*`, because each lab turns its own form into its
own `/v1` request.

| Method, path | Body or query → answer | Slice |
|---|---|---|
| `GET /labs/api/{lab}/groups` | `?before=<id>&limit=<n>&model=<alias>` → `LabGroupPage` | 1 |
| `GET /labs/api/{lab}/groups/{id}` | → `LabGroupView` | 1 |
| `POST /labs/api/{lab}/groups/{id}/delete` | → `LabUsage` | 1 |
| `POST /labs/api/{lab}/clear` | → `LabUsage` | 1 |
| `GET /labs/api/{lab}/usage` | → `LabUsage` | 1 |
| `POST /labs/api/{lab}/orphans/delete` | → `LabUsage` | 1 |
| `GET /labs/api/{lab}/export` | `LabExportQuery` → a file (§1.7) | 1 |
| `GET /labs/api/artifacts/{id}` | → the bytes | 1 |
| `GET /decision-lab/api/models` | → `DecisionLabModels` | 1 |
| `POST /decision-lab/api/count` | `DecisionCount` → `DecisionCounted` | 1 |
| `POST /decision-lab/api/groups` | `DecisionAsk` → `LabGroupView` (the plan, no runs yet) | 1 |
| `POST /decision-lab/api/groups/{id}/cells/{cell}` | → `LabCellResult` | 1 |
| `/labs/api/{lab}/presets…` | list, create, update, delete | 2 |
| `/decision-lab/api/batches…`, `/labs/api/{lab}/batches…` | §3.3 | 2 |

`POST /decision-lab/api/groups` and `/decision-lab/api/count` drop axum's body limit, as the
other labs' dispatch routes do (`web/mod.rs:357-410`), because a form can carry images; a cell's
route has no body. The real handler still applies `max_body_mb` to the bytes a cell hands it, so
an oversized request gets the route's own 413 in its cell.

*As built (WP2):* the `/v1` layer that bounds `handle`'s body is not in front of an in-process
call, so `dispatch` applies `max_body_mb` to the bytes itself and answers the same 413 the route
does. `handle` and `dispatch` share a private `relay` from the body on: `handle` cannot call
`dispatch`, because its extractor's rejection must still be logged on the row. Every route
answers the dashboard's `ApiError` (`{code, message}`), its malformed paths, queries and bodies
included (the Chat API's extractors). The lab's own codes: `not_found`, `bad_request`,
`cell_answered` (409), `cell_unbuilt` (500, a stored form or file that no longer builds a
request), `file_missing` (an artifact row whose file is gone).

### 1.7 Export

`GET /labs/api/{lab}/export` answers `Content-Disposition: attachment`; the page starts it with
the shared `download(href, name)` (`pages/docs.rs:825-842`), and in the Tauri window
`on_download` saves it to Downloads and toasts (`src-tauri/src/main.rs:150-155`), as for chat.
`LabExportQuery` is `format` (`jsonl`, the default, or `csv`) and a scope: the whole lab,
`group=<id>`, or a filter on `model=<alias>` with `from` and `to` (RFC 3339). The query stays
short whatever is selected, so no URL length bounds an export.

- **JSONL** is lossless: one line per group, `{"format": "lmgw.lab.v1", "lab", "group":
  LabGroupView}`, its runs inside in cell order. `request` and `response` are JSON strings
  holding the exact texts. When the scope has artifacts the file is a zip
  (`lmgw-lab-<lab>-<date>.zip`) of the JSONL plus `artifacts/<sha256>.<ext>`, and an artifact's
  `href` is its path inside the zip. The zip is written as chat's (`web/chat_export.rs:332-371`).
- **CSV** is the lab's flat table. The Decision lab's has one row per run and question:
  `group_id`, `run_id`, `created_at`, `model_asked`, `answered_by`, `upstream_model`, `variant`,
  `http_status`, `error_message`, `latency_ms`, `input_tokens`, `question_id`, `question_type`,
  `answer` (the option, the expected level, or P(yes)), `confidence` (for `noul` the certainty of
  Decision 26), `probabilities` (the JSON object text, in option order) and `state`. A failed run
  is one row with the question columns empty. Slice 2 adds `expected` and `correct`.
  `csv_field` moves from `web/api_usage.rs:1068` to `labs/csv.rs` for both writers.

*As built (WP1):* lines are oldest first. `to` is exclusive; `from` and `to` are compared as UTC
milliseconds, as `created_at` is stored. A scope names the file: `-group-<id>`, or `-<model>`.
The Image and Audio labs have no CSV table until slice 4 (a 400). A zip whose artifact file is
missing from the data dir names it in a `MISSING.txt` rather than failing the export. CSV records
end in CRLF. A zip has one path per file, `artifacts/<sha256>.<ext>` with the extension of the
first row naming it, and every row naming that file gets that path whatever its own type.

### 1.8 Auth and the API description

- Every route is `Cap::Admin` in `labs_routes()`: the labs spend on the owner's aliases with no
  key of their own (`web/mod.rs:348-350`). Each gets a `CAPABILITY_TABLE` row under the labs'
  comment (`server.rs:504`), which becomes "the labs and their store". `ui::API_PREFIXES` gains
  `decision-lab/api/` and `labs/api/` (`web/ui.rs:28-34`).
- Two new planes, `openapi/planes/decision_lab.rs` and `openapi/planes/labs_store.rs`, register
  after `image_lab` (`openapi/registry.rs:227-228`), with tags `decision-lab` and `labs`
  (`openapi/tags.rs`), `Dialect::Dashboard`, admin document only. No row goes into
  `openapi/exclusions.rs`.
- The export is a `Resp::Download` as the chat export is (`openapi/registry.rs:106-109`):
  `application/x-ndjson` with the `LabExportLine` schema, `text/csv` and `application/zip`
  without one. The artifact route is a `Resp::Binary`.
- Handlers build no JSON with `json!`; the ratchet's `web = 27` may only fall.

## 2. The Decision lab

### 2.1 The page

Route `/decision-lab` (`app.rs`, after `:61`); nav "Decision lab" after "Image lab" in the "Use"
group (`shell.rs:612-620`). On `LabFrame`: the head is the model and its facts (§2.2); the form
column holds examples, state, images, questions, compare and perturb, the gauge and the request;
the results column is the kept history, newest first; the folded reference panel holds the
question types' rules and the model's notes (question sets in slice 2). The action bar's line
says what Ask will do ("Ask decide/laya · 3 questions", "Ask 6: 2 models × 3 variants") or why
it cannot yet.

`LabFrame` gains one optional prop, `kept: Option<Signal<LabUsage>>`. When set, the results head
reads "newest first · kept · 1,234 runs · 2.1 MB", Clear deletes the lab's history on the server
(the confirm names runs and files), and the empty text says runs are kept until deleted. The
Audio and Image labs pass nothing until slice 4.

The palette is the existing one (`lmgw-ui-kit/assets/kit.css`): graphite surfaces; `--accent`
for bars, the marker and the meter fill; `--amber` for an answer from a fallback and for a gauge
over its limit; `--err` for a failed cell. Nothing pastel. Fits are judged at 125% interface
scale and in WebKitGTK (WP4).

### 2.2 Models and the head line

`GET /decision-lab/api/models` follows the Image lab's list (`web/image_lab.rs:59-108`): every
`exposed_entries` entry whose endpoints include `/v1/systemone`, which is the local `decide/*`
rows and every cloud alias or catalog entry of task `decision` (`capabilities/task.rs:78-79`).
It answers `DecisionLabModels { models, hold }`; each `DecisionLabModel` carries:
- **identity:** `name`, `owner`, `local`, `model_id`, and from the capabilities
  `input_modalities`, `context_length` and `notes`; `state` and `warnings` from the registry;
- **`held`:** whether the GPU hold applies to it now, and its `hold_fallback`, so the head line
  says "held — answers through jev" or "held — no fallback: 503";
- **`facts`** (local rows): `decision_type`, `max_head_tokens` and `shape` (the header through
  `gguf_cache`, `Shape::of`); `ctx_size`, and `n_ctx_slot` from the running container's `/props`
  when known; `ubatch` and `batch` in effect, each with its source (`row` or `llama.cpp
  default`); `backend` (`cpu`, `gpu`); `threads_in_effect` (`aux_engine::threads_in_effect`);
  `reads_images`;
- **`limits`:** one or two `DecisionLimit { applies_to, tokens, of, rule }` from a new pure
  `modelinfo/decision/limits.rs`, restating decision-models §3.4 and §1.4:

| Shape | Limit | What must fit |
|---|---|---|
| Non-causal (Laya, Julia-1) | the ubatch in effect | one pass's prompt: the state, one question and its options, the template |
| Causal with a head (Kev, Clef) | the ubatch in effect, which is the batch in embedding mode | the span from the first option marker to the end of the prompt |
| | `n_ctx_slot`, else `ctx_size` | the state and the question |
| Label logits (lev, OpenJev) | `n_ctx_slot`, else `ctx_size` | the state and the question |
| Cloud | the catalog's `context_length` | the request (TypeSafe documents 32k for the state and the longest question) |

The head line reads: `[decide/laya ▾] laya · non-causal · ctx 8192 · ubatch 8192 (row) · CPU,
16 threads · text only · stopped · Reload`, each chip's tooltip naming its source.

*As built (WP2):* a source is `row`, `args` (a flag in the row's extra args), `batch` (llama.cpp
lowered the ubatch to the batch), `ubatch` (embedding mode lowered the batch to the ubatch, on a
non-causal type and a causal type with a head) or `llama_cpp_default`. `held` is
`gpu_block_for`, so a benchmark's GPU lease counts too; `hold_fallback` names only a fallback
that can answer. A header that cannot be read gives `header_error`, no shape and no limits. A
rule names the model by its decision type ("laya evaluates the whole prompt …"), else its public
name. `modelinfo::decision` is `pub(crate)` for `Shape` and `limits`.

**The option limit (Q2).** The outline put "the per-type option limit" on the head line. No
header carries it; llama.cpp's source does (52 for openjev, 255 for the other types its README
lists), and lmgw deliberately keeps no table of llama.cpp's types (`modelinfo/decision.rs:6-10`).
So no number per type is shown. The `choice` editor says "N options · the wire allows 255; some
types allow fewer, and llama.cpp's 400 names the limit", and that 400 lands in the cell verbatim
("too many options (256), this model supports at most 255"). The head budget the header does
carry is shown: `decision.max_head_tokens` (192 for Laya, 256 for Julia-1), past which llama.cpp
truncates a question and its options.

**Image support.** `reads_images` is the registry's `vision` once the container has run, else
`input_modalities` containing `image` (the projector's header, read by `for_aux`, decision-models
§2.9). A cloud model reads images when its capabilities say so; Jev's do not.

### 2.3 The form

`DecisionForm` (`lmgw-api-types::decision_lab::form`) is the whole form, serialized into
`lab_groups.form`. Every list in it is a `Vec`, so storing it keeps its order.

```rust
pub struct DecisionForm {
    pub state_mode: StateMode,            // Text | Json
    pub state: String,                    // as typed; JSON mode keeps its text verbatim
    pub images: Vec<ImageInput>,          // DataUrl { name, url } | Artifact { id, name }
    pub questions: Vec<QuestionDraft>,
}
pub struct QuestionDraft { pub id: String, pub instructions: String, pub kind: QuestionKind }
pub enum QuestionKind {
    Choice { options: Vec<OptionDraft> },   // OptionDraft { name, description }; blank → null
    Score { levels: Vec<String> },          // lowest first
    Noul { yes: String, no: String },       // both blank → no `criteria`
}
pub enum Variant { AsWritten, NoDescriptions, Reversed }
```

`DecisionForm::check()` enforces what the wire documents: a non-empty state (JSON `null` allowed,
for LiquidAI's d1 types) that parses in JSON mode; at least one question; non-empty unique ids; a
`choice` with at least one option and non-empty unique names; a `score` with 2 to 10 levels. A
failure is the action bar's `Check::Invalid`, naming the field. Anything else is the upstream's
to refuse; other shapes (an object `instructions`) are the override's job (§2.4).

**Editors.** State: a Text | JSON seg over one textarea; switching keeps the text, and the check
says if it does not parse. Images: a drop zone and file picker, only when the model reads
images; thumbnails with sizes and ✕. When compared models differ, a line names those that read no
images; they get the images anyway and answer llama.cpp's 501 (decision-models §1.2). Questions:
a card each with id, type select, instructions and ↑ ↓ ✕, then the type's editor: `choice` option
rows (name, optional description, ↑ ↓ ✕, "Add option", the count line of §2.2); `score` level rows
lowest first with the count against 2 to 10; `noul` optional "a yes means" and "a no means". A
question's tokens against `max_head_tokens` show on its card once counted (§2.5).

### 2.4 The request

`DecisionForm::body(&self, model, variant, images: &[DataUrl]) -> Result<String, String>` is the
one builder. It writes indented JSON with members `model`, `state`, `questions`, then `images`
when there are any. The Request panel shows `body(first model, AsWritten, …)`, eliding each data
URL's payload ("data:image/png;base64,… 1.4 MB") and byte for byte otherwise; the server builds
each cell with the same function. A unit test pins it: the preview with payloads restored equals
the dispatched bytes.

**The override.** "Edit as JSON" copies the preview, payloads included, into a textarea, which
from then on is the request. A banner says "the override is sent; the form is not", with "Drop
override". The only check is "a JSON object", `SystemOneBody::parse`'s: a lab exists to see what
the route says, so a malformed question goes out. Each cell's body is
`SystemOneBody::with_model(cell model)` over the override's bytes, so Compare works. Perturb is
off with the reason "perturbations rewrite the form; drop the override to use them", because
rewriting an override means parsing it into a typed request, which drops unknown fields. Unlike
the Audio lab's override, this one replaces the body: a merge goes through a `Value` and would
sort the options.

*As built (WP2):* the route's rewrite only replaces a `model` that is there, so an override with
no top-level `model` gets the cell's inserted as its first member
(`web/decision_lab/model_member.rs`), every other byte as typed, before that rewrite. The stored
override stays as typed; the run's `request` has the member. The route is unchanged: a client
that sends no `model` gets its 400.

*As built (WP3 review):* an override may name a kept image as `lab-artifact:<id>`, any artifact
the store keeps, written as a plain JSON string among its top-level `images` or `files` (the
data URL rule of §1.3). The new group gets an input row of its own for that file and head (the
form's row when the re-opened form names the same artifact, one row per artifact), the stored
override names that row, and the cell sends `<data_url_head>,<payload>`, the URL as first typed;
deleting the old group takes nothing from the new one. An id the store does not keep is the
group's 400 naming it. So "Edit as JSON" and "Open in form" keep the refs in the override text;
nothing is fetched back.

### 2.5 The token gauge

`POST /decision-lab/api/count` takes `DecisionCount { model, form }` and counts each piece with
one `count_texts` call on the model's resolved route (`proxy/count.rs:235`), in process with
`ClientProto::SystemOne`. The pieces: the state (Text mode's string, JSON mode's text as typed);
each question (its instructions, then each option's name and description, each level, or the
`noul` criteria, joined by newlines); and each `choice` question's options alone (the option
span). `DecisionCounted` answers the count per piece; the counter's `approx` flags, plus
`flattened` always (llama.cpp's `systemone` template wraps the pieces and is not counted) and
`media_omitted` with images; the estimate against each of the model's limits (§2.2): the state
plus the largest question, or the largest option span; whether it is over; and the limit's rule
in one sentence.

The gauge is one bar per limit, the rule in its tooltip. Past the limit it turns `--amber` with
the rule written out ("Laya evaluates the whole prompt in one ubatch (8192); llama.cpp answers
500 past it"). It never blocks Ask: the 500 is llama.cpp's, relayed by design (decision-models
Q6). After an answer it also shows the measured `input_tokens`; on types that do not share the
state, that counts the state once per question (Kev's 15,910 for four questions on a 3,959-token
state, decision-models §6.1).

**When it counts.** Counting goes through the route, so on a local row that is not running it
would start the container. The gauge counts by itself, 800 ms after the last edit, only when the
model is `ready` or in the cloud (a cloud count without a native counter is a tokenizer guess,
flagged `tokenizer_guess`). Otherwise the bar says "not counted: decide/kev-4b is not loaded"
with a Count button that loads it: cheap on a CPU row, the owner's call on a GPU row. A count
that starts a model writes a log row; a cheap one writes none (`proxy/count_admit.rs:21-23`).

*As built (WP2):* `DecisionCount` has `load`. "Ready or cloud" is judged on the route the count
would hit, after the gate's resolve and the hold's swap (`proxy::count_texts_if`, `IfCold`): a
held row with a cloud fallback is counted there, `answered_by` naming it; a local model the gate
settles on that is not `ready` (a starting one included) is counted only with `load`, and
otherwise the answer is `not_counted: "<model> is not loaded"` (or "… answers through
<fallback>, which is not loaded"), starting nothing and writing no log row. A count that fails
(the hold's refusal of a row with no fallback among them) is a 200 with `not_counted` naming the
failure; a model that answers no `/v1/systemone` is a 404. The pieces
come from `DecisionForm::count_pieces`; `DecisionCounted` gives `state`, a `QuestionTokens`
per question (its tokens and its `option_span`), `approx`, a `DecisionEstimate` per limit and
`answered_by`.

### 2.6 Asking: groups, cells, compare, perturb

**Compare.** The head picker names the first model; a "Compare" card adds more ("Also ask on:"
chips, "Add model" as a `ModelPicker` over the same list). **Perturb** has two checkboxes,
"descriptions dropped" and "order reversed". The plan is the models × (as written, then each
checked variant), models outer. `body` builds the variants from the form: `NoDescriptions`
writes every `choice` description as `null` and leaves out the `noul` criteria; `Reversed`
writes each `choice` question's options in reverse. Question order, `score` levels and `noul`
are untouched: reversing a scale inverts its meaning, and question order is answer order.

**The flow.**
1. Ask posts `DecisionAsk { form, models, variants, override }` to `/decision-lab/api/groups`.
   The server runs `check()` (unless there is an override), ingests `DataUrl` images as input
   artifacts, stores the group with its plan, and answers `LabGroupView` with no runs.
2. The page posts `/decision-lab/api/groups/{id}/cells/{n}` for n = 0, 1, …, one at a time. The
   server builds the cell's bytes, calls `proxy::systemone::dispatch(state, RequestCtx::default(),
   bytes)` (a new function `handle` delegates to after its extractors, mirroring
   `handle_image_generation`), reads the whole body, records the run with data URLs as artifact
   refs, and answers `LabCellResult { run: LabRunView, store_error: Option<String> }`.
3. A cell that already has a run is a 409; a re-ask is a new group.

**One cell at a time, from the page (Decision 19):** each latency is the request's own, not a
queue's (cells at once on one llama-server would queue in its slots); cells appear as they land;
Stop means the page stops posting, and aborting the cell in flight drops its handler future,
which writes the request log's 499 row as any disconnect does. A group with fewer runs than
cells reads "2 of 6 answered — stopped" and offers "Run the rest". The answers that matter here
take milliseconds; a slow cell (a CPU state of 8,000 tokens) is slow either way.

**Status (Decision 20).** A dispatched cell answers 200 whatever the upstream said. The run
carries `http_status`, `error_code`, `error_message` and the body verbatim, so the card renders
the gateway's own envelope, `lmgw_code` included. The lab's own failures keep their status: a bad
form 400, an unknown group or cell 404, a repeated cell 409.

**What is real:** `policy_refusal`, `gate::open(RouteCheck::Decision)`, the hold's swap,
admission, `send_local`, the 404 → 501 mapping, and the request log row (ingress `systemone`,
class `aux`, priced). Only auth and the HTTP hop are skipped, as in the other labs. A cell on Jev
costs what any request costs, and Usage shows it.

*As built (WP2):* a cell is claimed in process before it is built (`labs::claims`, on the
gateway's state) and released once its run is recorded or its handler is dropped, so a second
post of a cell in flight is the 409 `cell_answered` without a dispatch: two posts of one cell
reach the model once. A run the store finds a second for its cell (`UNIQUE (group_id, cell)`,
inserted with `ON CONFLICT DO NOTHING`) is the same 409.

### 2.7 Answer views and the group card

The page reads `response` with `SystemOneResponse`. One view per question:
- **`choice`:** bars sorted by probability, highest first, each with the option, the percentage
  and its position as sent (`#3`), so a reversal shows. The winner's bar is `--accent`, the
  others `--border-strong`; confidence below.
- **`score`:** the levels as a strip, lowest first, each cell's fill alpha its probability, its
  index and legend under it (wrapped, never cut). A marker sits at the fractional expected level,
  `score / (levels − 1)` across the strip, with "2.37 of 0–4 · nearest: within a few days";
  confidence below.
- **`noul`:** a 0–1 meter with a 0.5 midline, the fill at P(yes), the number, yes or no by 0.5,
  and "certainty (the lab's: max(p, 1−p))".

The footer has the latency, `input_tokens`, the upstream's `model`, and an `--amber` answered-by
chip when a fallback answered ("via jev · hold"). "Raw" shows request and response, each with an
order-keeping pretty toggle (§1.3). A failed cell shows its status and the envelope's message,
the raw body under "Raw".

**The group card.** One cell: the question views stacked. Several cells: a matrix, a row per
question and a column per cell ("decide/laya · reversed"), each cell the compact answer (winner
and %, the score, P(yes)) with latency and tokens; a click opens its full views below. For each
perturbed model, a Δ line per question ("winner unchanged · billing 0.81 → 0.42 without
descriptions", "winner changed under reversal"): decision-models §1.1's point made visible,
descriptions move accuracy a lot. The card's menu: "Open in form" (the stored form and plan,
images by artifact id), "Export" (`group=<id>`), "Delete" (with a confirm).

### 2.8 Examples

`lmgw_api_types::decision_lab::examples()` returns three `DecisionForm`s, offered in an
"Examples" menu that replaces the form (with a confirm when it is dirty). All are English,
because Laya is English-only; Julia-1 is multilingual.
- **Ticket triage.** State: "Hi, I was charged twice for order A-1001 this month. The account is
  jane@shop.example. Please refund one of the two payments." Questions: `team` (choice: `billing`
  "payments, refunds, invoices", `technical` "bugs, errors, outages", `account` "sign-in,
  profile, access", `other`); `urgency` (score: "can wait a week", "within a few days", "today",
  "right now"); `needs_human` (noul: yes "a person must decide or act", no "a help-centre answer
  resolves it").
- **Sentiment.** A short product review; `sentiment` (choice: positive, neutral, negative) and
  `intensity` (score: five levels from "barely" to "strongly").
- **Is the user done talking?** JSON-mode state `[{"role": "assistant", "content": "Which day
  suits you?"}, {"role": "user", "content": "Thursday works, but only after"}]`; `done` (noul:
  "Has the user finished their turn?", yes "a complete thought that expects a reply", no "it
  breaks off or announces more"). This is decision-models §5's end-of-turn case.

A unit test builds each example and parses it into `SystemOneRequest`.

*As built (WP2):* `examples()` returns `Example { name, form }`: the menu needs the names.

### 2.9 History in the page

`pages/lab_history` is the store's client, shared with slice 4: it reads `groups` in pages
(§1.5) and renders each through the lab's own card, prepends a new group as soon as it is
created, deletes, clears, reads usage and builds export hrefs. The results head's Export menu
offers "All as JSONL", "All as CSV", and "This model as…" when a model filter is set. History
survives a reload and a gateway restart.

## 3. Question sets and set runs (slice 2)

### 3.1 Tables (slice 2's migration)

```sql
CREATE TABLE lab_presets (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    lab         TEXT NOT NULL CHECK (lab IN ('decision', 'image', 'audio')),
    kind        TEXT NOT NULL,            -- 'question_set'; later 'image_prompt', 'audio_settings'
    name        TEXT NOT NULL,
    body        TEXT NOT NULL,            -- JSON of the kind's type (QuestionSet, …)
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (lab, kind, name)
);
CREATE TABLE lab_batches (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    lab             TEXT NOT NULL CHECK (lab IN ('decision', 'image', 'audio')),
    preset_id       INTEGER REFERENCES lab_presets(id) ON DELETE SET NULL,
    preset_snapshot TEXT NOT NULL,        -- the set as run; later edits do not rewrite results
    model           TEXT NOT NULL,
    source_name     TEXT NOT NULL,        -- the uploaded file's name, or 'agent run <job id>'
    mapping         TEXT NOT NULL,        -- JSON: the state column, a label column per question
    item_count      INTEGER NOT NULL,
    concurrency     INTEGER NOT NULL CHECK (concurrency > 0),
    job_id          INTEGER,              -- the last job that ran it; no FK, jobs are pruned
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
ALTER TABLE lab_groups ADD COLUMN batch_id INTEGER REFERENCES lab_batches(id) ON DELETE CASCADE;
ALTER TABLE lab_groups ADD COLUMN item_ix INTEGER;
ALTER TABLE lab_groups ADD COLUMN expected TEXT;   -- JSON {question id: label}
CREATE INDEX idx_lab_groups_batch ON lab_groups (batch_id, item_ix);
```

A set run's item is a group with `batch_id` set: its form is the set with that item's state, its
plan one cell. All items are created with the batch; their runs come as they are answered. The
history list reads `batch_id IS NULL`; usage and export count both.

### 3.2 Question sets

`QuestionSet { questions: Vec<QuestionDraft>, default_model: String, thresholds:
Ordered<Threshold> }`, one `Threshold { value: f64, measured: Option<Measured { batch_id,
coverage, accuracy, labelled }> }` per question. "Save as set" in the lab saves the current
questions under a name, "Update set" overwrites them, loading one replaces the questions and
picks its model, and the reference panel lists the sets. A threshold applies to the question's
`confidence` (`choice`, `score`) or the certainty of Decision 26 (`noul`). Unmeasured it is 0,
"never needs attention", and the set says "not measured".

### 3.3 Set runs

- **Input:** CSV or JSONL, pasted or uploaded; the page reads the file and posts its text. CSV is
  RFC 4180 with a header row, parsed with the `csv` crate; JSONL is one object per line, its
  columns its top-level keys.
- **Preview:** `POST /decision-lab/api/batches/preview` answers the columns, the row count and the
  first 20 rows ("first 20 of 1,234"). The owner maps one column to `state` (JSON mode if its
  values are JSON) and optionally one per question to its expected answer: an option name
  (`choice`); `yes`, `no`, `true`, `false`, `1` or `0` (`noul`); a level index or a level's exact
  text (`score`). An unknown label refuses the input, naming each row and value. Nothing is
  dropped.
- **Start:** `POST /decision-lab/api/batches` creates the batch and its items in one transaction
  and spawns `JobKind::LabBatch`, keyed `lab-batch:<id>` (one live job per batch). Concurrency is
  the form's visible field, default 1; admission and the model's slots are the only other bound.
  Before a run on a priced model the form shows its price row; Usage records the real cost.
- **Execution:** the executor answers the items without a run through the same dispatch-and-record
  path as a lab cell; progress is answered of total; Cancel stops after the calls in flight. A
  boot marks the job failed (`state.rs:502`), and the batch shows "interrupted at 340 of 1,000"
  with **Resume**, a new job for the rest. Job pruning removes only the job row.

### 3.4 Metrics

`GET /decision-lab/api/batches/{id}/metrics?question=<id>&bins=<n>` computes from the stored
runs (`labs/metrics.rs`, pure functions with unit tests). Per question:
- answered, failed and labelled counts;
- **accuracy:** `choice`, the winner equals the label; `noul`, P(yes) ≥ 0.5 equals the label;
  `score`, the nearest level equals the label, plus the mean absolute error in levels;
- **confusion matrix**, labels × predictions in option or level order (2×2 for `noul`);
- **reliability:** `bins` equal-width bins of the predicted probability (the winner's, or the
  `noul` side's), each with count, mean prediction and accuracy; `bins` is a visible control,
  default 10;
- **threshold curve:** per distinct confidence value, the coverage at or above it and the
  accuracy of that part. The full curve is returned; the slider reads it in the page.

The view shows the matrix (cells shaded by `--accent` alpha), the reliability diagram (bars
against the diagonal), and the slider ("at 0.72: 81% covered, 96.4% correct (n = 812)"). "Use as
threshold" writes the value and its measurement into the set. Without labels the batch is bulk
classification: a table of state, answers and confidence, and the export (`batch=<id>`, with
`expected` and `correct` in the CSV and `expected` in each JSONL group).

### 3.5 As built (slice 2, server)

*The review of 2026-10-17 led to sixteen changes, marked (R1) to (R16) below. They are drafts.*

- **Tables:** `0084_lab_sets.sql` as §3.1, plus `idx_lab_batches_lab` and
  `idx_lab_batches_preset`, `lab_batches.ready` (R4), `lab_groups.item_key` (R5) and
  `lab_runs.readable` (R10). `expected` is NULL for an item without labels.
- **Presets** are `/labs/api/{lab}/presets` (GET `?kind=`, POST create), `…/presets/{id}` (GET,
  POST rewrites it whole) and `…/presets/{id}/delete` (answers the list). The wire puts `kind` and
  `body` beside the name (`LabPresetBody`, adjacently tagged, flattened into `LabPresetView`,
  `LabPresetWrite` and `LabPresetUpdate`); the column keeps the kind's own type (`QuestionSet`). A
  kind must be its lab's (a `question_set` posted to the image lab is a 400), a name is unique per
  lab and kind (409 `name_taken`). `QuestionSet::check` is the form's question check
  (`check_questions`, shared) plus thresholds that name a question and lie in 0 to 1. Clear
  leaves presets. **(R6)** An update carries the `updated_at` it read (`LabPresetUpdate`, required,
  a missing one is the extractor's 422) and is a 409 `set_changed` when the preset moved since.
  Every write moves `updated_at`, by a millisecond past the old value when the clock has not
  (`next_stamp!`), so two writes in one millisecond still differ. A question the update changes
  (any part of it: type, options with their descriptions, levels, text) loses its threshold's
  `measured`; the value stays and reads "not measured" (`QuestionSet::unmeasure_changed`). The
  threshold write keeps its precondition on the body. **(R12)** Create, update and the threshold
  write take a set whole and have no body limit, as Ask.
- **Set-run routes are all `/decision-lab/api/batches…`**, none under `/labs/api/{lab}/`: preview,
  start, list, one run, `items`, `cancel`, `resume` (optional `{concurrency, retry_failed}`),
  `delete` (answers `LabUsage`), `metrics` and `threshold`. An item cannot be deleted alone (409
  `batch_item`). **(R13)** `items` pages by `after`/`limit` as the history does, and the answer
  carries `next_after`, the page's last `item_ix` when more items follow (else null), beside
  `total`.
- **Start** takes a saved set (`preset_id`) or one sent whole (`set`), not both; a blank model is
  the set's `default_model`, and a model that answers no `/v1/systemone` is a 404 up front rather
  than a failed run per item. Concurrency 0 is a 400. The preview also takes the set's
  `questions` and a `mapping` and then lists every problem (`BatchProblem`: row, line, column,
  value, message) as Start would refuse them; Start's 400 `bad_input` names all of them.
  **(R4)** Each item keeps its full form (the set and its state), and the items are written in
  several short write transactions: the run's row first with `ready` 0, then the items in chunks
  of at most 2,000 items or 4 MiB of item text (an internal batching parameter, not a limit on
  the input), each chunk one `INSERT … SELECT … FROM json_each(?)`, and the last chunk sets
  `ready` 1. A run that is not ready is never run, listed, read or counted, nor are its items
  (`lab_group_ready!` in the group reads, `ready = 1` in the run reads); a failure half way
  deletes it, a boot deletes what a crash left (`delete_unready_lab_batches`, beside
  `fail_orphaned_jobs`), and a Clear in between answers 409 `batch_deleted`. The writes and the
  job's spawn run as a task of their own, so a client that goes away does not leave half a run.
  Measured on 100,000 rows of about 400-byte states (35 MiB of CSV), debug build, a file
  database (the ignored `labs_batch::boot::start_of_a_100k_row_file_measured`): the route takes
  8.4 s; `insert_lab_batch` alone 3.7 s in 51 write transactions, the longest 40 ms (timed from
  when `BEGIN IMMEDIATE` returns); a writer
  racing the route waited 186 ms at the longest over 476 writes (the busy timeout is 5 s); the
  database grew 103.1 MiB, about 1 KiB per item, most of it each item's copy of the set.
- **Labels:** an empty value (in JSONL also `null` or a missing key) is no label for that
  question; the item runs. `choice` takes the option's exact name, `noul` yes/no/true/false/1/0 in
  any case. They are kept as the option's name, the level's index (`"0"` …) or `yes` and `no`.
  **(R1)** A `score`'s label column is read as a whole, never value by value (`score_column` in
  `lmgw-api-types`, beside `read_label`, for the page to share): as level texts when every
  non-empty value is a level's exact text, as indices when every one is an index in range. Both
  fitting with different meanings (a scale written "1" to "5", values "1" to "4") is refused with
  one problem on the first disagreeing row; a column of both is refused with a problem per row
  of the smaller part, each naming a row of the other; both say to set the mapping's
  `score_labels` (per question, `text` or `index`), which settles the column, and a value that
  then does not read is that row's problem. A value that is a level neither way is its row's
  problem. `score_labels` naming no question, or a question that is no score, is a problem.
  **(R7)** An empty `mapping.state` is refused ("the mapping names no state column"), and so is
  an empty column name for a label or the key: a column whose header is empty (pandas' index)
  cannot be mapped, and the message says to name it in the file. An empty state, or one that is
  no JSON in JSON mode, refuses the input like a label. A JSONL value is a string's text, else
  its JSON as written (an object state keeps its order); a CSV needs its header and equal field
  counts, a BOM is dropped, and lines are counted from byte offsets (the `csv` crate's line is
  one short after a CRLF). A mapped column the header names twice is refused. Unmapped columns
  are not kept. **(R5)** The mapping's optional `key` column is kept per item (`item_key`, an
  empty value none) and shown as `LabBatchItem.key`.
- **The job** answers each item through the cell route's own path (`cells::answer`, in the job's
  `AnswerMode`), so claims stop double dispatch. Each item in flight is a task of its own (a
  `JoinSet` up to the concurrency): items polled as one stream stalled while the loop wrote the
  progress and waited for a connection an unpolled item held. A cell answered elsewhere is
  skipped and counted from the store at the end; a run the store could not keep leaves its item
  pending and ends the job failed with "N of M items have no run: …". `JOB_KIND`, `job_key` and
  the feed's detail `BatchJobDetail` are in `lmgw-api-types`. **(R2)** An answer that would be the same for every item is no item's answer:
  `proxy::systemone` marks it in process (`SameForEveryItem`, a response extension, never on
  the wire): the policy check; every failure of `gate::open` (the route check, the hold's
  `gpu_hold`, a benchmark's `gpu_benchmark`, admission's `vram_too_large` and
  `vram_queue_timeout`, `candidate_unavailable`, `unknown_alias` for a model gone, and a
  container that does not start, which the gate answers as a 502 `upstream`); and an upstream's
  401 or 403, the provider refusing the gateway's own key, which lmgw answers as a 502. (The
  route never goes through `gate::send`, so the KV pool's and a ladder's waits do not occur on
  it.) In the job's mode such an answer is not
  recorded: no new item is taken, the calls in flight finish, and the job ends failed with
  "stopped at item N: lmgw could not ask the model (503 gpu_hold): …; K of M items have
  no run: Resume runs the rest once the cause is gone" (under `retry_failed`, the message says
  to Resume with "retry failed"), so the run reads interrupted. The page's
  cells still record it, to show it. Everything else stays the item's
  failed run: the model's 4xx, 5xx and 429 as relayed, the named 501 for an image without the
  route, a transport failure, a timeout, a state over the context. **(R2)** Resume's
  `retry_failed` asks the items whose run is no 2xx again too: the new run replaces the failed
  one in the transaction that records it (`record::run` with `replace_failed`: the failed run
  goes, the insert keeps `ON CONFLICT DO NOTHING`, so a 2xx run another post recorded in between
  stays and this one is the 409; the claim is as before), and the failed run's output files go
  when no row names them. A done run with failed items takes it; without failed items, or
  without `retry_failed`, a done run is the 409 `batch_done`. **(R15)** The first Cancel is
  graceful. A second Cancel while the job still runs, a Delete and a Clear abort it
  (`JobManager::request_abort`, raced by `JobCtx::aborted`): its item tasks are dropped, so their
  handler futures write the request log's 499 rows as the page's Stop does, their claims go, no
  run is recorded, and the job ends canceled. **Status** is `running` while the job is live in
  this gateway, else `done` when every item has a run, else `canceled`, `interrupted` (the job
  failed, was pruned, or its row says running with no job here) or `not_started`, with
  `job_error` whenever the last job failed. **(R11)** The job is read by its key, the newest job
  row of kind `lab_batch` keyed `lab-batch:<id>`; the run's `job_id` column is written after the
  spawn (a failure is logged, never a 500 while the job runs) and stands in only once the rows
  are pruned; no job known but items with runs reads interrupted.
- **Metrics:** **(R10)** `failed` is a run that is no 2xx, everywhere; a 2xx without a readable
  answer is `unreadable`: per question in the metrics (none of the question's type, a winner the
  question does not offer, the nearest level's probability missing), and in `BatchView` and
  `BatchJobDetail` an item's 2xx run that does not read for every question of its set, judged as
  it is recorded from the item's form with the metrics' own `predict` (`lab_runs.readable`). A
  set of one question has the same count in both. `answered` needs a readable answer. **(R3)**
  Reliability is sparse: only the bins that hold an answer, each with its `index`, `lower`,
  `upper`, `count`, `mean_prediction` and `observed`, in O(items) memory whatever `bins` is; no
  maximum, 0 a 400; the page draws the empty ones from `bins`. **(R8)** An answer goes in the bin
  whose reported edges hold it (`bin_of`: k / bins ≤ p < (k + 1) / bins as `f64`, the last bin
  holding 1), so 0.57 at 100 bins is bin 57, where `floor(p × bins)` gave 56. **(R16)** `choice`
  and `score` bin P(winner) from `probabilities` (not `confidence`) or the nearest level's
  probability against the winner being right; `noul` bins P(yes) against the rate of `yes`
  labels, the standard reliability diagram. The matrix order is the options, the level indices,
  then `yes`, `no`. MAE uses the fractional score. A curve point's coverage is over every
  answered item, its accuracy over the labelled ones among those covered; points lowest first;
  a threshold cuts `noul` on its certainty max(p, 1 − p).
- **"Use as threshold"** is `POST …/batches/{id}/threshold {question, value}`: the server measures
  the cut from the stored runs and writes `{value, measured}` into the saved set the run ran.
  **(R14)** A cut with no labelled answer at or above it is the 409 `no_labels`, so
  `Measured.accuracy` is a number and `labelled` at least 1. 409 `set_deleted` for an unsaved or
  deleted set; 409 `set_changed` when the set's question is no longer the one the run asked, or
  the set changed while measuring (the write compares the body).
- **Store reads:** the history is `batch_id IS NULL`; usage counts every group of a ready run,
  gains `batches`, and its `text_bytes` adds `expected`, `item_key` and each set run's snapshot,
  mapping and source name. `LabGroupView` gains `item` (`batch_id`, `item_ix`, `key`,
  `expected`). The export's `batch=<id>` names its file `-batch-<id>`. **(R5)** The CSV has
  `batch_id`, `item_ix` and `item_key` after `group_id` (empty for the history) and `expected`
  and `correct` after `answer`; the JSONL carries `item.key`. **(R9)** A set run's item has a CSV
  row per question of its set whatever its run answered: a failed run's rows keep `expected`,
  and only `correct` is empty there and for an answer that does not read. A failed run of the
  history stays one row with the question columns empty. An item without a run has no CSV row
  (the JSONL has it, with no runs).
- **Child modules** the list does not name: `labs/batch_view.rs`, `labs/presets.rs`,
  `web/decision_lab/batch_control.rs` and `batch_metrics.rs`; in `lmgw-api-types`
  `decision_lab/{sets,batch,metrics}.rs` and `lab_store/presets.rs`. Tests: `labs_batch.rs` with
  its children `control`, `input`, `sets` and `boot`, the unit tests of `labs/ingest` and
  `labs/metrics`, `migrations/lab_sets.rs`.
- **From the UI's review (2026-10-17).** Four decisions, marked (U3), (U4),
  (U6) and (U12) here and in §3.6, are drafts; each changes the wire.
  **(U3)** What an answer predicts and how a run reads is one rule, moved into
  `lmgw_api_types::decision_lab::metrics` (pure over its types, beside `read_label` and
  `score_column`): `Prediction`, `Outcome`, `predict`, `certainty`, `classes`, `outcome`,
  `correct`, `is_2xx` and `read_response` (a run's text parsed only when it is a 2xx).
  `labs::metrics` re-exports them; the metrics, the `readable` judgement of an item's run
  (`cells::readable`, now `outcome` itself) and the CSV export's `correct` call it, and so does
  the page. Tests: `decision_lab::metrics::tests` in `lmgw-api-types`. **(U4)** `BatchView`
  carries `cancel_requested`: a Cancel already went to the job running it, so the next one
  aborts (`JobManager::cancel_requested` on the live job; false when no job runs it), for a
  reader that did not send the first. **(U6)** `GET /decision-lab/api/models` names the price:
  `DecisionLabModels.currency`, and per model `price: DecisionPrice { local, rows, unresolved
  }`, the rows `Snapshot::sheet_for` selects for the name's primary route (`Snapshot::resolve`,
  not the hold's fallback, which prices its own answers), one per unit in `PriceUnit::ALL`'s
  order, each resolved on its own (alias manual, alias catalog, upstream model manual, upstream
  model catalog; a row without a usable rate is none). The selection is
  `Snapshot::sheet_rows`, which `sheet_for` now sums, so the page shows the rows a request is
  priced by. `local` is a local upstream, free whatever rows exist; `unresolved` says why a
  name has no one route (a candidate alias). Test: `decision_lab::price`. **(U12)**
  `BatchProblem` gains `kind` (`file`, `mapping`, `state`, `label`, `score_labels_needed`)
  and `question` (the question it is about: a label, a `labels` or `score_labels` entry), filled
  in `labs/ingest.rs`, so the page keys the score reading on `score_labels_needed` for its
  question, not on the message. Beside them, `BatchJobDetail.retry_failed` says the live job
  asks the failed items again too, so the page knows a failed run is not final while it runs.
- **From the fix round (2026-10-17), two more wire changes, drafts.** `BatchJobDetail.cancel_requested` is the job's cancel flag, published in every
  frame (the job also reports the moment the flag goes up, so a hanging call does not hide it),
  so a Cancel from another tab, the generic jobs route or MCP reaches this page. `DecisionPrice`
  gains `fallback: Option<DecisionFallbackPrice { alias, local, rows, unresolved }>`, filled
  while the local row is held and its `hold_fallback` is usable, from the same
  `Snapshot::sheet_rows` path as the model's own rows. Test: `decision_lab::price`.
  The frame that first carries the raised flag bypasses the 500 ms publish throttle
  (`JobCtx::progress_now`, once per raise; later frames throttle as before), so it goes out
  even when the previous frame was under 500 ms old and the call in flight then hangs. Test:
  `labs_batch::control` (the second-Cancel test reads the persisted frame inside the window).

### 3.6 As built (slice 2, UI)

*The review of 2026-10-17 led to thirteen fixes; the four that change the wire are decisions (U3), (U4), (U6) and (U12) (§3.5), drafts.*

- **Sets in the page.** The form column has a Question set card under Examples: the loaded set's
  name and how the form differs from it ("questions edited · another model"), Load ▾, Save as
  set… (a name; the questions and the head model, which becomes the loaded set), Update set
  (enabled only when the form differs; it keeps the loaded set's thresholds of the questions
  still there and sends the `updated_at` it read) and New set run…. A 409 `set_changed` is a
  notice with "Reload the set" (read and load it) and Dismiss. Loading a set replaces only the
  questions (`FormState::load_questions`: state and images stay), drops an override as an
  example does, and makes its model the head model when the lab lists it, else a toast names
  it; with question edits the Load menu arms and loads on the second pick, as Examples do, and
  so does the reference panel's Load (a `ConfirmButton`, "Replace the edited questions?"). Each
  question card ends in its threshold line ("threshold 0.72 · 81% covered, 96.4% correct
  (n = 812) on set run #5 · open #5", "threshold: not measured", "not in the set yet", and
  "edited: Update set keeps the value, not the measurement" for a changed question). The
  reference panel leads with Question sets (Load, Rename inline, Delete with a confirm, "1 of 3
  thresholds measured") and Set runs (newest first: status chip, model and file, progress, a
  link), its pill counting running ones. A write of a set (save, update, rename, a threshold)
  takes the list read's ticket, so a list read already on its way cannot put the old list
  back. The usage line reads "kept · 30 runs · 1 set run ·
  74.1 kB"; Clear's confirm names the set runs.
- **New set run is a modal**, not a route: File (drop, pick or paste; a read file shows as its
  name and size, not in the box; CSV or JSONL by the extension or the first character until
  picked by hand), Mapping, Problems, Preview, Concurrency, and Start in the pinned foot with the
  line that says why it cannot go yet ("1 problem: fix the file or the mapping") or "Start: 30
  items as one job"; the state column is asked for as soon as a preview named the columns, and
  a preview that failed says so there and with Retry above the table. The preview is posted
  350 ms after the inputs stop, ticketed. A mapping field the file leaves open, or that names
  none of its columns, is guessed from the names (state `state`, `text`, `message`, `body`,
  `content`; key `key`, `id` or `*_id`; a question's label column called as its id); a field
  set to none stays none, and a guess that finds no column leaves the field open, to be
  guessed from the next file. A guess is a move (a new preview, the last one stale) only when
  the mapping it sends changed. A score's reading (as the column reads, level texts, level
  indices) appears where a preview problem of kind `score_labels_needed` names the question
  (U12) and stays while it is set. Problems are a table (row, line, column,
  value, the message without the lead those columns say), the first 20 shown with "Show all
  N". The run takes the loaded set by `preset_id` when the form's questions are that set's,
  else the questions as an unsaved `set`, and the modal says Use as threshold then has nowhere
  to write.
- **Price (U6).** The line renders what the server names on the model
  (`DecisionLabModel.price`, §3.5): a local model is free; a priced one lists one row per unit,
  each with its rates, currency, source and scope ("Tokens per 1M tokens: in 0.8 · out 2.4 USD
  (manual, alias)", "Requests per answered request: 0.001 USD (catalog, upstream model 3:jev)"),
  so a unit that falls through to the upstream model's row shows as such; none says Usage
  records the cost as unknown; a name without one route says why. The list is read again when
  the modal opens on a cloud model; a failed read says so with Retry. Unit-tested and
  server-tested only: the dev copy has no priced decision alias.
- **The run's page** is a `PageFrame` document: Progress (a bar of answered, unreadable and
  failed, "34 of 50 · 31 answered · 2 failed · 1 unreadable · 16 to go", the last job's error
  as "Interrupted: …"), the controls and the run's facts; Metrics; Items. While the job runs
  the numbers are its `BatchJobDetail`; a progress frame (any change of its counts: a retried
  item replaces its failed run without moving `done`) redraws the bar, the line and the chip
  alone. The controls are drawn from what they show (`Controls`: running, the next Cancel
  aborts, resumable, retry only, the failures they name), which a frame does not move, and
  Delete's `ConfirmButton` sits outside them, so an armed Delete, a press across a frame and
  the focus stay. The job leaving the feed re-reads the run, the items and the metrics. Once a
  Cancel went to the running job (`cancel_requested`, U4, also after a reload) the button reads
  "Abort the calls in flight" (danger) and its toast says the abort. A done run with failed
  items offers "Retry the N failed"; Resume shows concurrency and, with failures, a "retry the
  N failed" box.
- **Metrics** are read when the page opens with labels, on the bins field's change, on Refresh
  and when the job ends; while it runs a line counts the runs landed since. Per question: the
  accuracy (and MAE) and counts; the matrix, labels as rows and predictions as columns, each
  cell `--accent` at its share of its row (`--accent-ink` on it past 55%), a score's columns
  headed by the level index; the reliability diagram as inline SVG on `charts.rs`'s marks
  (columns of at most 24 px with a 4 px rounded data end, the diagonal, each bin's whole band
  its hover target), every empty bin a track while a bin is at least 3 px wide, else one track,
  and the bins as a table under it. The slider steps over the curve's points, starting on the
  set's cut when it was measured on this run; its line is "at 0.72: 81% covered, 96.4% correct
  (n = 812)", n the labelled answers at or above the cut. **Deviation:** where the point has no
  labelled answer (or the set is unsaved or deleted) Use as threshold is disabled with the
  reason beside it, rather than posting into the 409 `no_labels`; a 409 that comes anyway is
  shown under the button. The saved set is read ticketed: while it reads, the button says so; a
  404 is "the set this run ran is deleted"; any other failure is said with Retry, never as a
  deleted set. Until moved by hand the slider follows the set (a set read late puts it on the
  cut measured here); a cut chosen by hand is kept per question across reads of the metrics (the
  bins, Refresh, the job's end), on the new curve's nearest point.
- **Items** come in pages of 50 (a client constant; "N more · show them" reads one page, and
  the next when the table's end scrolls into view; the sentinel looks again after rows land,
  so the end of an empty table, seen before the first page, loads nothing). A question's cell
  is the compact answer and its confidence (`noul`: the certainty), under it the expected
  answer with ✓ or ✗, or "failed", "unreadable", read by the server's own rule (U3:
  `metrics::outcome` over the run's status and `read_response`, so a 2xx other than 200 reads,
  and a 2xx whose text is no answer is unreadable, not failed). A click opens the item: the
  state whole and the run through the lab's `RunView`. Each row holds its item in a signal of
  its own, found by id, so a re-read item redraws its row alone. A progress frame re-reads only
  the loaded pages holding the first (concurrency + 50) unsettled items (no run, or a failed
  one while the job's detail says `retry_failed`): the job takes items in order, so what landed
  is among them, and a later one is read once the front reaches it; the job's end reads every
  loaded page once. A read that fails is said once, with Retry, and the frames stop asking
  until it is pressed.
- **Child modules** §7.3 does not name: `batch_input.rs`, `batch_mapping.rs`,
  `batch_preview.rs`, `price_line.rs` (the modal), `batch_items.rs`, `batch_status.rs`,
  `item_cells.rs` (the run's page), `batch_list.rs`, `sets_panel.rs`, `set_card.rs`;
  `lab_history` gains `ExportScope::Batch`. `ui-matrix.py` fills
  `/decision-lab/batches/{batch}` from the newest set run.
- **Checked** on a dev instance over a copy of the decision acceptance database, Laya only:
  `scripts/drive/decision-sets.json`, 101 PASS at 1440×900 and at `--dsf 1.25 --size
  1152x768` at first, 118 after the review's fixes (save, 409 and reload, load; an unknown
  label refused naming row 3, line 4, column and value; a mixed score column and its reading;
  30 labelled tickets with a key picked as a file; cancel and resume; the ✓ count against the accuracy; bins 10 and 5; the slider against a
  hand count from the items; Use as threshold and the threshold measured on the card; both
  exports; a one-item run held in flight by a 6,000-token state and aborted by the second
  Cancel; a state past the ubatch failing with llama.cpp's 500, then Retry the 1 failed; 409
  `no_labels`; 120 items in pages at concurrency 2; rename and delete of a set; and from the
  review: a file without a state-like column asks for the state column, a page reloaded after
  the first Cancel offers the abort, the reference panel's Load arms with edits, "show them"
  reads one page and scrolling reads the rest, an armed Delete outlives two progress frames of
  a running 120-item run with the focus on it and its confirm deletes the run, the hand count
  now a boolean check).
  `decision-lab.json` 52 PASS at both sizes (its CSV header now slice 2's). `ui-matrix.py
  --routes /decision-lab,/decision-lab/batches/{batch}`: 14 measurements, 0 FAIL. All three
  again after the review's fixes, with the same counts. `webkit-check.py --backend broadway`
  rendered both before them. By hand: a run stopped by restarting the
  dev instance reads "interrupted at 1 of 3" with "Interrupted: interrupted by shutdown", and
  Resume finishes it.
- **Fix round (2026-10-17), drafts.** (1) A progress
  frame re-reads the first `moved + concurrency` unsettled items, where `moved` is the change of
  the frame's done, failed and unreadable counts since the last read (`frame_window`), not a
  guessed `concurrency + page`. (2) The page takes `cancel_requested` from frames as well as from
  the run (`Progress`), and `take()` ticket-bumps the run read so an older one cannot put the
  old value back (the ticket is taken when the call is sent since the slice 2 acceptance's flake
  fix, §7.3). (3) A held local row's price line says the run is answered by the fallback
  and shows its rows (`DecisionPrice.fallback`, §3.5), or "free". (4) The drive's label no
  longer claims a wait the check does not make; the unit test `a_guess_that_sends_the_same_mapping_is_no_move`
  pins it. (5) The history cards (`ask.rs`, `group_card.rs`) judge a run by `is_2xx`, like the
  server, not by `== 200`.
- **Open:** the price line on a priced cloud decision alias, and an interruption by the GPU hold
  or a cloud 401/403 (the restart's page path, not run live); the Tauri window's export of a
  run stays the owner's check.

## 4. Batch agents through a question set (slice 3)

- **Manifest:** `ItemOutput` gains `decide: Option<String>`, a `config.<field>` path like
  `enum_from`. The config field holds a question set id; its schema says so with `format:
  "lmgw-question-set"`, and the config form renders a select of the owner's sets. Empty means
  today's LLM call, unchanged.
- **With a set,** `classify_one` (`agents/batch.rs:1608`) asks the set's question whose id is
  `output.field`. It must be a `choice` question, whose options are the enum, the fallback
  appended once and last as `enum_values` does (`agents/manifest.rs:630`), so the review select
  keeps its shape; `config.categories` is not read, and the config form says so (Q3). The state
  is the rendered `user` template, already kept as `Row.prompt`; `system` is not sent. The
  dispatch is `proxy::systemone::dispatch` under the agent run's own context, so its log row and
  price name the agent like its other calls.
- **Attention:** `Row.output` is the winner. `Row.attention` is true when the winner's confidence
  is below the set's threshold for that question, or the call failed; a failed call gets the
  fallback as today (`agents/batch.rs:1700-1705`). `Row.raw` is the answer's text, so the details
  modal shows the probabilities. Re-run attention and the early abort are unchanged.
- **The mail labeler** gains the config field `decision_set` and `"decide": "config.decision_set"`
  in its `output`; its version moves from 3.0.0 to 3.1.0. A long mail on a non-causal row can
  exceed the ubatch: that row gets llama.cpp's 500 as its error and is an attention row, visible
  and not truncated. Privacy is the set's model: a CPU decision row without a fallback keeps mail
  on the machine, and a configured fallback always applies.
- **Labels from agent runs:** on a finished apply run, "Save as labelled set" creates a batch not
  yet run (`job_id` NULL). Each item's state is the classify run's `Row.prompt`, its expected
  answer the applied row's output field (Q4). The rows are copied, so later job pruning does not
  touch them; the action is offered while the jobs exist.

### 4.1 As built (slice 3)

*The choices where §4 left room. The review of 2026-10-17
led to fourteen changes, marked (F1) to (F14) below; they are drafts too.*

- **The field** is an `integer` with `format: "lmgw-question-set"` (`QUESTION_SET_FORMAT`), the
  set's id; that format is refused on any other type, the other formats stay a string's. Empty
  (null, absent) classifies with the model call. `decide` belongs to the `enum_from` form only and
  must name such a field (`agents/manifest/decide.rs`).
- **The run** reads the set once as a classify or a re-run starts (`agents/batch/decide.rs`). A set
  that is gone, no question set, has no question of the output field, asks it as another type,
  names no default model, or whose model answers no `/v1/systemone` fails the run before any tool
  call, saying which. With a set no chat model is resolved, and the run reports the set's model.
  Only the output field's question is asked; the body is `DecisionForm::body` with the prompt as a
  text state. Whether a model answers the route is `modelinfo::decision::answers`, shared with the
  set runs' start **(F11)**: the agent runtime no longer reaches into `web::decision_lab`, whose
  `models` is `pub(super)` again.
- **The context** is the agents' identity as `client_key` (`internal:agents`: its scope and budget,
  its `key_id`) and the run's job id as `run`, so each `systemone` row folds into the run's meter
  like a stamped call: `model_calls`, usage and cost in the result count them. (The agent's chat
  turns leave the row's `client_key` empty with the same `key_id`, and are not scope or budget
  checked at all: a gap older than slice 3, left as it is and noted here.)
- **The answer:** the winner is checked with the metrics' `predict` (an option of the question).
  **(F10)** An answer equal to the fallback needs attention, as on the model call's path, whatever
  its confidence. **(F6)** The threshold is applied only when it is measured (Decision 10: "the
  set's measured threshold"); an unmeasured one is kept, shown and not applied. A non-2xx is the
  fallback, attention, and the error `<model> answered <status>: <body as sent>`. **(F1)** An
  error the model itself answered, relayed as it came (no `lmgw_code` in the body: llama.cpp's
  500 over the ubatch, a provider's 5xx), is that row's own failure and never counts toward the
  early abort, so three long mails first no longer abort the run; lmgw's own errors (transport,
  connect, timeout, the 501 of an image without the route) feed it as before. A 2xx that does not
  read is the fallback and attention with the reason, `raw` kept, and counts as the model
  answering. **(F2)** An answer `proxy::systemone` marks `SameForEveryItem` (the policy check:
  a key's budget or scope; every failure of `gate::open`: the hold, an alias gone, admission, a
  container that does not start; the provider refusing lmgw's key) is no row's answer: the stage
  stops at once (the calls in flight are dropped, as the early abort drops them), the row is left
  as listed, and the run fails with "stopped at row <id>: lmgw could not ask <model> (503
  gpu_hold): …. The N of M rows done before it are kept; start the run again once that is
  fixed". The rows are kept the way a failed or cancelled classify already keeps them (the live
  buffer `Run::failure_result` reads), no new mechanism.
- **The stamp (F3):** each row the set classifies carries `set` (`AgentRowSet`: the set's id, its
  name and its `updated_at` as the run read it); a row of the model call has none, and its JSON is
  byte for byte as before. It travels to Apply in the row as the Run tab posts it.
- **The review** offers the set's options with the fallback last while the stored config's set is
  usable, else the categories. `AgentBatchShape.decide` (`AgentDecide`: the field, the question,
  the `enum_from` field, the set, its name, its threshold, **(F6)** whether it is `measured`,
  **(F5)** the `model_field` the model alias is, a problem) feeds the form and the review. The
  attention group says "the fallback, a confidence below the set's measured threshold of 0.7, or
  a failed call", or that the 0.7 is not measured, so not applied; **(F9)** its way out is "check
  them, or change the set or its threshold and re-run these", since a decision model answers the
  same state the same way. The set field's note reads "The set's threshold for 'category' is 0.7,
  not measured: not applied" for an unmeasured one.
- **The mail labeler 3.1.0** no longer requires `model`: with a set nothing uses one (list, the
  set, the script apply). **(F5)** The Run tab blocks Dry run and Re-run attention while the form
  holds neither ("set Model or Question set in Config above — a classify needs one of them"),
  List only stays free, and a classify started without either fails at start saying both ("this
  agent has no model and no question set: … Pick a model or a question set on the Run tab").
  **(F10)** The `decision_set` description says Other stays the review's catch-all and what a
  failed call falls back to, not an option the set's model is given. The seed moves an untouched
  3.0.0 row forward, keeping its config.
- **Save as labelled set** is `POST /decision-lab/api/batches/from-agent {job_id, preset_id?}`,
  answering `BatchFromAgentSaved {batch, skipped, set_changed}`; **(F13)** `job_id` is required (a
  body without it is the extractor's 422, not a 404 "job id 0"). Any ended apply job (done,
  failed, canceled: the labels are the review's, not the writes'), 409 `run_live` before.
  **(F3)** The set is the one stamped on the rows that become items; rows of several sets, or
  stamped rows beside unstamped ones, are a 400 `mixed_sets` naming each set's rows, and rows no
  set classified are a 400 asking for `preset_id`, which stays only as an explicit override. The
  result names the set (`BatchView.preset_name`), and `set_changed` says a stamp names an older
  `updated_at` than the set's now. The state is the received row's `prompt` (the Run tab posts the
  server's rows whole), else the `base_job` row's of that id; a row with neither was never asked
  and is in `skipped` (`kind: never_asked`) with its reason. **(F4)** A row whose call failed with
  its output still the fallback (error set, label the fallback or empty) is in `skipped` too
  (`kind: call_failed`, "its call failed: …"), whether or not the set offers the fallback; the
  400 `bad_input` stays for any other label that is no option of the question, naming each row
  (add the option to the set). An empty label is an item without one. **(F12)** Each item's form,
  and the run's snapshot, is the set restricted to the output field's question (with its
  threshold), the request the agent sends, so the run's measurement, token count and threshold
  describe it; "Use as threshold" still lands on that question of the saved set, which keeps its
  other questions. Its key is the row id; the mapping reads `state: prompt`, `key: id`, `labels:
  {category: category}`; the source is `agent run <job> of <agent>`; concurrency 1; written by
  `insert_lab_batch` as a task of its own.
- **UI:** `widgets/schema_form/question_set.rs` (the select, none first then `name · model`;
  **(F7)** a held id that is in no list reads "set N" while the sets load or when their read
  failed, which says so with Retry, and "set N · not there" only once the list has loaded; the
  notes) and `pages/agent_detail/save_labelled.rs` (**(F8)** offered on a finished apply run whose
  rows carry a set stamp; its line names the set, the rows left out by kind, and a changed set,
  and links to `/decision-lab/batches/<id>`). The drive is `scripts/drive/agent-decide.json`, its
  fixture `scripts/agent-decide-seed.py` (the set and a stamped apply job); **(F14)** its cleanup
  checks every delete's status.
- **Second review round (F15 to F21; drafts):**
  **(F15)** a relayed 4xx or 500 without `lmgw_code` is the row's own failure and never counts for
  the early abort; a relayed 502, 503 or 504 does (a loading server, a proxy in front of a dead
  one, a cloud outage), as before F1. **(F16)** `for_run` also resolves the gate for the set's
  model (`RouteCheck::Decision`, nothing started), so the hold already on refuses a set classify
  before any tool call as `model '<alias>': ...`; a held row with a usable fallback passes.
  **(F17)** a re-run keeps the base run's values for every target row it did not re-ask (the
  stage's rows that carry no answer, prompt, raw reply, error or set), in the live view and in
  the stored result, so a stop (the hold), a Cancel or the early abort leaves them in "Needs
  attention" as they were. **(F18)** not done: the apply input carries no marker of the owner's
  edit, so a failed-call row set by hand to the fallback is still skipped as `call_failed`; an
  edited-row flag on `Row` would be the change. **(F19)** `AgentRowSet.digest` stamps a digest
  (SHA-256) of the output question's definition at classify time; `set_changed` is true only when
  it differs from the current question's (`updated_at` stays in the stamp; a stamp without a
  digest reads as unchanged), so a "Use as threshold" write no longer reports a change.
  **(F20)** the `mixed_sets` message says to classify the rows again through one set, or pass
  `preset_id`. **(F21)** `scripts/agent-decide-seed.py` refuses `LMGW_PROD_DIR` and
  `XDG_DATA_HOME` as `scripts/dev-copy.sh` does, and opens the DB read-only before it makes the
  set on the gateway, so a wrong path leaves nothing behind.
- **Left as they are, noted:** the decide rows' `client_key` `internal:agents` beside the same
  run's chat turns with an empty one (above); "Apply N checked" and "Re-run attention" are offered
  on the apply run's own view too, as before slice 3.
- **Checked live** on a dev copy (prefix `lmgw-dev3`, a fixture apply job of three rows): the
  drive 23 PASS at 1440x900 and at 1.25 / 1152x768; WebKitGTK through Broadway renders the page;
  the saved set run resumed on Laya (CPU) answered both items (568 and 90 ms), accuracy 1.0, one
  at confidence 0.37, which the agent would put under a 0.7 threshold. Not live: the mail labeler
  classifying through a set on a mailbox (no `gws` server on a dev copy); the tests carry it.
  **After the fixes**, on a fresh copy seeded by `scripts/agent-decide-seed.py` (four rows, three
  stamped, one failed call): the drive 24 PASS at 1440x900 and at 1.25 / 1152x768 (the
  attention band keeps "Needs attention" whole and ellipsizes its reason, the whole of it in the
  tooltip); a save through the API without `preset_id` took the stamped set, skipped 18f3
  (never asked) and 18f4 (call failed), and a body without `job_id` was a 422; resumed on Laya
  (CPU), both items asked `category` alone (836 and 112 ms), accuracy 1.0. Not live, as
  before: a classify on a mailbox, and the Run tab's "Model or Question set" gap, which the
  unregistered `gws` server's own gap stands in front of on a dev copy (the UI test carries it).

## 5. Audio and Image labs on the store (slice 4)

- **Image lab:** `generate` and `edit` record a group and a run per dispatch through
  `labs::record`. Each `data[i].b64_json` becomes an output artifact (sniffed PNG, JPEG or WebP),
  `lab-artifact:<id>` in its place in the stored response. An edit's image and mask are input
  artifacts; the stored request is the multipart summary the lab already builds
  (`ImageEditSummary`) with the files as refs.
- **Audio lab:** speech output is teed into an output artifact as it streams to the page; a
  stream the page stops is recorded as far as it got, `error_code` `stopped`. An uploaded
  recording for transcription or alignment is an input artifact; task-run answers are text. The
  voice library (`/audio-lab/api/refs`) stays where it is: reference input, not history.
- **Presets** `image_prompt` (the `ImageGenForm`) and `audio_settings` (the speech form). Both
  pages pass `kept` to `LabFrame`, and their module docs lose "nothing is kept". Before slice 4
  ships, the owner sees the sizes measured on a dev copy (Q1).

### 5.1 As built (slice 4)

*Built 2026-10-17. The choices below that this section's outline did not make are the drafts.*

- **Image lab, a group then its run** (two requests since the review below; it was one).
  `POST /image-lab/api/groups` (the `ImageGenForm`) and `/groups/edit` (the edit's upload) keep a
  group of one cell (plan variant `generate` or `edit`, form `ImageLabForm`: the `ImageGenForm`
  flattened, with an edit's `image` and `mask` as `{id, name}` of its input artifacts) and answer
  it with no run; `POST /image-lab/api/groups/{id}/run` (no body) builds the request from the
  stored form and inputs and runs it. A request the page stops before the route answered leaves
  its group with no run, which the history shows as stopped once it is no longer in flight. Once
  dispatched, the run route answers 200 with
  `ImageLabAnswer { group, store_error }` whatever the route said, Decision 20 applied to the
  Image lab: the route's status, code, message and answer are in the run. Only the lab's own
  refusals keep their status (400 `image_lab_form`, 500 `lab_store` when the group cannot be
  kept, 404 `not_found`, 409 `cell_answered`, 500 `cell_unbuilt`, 404 `file_missing`, all in the
  OpenAI envelope, nothing sent). `ImageGenerateResult`, `ImageEditResult` and `ImageEditSummary` are gone;
  the multipart summary is `lab_store::LabMultipartSummary`, each file with `artifact:
  "lab-artifact:<id>"`. Each `data[i].b64_json` that decodes is an output artifact named
  `<model stem>-<i>.<ext>` (the answer's `output_format`, else the form's), spliced in place
  through borrowed `RawValue`s (`web/image_lab/outputs.rs`); the run row is inserted first and its
  texts rewritten once the outputs have ids (`labs::record::run_with`, `set_lab_run_texts`). A run
  the store cannot keep is answered with no `id` and its pictures inline. An edit's `form` part
  may name a kept picture (`image`/`mask`: `{id, name}`, a picture the store keeps) instead
  of an upload, so Open in form and "Send to edit" on a kept picture upload nothing; an upload
  part wins over the ref, an unknown id is the 400 naming `lab-artifact:<id>`. The edit's
  in-process request now carries the route's "no body limit" (`DefaultBodyLimit::disable` as a
  layer around the handler): it had none, so axum's multipart reader stopped at its default
  2 MB and an edit of a larger picture was a 400, a limit found here because a kept 1024² PNG is
  about 2 MB. The Audio lab reads an upload under its route's own mark the same way.
- **Audio lab, two requests.** The hand-offs keep their `/v1` bodies, so the form travels
  first: `POST /audio-lab/api/groups` (`AudioAsk { model, route, form: AudioLabForm }`, the page's
  whole form: task, panels, `AudioSettings`, `AsrForm`, `TaskForm`) makes the group, and the
  hand-off with `?group=<id>` (`GroupQuery`, and `group` in `DetailsQuery` and `TaskRunQuery`)
  records the run. The plan's variant is the `AudioRoute` (`speech`, `transcription`,
  `transcription_details`, `alignment`, `task_run`, `task_stream`); a group of another lab or none
  is 404, one made for another route 400 `wrong_route`, a request naming another model 400
  `wrong_model`, one answered or in flight 409 `cell_answered` (the cell claim of §2.6), all in
  the OpenAI envelope. Without `group` the
  hand-offs relay as before and keep nothing. The request is read whole and kept as sent (a JSON
  body byte for byte; an upload as `LabMultipartSummary`, its file parts the group's input
  artifacts, inserted with the run after the group's own inputs). The answer is **teed**: each
  chunk goes on at once, a copy is kept, and the run is recorded before the last byte goes on (at
  the body's end, or at its declared `Content-Length`'s last byte, where the server may drop the
  body without polling its end). Speech that succeeded is an output artifact named `<model
  stem>.<ext>` (`wav`, `sse`, `pcm`, `json`, …), spooled to its file as it streams, its
  `response` NULL; a task's base64 audio is an output artifact each, named in place; every other
  answer is its text. A body dropped before its end (the page's Stop) is recorded on a task of its own as far
  as it got: the route's status, `error_code` `stopped`, "the page stopped the answer after N
  bytes". The page reads the group after the answer ends until it is no longer in flight (a
  stopped run is recorded when the gateway sees the page go).
- **Presets.** `LabPresetKind` gains `image_prompt` (body `ImageGenForm`, checked by
  `check_fields`: the numbers, half a size and sd.cpp's delimiters, no model or prompt required)
  and `audio_settings` (body `AudioSettings { speech, pcm }`, checked by the speech builder and the
  PCM numbers). `SpeechForm` and `speech_body` moved to `lmgw-api-types::audio_lab::form` for that
  check; the page re-exports them. A kind posted to another lab is the 400 of §3.5. The Audio lab
  offers the card on its speech panel only.
- **CSV.** The Image and Audio labs have their tables (`labs/export_csv_media.rs`): a row per
  run, `group_id, run_id, created_at, endpoint, model_asked, answered_by, fallback_reason,
  http_status, error_code, error_message, latency_ms, response_type`, then the Image lab's
  `prompt, negative_prompt, size, n, steps, seed, output_format, images, image_bytes,
  artifact_ids` or the Audio lab's `task, input, voice, language, text, files, file_bytes,
  artifact_ids`. A group with no run has no row.
- **UI.** `pages/lab_presets.rs` is one presets card for any lab and kind (Load, Update from the
  version read, Delete with a confirm, Save as preset). The cards are `image_lab/card.rs` and
  `audio_lab/card.rs`; a kept audio file that a player cannot play as it is (an event stream, raw
  PCM, a JSON envelope) is fetched and wrapped in a WAV with the form's PCM format, the gateway's
  `x-lmgw-sample-rate` winning over the rate. Open in form is `image_lab/kept.rs` and
  `audio_lab/kept.rs` (an audio upload is fetched back from the store into the file slot). The
  usage line names the files when a lab keeps any ("1 run · 1 file · 1.9 MB",
  `lab_history/usage_line.rs`, shared with the Decision lab); `lab_frame.rs` is unchanged.
- **Checked:** `image_lab.rs` and `audio_lab_wire.rs` (its child `kept`) grew the cases of §7.5,
  `lab_presets_media.rs` the presets. `scripts/drive/lab-store-media.json` passes at 1440×900 and
  at `--dsf 1.25 --size 1152x768` (32 PASS, 0 FAIL each); `ui-matrix.py --routes
  /image-lab,/audio-lab` is 0 FAIL at its seven sizes; `webkit-check.py --backend broadway`
  renders both pages. On the dev copy, a generation and a speech clip survived a restart, the
  usage was the same, and both labs exported as a zip of the JSONL and the files. Not run live:
  an edit (no edit pipeline on the dev copy; flux2-klein-4b is a 7.1 GB download; the cloud
  fakes of `image_lab.rs` cover it), a stopped speech stream (the copy's TTS row is offline; the
  integration test streams and stops one), the Tauri window's export toast (the owner's check).
  The review's re-check is below.

**Sizes, measured (Q1)** on a dev copy, 2026-10-17: z-image-turbo (the recipe's Q4_K
diffusion model and Qwen3-4B Q4_K_M encoder, 8 steps, sd-server on a 24 GB GPU), kokoro 82M
Q8_0 (offline, CPU, 8 threads; 24 kHz mono 16-bit WAV) and canary 180M Flash Q8_0 (CPU).

| What | Kept file | Kept text |
|---|---|---|
| a 1024×1024 PNG generation (the row's default), 10 prompts | 1.39 to 2.20 MB, mean 1.9 MB | about 600 B a group |
| the same prompt and seed as JPEG / WebP | 0.96 MB / 0.41 MB (PNG 2.20 MB) | |
| 512×512 PNG (two in one request) / 768×512 PNG | 0.41 and 0.45 MB / 0.72 MB | |
| a speech clip | 48 kB per second of speech: 6 characters 65 kB, 110 characters 325 kB, 258 characters 898 kB (18.7 s); mean 359 kB over 8 clips of 6 to 258 characters | about 850 B a run |
| a transcription of a kept clip | none new: the upload is the clip's file (same digest) | about 850 B (answer 190 B) |
| a failed run (a refusal, the engine's 400/502) | none | 0.5 to 1 kB |

A session of 8 generations and 8 clips added 18.0 MB of files (15.1 MB of pictures, 2.9 MB
of speech) and 11.7 kB of text to the database; the files are everything that matters, and
none of them is in `lmgw.sqlite`. At that rate 50 generations a day are about 95 MB a day,
about 2.8 GB a month; speech at a minute of audio a day about 85 MB a month. The page's answer
for a generation is the 1.5 to 1.8 kB group, where it used to carry the picture as 2.5 MB of
base64. The size line shows the total in view; a retention rule stays the owner's call (§8 Q1).

*Deviation:* the dev copy's model files were downloaded into its own data dir (the scratch copy
under `target/`), not into a local model cache outside the data dir: a dev instance writes no models
directory outside its data dir (`config::dev_models_dir_refusal`). Nothing came from a real
data dir.

**Review fixes, 2026-10-17.** *An adversarial review of the slice found the points below; how
they were fixed is a draft.*

- **The Image lab answers what it holds.** The run route reads the group and its inputs before
  the dispatch and builds `ImageLabAnswer` from them and the recorded run, reading nothing back:
  a group deleted meanwhile (a Clear) is a `store_error` ("group N was deleted before its run was
  in", checked in the record's own transaction, `store::lab_group_in`), and the pictures still
  come back inline. It used to answer 404 and drop them.
- **A record outlives its client.** Both labs read and record an answer on a task of their own
  (`tokio::spawn`, the handler awaits its `JoinHandle`) that holds the cell's claim until the run
  is in: a Stop once sd-server answered, or a page that goes while a speech run is written, no
  longer takes the run (or its picture) with it. A Stop before the route answered still drops the
  dispatch, as §2.6 has it.
- **In flight.** `LabGroupView.in_flight` says a cell of the group is claimed now
  (`labs::claims`, `views::mark_in_flight`, on every history read, one group and a set run's
  items). A group in flight cannot be deleted: the 409 `in_flight` ("stop it, or delete it once
  its run is in"); the cards turn Delete off while it is. Clear is not refused (it empties the
  lab; a run landing after it is answered as not kept). *Draft:* a refusal rather than a delete
  that waits, because a wait would hold the request open for as long as the generation takes.
- **The pages hold the group from its creation.** The Image lab now makes its group first (the
  split above), so both pages put the group in front at creation, its card the live one
  ("rendering…" with Stop in the Image lab; the Audio lab's live run, its result and Stop, inside
  the group's card). `LabHistory::prepend` counts a group once when a reload had it already.
  After a Stop (or any end) the page reads the group until `in_flight` clears
  (`LabHistory::settle`, a read every 250 ms as a pace, no count: it ends when the claim is
  released or the page is left), so the stopped run lands with what it kept; the fixed 400 ms
  re-read is gone. A page read (a reload, an older page) that brings a group in flight follows
  it the same way, so a reload shows it in flight and then its run, never as stopped.
- **An audio answer that was not kept keeps its live card.** When an end that was not a Stop
  finds no run (the store failed, the lab refused the hand-off, the connection broke), the live
  card stays with the clip still playable and the group's note says "not kept: …" with the
  reason when the page knows it (the refusal's or transport's message), else "the gateway could
  not record it; its log says why". It goes at the next run, and the card then shows the note.
  Refusals and transport errors show in the card as errors.
- **No base64 audio in the database.** Speech is spooled to `labs/artifacts/spool-<pid>-<n>` as
  it streams and adopted under its digest by the record (the digest, size and first 64 kB for the
  sniff read back from the file, so they are what it holds even after a cut-off write; a spool
  being written is no orphan, one a crash left is). A task's answer (`/v1/tasks/run`,
  `/v1/tasks/stream`) is held whole, because the route relays audio.cpp's one buffered document
  and its audio is found by reading it as text: each `audio` string member anywhere in it
  (`audio`, `named_audio_outputs[i].audio`, an event's `audio`) that is canonical base64 written
  without escapes becomes an output artifact (`<model>[-<output id>].<ext>`), `lab-artifact:<id>`
  in its place (`web/audio_lab/task_audio.rs`); a kept task run plays those files. Measured on
  the dev copy: kokoro through `/v1/tasks/run`, 2.0 s of speech, kept as a 96 kB WAV and 128 B of
  text where it was 128 kB of base64 text.
- **The hand-off names the group's model.** A request whose `model` (the JSON body's, an upload's
  field) is not the group's plan model, or names none, is the 400 `wrong_model` naming both,
  before anything is sent and without taking the group.
- **Kept picture refs are pictures.** A ref to an artifact that is not `image/*` is the 400
  naming `lab-artifact:<id>` and its type. The relayed part carries the kept file's own name and
  type, and every part's filename and type go through one sanitiser (no quote, backslash or
  control character) on their way into the multipart header.
- **Upload parts.** A part is a text field only when it has no filename, no type of its own but
  text (`text/*`, JSON) and is UTF-8; any other is an input artifact, named after its field when
  it has no filename.
- **Smaller.** A record whose file writes fail part way unlinks the files it wrote before
  (`write_files`). `NewArtifact` carries `Bytes`: a picture is decoded once and taken, not
  copied, for its artifact; an upload part's bytes go to the record as read.
- **The run facts** (`gateway_headers`, `failure_of`, `answer_of`) live in `labs/run_facts.rs`
  for the three labs.
- **UI.** Open in form tickets the fetch of a kept upload (a later one, or a file picked by hand,
  wins); a kept card's fetch of its file lives on the card's own scope, so a card gone first makes
  no object URL; the labs' own fetches (the kept upload, a kept answer's file, the Image lab's
  group and run posts) go through `http::checked`, so a `401 session_required` raises the login
  gate; Enter in a preset's name field does nothing while a save is out.
- **Verification round (2026-10-17), drafts.**
  (1) An Image generation that lands after a Clear keeps its pictures on a card above the
  history, with the "not kept" note, until the next run, as the Audio page does. (2) An Audio
  card rebuilds only its runless branch on a `live` or note change (a `Memo` of its own state);
  a kept run is built once per run id and its fetch is aborted with the card, so a playing clip
  is not stopped and its file not fetched again when another run starts. (3) The Decision lab's
  card shows a group in flight in another tab as in flight, with Delete off. (4) A task answer's
  `audio` string becomes an artifact only when its bytes sniff as audio (the labs' own sniffer),
  so `"none"` stays text. (5) `orphans` reads the spool registry after it has read the directory,
  so a spool begun in between is not taken for an orphan. (6) The new Audio and Image logic moved
  into child modules (`audio_lab/kept.rs`, `image_lab/kept.rs`).
  (7) The sniffer knows a Standard MIDI File (`MThd` + header length 6, `audio/midi`, `.mid`), so
  a task answer's base64 MIDI under `audio` becomes an output artifact as well; the kept card lists
  it as a track with its file, the browser plays no MIDI, and the live result has no special MIDI
  handling either.

*Checked:* new cases in `image_lab/in_flight.rs` (in flight while its run is out and its delete
the 409; a group cleared mid-generation still answers its pictures, `store_error` set and no file
left; a client gone once the route answered does not take the picture; a non-picture ref refused,
a ref sent under its kept name), `audio_lab_wire/hand_offs.rs` (`wrong_model`; a task's audio
kept as files and named in place; a filename-less file part kept as a file; a speech run kept
when its client goes while it is written) and `labs_store.rs` (a record failing part way leaves
none of its files), the speech and stop cases of `audio_lab_wire/kept.rs` now also asserting no
spool is left. Live, on a scratch copy of the dev database (container prefix `lmgw-dev4`, the
model files reflinked from `<model-cache>/lab-media-slice4/` into its data dir, kokoro and
canary on the `localhost/lmgw-audio-cpp:audio-main` image, the copy's default audio.cpp image
lacking eSpeak NG): `scripts/drive/lab-store-media.json`, grown by a generation whose group is
in flight from its creation (its Delete off) through a history reload (the model filter) until
its picture lands, and a Stop of a long kokoro synthesis that settles as stopped, passes at
1440×900 and at `--dsf 1.25 --size 1152x768` (43 PASS, 0 FAIL each); `ui-matrix.py --routes
/image-lab,/audio-lab` is 0 FAIL at its seven sizes. *Not run live:* a stopped speech **stream**.
kokoro cannot stream: a streaming-mode kokoro row answers the event `Kokoro TTS only supports
offline sessions` (on both images) although the catalog lists `streaming` for the family; the
integration tests stop a stream.

## 6. Out of scope

Saved question sets as MCP tools or any lmgw-own classify route (Decision 11); import of an
export; ops or self-admin tools over the store; other lab kinds; cost estimates before a set run
beyond the price row; `lmgw-client` methods (the store is the dashboard's).

## 7. Slices

### 7.1 Slice 1: the store core and the Decision lab

| WP | Delivers |
|---|---|
| WP1 | the store core: tables, artifacts, record, usage, export, `/labs/api/*` |
| WP2 | the Decision lab's server: models, count, groups, cells, the shared builder |
| WP3 | the UI: `LabFrame` `kept`, `lab_history`, the Decision lab page, nav and route |
| WP4 | live acceptance on a dev instance (§7.2) |

Hub files hold only `mod` lines and re-exports (`store.rs`'s pattern, `:176-392`); new logic goes
in the child modules named here. Integration tests are modules of `tests/it/main.rs`.

**WP1, the store core.**
- *lmgw-core:* `migrations/0083_lab_store.sql`; `store/lab_groups.rs`, `store/lab_runs.rs`,
  `store/lab_artifacts.rs` (with `pub use` lines in `store.rs`); `labs.rs` hub with
  `labs/artifacts.rs` (dir, ingest, read, unlink, the lock, the orphan scan), `labs/record.rs`
  (`NewGroup`, `NewRun`, one `begin_write` each), `labs/usage.rs`, `labs/export.rs` (JSONL, zip),
  `labs/export_csv.rs` (the Decision lab's columns), `labs/csv.rs` (`csv_field`, moved);
  `web/labs_store.rs` hub with `web/labs_store/groups.rs`, `export.rs`, `artifacts.rs`; the
  routes in `labs_routes()`, the `CAPABILITY_TABLE` rows, `API_PREFIXES`;
  `openapi/planes/labs_store.rs` and the `labs` tag.
- *lmgw-api-types:* `lab_store.rs` (`LabKind`, `LabCell`, `LabGroupView`, `LabRunView`,
  `LabArtifactView`, `LabGroupPage`, `LabUsage`, `LabExportQuery`, `LabExportLine`,
  `LabCellResult`, all with the `schema` derive); `ordered_json.rs`.
- *Tests:* `labs_store.rs`: insert, keyset paging newest first with the total; deleting a group
  cascades runs and artifacts and unlinks only files nothing else names (an artifact shared by
  two groups survives one delete); clear; usage against hand sums; an orphan file counted, then
  removed; history across a restart (`AppState::init_for_tests_on`). `labs_export.rs`: every
  JSONL line reads into `LabExportLine` and round-trips (`common::round_trips`); a scope with
  artifacts is a zip whose files hash to their rows; the CSV header, a row per run and question,
  RFC 4180 quoting of a state with quotes and newlines; the `Content-Disposition` names.
  `migrations/lab_store.rs`: 0083 on a database at 0082, as
  `migrations/upstream_catalog_all_modalities.rs`. `route_walk` and `openapi_coverage` pass with
  the new rows and DocRoutes. `ordered_json` unit tests: order at every depth, escaped strings.
- *Done when:* `bash ci/check.sh` is green (fmt, clippy, the json ratchet unchanged or lower,
  `cargo nextest run`, `cargo test --doc`).

**WP2, the Decision lab's server.**
- *lmgw-api-types:* `decision_lab.rs` hub with `decision_lab/form.rs` (`DecisionForm`, `check`,
  `body`, `title`, `Variant`), `decision_lab/examples.rs`, `decision_lab/models.rs`
  (`DecisionLabModels`, `DecisionLabModel`, `DecisionFacts`, `DecisionLimit`),
  `decision_lab/ask.rs` (`DecisionAsk`, `DecisionCount`, `DecisionCounted`).
- *lmgw-core:* `proxy/systemone.rs` gains `pub async fn dispatch(state, ctx, body: Bytes) ->
  Response`, which `handle` calls after its extractors; `modelinfo/decision/limits.rs` (child of
  `modelinfo/decision.rs`, §2.2's rules); `web/decision_lab.rs` hub with
  `web/decision_lab/models.rs`, `count.rs`, `groups.rs`, `cells.rs`;
  `openapi/planes/decision_lab.rs`, the `decision-lab` tag, the rows and `API_PREFIXES`.
- *Tests:* `decision_lab.rs` on `support/decision_world.rs`'s wiremock decision container: the
  model list carries a non-causal row's facts and limits; cells dispatch through the real
  handler (the mock received exactly the builder's bytes with `model` rewritten, the request log
  has an ingress `systemone` row, the run stores request and response verbatim); option order
  survives an object state and a reversed variant; an upstream 400 and 500 are stored with status
  and message while the cell answers 200; a held GPU row gives a 503 `gpu_hold` cell
  (`support/gpu_world.rs`) and a fallback's cell stores `answered_by` and the reason; an override
  goes out with only `model` changed; Perturb with an override is a 400 saying why; a repeated
  cell is a 409; images become input artifacts named in the stored request; count answers per
  piece with `flattened` and starts no container unless asked. `decision_lab_wire.rs`: every
  answer validates against its documented component, as `image_lab_wire.rs`. Unit tests in
  `lmgw-api-types`: `body` order, a JSON state verbatim, Text mode as a JSON string, each
  variant, `check`'s messages, the preview equal to the dispatched bytes, every example parsing
  into `SystemOneRequest`; one `limits.rs` test per shape.
- *Done when:* as WP1.

*As built (WP1, WP2):* child modules the list above does not name: `labs/views.rs` (rows as the
API's views, shared by the history and the export), `labs/delete.rs` (a group or a lab, rows then
files), `labs/claims.rs` (the cells in flight), `web/labs_store/errors.rs` (an unknown lab's 404,
a store failure's 500), `web/decision_lab/images.rs` (an override's data URLs as refs and back)
and `web/decision_lab/model_member.rs` (an override's missing `model`). The
Decision lab's form types and builder landed with WP1, whose tests record decision groups. The
held-row test runs on `decision_world` with the hold set, as `systemone.rs` does, not on
`gpu_world`.

**WP3, the UI** (`lmgw-ui/src/pages`).
- `lab_frame.rs` gains `kept`; `lab_history.rs` hub with `lab_history/client.rs`, `list.rs`,
  `export_menu.rs`, `usage_line.rs`; `decision_lab.rs` hub with `decision_lab/page.rs` (the
  `LabFrame` wiring, the `Lab` state), `head.rs`, `state_editor.rs`, `images.rs`, `gauge.rs`,
  `questions.rs`, `question_choice.rs`, `question_score.rs`, `question_noul.rs`, `compare.rs`,
  `request.rs`, `answers.rs` (the three views), `group_card.rs`, `examples_menu.rs`.
- The nav entry (`shell.rs`, after `:620`), the route (`app.rs`, after `:61`), the export in
  `pages/mod.rs`, `.dl-*` rules in `assets/app.css`, `scripts/drive/decision-lab.json`.
- Native unit tests in the children: bar order, the score marker's position, the `noul`
  certainty, the `Check` states, the plan line's text.
- *Done when:* `ci/check.sh` is green with a fresh dist (check its mtime before trusting a
  screenshot), and WP4 passes.

*As built (WP3):*
- **`LabFrame`** takes `kept: Option<Signal<Option<LabUsage>>>`, `None` inside until the usage
  was read (the note then reads "newest first · kept", no size is guessed), and a second
  optional slot, `results_tools`, for the results head's model filter and Export menu. In a kept
  lab `results_n` is the kept groups, the one being asked among them. Counts use the app's
  `fmt::grouped` (thin spaces), not commas.
- **Four child modules** the list does not name: `decision_lab/form_state.rs` (the form as the
  page edits it, every question, option and level with a page-own uid for the keyed lists; a
  question keeps each type's fields, so switching its type and back loses nothing), `ask.rs`
  (Ask, the cell loop, Stop, the action bar's `Check` and plan line), `run_view.rs` (one cell
  in full: answers, footer, Raw) and `reference.rs` (the folded panel). `lab_history`'s filter
  sits in `export_menu.rs` beside the menu.
- **The score marker** sits at `(score + ½) / levels` across the strip, the centre of a whole
  level's cell, rather than `score / (levels − 1)`, which put level 0 on the strip's left edge.
- **Kept images in an override** stay `lab-artifact:<id>` in the override's text ("Edit as
  JSON", "Open in form"), and the banner says what one is; the server sends the data URL (§2.4,
  *As built (WP3 review)*). The preview shows a kept image as `<data_url_head>,… 1.4 MB` from
  the artifact's head and size, as it shows a new one.
- **The head model** of a fresh page is one that is running, else a CPU row, else a cloud model,
  else the first: opening the lab never proposes a cold GPU row.
- **The history** reads 20 groups a page (a client constant, the server sets none) and the next
  page when its end scrolls into view (an `IntersectionObserver`), "N older · show them" until
  none are left. The model filter is a select over the lab's models in the results head; with
  one set the Export menu adds "<model> as JSONL" and "<model> as CSV". A file keeps the name
  the server's `Content-Disposition` gives it.
- **History reads** (review): a reload, a filter change, a delete and Clear start a new
  generation; an older page lands only on the generation and the list end it was asked from, a
  first page read again keeps the runs that landed meanwhile, and the usage reads take tickets
  (the newest asked wins). An older page that fails stops the reading on: the list's end says
  "couldn't load older groups: <why> · Retry" once, no toast, until Retry or a new list. An Ask
  the model filter would hide clears the filter first, with a toast, so its card is in view.
- **The results head and the action line wrap** rather than cut: the count and the kept size
  stay whole, the tools (filter, Export, Clear) take a right-aligned line of their own when the
  column is narrow, and the action bar's line wraps to show all it says. Stop ignores the
  second click of a double-click (`MouseEvent.detail`), which would land on it when Ask is
  double-clicked.
- **A card follows its own group**: its cell in flight and its notes through per-card memos, so
  a cell of another group landing leaves it as it is (an open Raw stays open). A cell's note
  goes when a kept run answers the cell (it landed, or a reload brought it). A matrix cell of a
  200 that is no answer says "not an answer: …". The cell posts read their refusals as every
  call does, so a `401 session_required` raises the login gate (the kit's `http::checked`).
- **The group card** puts "#id · time · plan" under its title, and its menu is Open in form,
  Export as JSONL, Export as CSV, Delete. Open in form and an example arm on the first pick and
  load on the second while the form has edits (the menu's own confirm, as Delete). A Δ line is
  written only where the variant changes the question's type: `choice` for both, `noul` for
  descriptions dropped. A failure line leaves out an error code that repeats the status
  (llama.cpp's envelope carries `"code": 500`).
- **The gauge** keeps the last count up while the next runs (unless it was another model's), and
  says "measured on the last answer: N input tokens" for the head model. Another head model
  clears the bars and the per-question counts at once, and every change of its input, an idle
  one included, takes a count ticket, so a count on its way never lands over a newer state.
  What follows each keystroke (the check, the gauge's trigger, the preview, dirty) reads the
  form without copying its images' data URLs, and the override is parsed once per edit.
- **Checked:** `scripts/drive/decision-lab.json` (Ticket triage on `decide/laya` as written,
  perturbed, and through an override with two options swapped by hand; the JSONL and CSV read
  back; delete, reload, Clear) passes at 1440×900 and at `--dsf 1.25 --size 1152x768`;
  `ui-matrix.py --routes /decision-lab` is 0 FAIL at its seven sizes (it has no `--dsf`, so the
  125% fit is the drive's); `webkit-check.py --backend broadway` renders the page.

### 7.2 Slice 1 acceptance (live, on a dev instance)

A dev instance only, through `scripts/dev-instance.sh`, never a real data dir. It reuses
decision-models slice 1's rows and files:
- `<model-cache>/decision-acceptance/models-aux` holds `Laya-Q8_0.gguf` and
  `ggml-org/Kev-4B-GGUF/Kev-4B-Q8_0.gguf`; that directory's `lmgw.sqlite` has row 1, Laya
  (`backend: cpu`, ctx = ubatch = 8192), and row 2, Kev-4B (GPU, ubatch 2048). Copy the database
  with `sqlite3 -readonly … ".backup <scratch>/lmgw.sqlite"` and start with
  `LMGW_DATA_DIR=<scratch>`; the models dir is only read.
- Laya runs on `ghcr.io/ggml-org/llama.cpp:server` (b11561); the class default `server-cuda`
  b11176 cannot load it (decision-models §6.1 step 9).
- `<model-cache>/acceptance-scripts/laya-7669.json` and `laya-8519.json` carry a long state
  and an over-long one.
- Julia-1, the other small CPU model, is not on disk; `hf_add target=aux` from ggml-org's
  Decision models collection fetches it.

Steps:
1. `decide/laya` is listed: type `laya`, non-causal, ctx 8192, ubatch 8192 (row), CPU with its
   threads, text only, its state. With the card free, `decide/kev-4b`: causal with a head,
   ubatch 2048, GPU.
2. Ticket triage on `decide/laya` answers; the three views render with latency and
   `input_tokens`; Logs has a `systemone` row of class `aux` at cost 0; the exported request
   equals the preview.
3. The gauge is under 8192 with the 7,669-token state, and over (amber, with its rule) with the
   8,519-token one; asking anyway puts llama.cpp's 500 in the cell verbatim; both runs are kept.
4. Perturb with both boxes, compared against Kev when the card is free (else Laya alone): one
   card with 2 × 3 (or 1 × 3) cells, reversed `#` positions in the reversed cells, Δ lines.
5. With the GPU hold on, Laya's cell answers without a fallback chip; Kev's is a 503 `gpu_hold`,
   or answers through a configured fallback with the amber chip.
6. An override that swaps two options by hand is sent and stored in that order, and the answer's
   probabilities come back in it.
7. History survives a reload and a dev-instance restart; the usage line matches a `SELECT` of the
   sums; one group deletes; Clear asks, then empties the lab and the artifacts dir.
8. `scripts/ui-drive.py scripts/drive/decision-lab.json` exports JSONL and CSV in headless Chrome,
   and both read back as specified. In the Tauri window (the owner's manual check) the export
   lands in Downloads with its toast.
9. `scripts/ui-matrix.py --routes /decision-lab` passes at every size and at `--dsf 1.25`;
   `scripts/webkit-check.py --backend broadway` renders the page.

Outside the gate: an image attachment on OpenJev or Clef (a 27B download; the fake container
test covers it until then), and Jev through OpenRouter (not run against the cloud upstream, as in decision-models
§6.2; the wiremock test covers the cloud path).

*Accepted (slice 1), 2026-10-16:* on a dev instance at 127.0.0.1:8897 over a copy of the decision acceptance database, Laya on `ghcr.io/ggml-org/llama.cpp:server`; the GPU was otherwise idle, so Kev ran too.
1. Pass. `/decision-lab/api/models` and the head: `decide/laya` is type `laya`, non-causal, ctx 8192, ubatch 8192 (row), the CPU with its configured threads, text only, state ready, head budget 192; `decide/kev-4b` is causal with a head, ctx 262144, ubatch 2048 (row), GPU, text only.
2. Pass. Ticket triage on Laya answers with the three views; the footer reads latency and `input_tokens` (219 tokens at 220 to 250 ms a question on the short state); the stored request equals the preview byte for byte; `request_logs` has `systemone` rows of class `aux`, status 200, cost 0, price source `free_local`. The JSONL and CSV exports read back (step 8).
3. Pass. Triage with the 7,669-token state: gauge 7,672 of 8,192 (blue), answered, 46,031 ms and 23,019 input tokens for its three questions. With the 8,519-token state: 8,518 of 8,192 in amber with the rule shown, Ask stays enabled; asking anyway stores HTTP 500, code `server_error`, message `input (8516 tokens) is too large to process. increase the physical batch size (current batch size: 8192)` verbatim. Both groups kept. The gauge's count is 2 over llama.cpp's 8,516 (the template is not counted, and it says so).
4. Pass, against Kev. One card, 2 models x 3 variants, six cells, Δ lines for each model. Laya 200 at 247, 192, 209 ms (219, 193, 219 tokens); Kev 200 at 3,947 ms (cold) then 63 and 65 ms (202, 167, 202 tokens). Reversed cells: Laya billing #4, account #2, technical #3, other #1; Kev the same positions. Δ: Laya team billing 0.95 to 0.81 without descriptions, 0.92 reversed; Kev 0.63 to 0.46 and 0.75; needs_human P(yes) 0.28 to 0.14 (Laya) and 0.59 to 0.77 (Kev).
5. Pass. With the dev instance's hold on (`/api/op/hold_set`, stopped `aux/kev-4b`, kept `aux/laya` on the CPU): Laya answers 200 in 214 ms with no fallback chip; Kev is HTTP 503 `gpu_hold` ("release the hold or configure a fallback alias") in 5 ms, shown in its three cells; no fallback is configured. The head says `held — no fallback: 503` for Kev. The hold was released afterwards.
6. Pass (saved drive). The override swapping two options by hand is sent and stored in that order, the card shows the swapped places, Perturb is off while it is in effect.
7. Pass. The history is the same after a reload and after stopping and starting the dev instance (3 groups, 8 runs, 1 artifact, 14,534 text bytes, 70 artifact bytes). The usage line and `/labs/api/decision/usage` equal the sums of `lab_groups`, `lab_runs` and `lab_artifacts` with §1.5's definition (2 groups, 8 runs, 0 artifacts, 14,197 text bytes before the image group; 3, 8, 1, 14,534, 70 after). One group deletes (8 runs to 2, 4.2 kB), Clear asks ("Delete all 2 runs and 1 file?"), then the lab holds nothing and `labs/artifacts/` is empty.
8. Pass, with one drive nit. `scripts/ui-drive.py scripts/drive/decision-lab.json --podman-guard ""` at 1440x900 and at `--dsf 1.25 --size 1152x768`: 52 PASS, 0 FAIL both times, JSONL and CSV read back. The drive's first step (the SETUP note) prints `SyntaxError: Unexpected identifier 's'` (an apostrophe inside its quoted text), harmless to the rest. The Tauri window's export toast: the owner's manual check.
9. Pass. `scripts/ui-matrix.py --routes /decision-lab`: 7 sizes, 0 FAIL (the matrix takes no `--dsf`; the 1.25 scale is covered by step 8). `scripts/webkit-check.py --backend broadway`: renders, `:has()`, popover, `showModal`, container queries and ResizeObserver all true, no scroll overflow at 1440x900 and 1024x700.
Not run: an image on OpenJev or Clef (no vision decision model on disk), Jev through OpenRouter (not run against the cloud upstream).

### 7.3 Slice 2: question sets, set runs, metrics

- *Files:* slice 2's migration; `store/lab_presets.rs`, `store/lab_batches.rs`; `labs/batch.rs`
  (the executor, `JobKind::LabBatch`, registered in `JobManager::new`, `jobs/mod.rs:358-371`);
  `labs/ingest.rs` (CSV and JSONL with the `csv` crate, mapping, label checks);
  `labs/metrics.rs`; `web/labs_store/presets.rs`; `web/decision_lab/batches.rs`; the plane rows.
  UI: `pages/decision_lab/sets.rs`, `batch_new.rs`, `batch_view.rs`, `metrics_view.rs`,
  `confusion.rs`, `reliability.rs`, `threshold.rs`; a batch's own route
  `/decision-lab/batches/:id`.
- *Tests:* `labs_batch.rs`: a 50-item labelled CSV through the job and the wiremock container;
  cancel and resume; restart, interrupt and resume; an unknown label refusing the input with its
  row named. `labs/metrics.rs` unit tests on hand-built answers (accuracy, matrix, bins, curve).
  The CSV export with `expected` and `correct`.
- *Acceptance:* a labelled set of ticket-like states on Laya; metrics and the threshold slider
  agree with a hand count; a saved threshold shows as measured on that batch.

*Accepted (slice 2), 2026-10-17:* on a dev instance at 127.0.0.1:8897 over a copy of the decision acceptance database (container prefix `lmgw-dev-acc2`), Laya on CPU; the UI rebuilt first (`trunk build`, dist newer than the merge at 6476b466). The GPU was otherwise idle, so Kev ran in steps 5 and 8 and was stopped after each. All eight steps pass; three notes at the end.
1. Pass. A hand-made set of 40 ticket-like states (`.example` names, RFC 5737 addresses, key `ticket_id` T-5001 to T-5040) with `team` (`choice`: billing, technical, account, other) and `needs_human` (`noul`), made in the page from the Ticket triage example by removing `urgency` and saved as a set, was run through New set run at concurrency 2 on `decide/laya`: done, 40 of 40, 0 failed, 0 unreadable, 7.3 s from Start to done (job start included). Per item (one run answers both questions): 276 ms at the least, median 308, mean 328, p90 328, 796 ms at the most (the first, cold one); 4,428 input tokens over the 40 items.
2. Pass. Computed by hand from the exported CSV (`batch=1`) and compared with the page and `GET …/batches/1/metrics?bins=10`: `team` accuracy 25 of 40 = 62.5% (page "accuracy 62.5% (25 of 40 scored)", the CSV's `correct` column 25 true); its confusion matrix, expected rows by predicted columns in option order, billing 12/0/0/1, technical 2/8/2/2, account 0/4/5/4, other 0, equal by hand, in the API and in the page's cells. `needs_human` 21 of 40 = 52.5%: Laya answered P(yes) below 0.5 for all 40, so the matrix is yes 0/19, no 0/21 (2×2). Reliability: all 7 non-empty `team` bins and all 5 `needs_human` bins equal the hand binning in count, mean prediction and observed rate (bin 0.3 to 0.4: 9 answers, mean 0.3528, observed 1 of 9 = 11.1%; the page's table row reads 9, 0.353, 11.1%). Slider at position 23 of 39: the line "at 0.487: 42% covered, 88.2% correct (n = 17)" equals the hand count over `confidence` (17 of 40 at or above 0.48663 = 42.5%, 15 of the 17 right = 88.2%).
3. Pass. Use as threshold on `team` wrote `{"value": 0.4866, "measured": {"batch_id": 1, "coverage": 0.425, "accuracy": 0.882, "labelled": 17}}` into the saved set; the page says "the set 'acc2 triage' uses 0.487 · 42% covered, 88.2% correct (n = 17) on this run", the set card after loading it "threshold 0.487 · 42% covered, 88.2% correct (n = 17) on set run #1 · open #1" and `needs_human` "threshold: not measured". After editing `team`'s instructions in the form and Update set, the card reads "threshold 0.487 · not measured" and the stored threshold is `{"value": 0.4866, "measured": null}` (value kept, measurement gone).
4. Pass, both ways. A 160-item run at concurrency 1 stopped with SIGTERM at 46 of 160, then the instance restarted: the page reads "interrupted at 46 of 160", "114 without a run", "Interrupted: interrupted by shutdown", buttons Resume and Delete; Resume (in the page) finishes it, 160 of 160 answered. `SELECT` over `lab_runs` joined to the run's groups: 160 runs, 160 distinct groups, no group with other than one run. A 120-item run killed with SIGKILL at 32 done (boot path, container left running and adopted): after the restart it read interrupted at 32 of 120 by shutdown, Resume made 120 of 120, 120 runs over 120 distinct groups.
5. Pass. The dev instance's own GPU hold on (`kept_on_cpu: aux/laya`), a 6-item set run on `decide/kev-4b` at concurrency 2 read interrupted at 0 of 6 with "stopped at item 2: lmgw could not ask the model (503 gpu_hold): 'decide/kev-4b' is a local model and lmgw is holding the GPU — release the hold or configure a fallback alias; 6 of 6 items have no run: Resume runs the rest once the cause is gone"; 0 rows in `lab_runs` for its 6 groups, and `podman ps` shows no Kev container (GPU memory unchanged). Hold released; Resume on Kev (card free): done 6 of 6 in 5.5 s from cold, all HTTP 200, `answered_by` `decide/kev-4b`. Kev stopped afterwards.
6. Pass. By preview and Start on a copy of the 40 tickets with row 7's team written `Billing`: one problem naming row 7, line 8, column `team`, value `Billing`, "is not an option of 'team' (billing, technical, account, other)", and Start is 400 `bad_input` with the same text (nothing created). Score columns, levels "1" to "5": values 1 to 5 read as level texts, no problem; values 1 to 4 (both readings fit and differ) one problem of kind `score_labels_needed` on row 1 saying to set `score_labels`, and with `score_labels` `text` or `index` set, no problem; levels "low", "mid", "high" with a mix of texts and indices, two problems naming rows 2 and 4; levels "10", "20", "30" with their texts and levels "3", "2", "1" with values 1, 2, 3 read as texts; values 1, 2, 2, 0 on levels "3", "2", "1" read as indices (0 is no level text).
7. Pass. `GET /labs/api/decision/export?format=csv&batch=1` is named `lmgw-lab-decision-batch-1-2026-10-17.csv`, header `group_id,batch_id,item_ix,item_key,run_id,…,question_id,question_type,answer,expected,correct,confidence,probabilities,state`, 80 rows (40 items × 2 questions) with the key, expected and correct filled; the JSONL has 40 lines, `group.item` `{batch_id, item_ix, key: "T-5001", expected: {team, needs_human}}`.
8. Pass, with a flake (below). `decision-sets.json` (`--podman-guard ""`): at `--dsf 1.25 --size 1152x768` 118 PASS, 0 FAIL; at 1440x900 118 PASS, 0 FAIL on its third try after the restart (the first and second failed, see the second note). `decision-lab.json`: 52 PASS, 0 FAIL at both sizes. `ui-matrix.py --routes /decision-lab,/decision-lab/batches/{batch}` (set run 37, 40 items): 14 measurements, 0 FAIL. `webkit-check.py --backend broadway` on `/decision-lab` and `/decision-lab/batches/37` at 1440x900 and 1024x700: `:has()`, popover, `showModal`, container queries and ResizeObserver true, no scroll overflow, both pages render.
Notes:
- The drives assume the head starts on Laya. After Kev's container was stopped with `podman stop` (behind lmgw's back) the registry still read Kev "ready", the head started on Kev (`first_pick` takes a ready row first) and both drives failed 6 and 16 steps for that reason alone; after a restart the head was Laya and they passed. Environment, not product: stop a held model through lmgw.
- Flake, diagnosed and fixed 2026-10-17: `decision-sets.json` at 1440x900 failed twice, once at [175] ("canceled at 0 of 1" never read after the second Cancel; the API step after it passed), once from [77] (after the first Cancel of the 30-item run the status did not read "canceled at" within 30 s, which cascaded to 12 failures). The server was right; the page dropped the newer read. The Cancel's answer is the run as the handler read it while the job still ran (`running`, `cancel_requested`). When the job ends within milliseconds of the flag (the abort, or the last call in flight landing just then), the jobs frame without the job can reach the page first: the job leaving the feed re-reads the run, then the Cancel's answer lands, and `take()` took its ticket on arrival, so it put `running` back and dropped the re-read's `canceled` as the older read. Nothing read the run again. Evidence: a trace of the page's fetches and jobs frames on a dev instance caught that order in 2 of 20 cancel rounds (the answer 2 ms after the frame without the job and after the re-read went out; a later 409 hid both), and the first-cancel rounds missed it by 1 to 3 ms several times. Fix: `RunPage::control()` takes the ticket when Cancel or Resume is sent, so a read sent after it wins whichever lands first. Regression check: the drive holds the answer to the first Cancel and to the abort until the job left the feed (`__holdAnswer` in `decision-sets-data.js`). On the old page that fails both "canceled at" checks with the same 12-failure cascade; on the fixed one it passes. Two more drive races met while repeating it, both fixed in the drive: "show them" is pressed in place (a trusted click scrolls the table end into view, its sentinel reads the page first, and the rows it adds moved an item row under the click, 1 in 25), and the wait for the page reloaded after the first Cancel is 60 s (a reload of the debug WASM bundle can take over 20 s under load). After the fix: 5 runs in a row at 1440x900 and one at 1.25, 120 PASS, 0 FAIL each.
- Not run: the Tauri window's export of a run (the owner's check, as in slice 1); a priced cloud decision alias's price line (the dev copy has none); an interruption by a cloud 401 or 403.

### 7.4 Slice 3: batch agents through a set

- *Files:* `agents/manifest.rs` (`decide`, its validation, the config `format`);
  `agents/batch/decide.rs` (the classify branch); `builtin-agents/mail-labeler.agent.json`
  (3.1.0); the config form's set select; "Save as labelled set" on the agent's run view with
  `web/decision_lab/from_agent.rs`.
- *Tests:* `agents_runs` with `decision_world`: outputs from the winner, attention by threshold,
  the fallback for a failed call, and with no set the LLM path byte for byte as today; a
  labelled-set export from a fixture apply job.
- *Acceptance:* the mail labeler on a dev copy with a fixture mailbox source through a set on
  Laya; the review table splits by the threshold, and apply writes as before.

*Accepted (slice 3), 2026-10-17:* on a dev instance at 127.0.0.1:8899 over a copy of the decision acceptance database (prefix `lmgw-dev-acc3`), Laya only, no GPU model started. The dev copy has no `gws` server, so `scripts/fixture-mail-mcp.py` stands in: a stdio MCP server registered as `gws` with the five tools the manifest names, serving 29 fake unread mails (`.example` senders and RFC 5737 addresses; 4 long newsletters of about 18,850 tokens, one gibberish mail) and logging every call to a JSONL file.
1. Pass. Set "mail triage" (choice `category`, the manifest's ten categories, default model `decide/laya`); a labelled set run of 40 mails (24 from the fixture, 16 hand-written) at concurrency 2 finished in under a minute: accuracy 0.525 (Laya on bare option names), 40 answered, 0 failed. "Use as threshold" at 0.75 (the API route the button posts to) stored it measured: coverage 0.40, accuracy 0.625, labelled 16.
2. Pass. The mail labeler's `decision_set` pointed at the set; Classify of the 29 mails ran 5 s end to end at concurrency 2: 25 answered at 121 to 648 ms (mean 356 ms, 105 to 117 input tokens each, cost 0, class aux, `client_key` `internal:agents`), and the 4 long newsletters each got llama.cpp's 500 verbatim (`input (188xx tokens) is too large to process…`, 141 to 177 ms), fallback Other and attention. They did not abort the run (job done, 29 of 29, 19 attention, 4 errors). The review table split 19 "Needs attention" / 10 "Classified": the 4 errors plus 15 rows under the threshold; the group text reads "the fallback, a confidence below the set's measured threshold of 0.75, or a failed call", and the set field's note says the same. The threshold applies to the answer's `confidence` field, not the winner's probability (a row at P 0.7746 with confidence 0.7495 is attention). The gibberish mail did not land on Other: Laya answered Spam at confidence 0.50, so it is attention by confidence, not by the fallback (Other only comes from a failed call). The 10 unflagged rows held 3 wrong answers at confidence 0.81 to 0.92: the measured threshold trades coverage, it is no guarantee.
3. Pass. Apply of 14 rows (the 10 unflagged, and the 4 long newsletters set to Newsletter by hand), script path: the fake server saw `gmail_listLabels`, 7 `gmail_createLabel` (`lmgw/Receipts`, `Notifications`, `Newsletter`, `Promotions`, `Travel`, `Work`, `Spam`) and 7 `gmail_batchModify` carrying 2, 4, 4, 1, 1, 1, 1 message ids, which is `docs/agents.md`'s apply contract for those outputs; the result says applied 14. The classify run made 1 `gmail_search` and 29 `gmail_get` calls.
4. Pass. "Save as labelled set" on that apply job (through `POST /decision-lab/api/batches/from-agent {job_id}`, the button's route): set run #2 names "mail triage", source `agent run 4 of mail-labeler`, 14 items, 14 labelled, mapping `state: prompt`, `key: id`, asking `category` alone, nothing skipped. Resume on Laya answered 10 and failed the 4 long ones with the same 500; accuracy 1.0 over the 10 scored (the labels are the model's own answers, so this proves the plumbing, not the model); confusion matrix and reliability bins read. A second apply of 6 rows (the 4 failed-call rows still on the fallback, the gibberish row, one row with no prompt) saved as set run #3 with 1 item and 5 skipped: 4 `call_failed` with their 500 text, 1 `never_asked` with its fetch error. That apply also wrote to the fake server only.
5. Pass. A second set with `decide/kev-4b` on the mail labeler, the dev instance's GPU hold on (`kept_on_cpu: aux/laya`): Classify failed at start in 0 ms with `model 'decide/kev-4b': 'decide/kev-4b' is a local model and lmgw is holding the GPU — release the hold or configure a fallback alias`, no rows, and the fake server's log stayed at its 49 lines (no tool call). The hold was released; Kev was never started.
6. Skipped, half checked: the copy has no chat model or upstream, so the model path could not classify. With neither a model nor a set, Classify failed at start with `this agent has no model and no question set: …` and no tool call; the UI test carries the rest.
7. Pass. `scripts/ui-drive.py scripts/drive/agent-decide.json --podman-guard ""` after `scripts/agent-decide-seed.py`: 24 PASS, 0 FAIL at 1440x900 and 24 PASS, 0 FAIL at `--dsf 1.25 --size 1152x768` (each on a fresh seed). A separate drive opened the classify run on the Run tab: "Needs attention 19", "Classified 10", 29 rows. `scripts/webkit-check.py --backend broadway --routes /agents/mail-labeler`: renders at 1440x900 and 1024x700, `:has()`, popover, `showModal`, container queries and ResizeObserver true, no scroll overflow. Its first two attempts (this route, then `/decision-lab`) reported "page never rendered"; the same command passed on the third try with nothing changed, so a flake, not a page fault.
Not run: a real Gmail source (the fixture stands in), a chat-model classify (no model on the copy), Kev answering. Laya's 0.525 accuracy on bare option names suggests option descriptions matter.

### 7.5 Slice 4: Audio and Image labs onto the store

- *Files:* `web/image_lab.rs` and `web/audio_lab.rs` call `labs::record`; the speech tee in
  `web/audio_lab/record.rs`; `kept`, "Open in form" and delete on both pages; presets
  `image_prompt` and `audio_settings`.
- *Tests:* `image_lab.rs` and `audio_lab_wire.rs` grow cases: a generation's image stored as an
  output artifact and served back; a stopped speech stream recorded as stopped.
- *Acceptance:* a generation and a speech clip survive a restart and export as a zip with their
  files; the size line shows the files.

*Accepted (slice 4), 2026-10-17:* on a dev instance at 127.0.0.1:8900 (container prefix `lmgw-dev-acc4`) over a copy of the decision acceptance database, main at 5ce399ad, the dashboard rebuilt with `trunk build` first. Its image and audio rows were empty in that copy and were created through the ops API: `image/z-image-turbo` from the `image_recipe_add` row (GPU, `localhost/lmgw-sd-server:sdcpp-master`), and `audio/kokoro` (`kokoro_tts`) and `audio/canary` (`canary_asr`), both offline, CPU, 8 threads, on `localhost/lmgw-audio-cpp:audio-main`; the model files were reflinked from `<model-cache>/lab-media-slice4/` into the scratch data dir. No other model container was running (the GPU was otherwise idle).
1. Pass. Four images through `/image-lab/api/groups` and `/run`: 1024x1024 PNG 1,619,281 B (7.5 s cold), 512x512 PNG 489,434 B, 1024x1024 WebP 299,766 B, 1024x1024 JPEG 817,936 B. Three speech clips on kokoro (WAV, 24 kHz mono 16-bit): 12 characters 73,244 B, 101 characters 302,444 B (6.3 s), 480 characters 1,488,044 B (about 31 s, 8.0 s to synthesise). Two transcriptions of the 101-character clip on canary (the upload is that clip's file): the text came back exactly, 150 to 170 ms for 6.3 s of audio. The files under `labs/artifacts/` are named by their sha256 (every name equals `sha256sum` of its content) and their sizes equal `lab_artifacts.size`. `/labs/api/{lab}/usage` equals the `SELECT` sums: image 4 groups, 4 runs, 4 files, 3,226,417 B (the four sizes added); audio 5 groups, 5 runs, 3 files, 1,863,732 B (the three distinct clips: the clip uploaded twice and spoken once is one file); 5,090,149 B on disk, the two sums added; text 2,599 B (image) and 4,904 B (audio).
2. Pass. After stopping the instance by PID and starting it again, the usage and the `SELECT` listing of every artifact (id, group, name, size, digest) are byte for byte what they were, and the served bytes of artifacts 1 to 7 (`/labs/api/artifacts/{id}`) have the digests of the files. The picture and clip cards, and the size line ("1 run · 1 file · 110 kB", the files named), render from the store (steps 4 and 7 of the drive and the matrix shot).
3. Pass. `GET /labs/api/{image,audio}/export` answers `application/zip` with `Content-Disposition: attachment`. Image: 3,228,961 B zip, a JSONL of 4 lines (`lmgw.lab.v1`) and 4 files under `artifacts/<sha256>.<png|webp|jpg>`; audio: 1,866,406 B, 5 lines and 3 WAV files. Every `href`, `sha256` and `size` of every artifact in the JSONL names a file in the zip with that digest and size, and every file in the zip is referenced (0 missing, 0 bad). The zip is the files plus about 2.5 kB (image) of JSONL.
4. Pass. Saved `image_prompt` ("fox at dawn", seed 42) and `audio_settings` ("kokoro hello", pcm 24000 mono 16-bit) presets, listed by kind and read back by id with their bodies; posted to the other lab (`image_prompt` to audio, `audio_settings` to image, and `image_prompt` to the decision lab) each is HTTP 400 `bad_request` ("a image_prompt is the image lab's preset, not the audio lab's"), and a read of another lab's id is 404. Open in form (kept picture's prompt and seed back, a kept transcription's upload back in the file slot with its model and task, a speech run's model and text back) and a saved preset loading into the form are drive steps 34, 24, 126, 131 and 116, all PASS.
5. Pass. Delete of image group 4 removed its JPEG at once (7 files to 6 on disk, 3,226,417 B to 2,408,481 B). The clip shared by three groups (spoken in group 6, uploaded in groups 8 and 9, one digest): deleting group 8 and then group 6 left the file on disk, usage 4 and 3 groups, and still 3 distinct files; deleting group 9, the last one naming it, removed it (2 files, 1,561,288 B).
6. Pass. A 1536x1536 generation of two pictures (30 s) with its group in flight: Delete is HTTP 409 `in_flight` ("group 10 is being answered: stop it, or delete it once its run is in"), the group view says `in_flight: true`, and Clear answered with all counts 0 and deleted the other image files while the audio files stayed. The run still answered 200 with both pictures inline (6.8 and 6.2 million base64 characters), no run id and `store_error` "group 10 was deleted before its run was in"; nothing of it is in the store afterwards (0 groups, 0 files). The page's "not kept" card note is the drive's.
7. Pass. `scripts/ui-drive.py scripts/drive/lab-store-media.json --podman-guard ""` at 1440x900 and at `--dsf 1.25 --size 1152x768`: 50 PASS, 0 FAIL each (the drive clears both labs and their presets first, so steps 1 to 6 ran before it). `scripts/ui-matrix.py --routes /image-lab,/audio-lab` with a picture and a clip in the history: 14 measurements, 0 FAIL. `scripts/webkit-check.py --backend broadway` on both pages at 1440x900 and 1024x700: `:has()`, popover, `showModal`, container queries and ResizeObserver all true, no scroll overflow.
8. Retention numbers (Q1), measured above on z-image-turbo and kokoro (a 1536x1536 PNG is about 5.1 MB and 4.6 MB for the two pictures of the in-flight run, counted from the base64 length):

| What | Kept file | Kept text |
|---|---|---|
| 1024x1024 PNG | 1.62 MB | about 650 B a group |
| 1024x1024 JPEG / WebP | 0.82 MB / 0.30 MB | |
| 512x512 PNG | 0.49 MB | |
| 1536x1536 PNG | about 4.6 to 5.1 MB | |
| speech clip | 73 kB, 302 kB, 1,488 kB (48 kB a second of speech; mean of the three 621 kB) | about 1 kB a run |
| transcription of a kept clip | none new (same digest) | about 1 kB |

At 50 generations and 50 clips a day: 50 x 1.62 MB is about 81 MB of pictures and 50 x 0.62 MB about 31 MB of speech, so about 110 MB a day and **about 3.4 GB a month** (2.4 GB pictures, 0.9 GB speech, under 3 MB of text). The same pictures as WebP would be 0.45 GB, so about 1.4 GB a month in all. The files are everything; the database holds only the text.

Not run: the Tauri window's export toast (the owner's check), a stopped speech stream (kokoro cannot stream), an edit on a dev row (no edit pipeline on the copy). The drive's SETUP note and the matrix ran on the same instance after steps 1 to 6, so steps 2 and 3 were checked before the drive cleared the labs.

## 8. Questions for the owner

1. **Retention: keep everything, or a rule?** Approved 2026-10-16. *Recommended: keep everything
   through slice 3, with the size always visible, delete per group and Clear per lab (§1.5).*
   Decision runs are kilobytes; slice 2's set runs are the uploaded file plus the answers. Revisit
   before slice 4, with sizes measured on a dev copy, since generated images are megabytes each. A
   rule, if one comes, is a Settings → Retention field with 0 = keep all.
2. **The option limit on the head line.** Approved 2026-10-16. The outline asked for the per-type
   limit; it is in llama.cpp's source, not the header, and lmgw keeps no table of llama.cpp's types
   (decision-models §1.5, §3.4). *Recommended: show the count against the wire's 255 and say that
   llama.cpp's 400 names a type's own limit, which the cell shows verbatim (§2.2).* The alternative,
   a lab-only hint table dated to llama.cpp's source, goes stale with the next type.
3. **Slice 3: where the mail labeler's categories come from when a set is chosen.** Approved
   2026-10-16. *Recommended: from the set's `choice` options; `config.categories` is not read, and
   the config form says so (§4).* The alternative keeps `config.categories` and refuses a run whose
   set options differ, which leaves two lists to keep in step by hand.
4. **Slice 3: which rows of an agent run are labels.** Approved 2026-10-16. *Recommended: the rows
   the apply job received, the checked rows with the owner's edits.* Unchecked rows were not
   confirmed either way; including them would label the model's own guesses as truth.
