# folder-chat: reading PDF pages with a vision model

Approved 2026-09-24. Agent: `examples/agents/folder-chat`, version 0.1.1 → 0.2.0.

## Why

Two gaps found on a mixed sample folder (a scanned page, a payroll-style table, a timetable, invoices, a brochure, a two-column report):

- A scanned page has no text layer, so nothing of it is indexed (the scanned page).
- `pdftotext -layout` tables lose which column a value belongs to. With the plain chat model
  (`gemma4-12b`), the payroll-style answer confused two neighbouring columns, and
  the timetable's tasks were put on the wrong weekdays. Showing excerpts in reading order
  (0.1.1) fixed the payroll-style table only for a reasoning model.

The spike on 2026-09-24 (gemma4-12b through lmgw, 150 dpi PNG, temperature 0) measured:

- Scan, image only: near-perfect transcription (one word wrong on a rotated stamp).
- Table, image only, asked for a Markdown table: columns shifted (a task put on the wrong day).
  The same page asked for **one line per non-empty cell, `<row> | <column>: <value>`**: 88 of 88 cells right.
- Dense text page (the payroll-style table), image only: one digit misread (3.250,00 → 3.450,00). The same
  page **plus its pdftotext text**, told to copy every character from the text and use the image
  only for structure: exact numbers, each labelled by its column.
- 4–20 s per page. An A4 page at 150 dpi is ~1000 image tokens. Gemma 4 rows with a
  projector need `ubatch_size` ≥ the image's tokens or llama-server aborts (2048 covers an A4
  page at 150 dpi).

## Principle

quickdoc's chunk payload is a verbatim slice of the document, never model-rewritten. A vision
reading is model output, so it **never replaces** the text layer. It is stored as its own
text, appended to the PDF's indexed text under a header naming the page and the model, chunked
separately, and shown to the chat model and in citations as that model's reading.

## Config (agent.json + config.rs)

- `vision_model`: string, `format: model_alias`, optional. Empty = no page is read (0.1.1
  behaviour). It sees page images and, for table pages, the page's text, so it must be a
  local alias for a private folder. Same fallback rule as the embedding model: a reading
  answered through a GPU-hold fallback (`x-lmgw-fallback`, or 503 `gpu_hold`) stops the sync
  and is never stored.
- `vision_every_page`: boolean, default false. Read every PDF page, not only the pages the
  rule picks. The first sync gets slow (seconds per page); readings are cached.

## Which pages are read (pure function, new module `vision.rs`)

Per PDF page (pages as `chunk::pdf_pages` numbers them):

- No text (`page.trim().is_empty()`) → **OCR mode**: the image alone.
- Table-like → **structure mode**: the image plus that page's pdftotext text. A page is
  table-like when at least `TABLE_MIN_ROWS` (3) of its lines split into at least
  `TABLE_MIN_COLUMNS` (4) cells, where cells are separated by runs of at least
  `TABLE_GAP_SPACES` (3) spaces after trimming the line. Measured on the sample folder: this picks the
  payroll-style table, the timetable, the order and the invoice pages plus 2 of 13 brochure pages, and none of
  the 37 pages of the two-column report, whose prose plus sidebar has 3 columns.
- `vision_every_page`: every text page is read in structure mode as well.

The constants are named and reported in the sync report (a `vision_rule` string), like the
other constants.

## Rendering

`pdftoppm -r VISION_DPI -png -singlefile -f N -l N -`, with the PDF bytes on stdin and the PNG
on stdout, the same way `pdftotext` gets its bytes (no path is ever handed to the tool). The
timeout is the existing `PDF_EXTRACT_TIMEOUT`. `VISION_DPI` = 150, reported.
`SyncOptions` gains a `pdftoppm` path next to `pdftotext`, so tests can put in a fake.
pdftoppm is in the image already (poppler-utils). If pdftoppm is missing, a note says so and
the pages are recorded as failed, so a later sync retries them.

## The reading call (gateway.rs)

`POST /v1/chat/completions` through lmgw with the vision alias, one user message with
`[image_url (data:image/png;base64,…), text]`, `temperature: 0`, no `max_tokens` (the output
is not capped), not streamed. Use `message.content` only: a reasoning alias's thinking is
separate. Strip one wrapping `` ``` `` fence if the whole answer is fenced. Fallback and hold are
detected the same way the agent's embedder does it.

Prompts are constants with `VISION_PROMPT_VERSION` = "1". They are the ones that worked in the spike:

- OCR: "Transcribe this document page as plain text. Reproduce every piece of text exactly
  as printed, in reading order, without translating, summarising or adding anything. For
  every table, write one line per non-empty cell in the form '<row heading> | <column
  heading>: <value>', using the headings exactly as printed, so each value is named by the
  row and the column it sits in; skip empty cells. Output only the transcription."
- Structure: "Below is the exact text of this page, extracted from the PDF, with its layout
  approximated by spaces. Rewrite it as plain text in reading order. Copy every word and
  number character for character from the extracted text — never from the image, never
  corrected, translated or summarised; use the image only to see which row and column each
  value belongs to. For every table, write one line per non-empty cell in the form '<row
  heading> | <column heading>: <value>', using the headings exactly as they appear, so each
  value is named by its row and column; skip empty cells. Output only the rewritten
  text.\n\nExtracted text:\n<page text>"

## Storage (index.rs)

New table `folder_chat_vision`: `document_id` (→ document, ON DELETE CASCADE), `page`,
`content_hash` (the document's hash: PDF bytes + pdftotext version), `model` (the alias),
`prompt_version`, `mode` (`ocr` | `structure`), `status` (`read` | `failed`), `text` (the
reading, or the error), PRIMARY KEY (document_id, page). A row is **reused** only when
content_hash, model, prompt_version and mode all match and status is `read`. Each row is
written as soon as its page is done, so a hold in the middle of a file keeps the pages read
before it. When a file is indexed again, rows for pages that are no longer selected are
deleted. The alias is the key, so pointing the alias at another model does not re-read on its
own; the README says so.

## Indexed text and chunks

The indexed text of a PDF is the pdftotext output, followed for each read page in page order by
`"\n\n[page N, read by <alias> from the page image]\n"` + the reading + `"\n"`. `chunk_pdf`
and `pdf_pages` run on the pdftotext part only, so their spans and page numbers do not change.
Each reading is cut with `chunk_plain`, its spans offset into the combined text, with
heading `page N, read by <alias>`. Line numbers come from the combined text as today.
`chat::page_of` parses the leading page number of that heading too.

MCP `read` of a PDF must return that same combined text, rebuilt from pdftotext plus the stored
readings, so a citation's lines resolve. `pdf_pages_without_text` lists only the pages that
have no text **and** no reading.

## When a file is read again

- New meta key `vision_settings` (the alias, every-page, prompt version, DPI and the table
  rule, as one string). When it differs from the stored one, every PDF goes through the
  backfill path (extend `Backfill` with `vision`), which reuses every cached reading and every
  vector that still matches. Counted as `vision_refreshed_files`. When the alias is cleared,
  the same pass drops the reading chunks and rows.
- A document with a `failed` row is forced on the next sync, which retries that page.
- The existing rule stands: a backfill-only pass never drops a file.

## Failures

- One page fails (render error or timeout, a non-hold gateway error, an empty or unusable
  answer): a `failed` row, a per-file error naming the page, the alias and the reason, and the
  rest of the file indexed and marked current.
- A hold or a fallback-answered reading ends the sync with `AbortKind::GpuHold`, as an
  embedding batch does. Nothing from that reading is stored, and the next sync resumes.

## Report, events, UI

- `SyncReport`: `vision_model`, `vision_every_page`, `vision_dpi`, `vision_rule`,
  `vision_pages_read` (new this sync), `vision_pages_reused`, `vision_pages_failed` (list of
  path, page, reason; no cap), `vision_refreshed_files`. There is a note summarising pages read by mode and
  pages reused.
- `SyncEvent::Reading { file, page, of }` before each reading that is not cached. The Sync card
  shows it: a reading takes seconds, and without it the sync looks stuck.
- `/api/status` config carries the two new fields.

## Chat

One `SYSTEM_PROMPT` sentence: an excerpt headed "read by <model>" is that model's reading of
the page image, not the file's own text; its names and numbers can be misread, and where the
file's own text is also given, rely on that.

## Docs

README gains a "Reading pages with a vision model" section: what is read, what is sent, the
cache, cost, and that the alias should be local. "Keeping it local" gains the fourth alias.
The manifest description and field descriptions get the same.

## Tests

Use a fake pdftoppm (a script that prints a fixed tiny PNG) and the fake gateway's chat route
(it records bodies and returns canned text per mode). Cover:

- A scanned page read in OCR mode: indexed under its heading, found by FTS, gone from `pdf_pages_without_text`.
- A table page read in structure mode, with the request carrying the page text.
- A plain text page not read, unless `vision_every_page` is on.
- A re-sync makes no vision calls.
- Changing the alias re-reads.
- Clearing the alias drops the readings.
- A failure is an error, the file is still indexed, and the next sync retries.
- A 503 `gpu_hold` or an `x-lmgw-fallback` answer aborts with `GpuHold` and stores nothing.
- The MCP `read` lines of a reading match its citation.
- `page_of` parses the new heading.

## Not in scope

Images in non-PDF files, per-question image attachment, and embedding page images
(Qwen3-VL-Embedding).
