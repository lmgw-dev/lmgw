# Document conversion: any file in, text a model can read out

**Accepted, 2026-11-18, revised the same day with the owner's answers and rulings. Nothing is built yet.** lmgw
already turns uploads into text for chat attachments and knowledge bases. This record makes that a
feature of its own: a conversion API that takes one file or several, a docling-serve-compatible
adapter so the tools on a server that call docling today can call lmgw instead, optional OS tools
that add formats and quality, opt-in model help for what the toolless path gets visibly wrong and
for describing pictures, and a settings page that says what this machine can convert, what
installing a package would add, and what the docling callers actually send. "Decisions" (§2)
separates the owner's rulings from the decisions accepted with them. "Not yet" (§12) lists what this version leaves out, and nothing there is ruled out.

Companion records, cited by short name:
- "chat complete §n": [chat made complete](2026-09-30-chat-complete-design.md) (attachments §8,
  knowledge-base ingestion §9.2);
- "vision pages §n": [folder-chat vision pages](2026-09-24-folder-chat-vision-pages-design.md);
- "principals §n": [principals, origins and mounts](2026-09-22-principals-origins-mounts-design.md);
- "api docs §n": [API reference page](2026-09-28-api-docs-page-design.md);
- "headless server §n": [lmgw on a server](2026-11-08-headless-server-image-design.md);
- "Windows §n": [lmgw on Windows](2026-11-10-windows-native-design.md);
- "billable units §n": [billable units](2026-10-07-billable-units-design.md);
- "candidate aliases §n": [candidate aliases](2026-09-27-candidate-aliases-unified-kv-design.md)
  (fair queueing).

| § | Content |
|---|---|
| 1 | What exists today |
| 2 | Decisions |
| 3 | Tiers |
| 4 | The capability table, detection and its data model |
| 5 | The tools, one by one, the pure-Rust additions, pictures (§5.3) |
| 6 | Packaging: RPM, AppImage, server image, Windows and macOS |
| 7 | The API: native route, output, jobs, the docling-serve adapter, gate, hold, usage, docs, several files (§7.6), scheduling (§7.7), what callers send (§7.8) |
| 8 | The settings page |
| 9 | Limits and messages |
| 10 | Work packages, with the measurement plan (§10.1) |
| 11 | Open questions (none left) |
| 12 | Not yet |
| 13 | External facts checked on 2026-11-18 |

## 1. What exists today

Verified in the tree at `0ad243e0`:

- `crates/lmgw-core/src/extract/mod.rs`: `extract(&Sniffed, Bytes) -> Extracted` (text, PDF pages,
  office parts). Its callers are chat attachments (`web/chat_attach*.rs`), knowledge bases
  (`knowledge/ops.rs`, `knowledge/sections.rs`), the labs' artifacts and `/v1/responses` tool
  output.
- `extract/sniff.rs`: the file type from its bytes: PNG/JPEG/GIF/WebP, PDF, docx/xlsx/pptx,
  odt/ods/odp, WAV/MP3/Ogg/FLAC/M4A/WebM, UTF-8 text. It refuses legacy OLE2 Office
  (`SniffError::LegacyOffice`), other ZIPs, and video containers (`SniffError::Video`), each with
  the `ACCEPTED` list.
- `extract/office.rs` with `office_xlsx.rs`, `office_ods.rs`, `office_sheet.rs`,
  `office_xlsx_dates.rs`: pure-Rust readers (zip + quick-xml) to markdown, headings, lists and
  tables kept, all bounded by one `Budget` of `MAX_UNCOMPRESSED_BYTES` (1 GiB) against zip bombs.
- `extract/pdf.rs`: poppler's `pdftotext -layout` and `pdftoppm -r 150` as child processes. The PDF
  goes in on stdin and the result comes out on stdout, so no path is handed to poppler. Each run
  has `PDF_TIMEOUT` (300 s), `RLIMIT_AS` of `POPPLER_MEMORY_LIMIT_BYTES` (4 GiB), the output
  budget, `kill_on_drop`, and `PdfError::ToolMissing` ("install poppler-utils … to read PDFs").
  The child environment comes from `crate::child` (`system_tool_tokio`, `stdio_only`).
- `extract/vision_prompts.rs`: pages without text (`Mode::Ocr`) or with a table-like layout
  (`Mode::Structure`, `TABLE_MIN_ROWS`/`TABLE_MIN_COLUMNS`/`TABLE_GAP_SPACES`) are read by a vision
  model, with versioned prompts (`VISION_PROMPT_VERSION`).
- `crates/quickdoc-core/src/ingest/html.rs`: `to_text`, an HTML reducer that keeps headings, prose
  and fenced code and drops `script/style/nav/header/footer/aside/svg/noscript`.
- ASR: audio rows behind `/v1/audio/transcriptions`, and the `voice_transcribe` op
  (`ops/voice_library.rs`); CPU audio rows keep serving under the GPU hold.
- `ort` `=2.0.0-rc.13` is linked for Silero VAD (`crates/lmgw-core/Cargo.toml`, CPU build through
  `download-binaries`), so ONNX models on the CPU add no new native dependency.
- Packaging: `poppler-utils` is a hard `Requires` (`src-tauri/tauri.conf.json`
  `bundle.linux.rpm.depends`, mirrored in `ci/rpm-allowlist.toml` `[dependencies].requires`, whose
  `recommends` and `suggests` lists are empty). The server image installs it too
  (`packaging/server/Containerfile`, `--setopt=install_weak_deps=False`).
- Windows ships no poppler yet (Windows §3.1, its table row for `extract/pdf.rs`).
- `IDEAS.md`, "A document-conversion API in lmgw" (2026-11-17), leaves three points open: formats
  and outputs, CPU vs a model on the card, accounts and keys. §3, §7.1 and §7.5 answer them.

## 2. Decisions

**The owner's rulings (2026-11-17/18):**

1. **It replaces docling.** Tools on a server that call docling do so through
   docling-serve's HTTP API, so lmgw offers a docling-serve-compatible route as a thin client-shape
   adapter next to its own native route (§7.4).
2. **External tools are optional, never forced.** OS packages may be used, bundled or as package
   dependencies, but only as optional dependencies. That includes poppler-utils.
3. **A settings page** shows the tools found (with versions), the file types the user gets, and the
   missing packages with what each would add. One capability table drives both the page and the
   conversion API.
4. **The docling callers do plain conversion and OCR.** Several callers use docling on a
   deployment where lmgw would replace it. Which routes and options they send is unknown; none of
   them does anything special. So:
   - the adapter covers docling-serve's whole convert surface: the file, base64 and source routes,
     sync and async, with status poll and result (§7.4; the status WebSocket too, D24), since a
     caller may use any of them;
   - docling's OCR options (`do_ocr`, `force_ocr`, the OCR engine, the OCR language) map onto
     lmgw's OCR path. Any engine name, known or not, means "lmgw's OCR", with a visible note
     (§7.4.2), following "the gateway translates, never refuses".
5. **The adapter serves picture description.** Callers must keep working when docling is swapped
   out, and a caller cannot easily describe pictures itself: it gets Markdown placeholders, not
   cropped pictures with their context. Finding pictures and cropping them in reading order is
   needed anyway for docling's ordinary picture-export options (embedded/referenced images); once
   that exists, a description is one vision call per picture plus a line of text next to it (§5.3).
   - Both docling variants, a local model and an API, go to **one picture-description model** the
     user picks under Settings › Documents. It is off until picked, like the rest of tier 1 (D5).
     With none picked, the caller gets the picture without a description and a visible note, not
     an error.
   - The API variant's caller-supplied endpoint URL is never followed (lmgw would otherwise call
     arbitrary URLs). The configured model answers instead, with a visible note. The caller's
     prompt is honoured.
   - Under the GPU hold, fallbacks apply as configured (D14).
   - Picture classification is a small classifier in docling, not an LLM. It is not built yet; a
     request for it gets a visible note, not an error (§12).
   - Formula and code enrichment are conversion and belong in tier 1 (§3, D23).
6. **No documents leave the deployment.** A sample folder of the callers' real documents is not available. Measurement splits into WP0a on a public corpus on a test machine and
   WP0b, a self-contained script that an operator runs once at a deployment and that reports numbers only
   (§10.1).
7. **Several files per request**, on the native route and on the adapter (§7.6, §7.7).
8. **The earlier draft's recommendations are accepted:** D1–D7 below, and the draft answers to the
   former open questions 2 and 4–10, now D8–D15.
9. **Fetching `http` sources is a setting, off by default** (was Q11). Under Settings › Documents
   the owner can let the docling-serve adapter fetch `http`/`https` sources (§7.4.1). The native
   route has no source or URL input (§7.1: files only), so the setting does not touch it.
   - Off: a source with `kind: "http"` gets a named 400 that tells the caller to send the file as
     base64 and names the setting that would allow URL sources. The host of each refused source is
     recorded per key (§7.8).
   - On, with these safeguards: only hosts on a list the owner fills in; private, loopback and
     link-local ranges refused unless listed explicitly; each download bounded by the configured
     `max_body_mb`, with a named error when exceeded; redirects re-checked against the list.
   - It fetches the source and nothing else: callbacks, targets, `s3` sources and the picture
     API's `url` stay unfollowed (D21).
10. **Everything else is approved (2026-11-18).** Q12 and Q13 are accepted with their drafts
    (D26, D27), and D16–D25 are owner-accepted.

**Decisions accepted (2026-11-18):**

- D1. poppler-utils moves from `Requires` to `Recommends` (§6.1). Without it, PDFs answer with the
  named "install poppler-utils" warning; on Windows, pdfium is the planned path (§5).
- D2. Light tools go to `Recommends`, heavy ones get no RPM metadata until tauri can write
  `Suggests` (§6.1, D9).
- D3. The native mechanism is lmgw's own pipeline in `extract/`. docling.rs (§13) is a reference and
  a WP0a measurement, not a dependency (D8).
- D4. One handler per format at any moment, picked by a fixed order in the table (§4.2). pandoc
  never competes with a native reader.
- D5. Tier 1 is off until the user picks a model. With no pick, a conversion never touches the
  card.
- D6. The conversion route is `Cap::Inference`; tier-1 calls run as the calling principal (§7.5).
- D7. Detection results live in memory; only the user's choices are stored (§4.4).
- D8. **docling.rs is not a dependency** (was Q2). It conflicts with lmgw's `ort` pin and parses
  PDFs in-process; lmgw keeps one native pipeline shared with chat and knowledge bases, isolated in
  child processes. WP0a measures it. If it is clearly better, it is revisited as an optional
  external tool (its CLI) under the same rules as pandoc, never linked.
- D9. **Suggests through tauri** (was Q4): write the small upstream tauri-utils/bundler change for
  `suggests`. Until it lands, the heavy tools have no RPM metadata and the settings page names
  them. The RPM is not post-processed.
- D10. **The server image's tools** (was Q5): poppler-utils, tesseract (eng + deu by build arg) and
  ffmpeg-free. pandoc and LibreOffice not yet; a derived image is documented for them (§6.2).
- D11. **docx equations** (was Q6): pandoc for the `docx+math` variant now; a native OMML→LaTeX
  reader later, after which pandoc leaves that row.
- D12. **No install button in WP1** (was Q7): the page shows a copyable command. A button through
  the machine check's root helper comes later, with its own polkit action.
- D13. **ffmpeg-free's audio decoders** (was Q8): the probe lists the decoders, so the page and the
  warnings are right either way. WP7 checks AAC in a Fedora 45 container.
- D14. **Vision pages and pictures under the hold** (was Q9): as configured. A fallback is used, and
  the page says so beside the model choice (§7.5). No separate "never send documents to the cloud"
  switch, since a model without a fallback already gives that.
- D15. **No default tier-1 model** (was Q10): the page offers the vision models that exist.
  granite-docling is suggested once WP0a/WP0b show it reads documents well.

**Further decisions, accepted 2026-11-18 (ruling 10):**

- D16. **Adapter notes travel in one header,** `x-lmgw-option-notes` (§7.4.2), plus the usage
  record (§7.8) and a log line. The docling-shaped body only changes where output was really lost
  (`errors`, `partial_success`), so strict docling clients keep parsing it.
- D17. **lmgw records which routes and options each caller sent** (§7.8): per request in a side
  table of the usage row, summarised per key on Settings › Documents and through an op. Option
  values are recorded for enum-like options only; prompts, URLs, headers and document content
  never.
- D18. **The native route always answers a `documents` list,** one entry per file in request order,
  even for one file (§7.6). The HTTP status is 200 when at least one file converted.
- D19. **Tool runs share one gateway-wide pool** whose size is derived from the real cores and
  memory and shown in expert view (§7.7).
- D20. **A conversion keeps at most as many model calls in flight as the model has slots** (§7.7),
  and renders a page image only when a call can take it.
- D21. **lmgw makes no outbound call to a URL a caller chose:** docling's `callbacks`, the
  `put`/`s3`/`presigned_url` targets, `s3` sources and the picture-description API's `url` are not
  followed (§7.4). Each has a working equivalent (polling, the in-body or zip result, the
  configured model) or a named 400. The one fetch is an `http` source from a host the owner
  allowed, with the setting on (ruling 9, D28).
- D22. **Picture regions come from poppler's image positions** for PDFs until the ONNX layout model
  (WP8) also finds vector figures and charts; until then those are not found, with a note (§5.3).
- D23. **Formula and code enrichment** map onto tier 1: on the pages the vision model reads, its
  prompt returns formulas as LaTeX and code as fenced blocks; on other pages the option gets a
  note. Region-level formula and code reading comes with WP8's layout model (§3, §5.3).
- D24. **The status WebSocket is served** (`/v1/status/ws/{task_id}`), from the same job state as
  the poll route (§7.4).
- D25. **The report carries numbers, enums and salted hashes only,** checked against a
  whitelist schema before it is written (§10.1).
- D26. **The zip layout follows `main`** (was Q12): flat, `<stem>.md` at the archive root. docling-jobkit
  `main` (`_export_documents_as_files`) and the docling-serve 1.38.2 release (WP0a, §14) both write
  it; the numbered `<index:06d>/` directories are jobkit's export to remote storage targets, not the
  zip. WP0b still records what a deployed docling-serve writes, and the adapter switches if
  that differs (§7.4.3).
- D27. **Async jobs do not wait for the hold** (was Q13): pages and pictures that would need the
  held model get `model_held` and their tier-0 result, as in a sync request. A "wait for the
  card" job option only if a caller needs it (§7.7).
- D28. **`http` sources are fetched only with the setting on** (ruling 9, was Q11): the settings
  `documents.fetch_sources` (off) and `documents.source_hosts` (empty), the safeguards listed in
  ruling 9, one fetcher in `extract/convert/source_fetch.rs` (§7.4.1).

## 3. Tiers

**Tier 0: no model.** Always on.
- The pure-Rust readers. They are always there: text, markdown, CSV, HTML, docx/pptx/odt/odp,
  xlsx/ods, and the planned additions (EPUB, e-mail, legacy xls; §5.2).
- The optional tools, each used when found: poppler (PDF text, geometry, page images, picture
  positions), tesseract (OCR of scans and images), pandoc (formats lmgw has no reader for, docx
  equations), LibreOffice (legacy Word/PowerPoint, rendering slides to PDF), ffmpeg (audio out of
  video).
- Picture export: finding pictures and cropping them in reading order (§5.3).

Tier 0 runs on the CPU and keeps working under the GPU hold.

**Tier 1: model help, opt-in, for what tier 0 visibly gets wrong, and for describing pictures.**
Reading: pages with no text layer when tesseract is missing or reads badly, table-like pages,
slides whose content is a chart, formulas and code on the pages the model reads (D23). A model
*reading* a page is conversion. Describing: one call per exported picture, through the
picture-description model (§5.3). Two kinds of model:

- **A vision model through an lmgw alias.** This is today's knowledge-base path (`vision_prompts.rs`),
  with any llama.cpp vision alias. granite-docling-258M is a candidate: llama.cpp supports it since
  PR #16206 (merged 2025-10-05), and `ggml-org/granite-docling-258M-GGUF` has Q8_0/F16 weights
  with matching `mmproj` files (§13). It answers in DocTags, not markdown, so it needs a DocTags
  reader of its own (WP6). The PR author noted that generation sometimes fails to stop, and a
  follow-up PR (#16438) addresses it. An lmgw row gets a visible `max_tokens` derived from the
  page's real bound instead of a guessed cap (§9). The page-reading model and the
  picture-description model are separate choices; they may be the same alias.
- **ONNX models on the CPU through `ort`:** layout detection, OCR and table structure. `oar-ocr`
  0.10.3 (Apache-2.0) pins the same `ort =2.0.0-rc.13` as lmgw and covers PP-OCRv4 to v6,
  PP-DocLayout, SLANet table structure and formula models. docling's own layout model is on
  Hugging Face as `docling-project/docling-layout-heron-onnx` (Apache-2.0). Models arrive through
  lmgw's own Hugging Face downloader into the aux models directory, never through a crate's
  auto-download. A picture classifier would run the same way (§12).

ASR for audio and video sits between the tiers: it always needs a model, but a CPU audio row
(Parakeet on CPU) needs no card and serves under the hold.

## 4. The capability table, detection and its data model

### 4.1 One table

New module `crates/lmgw-core/src/extract/convert/caps.rs` (new logic in new child modules, one
per concern; `convert/mod.rs` stays a dispatcher):

```rust
pub struct Handler {
    pub id: &'static str,            // "pdf.poppler", "office.native", "legacy.libreoffice", …
    pub formats: &'static [Format],  // the sniffed formats it reads
    pub needs: &'static [ToolId],    // all must be found; empty = always available
    pub tier: Tier,                  // Zero | Model
    pub gives: &'static str,         // plain words: "text, headings and tables from Word files"
}
pub const HANDLERS: &[Handler] = &[ /* in preference order per format */ ];
pub struct Tool { pub id: ToolId, pub label: &'static str, pub packages: Packages,
                  pub probe: Probe, pub licence: &'static str }
pub const TOOLS: &[Tool] = &[ /* poppler, tesseract, pandoc, ffmpeg, libreoffice, pdfium */ ];
```

Both the settings op (§8) and the conversion route call `caps::resolve(format, &facts, &choices)
-> Resolution { handler, missing: Vec<ToolId>, warnings }`. The page renders `resolve` for every
format, and the route acts on `resolve` for the format it sniffed. Neither has a second source, so
the two cannot disagree. A test walks `HANDLERS` against `TOOLS` (every `needs` names a known
tool, every `Format` has a tier-0 row or a named "no reader yet" row).

### 4.2 Rows (one path per format)

| Format (sniffed) | Handler, first available wins | Needs | Without it |
|---|---|---|---|
| text, markdown, CSV, AsciiDoc, WebVTT | native passthrough | — | — |
| HTML | native reducer (`quickdoc-core` `html::to_text`, then htmd, §5.2) | — | — |
| docx, pptx, odt, odp, xlsx, ods | native `office.rs` | — | — |
| docx with OMML equations | pandoc (`docx+math` variant) | pandoc | native, with "equations left out; install pandoc to get them as LaTeX" |
| PDF with a text layer | poppler text + geometry (§5.1) | poppler | "install poppler-utils to read PDFs" (Windows: pdfium, not yet) |
| PDF pages without text, images | tier 1 vision if chosen, else tesseract | alias / tesseract | page listed in `warnings` as unread, with both remedies |
| table-like PDF pages | tier 1 vision if chosen, else poppler text | alias | text kept, warning says a model would read the table |
| pictures inside documents (export, §5.3) | PDF: poppler image positions + page render; office/EPUB: the embedded media in document order; an image file: itself | poppler for PDFs | placeholder only, with "install poppler-utils to export pictures from PDFs" |
| picture descriptions (§5.3) | the picture-description model | alias | picture without a description, `model_not_chosen` |
| legacy .xls | calamine (§5.2) | — | — |
| legacy .doc, .ppt | LibreOffice → docx/pptx → native | libreoffice | "install LibreOffice to read old Word/PowerPoint files" |
| slides with charts (pptx/odp) | LibreOffice → PDF → page images → vision | libreoffice + alias | slide text only, warning |
| EPUB | native (zip + quick-xml, §5.2) | — | — |
| e-mail (.eml) | native (`mail-parser`); attachments recurse through the table | — | — |
| RTF, LaTeX, reStructuredText, Org, DocBook, FB2, MediaWiki | pandoc | pandoc | "install pandoc to read RTF/LaTeX/…" |
| audio (WAV/MP3/Ogg/FLAC/M4A/WebM) | ASR through the chosen audio alias | alias | "choose a speech-to-text model" |
| video (MP4/QuickTime/Matroska) | ffmpeg → audio → ASR | ffmpeg + alias | "install ffmpeg to transcribe videos" |

**The docx equation case and "one path per format".** The `docx+math` variant is decided by
sniffing (`m:oMath` in `word/document.xml`, read under the existing `Budget`), so it is a format of
its own in the table. When pandoc is present, it has exactly one handler, pandoc. When pandoc is
missing, it falls back to the native reader with a named warning. A native OMML→LaTeX converter
would make pandoc unnecessary here; it is not built yet (D11).

`sniff.rs` grows the new formats (OLE2 subtypes through `cfb`: `WordDocument`, `Workbook`,
`PowerPoint Document` streams; RTF `{\rtf`; EPUB's `mimetype` entry; e-mail headers; the video
containers it already names). The chat and knowledge-base refusals keep their wording until those
callers also move to `resolve` (WP7).

### 4.3 Detection: how, when, re-check

`extract/convert/tools.rs`. A probe has two steps:
1. A **PATH lookup** of the tool's binaries. This is a cheap walk over the directories, with no
   process started. On Windows and macOS it also checks the tool's known install directories
   (`C:\Program Files\LibreOffice\program\soffice.exe`,
   `/Applications/LibreOffice.app/Contents/MacOS/soffice`).
2. A **version run** with `crate::child` hygiene, a private empty working directory, stdin closed,
   and `TOOL_PROBE_TIMEOUT` (10 s, named in the page's error if a probe hangs):

| Tool | Probe | Extra facts read |
|---|---|---|
| poppler | `pdftotext -v` (stderr: `pdftotext version 26.08.0`); `pdftoppm`, `pdftohtml` looked up | which of the three binaries exist |
| tesseract | `tesseract --version` (first line) | `tesseract --list-langs`: installed languages |
| pandoc | `pandoc --version` (first line) | — |
| ffmpeg | `ffmpeg -hide_banner -version` | `ffmpeg -hide_banner -decoders`: whether the audio decoders that matter (aac, mp3, opus, vorbis, flac, ac3/eac3) are present |
| LibreOffice | `soffice --version` with a throwaway `-env:UserInstallation` profile | which of Writer, Calc and Impress are installed (their libraries in `program/`; the exact file names are checked in WP1) |
| pdfium | load the library, `FPDF_GetVersion`-equivalent | not on Linux yet |

Probed on a Fedora 45 test machine on 2026-11-18: poppler-utils 26.08.0, pandoc-cli
3.7.0.2, ffmpeg 9.0.2 (RPM Fusion), LibreOffice 26.8.2.2, no tesseract.

**When.**
- At boot, in the background after the listener is up. It never delays the start.
- When the settings page asks: the "Check again" button, which calls the `doc_tools_check` op.
- Per conversion, for tools the snapshot marks **missing** only: the PATH lookup runs again
  (microseconds). If the binary has appeared, the tool is probed again before resolving, so a user
  who just ran `dnf install` does not need to restart lmgw or press anything.
- When a run fails with `ENOENT` or the binary's `(dev, inode, mtime)` has changed since the probe
  (an update or a removal), the tool is probed again and the snapshot replaced.

### 4.4 Data model

- **In memory, not stored (D7):** `ArcSwap<ToolFacts>` in the gateway state. For each tool:
  `Found { path, version, extras } | Missing | ProbeFailed { message }`, with `checked_at`.
  Detection is cheap, and a stored copy would only go stale. The machine facts the tool pool is
  sized from (§7.7) sit next to it.
- **Stored, in the `settings` table, through `settings_set`:**
  - `documents.vision_alias`: tier 1's page-reading model; empty means off.
  - `documents.vision_pages`: `missing_or_tables` | `every_page`.
  - `documents.picture_alias`: the picture-description model; empty means off (§5.3).
  - `documents.asr_alias`: the speech-to-text model.
  - `documents.ocr_languages`: defaults to the installed packs that match the desktop locale.
  - `documents.onnx_layout`: off until WP8.
  - `documents.tool_slots`: empty means derived (§7.7).
  - `documents.job_retention_days`: in the Retention category.
  - `documents.fetch_sources`: whether the docling adapter fetches `http`/`https` sources; off by
    default (ruling 9).
  - `documents.source_hosts`: the hosts it may fetch from, filled in by the owner; empty by
    default. An entry that names a private, loopback or link-local address allows that range
    explicitly.

  The page shows plain labels for these, never the keys.
- **Jobs (§7.3):** a `convert_jobs` table: id, key id, state, created/finished, file count, total
  input bytes, part count, parts done, error; and `convert_job_files`: job id, index, name, bytes,
  state, parts, parts done, error. Results go under `<data dir>/convert/<id>/<index>/`.
- **What callers send (§7.8):** a `convert_calls` table keyed by the `request_logs` row: route,
  file count, and the options seen with their outcome.
- **DTO** (`lmgw-api-types`, `documents.rs`):
  - `DocCaps { tools: Vec<ToolRow>, formats: Vec<FormatRow>, missing: Vec<MissingRow>, pool: PoolFacts, checked_at }`
  - `ToolRow { id, label, found, version, path, extras }`
  - `FormatRow { format, label, handler, tier, quality: Full | Partial(reason) | None(reason) }`
  - `MissingRow { tool, label, install: Vec<InstallHint{platform, command_or_link}>, gives: Vec<format label> }`
  - `PoolFacts { slots, derived_from: { cores, memory_bytes, per_run_bytes }, overridden: bool }`
  - `SourceFetch { enabled, hosts: Vec<String> }`, part of the Documents settings read
  - `AdapterUsage { keys: Vec<AdapterKeyRow> }`, `AdapterKeyRow { key, last_seen, requests, routes: Vec<(route, count)>, options: Vec<OptionSeen{ name, value, outcome, count }> }`

## 5. The tools, one by one

### 5.1 OS tools

All of them run as child processes, as pdf.rs does: `crate::child` hygiene (no inherited fds, no
`LD_LIBRARY_PATH`), `kill_on_drop`, a named timeout, `RLIMIT_AS`, stdout read against the output
budget, stderr's first `STDERR_LINES_SHOWN` lines in the error, and `ENOENT` as a named
"install X" error. The runner moves out of `pdf.rs` into `extract/convert/run.rs`, which pdf.rs
then uses too. Every run takes a slot from the tool pool (§7.7). The "Fedora packages" column gives
install sizes from the Fedora 45 metadata.

| Tool | Fedora packages | Adds | Licence, and what bundling would mean | Windows / macOS | How it runs |
|---|---|---|---|---|---|
| poppler | `poppler-utils` (0.8 MiB + `poppler` 4 MiB) | PDF text; geometry; page images; picture positions | GPL-2.0/3.0 mix; run as a separate process, never linked, so lmgw's MIT is unaffected; bundling ships GPL binaries with their source offer | no official builds; pdfium instead (below) | stdin → stdout, as today. `pdftotext -bbox-layout - -` gives blocks/lines/words with boxes; `pdftohtml -xml -stdout -i - <dummy>` gives text runs with **font size, family and bold**, which is what headings need (verified: from stdin it needs a dummy output name, and `-stdout -i` writes no file; run it in a private empty cwd anyway). Picture positions come from the same run without `-i` (§5.3) |
| tesseract | `tesseract` + `tesseract-langpack-<lang>` (~9 MiB with `eng`, which `tesseract-common` requires); 123 language packs, and each pack `Supplements: (tesseract and langpacks-<lang>)`, so the locale's pack follows automatically | OCR of scans and images, tier 0 | Apache-2.0; bundling is unproblematic | UB-Mannheim installer on Windows, Homebrew on macOS | `tesseract stdin stdout -l <langs> hocr` (hOCR gives word boxes) on the page PNG from `pdftoppm`; `OMP_THREAD_LIMIT=1`, so one run is one slot's worth of CPU (§7.7) |
| pandoc | `pandoc-cli` (204 MiB, static) | RTF, LaTeX, RST, Org, DocBook, FB2, MediaWiki; docx equations → LaTeX | GPL-2.0-or-later; separate process | official installers for both | `pandoc -f <fmt> -t gfm --wrap=none` on stdin/stdout; `--sandbox` (no file or network access from the document) |
| ffmpeg | `ffmpeg-free` (Fedora) **or** `ffmpeg` (RPM Fusion), which `Conflicts: ffmpeg-free` (checked on the installed RPM Fusion package) | audio out of video, then ASR | `ffmpeg-free` is GPL-3.0-or-later as packaged; separate process | gyan.dev builds on Windows, Homebrew on macOS | `-nostdin -i <file> -vn -ac 1 -ar 16000 -f wav pipe:1`. An input **file** in a private temp dir, because MP4/QuickTime with the `moov` atom at the end cannot be read from a pipe. ffmpeg-free's gaps are mostly video codecs, and the video stream is dropped; the probe lists the real audio decoders, so a gap is reported by name (D13) |
| LibreOffice | `libreoffice-core` (289 MiB) + `-writer` (13 MiB) / `-calc` (28 MiB) / `-impress` (1 MiB), plus their dependencies | legacy .doc/.ppt; rendering slides/documents to PDF for vision | MPL-2.0; separate process | official installers for both | `soffice --headless --norestore --nolockcheck --nodefault -env:UserInstallation=file://<private>/profile --convert-to docx\|pdf --outdir <private>/out <private>/in.<ext>`. A **private temp dir and its own profile per run**, seeded with a `registrymodifications.xcu` that sets macro security to the highest level with macros disabled and links never updated. The run is killed as a **process group**, because `soffice` forks `soffice.bin`. Named `LIBREOFFICE_TIMEOUT`, `RLIMIT_AS` as poppler. Parallel runs get separate profiles |
| pdfium | not in Fedora; `pdfium-render` 0.9.5 (MIT/Apache-2.0) loads a `libpdfium` built by bblanchon/pdfium-binaries | Windows/macOS PDF text, geometry and rendering (no poppler there) | BSD-3-Clause/Apache-2.0; bundling is fine | the reason it exists | in-process loading would drop the child isolation, so it runs in a **worker process** (`lmgw --pdf-worker`, stdin/stdout, under the Windows job object's memory limit; Windows §4.4) |

**poppler geometry** (`extract/convert/pdf_layout.rs`, tier 0, WP3):
- Headings: font size clearly above the page's body size (the most common size by character
  count), or bold on a short line → `#`/`##`. The levels come from the size ranking within the
  document.
- Columns and reading order: `-bbox-layout` blocks sorted column-wise when the x-ranges of the
  blocks that have another beside them separate into two or more bands. A block across the
  bands, or a narrow one with nothing beside it in no band (a centred author line or caption),
  separates sections. Side-by-side blocks that are no columns (labels beside values) are read row
  by row. Otherwise they go top to bottom.
- Repeated headers and footers: a line whose text (the same, or differing only in a page number
  that counts with the page) appears in the top or bottom band of at least a third of the pages
  is dropped and named in `warnings` (`header_removed`, each text with its page count). The
  band's height and the threshold are named constants measured on the WP0a corpus in WP3, and
  checked against the WP0b numbers.
- Tables: today's `table_like` rule, plus x-aligned cells from word boxes (and 2-3 columns of
  short cells on one baseline), into markdown tables.

### 5.2 Pure-Rust additions (crates.io state, 2026-11-18)

| Crate | Version, licence | Use | Note |
|---|---|---|---|
| zip + quick-xml | already linked | EPUB: `META-INF/container.xml` → OPF spine → XHTML through the HTML path | Own reader preferred over `epub` 2.1.6 (GPL-3.0-or-later) and `rbook` 0.7.13 (Apache-2.0, an option if the own reader grows) |
| `mail-parser` | 1.0.1, Apache-2.0 OR MIT | .eml: headers, text/HTML body, attachments recursing through the table | Outlook .msg (CFB) is not yet |
| `cfb` | 0.16.0, MIT, already in the lock (through `msi`; 0.7.3 through `infer`) | OLE2 subtype in `sniff` | reading .doc text natively is not yet |
| `calamine` | 0.38.0, MIT | legacy .xls (BIFF) and .xlsb | lmgw's own xlsx/ods readers stay; calamine covers only what they do not |
| `htmd` | 0.6.0, Apache-2.0 | HTML → markdown (tables, links) | `html2md` is GPL-3.0+, so not that one |
| `dom_smoothie` | 0.19.0, MIT | readability (main-content extraction) for web pages | `readability` 0.3.0 has not been released since 2023-12 |
| `oar-ocr` | 0.10.3, Apache-2.0, `ort =2.0.0-rc.13` | tier-1 ONNX OCR/layout/table | same `ort` pin as lmgw |
| `ocrs` | 0.14.0, MIT OR Apache-2.0 | a pure-Rust OCR alternative (its own `rten` runtime) | Latin scripts; measured against tesseract in WP0a |

**Where they run (WP4 review round, §14).** Not in the gateway: the readers built on these crates
(HTML, EPUB, e-mail, `.xls`) and the image decoders run in lmgw's own reader executable,
`lmgw-doc-reader`, a child process started through `run.rs` like the OS tools of §5.1, with a
timeout, a memory limit and an output bound. The gateway links none of these parsers.

### 5.3 Pictures: export and description

New module `extract/convert/pictures.rs`. Export comes first (WP10); description sits on top of it
(WP11).

**Finding pictures, in reading order (tier 0):**
- **PDF:** `pdftohtml -xml` without `-i` lists each raster image with its page and box (the exact
  element and attributes are checked in WP10). The box is cropped out of a `pdftoppm` render of the
  page, so a picture is what the reader sees, including masks and rotation. The render resolution
  is `72 × images_scale` DPI (docling's `images_scale`, default 2.0, so 144 DPI). The picture's
  place in reading order is the block order of §5.1. Vector figures and charts are not found until
  the layout model (WP8) marks them (D22); a page that has drawings but no found picture gets a
  `pictures_partial` note.
- **docx/pptx/odt/odp/EPUB:** the embedded media (`word/media/*`, `ppt/media/*`, `Pictures/*`, the
  EPUB manifest's images), placed where the document references them (`a:blip r:embed`,
  `draw:image`, `<img>`), so the order is the document's own. No render is needed.
- **An image file:** the whole image is one picture.
- **A minimum size:** `picture_min_area`, the picture's share of the page area, default 0.05.
  It is a request field and named in the docs, never a hidden constant. For office files without
  page geometry it applies to the picture's declared extent against the page size the document
  declares. It filters the export. docling's `picture_area_threshold` (0.05) only skips a
  picture's description and keeps the picture (corrected in the WP11 review round, §14 WP11): it
  is the native `describe_min_area`, and the adapter exports pictures with no minimum size.

**Exporting them** (`pictures`, §7.1): `placeholder` (an `<!-- image -->` line where the picture
was, as docling writes it), `embedded` (a `data:` URI in the markdown), or `referenced` (a relative
link, with the picture bytes in the response's `pictures` list, or in the zip's `artifacts/`
directory on the adapter). JSON output gets a `figure` block per picture with its `bbox`, its
caption when the layout gives one, and `picture` (the id) and `description` fields.

**Describing them (tier 1):** one vision call per exported picture, through the picture-description
model (`documents.picture_alias`), as the calling principal:
- The prompt is the caller's (`picture_prompt`) or docling's default, "Describe this image in a
  few sentences.", so a caller that never set one gets the same kind of answer as from docling.
  The picture's caption and the paragraph before it go in as context, so the model knows what the
  document says around the picture.
- The answer goes next to the picture: in markdown as a paragraph after the picture and its
  caption (docling's markdown serializer writes an item's meta after the item, and its picture
  item holds the caption; checked in WP11); in JSON as the `description`. A picture smaller than
  `describe_min_area` of its page is not described.
- `max_tokens` comes from the alias's real context minus the picture's tokens (§9).
- No model chosen: the picture keeps its placeholder or image, no description, and a
  `model_not_chosen` warning (the adapter: a note, §7.4.2). The key may not use the model:
  `model_not_allowed`. Held without fallback: `model_held`. With a fallback: the fallback answers,
  as configured (D14).
- Each call writes its own usage row, attributed to the caller's key (§7.5).

**Formulas and code (D23).** docling's formula enrichment returns formulas as LaTeX, and code
enrichment reads code blocks. On pages the page-reading model reads, its prompt (a new
`VISION_PROMPT_VERSION`) returns formulas as `$…$`/`$$…$$` LaTeX and code as fenced blocks. docx
equations go through pandoc (D11). On other pages the formula stays as its text layer, with a
`formulas_partial` note naming the remedy (choose a page-reading model with "every page"). With the
layout model (WP8), formula and code regions are found on every page and only those regions go to
the model or to oar-ocr's formula models.

**What stays outside conversion.** Describing a picture is enrichment of the document's own
content, so it is served. Calls to anything the caller names (a URL, a webhook, a storage bucket)
are not (D21).

## 6. Packaging

### 6.1 The RPM

- **tauri cannot write `Suggests` yet.** `RpmConfig` in tauri-utils 2.9.2 (the lock's version) and
  2.10.2 has `depends`, `recommends`, `provides`, `conflicts` and `obsoletes`, and no `suggests`.
  So the choice today is between Recommends and nothing.
- dnf installs `Recommends` by default (`install_weak_deps=True`). A heavy tool in Recommends puts
  hundreds of MiB on every desktop that installs lmgw.

| Package | Decision | Why |
|---|---|---|
| `poppler-utils` | **Recommends** (D1; was Requires) | small, and PDFs are the most common upload; a user who removes it gets the named warning, never a broken install |
| `tesseract` | **Recommends** | ~9 MiB with English, and the locale's pack follows through its `Supplements`; scans become readable with no model |
| `/usr/bin/ffmpeg` (a file dependency, met by `ffmpeg-free` or RPM Fusion's `ffmpeg`) | **Suggests**, so nothing until tauri has it | the libraries come to tens of MiB; naming `ffmpeg-free` would conflict with RPM Fusion users' `ffmpeg` |
| `pandoc-cli` | **Suggests**, so nothing for now | 204 MiB for niche formats |
| `libreoffice-writer`, `-impress` | **Suggests**, so nothing for now | ~300 MiB with `libreoffice-core`; many desktops already have it |

When tauri gains `suggests` (the upstream PR is still to be written, D9), the three go there.
Until then the settings page is their only advertisement. Every change goes through
`ci/rpm-allowlist.toml` (`requires` loses `poppler-utils`; `recommends` gains two names), and
`ci/rpm_guard_test.py` checks that they agree.

The AppImage has no dependency mechanism. Detection (§4.3) covers it unchanged.

### 6.2 The server image (D10)

Ship `poppler-utils`, `tesseract` with `tesseract-langpack-eng` and `-deu`, and `ffmpeg-free`.
With these, the formats the docling callers send (plain conversion and OCR, §2 ruling 4) work in
the image. `install_weak_deps=False` stays, so every package is named. The language list is a
build argument (`TESSERACT_LANGS`) for an administrator who builds their own. pandoc and
LibreOffice (~500 MiB together) are not in the image yet. An administrator who needs them derives a
layer (`FROM` the image, `dnf install`), and the settings page in the image says exactly that,
instead of a `dnf` line that would not survive a container recreate.

### 6.3 Windows and macOS

No tool is shipped with the installers yet. The page links each tool's official installer, and
detection checks the known install directories. PDF reading on Windows/macOS needs pdfium (WP9).
Until then, PDFs there answer with the named "PDF reading is not available on Windows yet" warning.

## 7. The API

### 7.1 Native route

`POST /v1/documents/convert`, `Cap::Inference`. One file or several (§7.6). Body:
`multipart/form-data` with one or more `file` parts and optional fields, or JSON
`{ "files": [ { "name", "data_base64" }, … ], … }`. Bounded by the existing `max_body_mb` (a
visible setting).

| Field | Default | Meaning |
|---|---|---|
| `output` | `markdown` | `markdown` \| `json` \| `text` |
| `pages` | all | `"3-7,10"`, 1-based; applies to every file |
| `model_help` | the settings page's choice | `off` \| `missing` (only pages nothing read; added in WP6's review round, §14) \| `missing_or_tables` \| `every_page`; it can lower the setting, never raise it past a model the key may not use |
| `ocr` | `auto` | `off` \| `auto` (pages without text) \| `force` (every page, replacing the text layer) |
| `ocr_languages` | setting | tesseract language codes |
| `tables` | `true` | `false` keeps tables as plain text |
| `pictures` | `placeholder` | `placeholder` \| `embedded` \| `referenced` (§5.3) |
| `picture_min_area` | 0.05 | §5.3 |
| `describe_pictures` | `false` | one call per picture through the picture-description model (§5.3) |
| `picture_prompt` | docling's default | the description prompt |
| `page_break` | empty | a marker written between pages in markdown |
| `compact_tables` | `false` | markdown tables without padding |
| `async` | `false` | answer 202 with a job (§7.3) |

The answer (`markdown`/`text`), one entry per file (§7.6):

```json
{ "documents": [
    { "index": 0, "name": "example-a.pdf", "status": "converted", "format": "pdf", "parts": 12,
      "content": "…", "pictures": [], "warnings": [ … ],
      "stats": { "parts_by_source": { "text_layer": 11, "ocr": 1, "vision": 0, "native": 0 },
                 "headings": [3, 9, 0, 0, 0, 0], "tables": 2, "table_cells": 48, "pictures": 1,
                 "model_calls": 0, "time_ms": 840 } } ],
  "summary": { "files": 1, "converted": 1, "partial": 0, "failed": 0, "skipped": 0 },
  "tools": [ {"id":"poppler","version":"26.08.0"} ] }
```

`stats` is what WP0b compares (§10.1), and it is useful to any caller that wants to know how a
file was read.

`json` gives the structure, built by `extract/convert/doc_json.rs` from the same intermediate the
markdown is printed from. Per file, `content` is `null` and the structure is in `structure` (`parts`
stays the part count, as in every output):

```json
{ "format": "pdf", "parts": 1, "content": null,
  "structure": [ { "kind": "page", "number": 1, "width_pt": 595, "height_pt": 842,
      "blocks": [
        { "kind": "heading", "level": 1, "text": "Example Report", "bbox": [72,60,410,88], "source": "text_layer" },
        { "kind": "paragraph", "text": "…", "bbox": [72,96,523,180], "source": "text_layer" },
        { "kind": "figure", "bbox": [72,190,300,330], "source": "text_layer", "picture": "p1",
          "caption": "Figure 1: Example chart", "description": "…" },
        { "kind": "table", "bbox": [72,340,523,480], "source": "vision",
          "table": { "rows": 3, "cols": 4, "cells": [ { "row":0, "col":0, "row_span":1, "col_span":1, "header":true, "text":"Item" } ] } } ] } ],
  "warnings": [ { "code": "tool_missing", "tool": "tesseract", "parts": [4,5],
                  "message": "2 pages have no text; install tesseract (sudo dnf install tesseract) or choose a vision model under Settings → Documents to read them" } ] }
```

- `kind` takes one of: `page`, `slide`, `sheet`, `document` (docx/EPUB: one part with no
  geometry, `bbox: null`).
- Block kinds: `heading`, `paragraph`, `list_item`, `table`, `figure`, `caption`, `code`,
  `formula`, `page_header`, `page_footer` (kept in JSON, dropped from markdown).
- `source` takes one of: `native`, `text_layer`, `ocr`, `vision`, `onnx`.

### 7.2 Warnings, never silent gaps

Every gap is a warning with a code and a sentence that names what to install or choose:
- `tool_missing`
- `model_not_chosen`
- `model_held`: the GPU hold paused the model; tier 0's text is returned
- `model_not_allowed`: the key may not use the chosen alias
- `model_cannot_see`: the fallback answering for the model takes no pictures; nothing is sent to
  it and tier 0's text is returned (WP6's review round, §14)
- `part_failed`: one page's tool run failed; the rest is returned
- `page_scaled`: a page larger than `PAGE_MAX_PIXELS` was sent to the model scaled down
- `header_removed`
- `equations_dropped`
- `pictures_partial`: a page has drawings no picture was found for (§5.3)
- `picture_context_cut`: a picture's caption or paragraph before was sent to the
  picture-description model cut at `PICTURE_CONTEXT_MAX_BYTES` (WP11, §14)
- `formulas_partial`: formulas kept as text on pages no model read (§5.3)
- `ocr_language_missing`: a requested language has no installed pack; it names the package

A file with no handler at all fails with a code that names the missing tool, never an empty
success (§7.6 for how that reaches the HTTP status).

### 7.3 Sync and async

Sync holds the request until done. A sync request whose client disconnects is cancelled, and its
running children are killed. Async (`async=true`) answers `202 { "job": "cj_…" }`:
- `GET /v1/documents/jobs/{id}`: state, `parts_done/parts`, warnings so far, and per file its
  state and progress.
- `GET /v1/documents/jobs/{id}/result`: the same body as sync. `?index=<n>` answers one file's
  entry, so a client can fetch files as they finish.
- `DELETE /v1/documents/jobs/{id}`: cancels; running children are killed.

The client chooses; there is no hidden size threshold. A job belongs to its key, and a different
key gets a 404. Results are kept for `documents.job_retention_days` (Retention category). Jobs
survive a restart as `interrupted`, never as silently lost; files that had finished keep their
results.

### 7.4 The docling-serve adapter

`extract/convert/docling_serve.rs` maps docling-serve's convert surface onto §7.1–7.3 and §7.6 and
adds no conversion logic of its own. Routes as read on 2026-11-18 from docling-serve's `app.py` on
`main`, its `docs/usage.md` and `docs/configuration.md`, and the request/response models in
docling's `docling/datamodel/service/` (§13). The release then current on PyPI was 1.38.2; where
`main` and the docs differ, the adapter accepts both spellings.

#### 7.4.1 Routes

| docling-serve | lmgw |
|---|---|
| `POST /v1/convert/file` (multipart: one or more `files` parts, options as form fields, `target_type`) | native convert, sync |
| `POST /v1/convert/source` (JSON: `sources[{kind: "file", base64_string, filename}]`, or the older `file_sources[{base64_string, filename}]`; `target: {kind}`) | native convert, sync |
| same route, `http` sources (`sources[{kind: "http", url, headers}]` or `http_sources[{url, headers}]`) | honoured when the setting "Let callers send web addresses" is on (ruling 9, below); off (the default): a named 400 that says lmgw does not fetch URLs unless the owner allows it, that the file can be sent as base64 instead, and which setting would allow URL sources |
| same route, `s3` sources | named 400 (D21) |
| `POST /v1/convert/file/async`, `/v1/convert/source/async` | async job; answers `{task_id, task_type, task_status, task_position, task_meta}` |
| `POST /v1/convert/source/batch` (always answers a task) | async job, for the `inbody` and `zip` targets |
| `GET /v1/status/poll/{task_id}` | job state, mapped to `pending\|started\|success\|failure` |
| `WS /v1/status/ws/{task_id}` (D24) | the same state pushed on change, as docling's `{message: connection\|update\|error, task, error}` |
| `GET /v1/result/{task_id}` | job result in the adapter's shape (§7.4.3); 404 with docling's "Task result not found. Please wait for a completion status." while running |
| `GET /health`, `GET /ready` | `{"status":"ok"}`, `Cap::Public` |
| `GET /version` | lmgw's version and `"docling-serve-compat": "1.38"` |
| `/v1/chunk/*`, `/v1/capabilities`, `/v1/clear/*` | not yet: a named 501 that names the route (§12) |

**Fetching `http` sources (ruling 9, D28).** With `documents.fetch_sources` on, the adapter
downloads each `http`/`https` source itself, in `extract/convert/source_fetch.rs`, and then
converts the bytes like an uploaded file. The rules:
- the URL's host must be on `documents.source_hosts`; any other host is a named 400 that gives the
  host and the setting;
- addresses in private, loopback and link-local ranges are refused unless the list names them
  explicitly, and the check runs on the address the name resolved to, so a name that resolves into
  such a range is refused too;
- the download is bounded by `max_body_mb`, and exceeding it is a named error that quotes the
  setting and its value; the source fails and the other sources go on (§7.6);
- redirects are followed by hand, and every hop is checked against the list and the range rule
  again;
- the caller's `headers` are sent to that host, and only that host; they are never logged or
  recorded (§7.8);
- the fetch has a named `SOURCE_FETCH_TIMEOUT` (§9), and each fetch is logged with its host.

The host of every `http` source is recorded per key, fetched or refused (§7.8). The native route
takes files only (§7.1), so it has no URL input to guard.

**Targets.** `inbody` and `zip` are served. `put`, `s3` and `presigned_url` would make lmgw write to
a caller-chosen place: a named 400 (D21). docling-serve's own default target is chosen by its
deployment policy at start-up; the adapter defaults to `inbody`, which with one file gives the
plain JSON answer. WP0b records what a deployed docling-serve does by default, and the
adapter follows that if it differs.

**`callbacks`** are not called (D21), with a note; the caller can poll or use the WebSocket.

#### 7.4.2 Options

Each option a request sets gets one outcome: `honoured`, `mapped` (served by lmgw's equivalent),
`not_yet` (left out for now, the output says so) or `refused` (a named 400). Every outcome other
than `honoured` is listed in the response header `x-lmgw-option-notes` (an `LMGW_HEADERS` row),
for example `ocr_engine=mapped(easyocr→lmgw-ocr); do_picture_classification=not_yet`, logged once
per request, and recorded (§7.8). The body is docling's shape (D16).

| docling option | Outcome |
|---|---|
| `from_formats` | honoured: a file whose sniffed format is not in the list is `skipped`, as in docling |
| `to_formats` | `md` → markdown, `text` → text: honoured. `json` (a DoclingDocument), `html`, `html_split_page`, `doctags`, `doclang`, `dclx`, `yaml`, `vtt`, `latex`, `chunks`: not yet; their fields stay empty, `status` is `partial_success`, and `errors` names the missing output |
| `do_ocr` | honoured → `ocr=off` when false |
| `force_ocr` | honoured → `ocr=force`: tesseract on every page if found, else the page-reading model on `every_page` if chosen, else a note |
| `ocr_engine`, `ocr_preset` (`auto`, `easyocr`, `tesseract`, `tesseract_cli`, `ocrmac`, `rapidocr`, or any other name) | mapped: lmgw's OCR path (tesseract, then the page-reading model). An unknown name is not an error; the note carries the name the caller sent |
| `ocr_lang` (BCP-47: `en`, `de-DE`, `zh-Hant`, `mul`) | mapped to tesseract codes (`eng`, `deu`, `chi_tra`; `mul` = every installed pack). A tag without an installed pack: OCR runs with the rest and `ocr_language_missing` names the package |
| `ocr_custom_config` | not yet, note |
| `pdf_backend` | mapped: poppler (pdfium on Windows/macOS), note |
| `table_mode` | `accurate` → tables go to the page-reading model when one is chosen; `fast` → tier-0 tables only: with a model chosen the native `model_help` is `missing`, so the model reads only pages nothing else read |
| `do_table_structure` | honoured → `tables` |
| `table_cell_matching`, `table_structure_preset`/`_custom_config`, `layout_preset`/`_custom_config` | mapped: lmgw's table and layout path, note |
| `pipeline` | `standard`, `legacy`, `native` → the native pipeline; `vlm` → the page-reading model on every page (reading a page is conversion), or tier 0 plus a note when none is chosen; `asr` → the speech-to-text model |
| `vlm_pipeline_preset`, `vlm_pipeline_custom_config`, `vlm_pipeline_model`, `_model_local`, `_model_api` | mapped: the page-reading model. An API `url` in them is never followed (D21), note |
| `do_pdf_heading_hierarchy`, `pdf_heading_hierarchy_options` | mapped: poppler geometry's heading levels (§5.1) are always on |
| `page_range` | honoured → `pages` |
| `document_timeout` | honoured: a caller-set deadline per file that the error names |
| `abort_on_error` | honoured: the first failed part fails the file |
| `image_export_mode` | honoured → `pictures` (after WP10; before it `placeholder` with a note) |
| `include_images`, `images_scale` | honoured (§5.3) |
| `include_page_images` | not yet (only meaningful in DoclingDocument JSON), note |
| `md_page_break_placeholder`, `md_compact_tables` | honoured → `page_break`, `compact_tables` |
| `do_picture_description`, `picture_description_preset`, `_custom_config`, `_local`, `_api`, `picture_description_area_threshold` | mapped onto §5.3: the picture-description model; the caller's `prompt` honoured; `repo_id`, `generation_config`, `params`, `timeout`, `concurrency` and the object's other model settings replaced by the configured model, `url` and `headers` never followed, the note naming each key; `picture_description_area_threshold` is the native `describe_min_area` (a smaller picture kept, not described), and the adapter exports pictures with no minimum size, as docling does. No model chosen: pictures without descriptions, note |
| `do_picture_classification`, `picture_classification_preset`/`_custom_config` | not yet, note (§12) |
| `do_formula_enrichment`, `do_code_enrichment`, `code_formula_preset`/`_custom_config` | mapped onto tier 1 (D23); `formulas_partial` as a note where no model read the page |
| `do_chart_extraction`, `chart_extraction_preset`/`_custom_config` | not yet, note (§12) |
| `chunking_options`, `chunking_preset` | not yet, note |
| any option name the adapter does not know | `not_yet`, note with the name, never a 422; docling's own validation errors for known options (a wrong type) stay 422 as in docling |

#### 7.4.3 Response

- **One file, `inbody` target:** docling's `ConvertDocumentResponse`: `{ "document": { "filename",
  "md_content", "text_content", "json_content": null, "html_content": null, "doctags_content":
  null, "doclang_content": null }, "status", "errors", "processing_time", "timings": {} }`.
  `status` is `success`, `partial_success` (a part failed or an output is not produced),
  `skipped` or `failure`.
- **Several files, or the `zip` target (§7.6):** `application/zip` with
  `Content-Disposition: attachment; filename="converted_docs.zip"`, as docling-serve answers. Its
  layout is **flat**, as docling-jobkit `main` and the 1.38.2 release write it: `<stem>.md`,
  `<stem>.txt` and so on per requested output at the archive's root, and `artifacts/` for referenced
  pictures. The numbered directories (`000000/`, `000001/`, …) are jobkit's export to remote storage
  targets, which lmgw does not serve (D21). Where two files share a stem docling's flat zip loses
  one; lmgw keeps both under unique names (`<stem>_<index>`, compared without regard to case), cuts a
  stem past `MAX_STEM_BYTES` and says how many names it changed in `x-lmgw-documents`. The adapter
  switches layout if WP0b finds a deployment's differs (D26). A file that failed is absent from the
  zip, as in docling; lmgw adds the header `x-lmgw-documents: converted=…, partial=…, failed=…,
  skipped=…` and records the per-file errors (§7.8).
- **Async:** `GET /v1/result/{task_id}` answers the same two shapes. A task that failed as a whole
  answers docling's `TaskFailureResult` with status 200, as docling does; the sync routes answer it
  with 422.

**Auth:** docling-serve's `X-Api-Key` is a spelling lmgw's resolver already accepts on `/v1`
(`principal.rs`), so callers keep their header and only the key changes.

### 7.5 Gate, keys, hold, usage, docs

- **Capabilities.** Every new route gets a `CAPABILITY_TABLE` row (`server.rs`): the convert and
  job routes and the docling-serve routes are `Inference`, `/health` and `/ready` are `Public`, and
  the settings reads (`doc_tools`, `doc_adapter_usage`) are `Admin`.
- **Keys and principals.** Tier-1, picture-description and ASR calls are made as the calling
  principal through the normal alias path. A key whose policy does not allow the chosen model gets
  tier 0 plus `model_not_allowed`, and never borrows the owner's reach. Owner sessions see every
  job; other keys see only their own.
  *Budget note (WP6's review round, §14):* each page call asks the key's expiry, scope, budget
  and tokens-per-minute window before it is sent (requests-per-minute counts the conversion, not
  its pages). A call's cost is known when it ends, so a budget or a token window stops the *next*
  call; with several calls in flight a conversion can pass either by at most `in_flight_bound`
  calls (the model's slots, or the tool pool's size on a cloud route), each up to the model's
  largest answer. That is more than the "one request" the key-policy design states for a single
  model call, and it is what the user docs say.
- **GPU hold.** Tier 0 and CPU ONNX/ASR keep working. A held local model without a fallback gives
  `model_held` and the tier-0 result (pages as text, pictures without descriptions). A model
  **with** a fallback is used as configured (fallbacks always apply, D14), and its pages and
  pictures go to that cloud model. The settings page says so next to each model choice ("Under
  the GPU hold, pages go to <alias>"), so the privacy consequence is visible before it happens.
  The answer's `tools`/`stats.model_calls` names the alias that answered, from `x-lmgw-fallback`.
  §7.7 covers a hold switched on mid-request.
- **Usage.** Each conversion writes one `request_logs` row (the route as `ingress_proto`, the
  status, the total time) and a `convert_calls` row (§7.8). Each tier-1, picture-description or
  ASR call writes its own normal row, attributed to the same key. A "pages" unit for pricing is not
  yet (billable units §2 has seconds, characters, images and requests).
- **API docs.** Every route gets a `DocRoute` (`openapi/registry.rs`). The docling-serve routes go
  under their own tag, "docling-serve compatible", with the option table of §7.4.2. The new ops
  (`doc_tools`, `doc_tools_check`, `doc_adapter_usage`) go into `web/op_names.rs` and get `OpDoc`
  rows (`openapi/ops/table/`). `x-lmgw-option-notes` and `x-lmgw-documents` go into
  `LMGW_HEADERS`. The self-admin MCP plane gets `lmgw__doc_tools` and `lmgw__doc_adapter_usage`
  from the same ops.

### 7.6 Several files per request

**Request.**
- Multipart: any number of `file` parts (the adapter: docling's `files` parts). Option fields apply
  to every file.
- JSON: the `files` array (the adapter: `sources` or `file_sources`).
- One file is the same shape with one entry; there is no second single-file shape.
- Files are identified by their position (`index`, 0-based, in request order). Names may repeat.

**Limits.** No hidden ones. The request as a whole is bounded by `max_body_mb`, the visible body
setting, so the file count and total size are bounded by it and by nothing else. The 413 names the
setting and its value, and for JSON says that base64 makes a file about a third larger. A file's
own expansion and tool runs keep their per-file bounds (§9). There is no per-request file count
constant.

**Answer.**
- `documents` holds one entry per file in request order, whatever order they finish in.
- Each entry has its own `status` (`converted`, `partial`, `failed`, `skipped`), `warnings`,
  `stats`, and on failure an `error { code, message }`. One bad file never fails the others: a
  file that cannot be sniffed, has no handler, or whose tool crashed fails alone.
- `summary` counts the statuses.
- HTTP status (D18): 200 when at least one file is `converted` or `partial`. When none is, a
  one-file request keeps that file's own code (415 for "no handler", naming the missing tool; 422
  for an unreadable file), and a several-file request answers 422. The body has the same shape
  either way, so a client reads every file's error.
- `abort_on_error` (adapter) and a caller-set deadline act per file, never across files.

**Async.** One job per request, covering all its files (`convert_jobs` + `convert_job_files`,
§4.4). The job's `parts_done/parts` sums over the files, and its per-file states show which are
done. `?index=<n>` fetches one finished file's result while the rest still run. Cancelling the job
cancels every file still running. The job ends `succeeded` when every file has an outcome, even if
some failed; it ends `failed` only when no file converted.

**The adapter.** One file with the `inbody` target answers docling's JSON; several files always
answer the zip, whatever the target says, because that is what docling-serve does (docling-jobkit
writes the in-body answer only for exactly one document with an in-body target). docling-serve has
a per-request source count from its deployment policy; lmgw has none beyond the body size.

### 7.7 Scheduling and concurrency

**Tool runs: one gateway-wide pool (D19).** Every CPU-bound step takes a slot: a child process
(poppler, tesseract, pandoc, ffmpeg, LibreOffice, the pdfium worker) or a native reader on a
blocking thread. The pool's size is derived from real maxima: the smaller of the logical cores
(`std::thread::available_parallelism`, which respects the cgroup CPU quota in the server image)
and the usable memory (the cgroup `memory.max` in the image, the physical RAM elsewhere) divided by
the largest per-run memory (`PER_RUN_BYTES`: `POPPLER_MEMORY_LIMIT_BYTES` for the child plus the
two geometry outputs a PDF holds while its slot is held, `TOOL_MEMORY_LIMIT_BYTES`). The
expert view shows the size and both inputs; "Parallel tool runs" (`documents.tool_slots`) can set
it, and the page then says it was set by hand. tesseract runs with `OMP_THREAD_LIMIT=1`, so one run
uses one slot's worth of CPU.

**Order and fairness.** Within a request, files start in request order and pages in page order;
work from several files runs in parallel as slots free up. Across requests, free slots go round
robin over the waiting requests, grouped by key, so a 300-file batch does not block a one-page
caller (the same idea as fair queueing, candidate aliases §1, §4). A waiting request's job status
says how many tool runs are ahead of it.

**Model calls are a separate pool (D20).** A page or picture waiting for a model does not hold a
tool slot. Per conversion, at most as many model calls are in flight as the model has slots: for a
local llama.cpp row, its slot count from `/props` (`total_slots`); for a cloud alias, the
upstream's concurrency setting where it has one, otherwise the tool pool's size, shown in the job
status as the bound in use. A page image is rendered only when a call can take it, so a 500-page
scan does not hold 500 images in memory. Across conversions and chat, the calls queue in the
alias's normal queue with its fair queueing, so a conversion takes its turn like any other client.

**The GPU hold.**
- Tier 0, CPU ONNX and CPU ASR never wait on the hold.
- A held local model without a fallback: each page or picture that would have gone to it gets
  `model_held` at once and keeps its tier-0 result; the file ends `partial`, and the other files
  continue.
- With a fallback: the fallback answers, as configured (D14).
- Hold switched on mid-request: calls already running finish or fail as the hold handles them, and
  everything not yet sent takes the held path. Released mid-request: pages not yet sent go to the
  model again. Async jobs do not wait for a release (D27).

### 7.8 What callers send

A deployment answers over time what the callers use (§2 ruling 4, D17).

- **Per request,** a `convert_calls` row next to the `request_logs` row: the route, the file count,
  the target, and every option the request set with its outcome from §7.4.2. Values are kept for
  enum-like options (`to_formats=md,json`, `ocr_engine=easyocr`, `pipeline=vlm`); free text and
  addresses are recorded as "set" only (prompts, URLs, headers, callback targets). For `http`
  sources the host is recorded, refused or fetched, so the owner can see which hosts to list. No document content and no file names.
- **Summary:** the `doc_adapter_usage` op aggregates per key: requests, last seen, routes with
  counts, and each option with its values and outcomes. Settings › Documents shows it as "Who calls
  the docling route" once the adapter has been called, sorted so options that were `mapped`,
  `not_yet` or `refused` come first. The Usage page can filter its rows by the docling routes.
- The rows follow the usage retention; the summary is computed from them, so it needs no store of
  its own.

## 8. The settings page

A Settings category, **Documents** ("what files lmgw can turn into text, and what would add more"),
in `crates/lmgw-ui/src/pages/settings/` as its own child module (`documents.rs`). It reads
`doc_tools` (`DocCaps`, §4.4) and `doc_adapter_usage`, and "Check again" calls `doc_tools_check`.

```
 Settings › Documents                                                   [ Check again ]
 Checked 2 minutes ago                                                   [ ] Expert view
 ┌ What you can convert ─────────────────────────────────────────────────────────────┐
 │ ● Text, Markdown, CSV, web pages                       always                     │
 │ ● Word, PowerPoint, Excel, OpenDocument               always                     │
 │ ● PDF                                                  text, headings, tables     │
 │ ◐ Scanned PDF pages and images                         not read yet: see below    │
 │ ● Old Word/PowerPoint files (.doc, .ppt)               via LibreOffice            │
 │ ○ RTF, LaTeX, EPUB…                                    EPUB always; others: below │
 │ ◐ Pictures in documents                                kept, not described yet    │
 │ ◐ Audio                                                choose a speech model      │
 │ ○ Video                                                install ffmpeg             │
 └───────────────────────────────────────────────────────────────────────────────────┘
 ┌ Found on this computer ───────────────────────────────────────────────────────────┐
 │ PDF tools (poppler)            0.0.0-example                                      │
 │ LibreOffice                    0.0.0-example   Writer, Impress                    │
 └───────────────────────────────────────────────────────────────────────────────────┘
 ┌ Not installed ────────────────────────────────────────────────────────────────────┐
 │ Text recognition   Reads scanned pages and photos   sudo dnf install tesseract [Copy] │
 │ Pandoc             RTF, LaTeX, Org…; Word equations sudo dnf install pandoc-cli [Copy]│
 │ ffmpeg             The sound of videos, as text     sudo dnf install ffmpeg-free [Copy]│
 └───────────────────────────────────────────────────────────────────────────────────┘
 ┌ Model help ───────────────────────────────────────────────────────────────────────┐
 │ Read pages with a model     [ Off ▾ ]   (pages without text, and tables)          │
 │ Describe pictures with      [ Off ▾ ]   (one short description per picture)       │
 │ Speech-to-text model        [ example-asr ▾ ]                                     │
 └───────────────────────────────────────────────────────────────────────────────────┘
 ┌ Web addresses ────────────────────────────────────────────────────────────────────┐
 │ Let callers send web addresses   [ Off ]   (docling callers can name a page or file│
 │                                            to fetch; otherwise they send the file) │
 │ Allowed sites (expert view)      docs.example                                      │
 └───────────────────────────────────────────────────────────────────────────────────┘
 ┌ Who calls the docling route ──────────────────────────────────────────────────────┐
 │ example-key-1   41 requests, last today    asks for: picture descriptions (no     │
 │                                            model chosen), OCR engine "easyocr"    │
 │ example-key-2    7 requests, last 3 days   nothing lmgw handles differently       │
 └───────────────────────────────────────────────────────────────────────────────────┘
```

**Newcomers read plain words:** what they can convert, and what one install would add. No raw
setting keys appear as labels. The callers' box speaks of what was asked for ("picture
descriptions", "the OCR engine easyocr"), not option names; the expert view adds the option names
and counts. **Expert view** (the page's expert switch, from the newcomer direction's per-page
switch; no page has one yet) adds the following to the same rows, never as a second UI:
- binary paths
- the handler id per format
- language packs
- ffmpeg's audio decoders
- licences
- the probe's stderr when a probe failed
- the tool pool's size, what it was derived from, and "Parallel tool runs" (§7.7)
- the list of allowed sites for web addresses (the on/off row is always shown; the list is only
  edited here, and the page says so beside the row while the list is empty)
- the docling option names, values and outcome counts per key (§7.8)

On the server image, the install column says how to derive an image instead of `dnf` (§6.2). On
Windows/macOS it links each tool's installer. The static mockup is
[2026-11-18-document-conversion-settings-mockup.html](2026-11-18-document-conversion-settings-mockup.html)
(kit.css's dark palette, fake data).

## 9. Limits and messages

Every bound is either derived from a real maximum or a named constant that its error quotes:
- the body: `max_body_mb`, which also bounds how many files one request carries (§7.6)
- office expansion and tool output: `MAX_UNCOMPRESSED_BYTES` (1 GiB)
- the tools: `PDF_TIMEOUT`, `POPPLER_MEMORY_LIMIT_BYTES` (4 GiB), and the new
  `TESSERACT_TIMEOUT`, `PANDOC_TIMEOUT`, `FFMPEG_TIMEOUT`, `LIBREOFFICE_TIMEOUT` and
  `TOOL_MEMORY_LIMIT_BYTES`, each with its measured reason in its doc comment like `PDF_TIMEOUT`'s
- `TOOL_PROBE_TIMEOUT`
- fetching `http` sources (only with the setting on): each download bounded by `max_body_mb`, and
  `SOURCE_FETCH_TIMEOUT`, named in the error
- the tool pool: derived from cores and memory, visible (§7.7)
- model calls in flight per conversion: the model's real slot count, visible (§7.7)
- `picture_min_area`: a request field with docling's default (§5.3)

There is no page cap and no file-count cap. A vision page's or picture's `max_tokens` comes from
the alias's real context minus the image's tokens. An overrun is a visible `part_failed`, never
truncated text. Byte figures in messages go through the shared binary formatter
(`crate::hf::fmt_bytes`).

## 10. Work packages

| WP | Content | Depends on |
|---|---|---|
| WP0a | **Measure on a test machine, on a public corpus** (§10.1): tier 0 today vs docling-serve vs docling.rs vs `ocrs`/tesseract on scans; granite-docling on llama-server (does it stop, DocTags quality, time per page on the card). Once WP2 exists, a dry run of the WP0b script against the same corpus. Measured 2026-11-18 (§14 WP0a; the WP0b dry run is not yet, its script comes later). The dry run ran 2026-11-18 (§14, WP0b) | — (the script part: WP2) |
| **WP1** | **Settings page, detection, packaging.** `convert/caps.rs` (the table, with today's formats only), `convert/tools.rs` (probes, snapshot, re-check), `doc_tools`/`doc_tools_check` ops (+ `op_names`, `OpDoc`, MCP tool), the Documents category, `poppler-utils` Requires → Recommends, `tesseract` Recommends, `ci/rpm-allowlist.toml`, the server image's packages (§6.2), and the user docs page | — |
| WP2 | `POST /v1/documents/convert` sync, markdown/text, several files (§7.6), `stats`, with warnings; the tool pool (§7.7); extract the child runner out of `pdf.rs` into `convert/run.rs`; `DocRoute`s | WP1 |
| WP3 | poppler geometry (`pdf_layout.rs`): headings, columns, repeated headers/footers; `json` output (`doc_json.rs`). Built 2026-11-18 (§14, WP3) | WP2 |
| WP4 | tesseract OCR (hOCR → blocks), `ocr` off/auto/force; pure-Rust EPUB, e-mail, calamine .xls, htmd/dom_smoothie. Built 2026-11-19; security review round 2026-11-19: every third-party parser of untrusted files runs in the `lmgw-doc-reader` child (§14, WP4); rebased on WP10 2026-11-19 (§14, WP4 on WP10) | WP2 |
| WP5 | async jobs (`convert_jobs`, `convert_job_files`, retention), the docling-serve adapter with every route of §7.4.1 (WebSocket included), the option table of §7.4.2, the zip answer, `x-lmgw-option-notes`, `x-lmgw-documents`, `convert_calls` and the "Who calls the docling route" summary (§7.8). Built 2026-11-18 (§14, WP5) | WP2 |
| WP5b | `http` sources behind the setting (ruling 9, D28): `source_fetch.rs` with the host list, address-range check, `max_body_mb` bound and hand-followed redirects; the `documents.fetch_sources`/`source_hosts` settings and their page row; the named 400 while off; tests against a loopback server and a redirect to a private address. Built 2026-11-18, security review round 2026-11-19 (§14, WP5b) | WP5 |
| WP0b | **Measure at a deployment** (§10.1): an operator runs `scripts/convert-compare.py` once there. Built 2026-11-18 (§14, WP0b): the script, its tests and its dry run on a test machine; the run at a deployment is not yet | WP3, WP4, WP5 |
| WP6 | tier 1 vision through the alias (the existing prompts, the key's policy, the hold, `model_held`, the model-call pool of §7.7); formulas and code in the prompt (D23); DocTags reader if granite-docling wins WP0a. Built 2026-11-19, review round 2026-11-19 (§14, WP6) | WP2 |
| WP10 | picture export (§5.3): poppler image positions and crops, office/EPUB media in document order, `pictures` placeholder/embedded/referenced, `figure` blocks, the zip's `artifacts/`, `image_export_mode`/`include_images`/`images_scale` on the adapter. Built 2026-11-19 (§14, WP10); on WP8 2026-11-19 (§14, WP10 on WP8) | WP3 |
| WP11 | picture description (§5.3): the picture-description model setting and page row, one call per picture with caption context, the docling picture-description options mapped (§7.4.2). Built 2026-11-19 (§14, WP11) | WP10, WP6 |
| WP7 | LibreOffice (legacy .doc/.ppt, slides → PDF → vision), pandoc formats + `docx+math`, ffmpeg → ASR; chat attachments and knowledge bases resolve through `caps` too. Built 2026-11-19 (§14, WP7); slides → vision waits for WP6's hook; review round 2026-11-19: LibreOffice runs only Landlock-confined, transcription on WP6's per-call check, clock and 499 rows (§14, WP7 review round); on WP11 2026-11-19 (§14, WP7 on WP11) | WP4 |
| WP8 | ONNX layout/OCR/table through `oar-ocr` or docling's heron model, by WP0a's and WP0b's numbers; vector figures and formula/code regions from the layout. Built 2026-11-19: the heron layout model, opt-in; table and OCR models not yet (§14, WP8) | WP0a, WP3 |
| WP9 | pdfium worker for Windows/macOS | Windows WPs |
| WP12 | Labs › Documents: the conversion API tried from the dashboard (owner request 2026-11-18): files dropped or picked, the native options in plain words, sync or as a job followed live, the docling-compatible route beside it, each file's result as rendered markdown, raw text and the structure as an outline, stats, warnings and tools, the request as curl; runs kept in the lab store. Built 2026-11-19 (§14, WP12) | WP2, WP3, WP5 |

WP numbers are names, not an order: WP10 and WP11 were added after WP9 existed. WP0b runs once
tier 0 is complete (WP3, WP4) and the adapter exists (WP5), so a deployment compares what the
callers would really get. An operator may run it again after WP6/WP11 if models are chosen there;
the report says which models were chosen.

### 10.1 The measurement plan: WP0a on a test machine, WP0b at a deployment

**WP0a, on a test machine, on a public corpus.**
- Corpus: docling's own test documents, `tests/data/` in `docling-project/docling` (per-format
  folders, `pdf/`, `docx/`, `pptx/`, `xlsx/`, `html/`, `ocr/`, `tiff/`, `epub/`, `email/` and more,
  several with a `groundtruth/` folder of docling's expected output). The repository is MIT, but
  the folder carries no licence note of its own and some files are third-party documents (arXiv
  papers and the like), so the files are **not committed** to lmgw.
- Where it lives: a committed fetch script, `scripts/convert-corpus-fetch.sh <dest>`, does a
  sparse checkout of `tests/data` at a pinned docling commit into a named place outside `target/`,
  `~/.local/share/lmgw-corpora/docling-tests/<commit>/` by default. The pinned commit is the
  record; re-running the script restores the corpus.
- docling-serve for the comparison runs in Podman from its quay image, pinned by its `-immutable`
  digest, on a loopback port.
- The results go into this record (or a follow-up) with date, versions and the command that
  reproduces them; the raw outputs are scratch.

**WP0b, at a deployment: `scripts/convert-compare.py`.** A committed, self-contained script to
run once at a deployment, with Python's standard library only.
- **Needs:** `python3` with its standard library only (no `pip`), the docling-serve URL and key,
  the lmgw URL and key, and a folder of documents. It is run with `python3 -I`.
- **Usage:** `python3 -I scripts/convert-compare.py --docling URL --lmgw URL --dir FOLDER --out
  report.json` (keys from `DOCLING_KEY` and `LMGW_KEY` in the environment, never on the command
  line).
- **What it does per file:** sends it to docling-serve's `/v1/convert/file` (`to_formats=md`, the
  deployment's defaults otherwise) and to lmgw's native route (markdown, with `stats`), one after
  the other so the timings do not compete. `--also-adapter` sends it to lmgw's docling route too
  and records only whether the answer has docling's shape. Both outputs stay in memory; nothing is
  written but the report.
- **Compared automatically:**
  - text similarity of the normalised text (whitespace, markdown markup and case folded): a
    character 3-gram Jaccard and a line-level `difflib` ratio;
  - headings per level, tables, table cells, pictures, as counted in each markdown;
  - pages, and for lmgw the pages read by text layer, OCR and model (`stats`);
  - time per file on each side;
  - errors as codes: HTTP status, docling `status`, lmgw `error.code` and warning codes; never
    message text.
- **The report holds numbers only:**
  - per file: a run-local index, a salted hash (HMAC-SHA-256 of the file's bytes, 12 hex digits;
    the salt is generated per run and stays in a file next to the folder, never in the report),
    the extension, the size rounded to a power-of-two bucket in KiB, and the measures above;
  - totals and medians per extension;
  - the environment: docling-serve's `/version`, lmgw's version and `tools`, the chosen models'
    alias names as lmgw reports them, the core count, and whether a GPU was used;
  - what docling-serve answered by default for one file and for several (JSON or zip, and the zip
    layout's shape as "numbered directories" or "flat"), for D26.
- **Safe to share (D25):** before writing, the script validates the report against a
  whitelist schema in the script: every key is known, and every value is a number, a boolean, one
  of the fixed enums, a 12-digit hex hash, or a version string matching
  `^[0-9A-Za-z.+_-]{1,40}$`. Anything else aborts the run without a report. The report is a few
  KiB of JSON an operator can read in full before sharing it. No document text, no file
  names, no paths, no error messages.
- **Tests:** `scripts/convert_compare_test.py` runs it against two mock servers on loopback and
  checks the schema guard rejects a planted file name.

## 11. Open questions

None are left open. Questions 1–13 are answered (§2 rulings 4–10 and D8–D15, D26–D28); their
numbers are not reused.

## 12. Not yet

None of these is ruled out:
- DoclingDocument JSON, HTML, DocTags, DocLang, YAML, WebVTT and LaTeX output on the adapter
- `/v1/chunk/*` and chunking options on the convert routes
- `/v1/capabilities` and `/v1/clear/*`
- `s3` sources, and the `put`/`s3`/`presigned_url` targets (`http` sources are served when the owner allows them, ruling 9)
- picture classification: a small classifier in docling, not an LLM; an ONNX model through `ort`
  on the CPU is the likely route
- chart extraction (numbers out of charts)
- vector figures and charts as pictures before the layout model (WP8)
- page images in the output (`include_page_images`)
- engine-specific OCR settings (`ocr_custom_config`)
- Outlook .msg
- native legacy .doc text
- JATS/USPTO/XBRL XML
- a "pages" billing unit
- network isolation for LibreOffice runs (a namespace or `bwrap`)
- tools shipped inside the Windows/macOS installers
- an install button (D12)
- a job option to wait for the hold's release (D27)

## 13. External facts checked on 2026-11-18

- **docling-serve:** 1.38.2 on PyPI (2026-10-27).
  - Routes from `docling_serve/app.py` on `main`: `/v1/convert/{source,file}[/async]`,
    `/v1/convert/source/batch`, `/v1/status/poll/{id}`, `/v1/status/ws/{id}`, `/v1/result/{id}`,
    `/v1/chunk/{hybrid,hierarchical}/{source,file}[/async]`, `/health`, `/ready`, `/version`,
    `/v1/capabilities`. `/v1/convert/file` takes `files: list[UploadFile]` and a `target_type` form
    field whose default comes from a deployment's service policy; it rejects more files than the
    policy's `max_sources_per_request`. A sync request waits at most `DOCLING_SERVE_MAX_SYNC_WAIT`
    (default 120 s) and then answers 504.
  - Options, sources and the single-file response shape from `docs/usage.md`; the request,
    response and option models from docling's `docling/datamodel/service/{requests,responses,
    options}.py` (docling-serve's own `datamodel/` modules are shims re-exporting them).
    `ConvertSourcesRequest` has `sources: list[…]` (min 1) with `kind` `http`/`file`/`s3`, `target`
    and `callbacks`; `usage.md` still documents `http_sources`/`file_sources`.
  - Picture description: `PictureDescriptionApi` has `url`, `headers`, `params`, `timeout` (20),
    `concurrency`, `prompt`; `PictureDescriptionLocal` has `repo_id`, `prompt`,
    `generation_config` (`max_new_tokens: 200`). The default prompt is "Describe this image in a
    few sentences."; the default `picture_area_threshold` is 0.05. Remote services need
    `DOCLING_SERVE_ENABLE_REMOTE_SERVICES=true` (default false).
  - `ocr_engine` is a free string (deprecated for `ocr_preset`, default `auto`); docling's
    deprecated `OcrEngine` enum has `auto`, `easyocr`, `tesseract_cli`, `tesseract`, `ocrmac`,
    `rapidocr`. `ocr_lang` takes BCP-47 tags, with `mul` alone. `ProcessingPipeline` has `legacy`,
    `standard`, `native`, `vlm`, `asr`.
  - Several files: docling-jobkit's `convert/results.py` writes the in-body answer only for exactly
    one document with an in-body target; anything else becomes `converted_docs.zip`
    (`application/zip`), and a failed document is counted, not raised. Its `convert/export.py`
    writes `<index:06d>/<stem>.{md,json,html,txt,doctags,dclg}` with `artifacts/` for pictures (its export to
    remote storage targets; the zip is flat, below).
  - `X-Api-Key` when `DOCLING_SERVE_API_KEY` is set. `DOCLING_SERVE_SINGLE_USE_RESULTS` defaults
    to true (a result can be fetched once).
  - **Run on a test machine on 2026-11-18 (image `docling-serve-cpu` v1.38.2, docling 2.142.1, §14 WP0a):**
    with the defaults, one file to `/v1/convert/file` answers JSON in the body; two files answer
    `converted_docs.zip` (`application/zip`, `Content-Disposition: attachment`) whose entries are
    **flat**, `<stem>.md` (`csv-comma.md`, `duck.md`), not `main`'s `<index:06d>/` directories
    (D26). `/v1/capabilities` exists and answers `versions`, `stages` (`ocr`, `layout`,
    `table_structure`, `vlm_pipeline`, `picture_description`, `picture_classification`,
    `code_formula`, `chart_extraction`, `chunking`, each with `option`, `custom_config_option`,
    `default`, `presets`), `sources`, `targets` (`allowed`, `default: inbody`), `output_formats`,
    `image_export_modes`, `limits` (`max_document_timeout`, `max_images_scale`,
    `max_sources_per_request` 3, `max_num_pages`, `max_file_size`) and `features`
    (`api_key_required`, `artifact_storage`, `websocket`).
  - **Checked in WP11 (2026-11-19):** the markdown form docling gives a picture description
    (§14 WP11). **Not verified:**
    `pdftohtml -xml`'s image element (checked in WP10, poppler 26.08, 2026-11-19): without `-i`
    it is `<image top left width height src="out-<page>_<n>.png|jpg"/>` in points at `-zoom 1`,
    and each image is written as a file into the working directory (no element when the file
    cannot be written); with `-dataurls` the `src` is a `data:` URI and nothing is written (§14
    WP10).
- **docling test corpus:** `docling-project/docling` `tests/data/` (MIT repository; per-format
  folders, `groundtruth/` subfolders; no licence note in the folder).
- **granite-docling in llama.cpp:** PR #16206 "Granite docling + Idefics3 preprocessing", merged
  2025-10-05 (`ca71fb9`). `ggml-org/granite-docling-258M-GGUF` holds
  `granite-docling-258M-{Q8_0,f16}.gguf` and `mmproj-granite-docling-258M-{Q8_0,f16}.gguf`,
  Apache-2.0. Run on a test machine on 2026-11-18 (llama.cpp b12493, §14 WP0a): the DocTags structure tags are
  special tokens, so llama-server returns them only with `--special`; without it the answer is
  bare text with `<loc_*>` boxes.
- **ONNX:**
  - `oar-ocr` 0.10.3 (Apache-2.0, `ort =2.0.0-rc.13`; PP-OCRv4–v6, PP-DocLayout, SLANet,
    formula models; models from ModelScope on its own auto-download, which lmgw would not use).
  - `docling-project/docling-layout-heron-onnx` (`model.onnx`, Apache-2.0).
  - `paddle-ocr-rs` 0.6.1, last released 2025-08.
  - `docling` / `docling-pdf` / `docling-serve` crates 2.85.0 (2026-11-17).
- **crates.io:** `mail-parser` 1.0.1, `calamine` 0.38.0, `cfb` 0.16.0, `htmd` 0.6.0, `html2md`
  0.2.17 (GPL-3.0+), `dom_smoothie` 0.19.0, `readability` 0.3.0 (2023-12), `epub` 2.1.6
  (GPL-3.0-or-later), `rbook` 0.7.13, `pdfium-render` 0.9.5, `ocrs` 0.14.0.
- **Fedora 45 metadata (dnf cache of 2026-11-07):**
  - `tesseract` 5.5.4, Apache-2.0; langpacks carry `Supplements: (tesseract and langpacks-<lang>)`.
  - `pandoc-cli` 3.7.0.2, 204 MiB.
  - `ffmpeg-free` 9.0.2, GPL-3.0-or-later.
  - `libreoffice-core` 26.8.2.2, 289 MiB.
  - `poppler-utils` 26.08.0.
- **tauri:** `RpmConfig` in tauri-utils 2.9.2 and 2.10.2 has no `suggests`.

## 14. Build notes

### WP0a

**Measured 2026-11-18** on a 16-core test machine with a 24 GiB GPU, under load from other builds, so every time below is from a loaded machine. Within a
run the systems took each file one after the other, so their times are close together. The
docling.rs pass ran right after the main pass instead of inside it (its models arrived late), so
its files were not interleaved with the others'; the OCR pass over rendered PDFs ran before both.

**Versions.**
- Corpus: docling `tests/data` at v2.150.1, commit `5e600e8d37a11f70b62f795ea1e3ce3a4052e551`
  (`scripts/convert-corpus-fetch.sh`; 1087 files, 172 MiB). Its `groundtruth/` was written by
  that docling version.
- lmgw: `main` at `397cabdc` (WP2), dev build, headless example; poppler 26.08.0.
- docling-serve: `quay.io/docling-project/docling-serve-cpu@sha256:7a480a1604fa3b42efeddc7db9527690d9f9bc903226136013e2bfe6b1a4dfb0`,
  the digest of tag `v1.38.2`. quay has no `-immutable` tag for this repository (checked
  2026-11-18), so the version tag's digest is the pin. Inside: docling-serve 1.38.2, docling
  2.142.1, docling-core 2.104.0, docling-ibm-models 4.0.4, docling-parse 7.23.0, the image's
  `OMP_NUM_THREADS=4`. The models are baked into the image (`docling-layout-heron`, its ONNX
  export, `docling-models`, `DocumentFigureClassifier-v2.5`, EasyOcr, RapidOcr); auto OCR chose
  RapidOCR (onnxruntime). The only download at run time: the first METS-GBS file fetched
  `docling-project/docling-layout-heron@main` from Hugging Face. Audio fails: the CPU image has no
  `whisper` (its answer is a 404).
- docling.rs: `docling-cli` 2.85.0 (binary `docling-rs`, ONNX Runtime 1.31), `cargo install
  --locked` through `scripts/build-slot.sh`, 1 min 42 s. Its models are the `models-v1` release
  assets its `download_dependencies.sh` names (layout heron fp32 + int8, PP-OCR det/rec v6/en/ch +
  textline orientation, TableFormer encoder/decoder/decoder_kv/bbox + int8/fp16), 705 MiB in
  `<install root>/.models`, each checked against the release's `SHA256SUMS`; they were fetched
  with curl, not with the script. At first run it writes optimised graphs, 289 MiB, to
  `$XDG_CACHE_HOME/docling-rs/graphs` (`~/.cache` by default); the timed pass ran with that cache
  warm. Default threads.
- tesseract 5.5.4 (leptonica 1.87.0), `eng+deu`, `OMP_THREAD_LIMIT=1`.
- ocrs: `ocrs-cli` 0.14.0, models `text-detection.onnx` (sha256 `a917b23d…`) and
  `text-recognition.onnx` (`86c145c2…`) from `ocrs-models.s3-accelerate.amazonaws.com`, passed by
  path (its default cache is `~/.cache/ocrs`), `RAYON_NUM_THREADS=1`. Pages rendered at 300 DPI
  with `pdftoppm` for both OCR engines; the render is inside their times.
- granite-docling: `localhost/lmgw-llama-server:official-master` (llama.cpp b12493, commit
  `9048bad`, CUDA 13.0.0), GGUFs from `ggml-org/granite-docling-258M-GGUF` revision `684c1800`:
  `granite-docling-258M-Q8_0.gguf` sha256
  `b687245c212b59da938e411e2d4485596cbaeed1ae52706582ac38ed6a970f76`,
  `mmproj-granite-docling-258M-Q8_0.gguf` sha256
  `3d440e65a91e640dc0a53897ad7897700c39324802aaff2072232b6f21d0cadb`; `-c 0` (the trained 8192),
  `-ngl 99`, `--special`, temperature 0, pages at 144 DPI, prompt "Convert this page to docling.".

**Commands** (`$CORPUS` = `~/.local/share/lmgw-corpora/docling-tests/5e600e8d37a11f70b62f795ea1e3ce3a4052e551`;
`LMGW_KEY` = the dev instance's owner token from its log):

```sh
scripts/convert-corpus-fetch.sh
LMGW_CONTAINER_PREFIX=lmgw-wp0a scripts/dev-instance.sh 127.0.0.1:8890
podman run -d --name lmgw-wp0a-docling -p 127.0.0.1:8891:5001 -e DOCLING_SERVE_MAX_SYNC_WAIT=900 \
  quay.io/docling-project/docling-serve-cpu@sha256:7a480a1604fa3b42efeddc7db9527690d9f9bc903226136013e2bfe6b1a4dfb0
scripts/build-slot.sh cargo install --root target/wp0a/docling-rs --target-dir target/wp0a/docling-rs/build --locked docling-cli@2.85.0
scripts/build-slot.sh cargo install --root target/wp0a/ocrs --target-dir target/wp0a/ocrs/build --locked ocrs-cli@0.14.0
OCR=(--page-cmd "tesseract=env OMP_THREAD_LIMIT=1 tesseract {in} - -l eng+deu"
     --page-cmd "ocrs=env RAYON_NUM_THREADS=1 $OCRS/bin/ocrs --detect-model $OCRS/models/text-detection.onnx --rec-model $OCRS/models/text-recognition.onnx {in}")
python3 -I scripts/convert-measure.py --corpus "$CORPUS" --lmgw http://127.0.0.1:8890 \
  --docling http://127.0.0.1:8891 "${OCR[@]}" --out main.json
python3 -I scripts/convert-measure.py --corpus "$CORPUS" \
  --cmd "docling-rs=env XDG_CACHE_HOME=$DRS/cache $DRS/bin/docling-rs --no-progress {in}" --out doclingrs.json
python3 -I scripts/convert-measure.py --corpus "$CORPUS" --formats pdf --ocr-formats pdf "${OCR[@]}" --out ocr-pdf.json
podman run -d --name lmgw-wp0a-granite --device nvidia.com/gpu=all -p 127.0.0.1:8892:8080 \
  -v ~/.local/share/lmgw-corpora/models/granite-docling-258M:/models:ro,Z localhost/lmgw-llama-server:official-master \
  -m /models/granite-docling-258M-Q8_0.gguf --mmproj /models/mmproj-granite-docling-258M-Q8_0.gguf \
  -c 0 -ngl 99 --special --host 0.0.0.0 --port 8080 -np 1
python3 -I scripts/convert-measure.py --corpus "$CORPUS" --lmgw http://127.0.0.1:8890 --formats none \
  --granite http://127.0.0.1:8892 --granite-max-pages 38 --out granite.json --granite-docs \
  pdf/sources/2206.01062.pdf,pdf/sources/2305.03393v1-pg9.pdf,pdf/sources/amt_handbook_sample.pdf,pdf/sources/code_and_formula.pdf,pdf/sources/picture_classification.pdf,pdf/sources/table_misidentified_as_form.pdf,pdf/sources/table_mislabeled_as_picture.pdf,pdf/sources/newspaper-00.pdf,pdf/sources/multi_page.pdf,pdf/sources/normal_4pages.pdf,ocr/sources/ocr_test.pdf,ocr/sources/nemotron_multipage.pdf,pdf/sources/redp5110_sampled.pdf
python3 -I scripts/convert-measure.py --table main.json doclingrs.json
```

(`--formats none` names no folder, so only the granite pages run.)

**What the numbers measure.** The reference is docling's own `groundtruth/` output, so the
similarity says how close a system comes to docling, not to the truth; docling-serve and docling.rs
are near the ceiling by construction (and docling-serve's docling is 2.142.1, the ground truth's
2.150.1). The 3-gram Jaccard compares the text; the line ratio also compares how it is cut into
lines, so it is low for anything that keeps a PDF's physical lines (tier 0, OCR). docling-serve's
sync route polls its own job queue: its wall clock has a floor of about 2 s a file, and its own
`processing_time` is 1.56 s a PDF page against 1.85 s wall.

**Results, main formats** (files converted / files; s/file median and s/page are wall clock; s/page
covers the files with pages only, so for html it is the folder's one PNG; "with
ref" are the converted files that have a reference; the last three columns are counted in the
output against the reference):

| format | system | conv. | failed | s/file med. | s/page | with ref | 3-gram J | line ratio | headings | tables | cells |
|---|---|---|---|---|---|---|---|---|---|---|---|
| pdf (28, 115 p.) | lmgw tier 0 | 28/28 | - | 0.039 | 0.018 | 18 | 0.807 | 0.091 | 2/159 | 0/31 | 0/1322 |
| | docling-serve | 28/28 | - | 4.004 | 1.847 | 18 | 0.961 | 0.915 | 160/159 | 32/31 | 1387/1322 |
| | docling.rs | 28/28 | - | 0.714 | 0.344 | 18 | 0.972 | 0.815 | 161/159 | 30/31 | 1413/1322 |
| | tesseract (rendered) | 28/28 | - | 2.384 | 2.275 | 18 | 0.637 | 0.087 | 3/159 | 0/31 | 0/1322 |
| | ocrs (rendered) | 28/28 | - | 1.770 | 1.569 | 18 | 0.537 | 0.036 | 2/159 | 0/31 | 0/1322 |
| docx (38) | lmgw tier 0 | 38/38 | - | 0.026 | - | 38 | 0.902 | 0.776 | 75/77 | 26/25 | 311/361 |
| | docling-serve | 38/38 | - | 2.004 | - | 38 | 0.978 | 0.946 | 77/77 | 25/25 | 348/361 |
| | docling.rs | 38/38 | - | 0.017 | - | 38 | 0.982 | 0.961 | 77/77 | 25/25 | 348/361 |
| pptx (8) | lmgw tier 0 | 8/8 | - | 0.011 | - | 8 | 0.793 | 0.592 | 18/13 | 1/3 | 63/86 |
| | docling-serve | 8/8 | - | 2.004 | - | 8 | 0.945 | 0.942 | 13/13 | 2/3 | 78/86 |
| | docling.rs | 8/8 | - | 0.007 | - | 8 | 0.963 | 0.973 | 13/13 | 3/3 | 86/86 |
| xlsx (12) | lmgw tier 0 | 12/12 | - | 0.011 | - | 12 | 0.625 | 0.000 | 24/22 | 23/50 | 1271/787 |
| | docling-serve | 12/12 | - | 2.004 | - | 12 | 0.786 | 0.701 | 0/22 | 50/50 | 787/787 |
| | docling.rs | 12/12 | - | 0.007 | - | 12 | 0.813 | 0.812 | 0/22 | 50/50 | 787/787 |
| odf (12, 6 are PDFs) | lmgw tier 0 | 12/12 | - | 0.028 | 0.021 | 12 | 0.832 | 0.447 | 22/48 | 11/30 | 152/408 |
| | docling-serve | 12/12 | - | 2.016 | 2.150 | 12 | 0.907 | 0.660 | 50/48 | 25/30 | 357/408 |
| | docling.rs | 12/12 | - | 0.249 | 0.894 | 12 | 0.914 | 0.690 | 49/48 | 25/30 | 367/408 |
| html (33) | lmgw tier 0 | 32/33 | no_reader 1 | 0.009 | - | 32 | 0.682 | 0.464 | 0/68 | 0/24 | 0/276 |
| | docling-serve | 33/33 | - | 2.004 | 2.006 | 32 | 0.962 | 0.899 | 68/68 | 24/24 | 276/276 |
| | docling.rs | 33/33 | - | 0.006 | 0.91 | 32 | 0.974 | 0.962 | 68/68 | 24/24 | 276/276 |
| md (11) | lmgw tier 0 | 11/11 | - | 0.009 | - | 11 | 0.972 | 0.858 | 35/37 | 4/5 | 62/82 |
| | docling-serve | 11/11 | - | 2.004 | - | 11 | 0.962 | 0.853 | 37/37 | 4/5 | 62/82 |
| | docling.rs | 11/11 | - | 0.006 | - | 11 | 0.991 | 0.984 | 37/37 | 5/5 | 82/82 |
| csv (9) | lmgw tier 0 | 9/9 | - | 0.014 | - | 9 | 0.489 | 0.241 | 0/0 | 0/9 | 0/380 |
| | docling-serve | 9/9 | - | 2.004 | - | 9 | 0.887 | 0.918 | 0/0 | 9/9 | 380/380 |
| | docling.rs | 9/9 | - | 0.005 | - | 9 | 0.887 | 0.918 | 0/0 | 9/9 | 380/380 |
| ocr (10 scans, 5 with ref) | lmgw tier 0 | 0/10 | no_reader 10 | - | - | 0 | - | - | - | - | - |
| | docling-serve | 10/10 | - | 2.014 | 2.173 | 5 | 0.797 | 0.600 | - | - | - |
| | docling.rs | 10/10 | - | 1.687 | 1.477 | 5 | 0.786 | 0.400 | - | - | - |
| | tesseract | 10/10 | - | 0.933 | 0.877 | 5 | 0.476 | 0.000 | - | - | - |
| | ocrs | 10/10 | - | 0.744 | 0.708 | 5 | 0.261 | 0.000 | - | - | - |
| tiff (1 file, 2 p., 72 DPI) | lmgw tier 0 | 0/1 | unsupported 1 | - | - | - | - | - | - | - | - |
| | docling-serve | 1/1 | - | 10.008 | 5.004 | 1 | 0.560 | 0.116 | 6/8 | - | - |
| | docling.rs | 1/1 | - | 2.457 | 1.228 | 1 | 0.568 | 0.308 | 5/8 | - | - |
| | tesseract | 1/1 | - | 2.564 | 1.282 | 1 | 0.382 | 0.013 | 0/8 | - | - |
| | ocrs | 0/1 | exit_1 1 | - | - | - | - | - | - | - | - |
| webp (1) | docling-serve / docling.rs / tesseract / ocrs | 1/1 each | lmgw: no_reader | 2.003 / 1.439 / 0.285 / 0.258 | | 1 | 0.474 / 1.0 / 1.0 / 0.819 | | | | |

"tesseract/ocrs (rendered)" is the text PDFs rendered at 300 DPI and read as if scanned. The ocr
references are docling's per-engine outputs, preferring an engine not measured here (nemotron, then
ocrmac); the TIFF is pages 1–2 of `2206.01062` against that PDF's JSON ground truth. Per scan:
`ocr_test` (upright) tesseract 1.0, ocrs 0.901, docling-serve and docling.rs 1.0; rotated 90°
tesseract 0.023, ocrs 0.0, docling-serve 1.0, docling.rs 1.0; 180° 0.029 / 0.009 / 0.342 / 0.362;
270° 0.921 / 0.0 / 1.0 / 0.921. ocrs cannot read the TIFF (exit 1).

All 270 corpus files: lmgw converted 210, docling-serve 231, docling.rs 253. Where lmgw fails it
answers `unsupported` (afp, epub, keynote, numbers, pages, mets_gbs, tiff, 3 ebcdic, 3 audio) or
`no_reader` (the 10 scans and images in `ocr/`, the WebP, doc, ppt, xls, 13 audio, 2 email, one
file each in asciidoc, html and json_paddleocr_vl), and one password PDF is `unreadable`. RTF,
calendar, EBCDIC, XBRL, JATS, USPTO, LaTeX and AsciiDoc it converts as text, with no headings or
tables found. docling-serve failed doc, ppt, xls, rtf, the password PDF, 6 of 7 ebcdic, 2 of 4
json_dots and 2 of 3 json_paddleocr_vl (`failure`), calendar and one uspto file (`skipped`),
audio and xbrl (404). docling.rs failed only audio (no ASR models fetched) and the password PDF.

**granite-docling on llama-server** (40 page generations in all: 2 trial pages without
`--special`, then 38 with it, from 13 PDFs, 33 with a text layer and 5 scans):
- **Stops on its own on 35 of 38 pages.** 3 ran to the context limit (`finish_reason: length`,
  7050–7315 completion tokens, 9.5–10.1 s each), each in a repetition loop: one newspaper line
  repeated (`newspaper-00`), a rotated scan (`nemotron_multipage` p. 2), dot leaders of a table
  of contents (`redp5110_sampled` p. 2).
- **Well-formed DocTags on 35 of 38**, exactly the pages that stopped: one `<doctag>…</doctag>`
  with every container closed in order.
- **Time:** 2.29 s a page mean, 1.46 s median; 1.65 s mean over the pages that stopped; 683
  completion tokens a second overall. A letter or A4 page at 144 DPI is 1142 prompt tokens.
- **VRAM:** the llama-server process held 930 MiB after loading and 964 MiB when sampled during and
  after the run (`nvidia-smi` per process); the peak was not recorded.
- **Text against docling's JSON ground truth, same page**, on the 33 text-layer pages where
  lmgw's tier 0 also answered: granite 3-gram Jaccard mean 0.808, median 0.935, line ratio mean
  0.394; lmgw tier 0 Jaccard mean 0.848, median 0.883, line ratio mean 0.108. granite's Jaccard is
  higher on 21 of 33 pages. Its low pages: the 4 Korean pages of `normal_4pages` (0.39–0.64; tier
  0 0.80–0.88: it misreads Hangul), the newspaper runaway (0.37) and
  `table_misidentified_as_form` (0.33).
- **Scans** (tier 0 answers `no_reader`): `ocr_test` 1.0; `nemotron_multipage` p. 1 0.64, p. 2
  the runaway, pp. 3–4 (rotated) 0.0, where it put the text into a `page_header`, which the
  scorer drops as furniture.

**What this means for WP6.** granite-docling is cheap enough to read pages (about 1.5 s and under 1 GiB on the test card) and it recovers what tier 0 loses: its line ratio is more than three times tier
0's, because it joins paragraphs and finds headings and tables the way docling does. On plain
text-layer pages its text is not better than the text layer (mean 0.81 against 0.85), so it earns
its place on the pages model help is for (scans, tables), not on every page. The numbers support
building the DocTags reader, with what the run showed it needs: the lmgw row must start
llama-server with `--special`, or the structure is gone; a page answered with `finish_reason:
length` is a runaway, so the reader must treat it as a failed model page (tier-0 text or OCR for
that page, with a warning), and the `max_tokens` bound of §9 is what keeps a runaway at a few
seconds; Hangul and rotated scans are not its strength. Whether that counts as "granite wins" is
the owner's reading (D15); WP0b has not run.

**What this means for WP8.** docling's own route, the heron layout model and TableFormer through
ONNX Runtime as docling.rs runs them, reproduces docling's structure on PDFs (Jaccard 0.972,
headings 161/159, tables 30/31) at 0.34 s a page, five times faster than docling-serve's 1.85 s on
the same machine (thread counts differ: docling-serve 4 OMP threads, docling.rs its default).
That is evidence for the heron route; `oar-ocr`'s PP-DocLayout/SLANet route was not measured here.
For OCR, tesseract read better than ocrs on every scan measured (rendered PDFs 0.637 against
0.537, the upright scan 1.0 against 0.901, the WebP 1.0 against 0.819), ocrs cannot open TIFF, and
neither handles 90° and 180° pages without an orientation step, which docling-serve (RapidOCR) and
docling.rs (PP-OCRv6 with its text-line orientation model) do. ocrs is faster single-threaded
(1.57 against 2.28 s a rendered page) but by these numbers it is not a candidate; the PP-OCR models
are. For WP4: tesseract needs page orientation (OSD) before reading, or rotated scans come out as
noise. For D8: on these files docling.rs is clearly better than tier 0 on structure in every
office and PDF format; revisiting it as an optional external tool is the owner's decision.

**Not measured.** Audio (no system here had ASR set up for it). Per-page granite against
docling-serve. A repetition penalty or other sampling for granite's runaways (the GPU budget was
40 pages). Whether llama.cpp PR #16438 is in b12493. The granite VRAM peak. Thread-matched
timings for docling-serve against docling.rs. The WP0b script's dry run (not yet written).

### WP3

**Built (2026-11-18, poppler 26.08.0).**
- `extract/convert/doc.rs` (+ `doc/render.rs`, `doc/from_md.rs`): the one
  intermediate, parts (page/slide/sheet/document) of blocks with kind, text,
  box, and `Source` per block and per part (`None` on a part: not read). The
  PDF layout pass and the office/text markdown fill it; markdown, text and
  `json` (`doc_json.rs`) are renderers over it. WP4 (OCR), WP6 (vision
  pages), WP8 (layout regions) and WP10 (`figure` blocks) fill the same model.
- `extract/pdf/geometry.rs`: `pdftotext -bbox-layout` and `pdftohtml -xml -i
  -zoom 1 -noroundcoord` (in a private empty cwd), both with `-f`/`-l` from
  `pages`, one after the other on one pool slot that the layout then uses
  too (review round).
- `extract/convert/pdf_layout.rs` + `bbox`, `fonts`, `repeat`, `table`,
  `order`, `blocks`: the pass of §5.1, linear in lines (sorts are `n log n`);
  1 000 synthetic pages / 60 000 lines are a test, as are degenerate pages
  (20 000 lines on one row, 20 000 stacked lines) and hostile XML.
- `output: json` on the native route for PDFs, office files and text
  (`structure` field, wire types in `lmgw-api-types/src/documents/structure.rs`).

**Decisions:**
- The json array is `structure`, not `parts`: `parts` already carries the
  part count in every output, and one field keeps one type. `content` is
  `null` for json.
- Headings take the **document's** body size, not the page's: a title page
  or chapter opener has no body of its own.
- Levels rank only sizes that more than one heading has, plus the first
  page's largest (the title); a one-off size takes the level of the ranked
  size above it, and vertical text is no heading. Plain size ranking gave
  the LNCS paper of the corpus (2305.03393v1) `####` for its title and
  `#####` for its sections (a rotated arXiv margin note and figure labels
  took the top levels); now `#` and `##`. Levels come from the pages read,
  so `pages` on a book without its title page ranks its chapters `#`.
- Headers need space: candidates are the lines above the first gap of a line
  height (below it for footers), all inside the band. Without it, body lines
  that repeat verbatim in the band were taken as headers.
- Repeats are found among the pages read: `pages` with one page finds none.
- `HEADER_BAND` 0.13, `HEADER_GAP_LINES` 1.0 and `HEADER_MIN_SHARE` 1/3 (not "half") from the WP0a
  corpus at `5e600e8d`: running headers sit at 4-13 % of the height, footers
  at 89-97 %; papers alternate their headers (each variant on just under half
  the pages), a Redbook's chapter footers are on a third.
- Bold is `<b>` or a bold face in the font's family name (`…Bold`, `…TB`,
  `-Heavy`…): poppler writes `<b>` only from font flags, which many embedded
  fonts lack (DocLayNet paper).
- `pdftohtml` missing or failing is not an error: sizes come from the line
  boxes, no bold, with a `tool_missing`/`part_failed` warning.
- A first page past the last: the page count is read from poppler's refusal
  ("the last page (N)") to name it in `pages_out_of_range`.
- Office/text json is read from the readers' markdown (`from_md`), so an
  office part's own heading ("Slide 3") is not a block: slides and sheets are
  parts with `number` (and a sheet's `name`). Their markdown and text output
  stay the readers' own; json `stats` count the structure.
- `tables: false` in json makes tables paragraphs of tab-separated lines;
  `compact_tables` and `page_break` with json are `option_not_applicable`.
- Chat attachments and knowledge bases keep `pdftotext -layout`
  (`extract::pdf::text`) until WP7 moves them to `resolve`; their tests are
  unchanged.
- PDF `output: text` is rendered from the blocks, no longer poppler's
  `-layout` text.
- An XML error in poppler's layout output fails the file as `unreadable`
  (naming the byte); control characters pass through (quick-xml does not
  reject them; the corpus has them).
- `FONT_RUNS_PER_LINE` (64): a line is matched against at most 64 nearby font
  runs and rows; it bounds stacked hostile text, only font facts are sampled.

**Review round (2026-11-18).** Decisions of this round:
- Tables are found before headers and footers, and a table's line is never
  a header: a long table's header row repeated on each page stays (it was
  removed from every page, its first included).
- Repeats match on the exact text, or on the text with its first or last
  run of up to `PAGE_NUMBER_MAX_DIGITS` (5) digits as `#` when that number
  minus the page index is the same on each page. Anything else that changes
  keeps lines apart (it was "every digit run as `#`", which removed carried
  amounts and numbered slide titles unseen). The warning names every text
  removed, with the picked pages it left ("its page number changing with the
  page" for numbered ones); lines removed only from pages not picked are not
  named.
- Narrow tables (2-3 columns). Before this round the first build's note
  "2-3 column tables are not found" understated it: such tables and
  label/value pages came out column by column (all items, then all
  quantities). Now: at least `TABLE_MIN_ROWS` rows whose cells sit
  on one baseline (`TABLE_BASELINE_SHARE` 0.25 of the line height), each in a
  column of its own, no column of prose (half its cells `TABLE_PROSE_WORDS` 5
  words or more). A one-cell row under it in a column past the first is that
  cell wrapping. The first row is the header, also for label/value lists.
  Every table must fill `TABLE_MIN_FILL` (1/4) of rows × columns.
- Reading order: only narrow blocks that have another narrow block beside
  them (an `n log n` sweep, `BESIDE_SLACK_PT` 2) make the column bands; a
  narrow block with nothing beside it in no band separates sections. Where a
  band too narrow to be a column joined one beside it, vertically
  overlapping blocks are read row by row (one paragraph, a line a row, the
  row's lines joined by a space) when at least `ROWWISE_MIN_PAIRED` (2) and
  half of their visual rows pair up.
- A table's grid is never built: renderers write rows from the sparse cells,
  and the grid's cost (3 bytes a slot plus the text, as the office reader
  charges) is taken from the request's output budget before a row is
  written. The budget counter saturates instead of wrapping.
- A PDF takes one pool slot for both poppler runs (now one after the other)
  and the layout; `PER_RUN_BYTES` is 6 GiB (poppler's 4 GiB plus the two
  outputs of up to `MAX_UNCOMPRESSED_BYTES` each held in the gateway), so the
  derived pool is smaller on the same machine. A layout output over
  `MAX_UNCOMPRESSED_BYTES` is `output_too_large` naming the layout output and
  `pages`.
- `pdftohtml`'s overlapping `<b>`/`<i>` are read (end names unchecked); output
  that stops being XML keeps the runs before it. Each gap (failed run,
  unreadable output, pages without runs, no runs at all) is a `part_failed`
  note naming the pages.
- json of a text file: markdown (by its name: `.md`, `.markdown`, `.mdown`,
  `.mkd`, `.mkdn`) is read into blocks; any other text file is one
  `paragraph` block, verbatim. `from_md` opens list items only on `-`/`*`/`+`
  and keeps paragraph lines untrimmed.
- Markdown escaping at a line start also covers code fences, `<`, `>`, list
  markers, ordinals (`1999\. The`) and thematic-break/setext lines; list
  items are escaped too.
- Office json `stats.headings` count each part's own title as a `##`
  heading, as the markdown has it.
- Right-to-left text: `-bbox-layout` writes Arabic/Hebrew words with their
  characters in visual order (corpus `right_to_left_0*.pdf`), so WP3's first
  build spelled every such word backwards (3-gram Jaccard 0.66/0.56/0.47 with
  `-layout` before WP3, 0.07/0.12/0.11 after, 0.88/0.73/0.74 with the fix). `pdf_layout/bidi.rs` puts a
  line's words back in reading order (a simplified bidi: RTL words reversed,
  marks kept with their letter, runs read from the right in a line with more
  RTL than LTR words). Found by the measurement below; not a review item.
- The two poppler runs now go one after the other (one slot): the 826-page
  LibreOffice book takes 8.4 s markdown / 8.7 s json (6.4 / 7.4 s side by
  side), `pages=400-402` 0.17 s. Running them side by side in one slot would
  need `PER_RUN_BYTES` of two children (10 GiB).

**Measured (2026-11-18, dev instance, debug build, poppler 26.08.0, a 16-core test machine):**
the WP0a corpus' 28 PDFs (115 pages, 18 with a reference) against docling's
ground truth, lmgw side only:

| | conv. | s/file med. | s/page | 3-gram J | line ratio | headings | tables | cells |
|---|---|---|---|---|---|---|---|---|
| before WP3 (`-layout`, §14 WP0a) | 28/28 | 0.039 | 0.018 | 0.807 | 0.091 | 2/159 | 0/31 | 0/1322 |
| WP3 after its review round | 28/28 | 0.052 | 0.025 | 0.857 | 0.270 | 167/159 | 39/31 | 1980/1322 |

```sh
LMGW_CONTAINER_PREFIX=lmgw-wp3 scripts/dev-instance.sh 127.0.0.1:8893
LMGW_KEY=<owner token from its log> python3 -I scripts/convert-measure.py --corpus "$CORPUS" \
  --lmgw http://127.0.0.1:8893 --formats pdf --per-file --out measure-pdf.json
python3 -I scripts/convert-measure.py --corpus "$CORPUS" --formats pdf --per-file \
  --cmd "layout=pdftotext -layout -enc UTF-8 {in} -" --out measure-layout.json
```

The `layout` run reproduces the before-WP3 row exactly (0.8071, 0.0906), so
per-file numbers compare: the papers gain +0.01 to +0.03, `normal_4pages`
0.83 → 0.90, the RTL files 0.66/0.56/0.47 → 0.88/0.73/0.74 (with `bidi.rs`);
five files lose 0.001 to 0.016 (`acroform_sample` 0.851 → 0.835, the most;
not traced one by one). The time per page is worse: two poppler runs (each about the `-layout` run's
time; `2206.01062`: 81 ms `-layout`, 86 ms `-bbox-layout`, 90 ms
`pdftohtml`) one after the other plus the layout pass in a debug build. Tables
over the reference (39/31, cells 1980/1322) are mostly figure text taken as
tables and the "table beside a text column" case (not yet): on `2203.01017v2`
page 1 a figure's cell numbers with the abstract's lines beside them form an
8-column table, which also splits that page's columns into two sections.
Papers otherwise read column by column with title, authors and "Abstract"
in place; `header_removed` names the running heads, the page numbers as
numbered, and `table_misidentified_as_form.pdf` (fonts but no `<text>` from
`pdftohtml`) now says so (`part_failed`).


**Not yet:** a cell of a 4+ column table wrapping onto a second line
(becomes its own row); a table beside a text column (the column joins as an
extra column); 2-3 column tables whose cells are sentences (read row by row
where a column is too narrow to be one, otherwise column by column like a
paper); numbered list items (stay paragraphs); nested list levels; headings
inside blocks read row by row; captions, code and formulas in PDFs;
right-to-left reading order; the `table-like page` warning of §4.2, which
comes with tier 1 (WP6).

**Measured live (dev instance, debug build, 2026-11-18):** a LibreOffice
826-page text PDF (5.3 MiB) in 6.4 s markdown / 7.4 s json, of which
poppler's two runs side by side take about 2.8 s (`pdftotext -bbox-layout`
2.1 s, `pdftohtml -xml` 2.8 s, `pdftotext -layout` alone 1.4 s); `pages=400-402`
of it 0.14 s.

**Unverified:** the band and threshold against the WP0b numbers, and the
review round's (`TABLE_PROSE_WORDS`, `TABLE_BASELINE_SHARE`,
`TABLE_MIN_FILL`, `ROWWISE_MIN_PAIRED`, `PAGE_NUMBER_MAX_DIGITS`); heading
quality on the corpus beyond the files checked live; `bidi.rs` beyond the
corpus' three RTL files (no RTL paper with columns there); Windows (no
poppler there).

**WP3 on WP5 (2026-11-18, the rebase's integration).**
- An async native job with `output: json` stores the entry as the sync route answers it (`structure`,
  `content: null`); `tests/it/documents_structure_jobs.rs` compares the job's result with the sync
  answer for the same files.
- The docling adapter writes `text_content` from the same conversion as `md_content`: when both
  `to_formats` are asked, the plan's `also_text` makes the conversion write the block model's text
  form beside the markdown (`ConvertedDocument.text`, set only by the adapter, left out of the
  answer otherwise). Before, it stripped the markdown, which differs from `output: text` for
  escaped markdown and for text files (a text file's text form is itself). `to_formats: json` stays
  `not_yet`: `structure` is not a DoclingDocument.
- A PDF holds one slot for both poppler runs and the layout, so the per-file clock (it starts at the
  file's first slot) and the job's admission of as many files as there are slots hold; both are
  tested with a PDF. `PER_RUN_BYTES` is 6 GiB for the pool size and the page.
- `header_removed` quotes removed header text: it stays in the native answer's file warnings; the
  adapter's body names only lost text, its headers only option notes and counts, and the usage rows
  and job rows hold codes and counts (tested).

### WP5

**Built (2026-11-18).**
- Jobs: `extract/convert/jobs.rs` (+ `answer`, `results`, `sweep`), `store/convert_jobs.rs`,
  migration 0114 (`convert_jobs`, `convert_job_files`, `convert_calls`). `async: true` on the
  native route answers `202 {job}` with a `Location`; `GET/DELETE /v1/documents/jobs/{id}`,
  `GET …/result[?index=n]`. One job per request runs `batch::convert_each` (the sync path with
  a per-file report and deadline hook) on its own task as its caller's pool steps; each file's
  entry is written as it finishes. Stored results: `<data dir>/convert/<job>/<index>/entry.json`
  (the entry's JSON, content included). Deleted: on cancel at once; for an ended job after
  `documents.job_retention_days` (Settings › Retention › Conversion jobs, default 1, 0 keeps);
  for a docling task `RESULT_REMOVAL_DELAY` (300 s, docling's default) after its first
  `/v1/result` fetch; a directory without a row at the hourly upkeep. A job running at a stop is
  `interrupted` at the next start (`sweep::boot`), finished files keep their entries, the others
  answer `interrupted`.
- Adapter: `extract/convert/docling_serve.rs` (+ `options`, `lang`, `sources`, `formats`,
  `answer`, `zip`), HTTP half `server/docling.rs` (+ `form`, `tasks`). Every route of §7.4.1;
  `/health`, `/ready`, `/version` at the root (`Public`); the WebSocket takes docling's
  `?api_key=` (read in `principal_mw` for that path only, like realtime's subprotocol key).
- §7.8: `extract/convert/calls.rs`, `store/convert_calls.rs`; `doc_adapter_usage` op (+ OpDoc,
  `lmgw__doc_adapter_usage`); Settings › Documents "Who calls the docling route"
  (`documents/callers.rs`). Native requests record too (route `/v1/documents/convert`).
- OpenAPI: tag "docling-serve compatible" with the option table, dialect `docling-serve`
  (`{detail}` errors), `x-lmgw-option-notes`, `x-lmgw-documents` in `LMGW_HEADERS`.

**Differs from §13 (sources read 2026-11-18, `main` of docling, docling-serve, docling-jobkit).**
- The zip is **flat** on `main`: jobkit's `_export_documents_as_files` writes `<stem>.md` etc.
  at the archive root (with `artifacts/`); `<index:06d>/` is its export to remote storage
  targets. D26 says "follow main", so `zip::LAYOUT` is flat (WP0a measured the 1.38.2 release
  flat too); the numbered form is kept beside it for WP0b's switch. A repeated stem (docling
  overwrites it) gets `_<index>`, then `_<index>_<n>` until the name is free.
- `ConvertSourcesRequest.target` defaults to `presigned_url` in docling's model; docling-serve
  replaces it with the policy's default target at start-up. The adapter defaults to `inbody`
  (§7.4.1).
- Sources and targets also have `azure_blob`, `google_cloud_storage`, `google_drive`: refused
  like `s3`. `target_type` on the file routes includes `presigned_url`: refused.
- Single-use results: a fetched result is deleted `result_removal_delay` (300 s) later, not at
  once; lmgw does the same.
- The status WebSocket authenticates with `?api_key=` (a header is not possible from a browser).
- `from_formats` has grown (email, epub, boxnote, iwork_*, ebcdic, afp, …): values are not
  checked against a list.

**Decisions.**
- The async request's usage row is written at acceptance (202, the time to accept); the
  conversion's outcome is the job's state, not a second row.
- Jobs that end with no converted file are `failed`; docling's poll then says `failure`, and
  `/v1/result` answers `TaskFailureResult`, except one file with the `inbody` target, which
  answers `ConvertDocumentResponse` with `status: failure`.
- Sync zip with nothing exported: 422 with `TaskFailureResult` (docling raises there).
- A one-file sync failure is HTTP 200 with `status: failure`, as docling, not D18's 415/422.
- Enum values are read leniently (an unknown `image_export_mode` maps to placeholder, an unknown
  `pipeline` to standard, each with a note); only wrong types are 422.
- `to_formats` md and text together: one conversion to markdown, `text_content` derived from it
  (text alone uses the native text form, which for a PDF is poppler's layout text).
- `page_range` `[a, b]` → `pages "a-b"`; `[1, sys.maxsize]` is every page.
- `abort_on_error`: a `partial` file becomes `failed` (`unreadable`, the warning codes named);
  the conversion itself is not cut short.
- `document_timeout` → a per-file deadline in `batch::convert_each`, new `ErrorCode::
  DeadlineExceeded` (504). `ErrorCode::Interrupted` (503) for a job's unfinished file.
- Options with no native option are `not_yet` notes (list under "Not yet"); the others take the
  native route's own answer (review round, below).
- Header details use `->`, not `→`, as header values are ASCII.
- A docling option takes the native route's note only when it changed the native option
  (`do_ocr: true`, the default, stays `honoured` while `force_ocr: true` is `not_yet`).
- The record keeps each outcome's detail (`model_not_chosen`, `easyocr->lmgw-ocr`), so the
  page can say "picture descriptions (no model chosen)".
- DELETE on an ended native job deletes it with its results (privacy); on a running one it
  cancels.
- `ocr_lang`: a fixed BCP-47 → tesseract table (`docling_serve/lang.rs`); `mul` → the native
  default (installed packs); an unknown tag is passed through unless it is not 3 letters, then
  named in the note.
- `/v1/capabilities` is `Inference` like the other 501 routes (docling serves it without a key).

**Review round (2026-11-18).** Ten should-fix items and four nits from the review, each with a
test that fails on the old code (`tests/it/documents_review.rs` and unit tests beside the code).
- Uploads: a job's files go to `<data dir>/convert/<job>/input/<index>` at accept time and are read
  when the file is admitted; a file's upload is deleted when the file is done, the whole directory
  when the job is cancelled or deleted, the inputs of a job left running at a stop at the next start.
  At most as many files of a job are admitted at once as the tool pool has slots (the real value:
  a file waiting for a slot holds no bytes). No admission bound was added; the queue and the disk
  use show under Settings › Retention › "Stored conversions" (`doc_jobs_stored`,
  `lmgw__doc_jobs_stored`): jobs with files on disk, their bytes, jobs running, files waiting. A
  full disk logs a warning at the write that failed.
- Files are private (0700 directories, 0600 files on Unix): the entries and the uploads.
- The stray-directory sweep lists directories before it reads the rows and skips live jobs;
  `after_fetch` runs only when `mark_fetched` set the time; a retention past what a date holds keeps
  the jobs (no setter bound).
- `wait` and `document_timeout` use `Duration::try_from_secs_f64`: a `wait` that no duration holds
  is the longest wait the route allows (until the task ends); a `document_timeout` that does is
  docling's 422 with its `loc`.
- `/v1/result` answers a failed task's `TaskFailureResult` with 200, as docling; the sync routes
  keep 422.
- Concurrency (the review pointed at `policy.take_slot`, as Chat turns use): the request's own
  gate slot is handed to the job (`SlotHandover`, as a realtime session takes it) and held until
  the job ends, because `take_slot` while the request still holds its slot would refuse a key with
  `concurrency_limit` 1 every time. A key with a concurrency limit therefore runs that many jobs
  at once, and a further async request is a 429 while they run. Looking at a job (native GET/DELETE
  and result, docling poll, WebSocket, result) takes no slot and counts no request; it only checks
  the key's expiry, as a call's hangup does.
- A file's `document_timeout` clock starts when it gets its first pool slot (`pool::on_first_slot`);
  waiting for one is not late. Later steps' waits count.
- Zip names: unique by loop, compared case-insensitively; a stem past `MAX_STEM_BYTES` (200) is
  cut; `x-lmgw-documents` ends with `entry_names_renamed=N` and `entry_names_shortened=N (stems cut
  to 200 bytes)` when that happened.
- Option map (§7.4.2): `include_images` and `image_export_mode` set the native `pictures`
  (`include_images: false` keeps placeholders; true without a mode is docling's default, embedded);
  `table_mode: accurate` sets the native `model_help` to `missing_or_tables` (never lowering
  `pipeline: vlm`'s `every_page`); they take the native route's note, so WP6/WP10 change nothing
  here. The module doc in `docling_serve/options.rs` names which options are not like that.
- The option header collapses past `NOTES_HEADER_MAX_BYTES` (4096) to
  `unknown_options=not_yet(N names; see the request record)` and, if still past it, to
  `option_notes=not_yet(N notes; …)`. Unknown option names are kept cut to 64 bytes (with their
  length) and only the first 32 are noted, the rest as a count.
- A result that cannot be written is that file's own failure (`result_not_stored: <io kind>`), not
  a file the gateway "stopped" on; a job whose start fails (spool or result write) is deleted, not
  left `running`. `abort_on_error` fails a partial file as it is recorded, so the poll's counts,
  the job's end state and the result agree.

**Not yet.** Options with no native option, each answered `not_yet` by name until the work package
that adds the native option also changes `docling_serve/options.rs`: `images_scale` (WP10: a
native picture scale, §5.3), `do_formula_enrichment` and `do_code_enrichment` (WP6: the tier 1
prompt asks for formulas and code on pages it reads, no option picks them), `pipeline: asr` (WP7:
the speech-to-text model), `include_page_images`, picture classification, chart extraction and
chunking (§12). Progress inside one file (parts grow per finished file); a job queue position
(`task_position` is `null`); the per-caller "runs ahead of you" count (the job status shows the
pool's running/waiting); `http` source fetching (WP5b plugs into `sources::fetch_http`);
DoclingDocument JSON and the other outputs; chunking, capabilities, clear routes; a Usage-page
filter for the docling aliases (the rows carry `docling/<route>` as alias).

**Live check (2026-11-18, dev instance).** curl: one PDF sync → 200 `ConvertDocumentResponse`;
four files → `converted_docs.zip` (`example-notes.md/.txt`, `example-notes_1.md/.txt`,
`example-report.*`, `example-slides.*`); an async file task polled with `wait` → `success`,
result zip; an `http` source → 400 `http_source_refused`; `ocr_engine=example-engine` →
`x-lmgw-option-notes: ocr_engine=mapped(example-engine->lmgw-ocr)`. docling's own models
(`docling-slim` 2.150.1 from PyPI, `python:3.12-slim` in Podman) parsed the sync answer, the
async `TaskStatusResponse`s and the result. The WebSocket is covered by `tests/it/
docling_tasks.rs` (websocat is not installed here).

**Unverified.** The 1.38.2 release's default target (WP0a saw `inbody`; WP0b records the
deployment's); a full-disk write (the warning is in the code path, not exercised); a real docling
client library driving the routes end to end (the check above used docling's models on
answers fetched with urllib).

### WP0b

**Built (2026-11-18).** `scripts/convert-compare.py` (one file, standard library only, plain
ASCII, Python 3.6 or newer, run with `python3 -I`), `scripts/convert_compare_test.py` (59 tests
against two mock servers on loopback; `ci/check.sh` runs it, `RUN_CONVERT_COMPARE` in
`ci/changed.py`), and the steps in `docs/documents.md` ("Comparing lmgw with docling-serve
on your own documents"). The run at a deployment has not happened yet.
- Per file, as §10.1: docling-serve's `/v1/convert/file` (`to_formats=md`), then lmgw's native
  route (markdown), then with `--also-adapter` lmgw's `/v1/convert/file`, of which only the shape
  is kept. One file at a time; both outputs in memory, compared, dropped.
- Measures: the similarity and counting code of `convert-measure.py` (3-gram Jaccard, line ratio,
  headings per level, tables, cells) plus pictures; lmgw's `parts`, `stats` and
  `parts_by_source`; wall-clock and server seconds per side; codes.
- The report: `run`, `environment`, `defaults` (D26: what docling-serve answers with no option
  for one file and for two: JSON or zip, the zip's layout as `flat`, `numbered_directories` or
  `other`), `guard`, `totals`, `by_extension` (totals and medians), `files` (index, 12-digit
  HMAC-SHA-256, extension word, size bucket, both sides, similarity). `--show` prints it in full.
- The guard (D25), five layers:
  1. every value is computed by the script or passed through a mapper (`enum`, `count`, `num`,
     `flag`, `http_code`, `FreeStrings.version`/`.alias`); a code not in the script's lists is
     `other`, never the server's string; an extension is one of a fixed list or `other`;
  2. the whitelist (`validate`): every key known, every value a number, a boolean, a fixed word,
     a 12-digit hash, or one of the two kinds of server text (a version string by the §10.1
     pattern, an alias name by `[0-9A-Za-z._:/+@-]{1,80}`); run on every file's entry, on every
     line of the state file, on the report, and on the report's written text parsed back;
  3. the scan of the written text: ASCII only; nothing but JSON punctuation and numbers outside
     its strings; every string is fixed vocabulary, a hash, or a string a mapper let through
     before the first file; those strings hold no file or folder name under `--dir` and no word
     of one, and the version strings no word of four letters or more of the converted texts
     (also checked after every file, so a clash stops the run at that file);
  4. one print function (`say`) that prints numbers, the fixed vocabulary, hashes and the
     owner's own arguments, and `?` for anything else; progress lines show the file's number,
     extension word, codes and times;
  5. an unexpected error prints its type and the script's line numbers; warnings and
     unraisable-error output are off.
  A guard that fires names its rule and writes no report.
- Robustness at a deployment: `--ca-file`, `--insecure` (recorded), proxy variables ignored unless
  `--use-env-proxy` and never used for loopback, redirects not followed, keys from `DOCLING_KEY`
  and `LMGW_KEY` only, `--timeout` 1800 s and `--quick-timeout` 30 s, the 504 of docling's sync
  wait limit answered by its async route for that file (`route: async_fallback`), `--limit`,
  `--resume` (state file), `--retry-failed`, `--dry-run`, `--state-dir`, an address with or
  without `/v1`, output that survives an ASCII terminal and a closed pipe.

**Dry run (2026-11-18).** lmgw `main` at `5b09eda9` (0.7.0, the headless dev build, poppler
26.08.0; WP3 and WP4 are not on it, so scans answer `no_reader`), docling-serve image
`quay.io/docling-project/docling-serve-cpu@sha256:7a480a1604fa3b42efeddc7db9527690d9f9bc903226136013e2bfe6b1a4dfb0`
(1.38.2, docling 2.142.1), Python 3.15.2 on the host and 3.6.15 (`python:3.6-slim`). Folder: 41
files copied from the WP0a corpus (13 pdf, 8 docx, 4 pptx, 3 xlsx, 1 xlsm, 6 html, 2 md, 1 csv,
2 png, 1 jpg; five of them scans or pictures, one password PDF).
- The documented sequence (`--dry-run`, `--limit 3`, `--show`, `--resume --overwrite`, with
  `--also-adapter`) ran over all 41 files: docling-serve converted 40, lmgw 35 (5 `no_reader`,
  the password PDF `unreadable`, which docling-serve fails too); the adapter answered docling's
  shape for all 41. The measurements themselves are WP0a's; nothing new is recorded here. That
  run had docling-serve's sync wait limit at 3 s (below), so 6 files took the async route. It
  ran one revision before the last: the D26 question's async route, the refusal of a changed
  state file and two messages came after it and were run with `--limit 3` and by the tests.
- D26 on this image: one file answers JSON, two answer the zip, layout `flat`; its
  `/v1/capabilities` says `targets.default: inbody`.
- A run interrupted with SIGINT after 16 files and resumed wrote one whole report (the image's
  defaults, every file on the sync route).
- With `DOCLING_SERVE_API_KEY` set: no key and a wrong key stop at the test page ("refused the
  key (http 401)"), before a salt or state file exists; the right key runs.
- With `DOCLING_SERVE_MAX_SYNC_WAIT=3`: the files past it got the 504 and went through the async
  route (`async_fallback`), each converted; the D26 question went the same way and got the same
  answers.
- Without `LMGW_KEY` (the dev instance has auth off, its admin routes still want the bearer):
  the settings and tool reads answer 401, the report says `unavailable`, and the tools are the
  test page's.
- docling-serve logs every upload's name; its log held `doc000012.pdf`-style names and
  `probe.pdf` only.
- 2000 made-up files against the tests' mock servers: about 20 ms of script time a file, 70 MiB
  peak, 9760 names and 167 330 words checked, a 1.6 MiB report; resuming the finished run took
  under a second.
- The same script on Python 3.6.15 under `LC_ALL=C` against both systems; the tests on 3.6,
  3.11, 3.12, 3.13 and 3.15 under `C` and `C.UTF-8`.
- Against the guard: one copy renamed to a made-up name, and a relay in front of lmgw that
  planted (1) its stem in upper case as lmgw's version, (2) its words with another separator,
  (3) one word of it inside a model alias, (4) a word of the first PDF's text inside the
  version. 1–3 stopped before any document was sent (`scan_file_name`), 4 after the first file
  (`scan_document_word`); none wrote a report or printed the text; `--resume
  --withhold-strings` then wrote it with `withheld`. A separate search of the report for every
  corpus name and every word of five letters or more found only the script's own vocabulary.

**Review round (2026-11-19).** An adversarial review ran the script against hostile mock
servers: no path put document text, a name or a server message into the report, the state or the
console; one blocker and seven should-fix items on the "works first time" side, plus nits. Fixed,
each with a test that fails on the previous revision (`ReviewRound` in the test file; the four
guard mutations it found surviving now fail a test):
1. **Large files.** Uploads stream: the file is hashed in pieces of `CHUNK` (1 MiB) and sent as
   a multipart body that is iterated piece by piece with an explicit Content-Length (re-read from
   disk for every send: docling, the 504's async fallback, lmgw, the adapter, D26). The file is
   never held whole. Measured with a 1 GiB file against two mock servers that answer a few bytes:
   35 MiB peak RSS, against 2.0 GiB for the previous revision (`/usr/bin/time -f %M`, Python
   3.15.2). What memory follows now is the larger of the two answers: an answer is read whole, and
   its text is held in about two more copies while it is measured. Every file's measurement runs
   inside a catch-all: any exception (MemoryError, zlib.error from a broken zip, a bug) records the
   entry with `failed` (`memory`, `guard`, `file_changed`, `error`), `docling`/`lmgw`/`similarity`
   null, prints the exception's type and the script's line numbers only, and the run goes on; such
   a row is measured again by `--retry-failed`. The zip readers catch any exception too.
   `--dry-run` prints the largest file's size bucket and the machine's memory.
2. **Per-file fingerprint.** A file's `chars` and `lines` (both sides) are rounded to two
   significant digits in its report entry; the state keeps them exact, so `totals` and
   `by_extension` have exact sums, except an aggregate over a single file, which is rounded like
   the file. The other per-file counts stay exact, as §10.1 lists them: headings per level,
   tables, cells, pictures, lmgw's parts and parts by source, seconds, the similarity values. So a
   report entry is **not** a guarantee against membership testing: someone who holds a candidate
   document and the same lmgw (and docling-serve) version can convert it and compare those counts.
   The closing summary says so in one sentence.
3. **Retry rows.** A row written by `--retry-failed` carries `replaces: true`; on load it takes the
   place of the earliest row of that hash a retry would measure (a folder may hold the same file
   twice, so rows of one hash stay a list). Tested: three files unanswered, retry, plain resume.
4. **D26 half not asked.** Each half of the default-answer question is asked whenever its stored
   half says `run: false` and enough converted files exist now.
5. **Slow files stop the run.** After three files in a row without an answer the test page is
   sent again; if each system converts it (or answers it with the same code as before the first
   file, for a system that never converted it), the streak resets and the run goes on
   (`run.test_page_resent` counts it); only a test page without an answer stops the run. The
   message, the docstring and the user docs say that plain `--resume` keeps those files' results.
6. **Line ratio cost.** The bound is difflib's own work: the sum over distinct lines of
   count in a × count in b, times the shorter side's line count (`LINE_RATIO_MAX_WORK`,
   100,000,000, about 2 s on a test machine). Measured through `similarity()`: N identical lines
   against N lines alternating with them take 0.17 s at N = 250 (7.8 M), 1.3 s at 500 (62.5 M),
   10 s at 1000 (500 M); identical table rows against every-second-row-different the same; 5000
   distinct lines against them shuffled, reversed or equal 0.01 s at 25 M (computed). The
   reviewer's 2000-line pair is skipped in under 0.1 s. A skip is `line_ratio_skipped:
   work_bound` (and `line_ratio_skipped` in the aggregates).
7. **Embedded pictures.** `image_export_mode` is not sent (a deployment's defaults stand);
   `data:<type>/<subtype>[;,]…` URIs are cut to `data:` in both markdowns before anything is
   counted, compared or collected as words, and an embedded picture (`![..](data:…)` or `<img
   src="data:…">`) counts as a picture. A version string that holds a word of a file's text is
   withheld from then on (`withheld`, its salted hash in a `withheld` state row, so resumed runs
   keep it withheld; `guard.strings_withheld_for_words`), and the run goes on. A file or folder
   name in a version string or alias name still stops the run before any document is sent, and
   the final scan still refuses a report whose strings hold a word.
8. **`--show`** refuses a file whose bytes differ from `serialise()` of what they parse to
   (a key read twice, extra float digits, anything added).
9. **Nits.** A test page answered with http 200 but not converted prints a warning and stops
   before any document unless `--accept-test-page` (recorded as `run.test_page_accepted`); any
   other test page that does not convert prints a warning and the run goes on. File-name checks
   look the pieces of each (short) version or alias string up among the names instead of one
   regex per name: 100,000 numbered names take milliseconds. `--fresh` moves the earlier salt
   and state to `.old` (`.old.2`, … when taken), never deleting one. A state line that does not
   read or pass the whitelist past the header is dropped with every line after it (they are cut
   off the file once the header is known to be this run's), the rows before it are kept, and the
   count is printed. `sys.unraisablehook` is set only where it exists (3.8+); before that, stderr
   is wrapped so only `say`'s own lines get through. Schema and script revision are 2: a state of
   revision 1 is refused.

End to end after the round (2026-11-19): `main` at `9728c95f` with WP3 (headless dev build,
poppler 26.08.0) and the same docling-serve image, the 41 corpus copies, the documented sequence
(`--dry-run`, `--limit 3`, `--show`, `--resume --overwrite`, all with `--also-adapter`) on Python
3.15.2: streamed uploads were read by both servers and the adapter; docling-serve converted 40,
lmgw 35 (5 `no_reader`, the password PDF `unreadable`), adapter shape `docling_document` for 41,
no `failed` rows, no line ratio skipped, D26 JSON for one file and a flat zip for two, `--show`
accepted the written file. `--limit 4` with `--also-adapter` on Python 3.6.15 (`python:3.6-slim`,
`LC_ALL=C`) against the same two systems ran too.

**Decisions.**
- Files are uploaded under neutral names (`doc000012.pdf`, the real extension kept when it is
  1–8 letters or digits): the servers and their logs never see a name, and no name can come back.
- The state file holds each finished file's report entry and the first reading of the two
  systems with every version string and alias name replaced by a salted hash, not indexes and
  hashes alone: a resumed run must write one whole report. It holds no server text. On resume a
  string is put back only where the server still reports the same one; otherwise the field says
  `changed`. Version strings are read before the first file and never after.
- Alias names are a sixth value kind next to §10.1's five, and `null` a seventh. Alias names are
  searched for file-name words but not for document words (an alias is made of ordinary words);
  the closing summary lists them, and `--withhold-aliases` leaves them out.
- A version string of twelve hex digits is written `unparseable` (it would look like a hash).
- The report is not "a few KiB": about 0.8 to 1.3 KiB per file plus about 1.5 KiB per extension
  (58 KiB for the 41 files). `--show` is the readable form.
- "Whether a GPU was used" cannot be read from either server: `--docling-gpu` and `--lmgw-gpu`
  (yes/no/unknown, the owner's word), lmgw's `hold_active`, and the model calls lmgw counted.
- lmgw's tools with versions, pool and chosen models come from `/api/op/doc_tools` and
  `/api/settings-full`, which need the owner's bearer; with a plain key they are `unavailable`,
  and the tools are those the test page's answer names.
- A built-in one-page PDF goes to both systems before any document: an unreachable server, a
  refused key, a redirect, a missing route or a 502/503 stops the run before anything is written.
- Three files in a row without an answer (no connection, a timeout, 502/503) send the test page
  again; only when it gets no answer does the run stop (review round, finding 5).
- A file whose entry the whitelist refuses is recorded as `failed: guard` with nothing measured,
  and the run goes on; a refused state line or report still stops it.
- An aggregate over one file has its `chars` and `lines` rounded like that file's entry.
- D26 is asked at the end, with the two smallest files docling-serve converted, sent with no
  option at all (through the async route when the sync wait limit answers 504; the report says
  which). If a deployment's default target keeps the result out of the body, the per-file
  requests ask for `target_type=inbody` from then on, and the report says so.
- Hidden files and `~$` lock files are skipped and counted, as are folders the user may not
  read and entries that are neither file nor folder; symlinks too unless
  `--follow-symlinks`.
- `--identify HASH` (needs the salt) is the one mode that prints paths.
- Python 3.6 is the floor (the `python3` of older enterprise distributions), not 3.9.
- A work package that adds a warning code, an error code, a sniffed format or a tool adds it
  to the script's lists (`LMGW_WARNINGS`, `LMGW_ERRORS`, `LMGW_FORMATS`, `TOOL_IDS`), or the
  report says `other` for it.

**Not yet.**
- lmgw reports no chosen document models yet (WP6, WP11): the script looks for `vision_alias`,
  `picture_alias` and `asr_alias` under `documents` in `/api/settings-full` (or as
  `documents_<name>`) and says `not_reported` until they are there; the work package that adds
  them checks the script reads them.
- docling-serve's page count (its markdown answer has none; pages are lmgw's `parts`).
- Digit runs of the documents (account or case numbers) are not searched for in version strings;
  numeric file names of six digits or more are.
- Answers are read whole: a server answer of several GiB (a huge document's markdown with
  embedded pictures, a zip) needs that memory; a MemoryError there is recorded as `failed:
  memory` for that file and the run goes on.
- The other outputs (`text`, lmgw's `json`), per-page times, more than one file at a time.

**Unverified.**
- TLS with a private CA and `--insecure`: against the tests' own TLS servers (an openssl-made
  certificate) on every Python above, not against a real reverse proxy.
- `--use-env-proxy` through a real proxy (tested: the variables are ignored by default, and
  loopback bypasses them).
- A docling-serve other than 1.38.2; a deployment whose default answer is the zip, numbered
  directories or a storage target (mock only).
- The alias fields against an lmgw that has models chosen, and every OCR, vision and model-call
  field with a value other than 0 (mock only: `main` reads no scans).
- lmgw with gateway auth on and a plain key (mock only; the dev instance has auth off, so only
  the admin routes' 401 was seen live).
- Large folders: not measured beyond 2000 made-up files. Large files: measured against mock
  servers (1 GiB, 35 MiB peak), not against real servers converting one.
- The script on Windows or macOS.

### WP5b

**Built (2026-11-18).**
- Fetcher: `extract/convert/source_fetch.rs` (+ `hosts`, `ranges`, `resolve`, `get`, `tests`). The URL
  is parsed once (`url` crate, the version reqwest locks) and only its parts are used. Per hop: the
  host and port against the list, one resolution through a `Resolve` trait (the system's
  `getaddrinfo` in the gateway, a fixed table in tests), every address classed, and a reqwest
  client built for that hop whose resolver is pinned to exactly the checked addresses
  (`resolve_to_addrs`, over a resolver that answers no other name), with `no_proxy`, no redirects,
  no decompression, the gateway's rustls with the platform verifier. Named bounds: `SOURCE_FETCH_TIMEOUT` (300 s, the whole fetch),
  `SOURCE_FETCH_CONNECT_TIMEOUT` (10 s), `SOURCE_FETCH_MAX_REDIRECTS` (10); the body streamed and
  cut at `max_body_mb`, `Content-Length` used only to refuse early.
- Adapter: `docling_serve/web.rs` (`fetch_all`), called from the HTTP half's body read; sources
  keep their request position as index; `Parsed.web` carries each host with its outcome
  (`refused`/`fetched`/`failed`) for `convert_calls`, `Parsed.decided` the failed sources as
  failed entries. `ErrorCode` gains `SourceRefused`, `SourceUnreachable`, `SourceTimeout`,
  `SourceTooLarge`, `SourceHttpStatus`; the message starts with the rule's name.
- Settings: `documents.fetch_sources` (off), `documents.source_hosts` (empty, stored normalised);
  `settings_set` fields `documents_fetch_sources`, `documents_source_hosts`; `doc_tools` answers
  them as `web`. Settings › Documents › "Web addresses" (`documents/web.rs`): the switch with what
  it allows, the list read-only with "edited in the expert view" (or "no site is allowed yet"), and
  in expert view the list with its rule and "Save sites". "Who calls the docling route" says per
  host downloaded, download failed or refused.

**The host list's matching rule.** An entry is a host with an optional `:port`, nothing else (no
scheme, path, userinfo, wildcard or `%`). Host read by `url::Host::parse`: lowercase, IDNA to
punycode, every IPv4 spelling to its address, trailing dot dropped; IPv4-mapped IPv6 entries
refused. A name matches the same name only (no subdomains); an address the same address of the same
family (`[::ffff:127.0.0.1]` never matches `127.0.0.1`). No port: the scheme's default port only
(80 for `http`, 443 for `https`; review round); a port: that port, either scheme. A URL with
userinfo is refused before matching.

**"Listed explicitly".** §7.4.1: refused ranges "unless the list names them explicitly", the check on
the resolved address. Built as: a hop into a range refused unless listed goes ahead only when the
list has an address entry for that very address and the hop's port (a mapped address counts as its
IPv4). So `intranet.example` on `172.16.0.5` needs `intranet.example` and `172.16.0.5`; a name entry
alone never opens a private address.

**Refused ranges as built** (`ranges.rs`). Unless listed: 10/8, 172.16/12, 192.168/16, 127/8,
169.254/16, 100.64/10, 192.0.0/24, 192.0.2/24, 198.51.100/24, 203.0.113/24, 198.18/15,
192.88.99/24; `::1`, fc00::/7, fe80::/10, fec0::/10, 2001:db8::/32, 3fff::/20, 2001::/23 (Teredo
and IETF), 2002::/16, 64:ff9b:1::/48, 100::/64, 5f00::/16, the rest of ::/8, anything outside
2000::/3. Never, even listed: 0.0.0.0/8, 224/4, 240/4, 255.255.255.255, `::`, ff00::/8.
IPv4-mapped and NAT64 (64:ff9b::/96) addresses are classed by their IPv4 address. Then this
machine's own (review round, `source_fetch/local.rs`), also unless listed: every interface address,
every address inside an interface's on-link prefix (an interface with prefix `/0` counts with its
address alone), and the addresses of the gateway's own listeners (`bind_addr`, `tls.listen`), read
with `if_addrs` 0.15 (already a dependency) once per request. Every resolved address must pass; one
refused address refuses the hop.

**Decisions.**
- An unlisted host, a scheme other than http/https, userinfo or an unreadable URL refuses the whole
  request (400, before anything is fetched), as §7.4.1 says for hosts; everything found while
  fetching fails that source alone.
- A request's sources together are bounded by `max_body_mb` too (`source_too_large` says "what is
  left"), so a few bytes of JSON cannot make lmgw hold more than an upload request could.
- Async requests fetch at acceptance: the job then holds no URL and no header (§7.4.1: headers are
  never stored) and its files are spooled like uploads; the cost is that the task answer waits for
  the downloads.
- Redirects: `https` → `http` refused, `http` → `https` followed; 301/302/303/307/308 only, always
  as GET; an empty or missing `Location` refused. The caller's `headers` go to the source's own
  origin only (§7.4.1): scheme, host and port, and once a hop has left it they are never sent again
  on that walk (review round). Framing and hop-by-hop headers (`host`, `content-length`,
  `transfer-encoding`, `connection`, `keep-alive`, `upgrade`, `te`, `trailer`, `expect`,
  `proxy-authorization`, `proxy-authenticate`, `proxy-connection`, `accept-encoding`) are dropped
  with `headers=mapped(not sent, lmgw sets them: …)`, never a 422; a header name or value HTTP does
  not allow, or more headers than an HTTP header map holds, is docling's 422 without the name.
- No user agent is sent (a site that needs one gets it from the caller's `headers`).
- Errors name the source's host and the rule, never a redirect's target (not its scheme either), a
  resolved address or anything the site answered (`source_http_status` has the status only); logs
  hold hosts only.
- The hop's client is pinned explicitly: `http1_only`, no gzip/brotli/zstd/deflate, whatever
  reqwest features the workspace enables elsewhere.
- The two settings are refused by `lmgw__settings_set` (`OUTBOUND_SETTINGS` beside the access
  settings): they decide where lmgw sends requests for its callers.
- They save at once (like "Parallel tool runs"), not through the page's draft.
- The options are judged before the fetch (review round): a request that answers 422 downloads
  nothing.
- **Web sources need a key** (security review round, finding 1). A request from the anonymous
  principal (no credential, legal on `/v1` while "Require gateway API keys on /v1" is off, the
  default) is refused for `http` sources with a 401 `http_source_needs_key`, whatever that setting
  says; everything else on the route keeps working keyless. Reason: `/v1` answers cross-origin
  (`CorsLayer::permissive`), so without this any web page open in a browser on the same machine
  could post a source naming a listed host and read its content through the gateway, which the
  browser's same-origin rule would otherwise prevent. The Settings row says so in plain words.
- **`max_body_mb = 0` refuses web sources** with a 400 `http_source_needs_limit` ("web sources need
  a set request size limit"), and the Settings row warns while it is 0: 0 means "no limit" for
  uploads, and a download needs a bound.
- **A gateway-wide fetch pool** (finding 3, `source_fetch/slots.rs`): the tool pool's type
  (`ToolPool`, waiting round robin per caller) and the tool pool's size in force (the D19 figure, or
  "Parallel tool runs" set by hand, which sets both), separate from the tool pool so slow downloads
  cannot starve conversions. Each source's fetch holds one slot for its walk. Worst-case fetched
  bytes in memory: size × `max_body_mb`, shown with the size and the running/waiting counts in
  `doc_tools`' `web.pool` and the expert view of "Web addresses". The wait for a slot ends with the
  request and does not count against `SOURCE_FETCH_TIMEOUT`, which starts with the slot; the caller
  sees its request open meanwhile (the async task answer waits too). The buffer is sized once from
  `Content-Length` (at most the bound) instead of doubling.
- A fetch stopped by a refused address or redirect (`source_address_refused`,
  `source_redirect_refused`) is recorded as `refused` in "Who calls the docling route", not as a
  failed download.

**Not yet.** Wildcard or range entries; docling's `Content-Disposition` file name (docling-core reads
it before the URL path; lmgw takes the path only); fetching several sources at once
(one after the other now); a per-host credential store (the caller's `headers` only).

**Live check (2026-11-18, dev instance, `python3 -I -m http.server` on 127.0.0.1:8897).** Off →
400 `http_source_refused` naming `documents.fetch_sources`; on with `docs.example` only → 400
`http_source_host_refused` (127.0.0.1, port 8897); `localhost:8897` listed → 200, `status: failure`,
`source_address_refused: localhost resolves to an address in loopback (::1)`; `127.0.0.1:8897`
listed → a Markdown file and a PDF converted (`success`), a missing file beside an upload → zip with
the upload, `x-lmgw-documents: … failed=1`; userinfo and `file://` → their named 400s; the log lines
carry hosts only, the usage summary lists each host with fetched/failed/refused; a settings entry
`https://docs.example/x` is refused by name.

**Security review round (2026-11-19).** One blocker, three should-fix, five nits, each fixed with a
test that fails without its protection (`source_fetch/tests/hardening.rs`,
`tests/it/docling_web/guards.rs`): keyless and cross-origin posts refused, a keyed one fetched;
headers on another port of the same host and on a return to the origin; a resolver that answers
differently on its second call (one lookup per hop, the connection to the first answer); the hop's
client resolving a name itself (fails without `NoLookup`); three fetches against a slow loopback
server through a pool of two never more than two at once, the third within its timeout only
because its wait is not counted; empty, missing, scheme-relative and userinfo `Location`;
`https` → `http` on the redirect rule; a neighbour on an interface's global IPv6 prefix refused;
`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` set (a default client reaches the proxy, the fetch does
not); a certificate from a test CA (`tests/it/main_tls`'s TLS listener) failing the source;
`max_body_mb = 0`; 30 000 header names → 422; options judged first; refused redirects recorded as
`refused`.

**Live check of the review round (2026-11-19, dev instance, two loopback servers: 127.0.0.1:8897
answering `/start` with a 302 to a second, ephemeral port, both listed).** Keyless with
`Origin: https://page.example` → 401 `http_source_needs_key`, nothing fetched; keyless upload →
200 `success`; with the owner key and `headers: {"Authorization": "Bearer example-secret"}` → 200
`success`, the first server saw `Authorization`, the second did not; `doc_tools` answered
`web.pool` `{slots: 31, running: 0, waiting: 0, per_fetch_bytes: 64 MiB}` beside the tool pool's
31; "Who calls the docling route" listed the keyless caller's host as refused.

**Unverified/limits.** An `https` → `http` redirect after a successful TLS handshake: the fetcher
trusts the platform verifier only, so no test server can complete a handshake; the rule is tested
on `get::next_url`, the one place the walk takes its next URL. When the interfaces cannot be read
(`if_addrs` fails), the log says so once and the fixed ranges and the listeners' addresses apply
alone: failing closed there would refuse every fetch on such a machine. Interfaces are read once
per request, so an address that appears during a request is classed from the next one. The
gateway's public address behind NAT or a port forward is not an interface address and stays
public (nothing on the machine names it).


### WP12

**Built (2026-11-19).** Labs › Documents (owner request 2026-11-18: "a new labs page for these
document conversions, so the API can be used and tested from the UI like the audio and image
APIs"), one more lab on the lab store (labs-store design §5), on the frame, history, presets and
export the other labs share.
- Store: `LabKind::Documents` (`documents`) and the preset kind `document_options`
  (`DocumentsLabSettings`: route and native options); migration 0115 widens the `lab` CHECK of
  `lab_groups` and `lab_presets` (0090's twelve-step rebuild, foreign keys off, every column and id
  copied, the AUTOINCREMENT marks carried); the CSV export's own table
  (`labs/export_csv_documents.rs`), the files export names a run's texts after its title.
- Server: `web/documents_lab.rs` (+ `groups`, `run`, `job`, `kept`, `texts`), `/documents-lab/api/
  groups` (multipart: `form` + `file` parts; a file with id 0 is the next upload, any other names a
  kept file, so a re-run uploads nothing) and `/groups/{id}/run`. The run builds the multipart a
  client sends (`documents_lab::native_fields`/`docling_fields` in `lmgw-api-types`, shared with
  the page's curl) and hands it to `server::documents::convert` or `server::docling::convert_file`
  in process, under the route's `max_body_mb` (its own 413 by `declared_too_large`). The answer is
  the run; each text (`content`, `structure`, `text`; docling's `md_content`, `text_content`, …) is
  an output artifact spliced in as `lab-artifact:<id>` through borrowed `RawValue`s (nothing
  re-serialized); docling's zip is kept whole and entry by entry. An `async` run answers at once
  with `job`; a task holds the cell's claim (the group is in flight) until the job's watch ends,
  then keeps `answer::full` (a cancelled job: its state, 409 `job_cancelled`). DocRoutes under the
  tag "Documents lab"; `CAPABILITY_TABLE` rows `Admin`.
- Page: `pages/documents_lab.rs` (+ `form`, `card`, `viewer`, `outline`, `pager`, `rendered`,
  `result`, `job`, `side`, `kept`, `words`). The route is the header's task menu (Convert /
  As docling-serve), the options are every native option, each with its field name beside its
  plain label; the reference panel is `doc_tools`. The job is followed on the public
  `/v1/documents/jobs/{id}` with the owner's session (500 ms pace) and cancelled there. Markdown is
  rendered by the Chat's `md_to_html` (raw HTML as text, unsafe link schemes `#`, remote pictures
  as links, data pictures shown); a link click opens http(s)/mailto in a new window (the shell's
  system browser) and does nothing else. Long texts page at about `PAGE_BYTES` (48 KiB, cut at a
  blank line outside a fence) and the outline at `PAGE_BLOCKS` (200); a table shows 50 rows until
  "Show all", then 500 rows a page.

**Decisions.**
- The lab has its own server like its siblings, not the page calling `/v1` directly: the uploads
  are the lab store's inputs once, and the run is recorded by the gateway even when the page goes
  away. The job's live status and cancel are the public routes.
- Options are never hidden or greyed: each group shows what `doc_tools` says about its format row
  (`scans`, `pictures`) when not read in full, and after a run the route's own notes on its
  options (by the note's `option`), so a WP that builds a feature lights it up without a change.
- The docling switch maps the native options onto docling's (`to_formats`, `page_range`,
  `do_table_structure`, `do_ocr`/`force_ocr`, `ocr_lang`, `pipeline: vlm`/`table_mode`,
  `image_export_mode`, `do_picture_description`, `picture_description_area_threshold`,
  `md_*`); a page set of several spans and a background job are refused there before sending
  (docling's `page_range` is one span; docling's async shape is not followed yet), and
  `picture_prompt` is named as not sent.
- A results card is folded to its file chips except the newest, so the history does not fetch
  every text; an Expert view switch (the page's own, persisted) adds the blocks' boxes and the
  stored answer.

**Not yet.** Following a job another tab or a reload found (the card shows "in flight" until the
run lands; only the page that started it knows the job id); a job interrupted by a restart keeps
no run (the group shows stopped); docling's async routes in the lab; a byte bound shown before
sending (the route's 413 says it); a stray-answer card when a Clear lands mid-run (a toast says it
was not kept).

**Review round (2026-11-19).** A review found no blocker, three should-fix items and six nits; all
are fixed.
- Uploads: `POST /documents-lab/api/groups` spools each `file` part to disk through the lab
  store's `Spool` as it arrives (`labs/artifacts.rs`; recorded as `NewArtifact::Spooled`), counts
  the bytes against `max_body_mb` as they come and answers the route's own 413 envelope (the
  native one, or docling's for the docling route) the moment the count passes it, with nothing
  kept (the spool files drop). Kept files the form names again count into the same request. A run
  no longer reads the files into memory: `web/documents_lab/body.rs` streams the multipart from the
  artifact files in 1 MiB reads with its exact `Content-Length`, so the route's `max_body_mb` check
  sees a declared length as for a client. Peak memory per run in the lab is one 1 MiB read plus
  whatever the route itself holds for the request (it reads the files whole into memory: about 1×
  the files' total, at most `max_body_mb`); before it was about 3× (the files, then a second
  buffer of the whole body). `multipart_body` (the Image lab's) now reserves its capacity.
- Migration 0115 has a data test (`tests/it/migrations/lab_documents.rs`): a version-114
  database with groups of all three labs (a set run's items with `batch_id`), runs, artifacts and
  presets a batch names; counts, ids, every column, the sequence (a deleted top id is not handed
  out again), `PRAGMA foreign_key_check`, no cascade, `lab_batches.preset_id`, a `'documents'` row,
  the cascades after the rebuild, and a replay after the commit. It found no defect in the file.
- Large results: each text's load state and the structure's parse are held per file view
  (`viewer.rs` `Held`), so a switch between forms fetches and parses nothing again; a line longer
  than `FORCE_BYTES` (192 KiB, the structure's compact JSON) is cut inside itself at a character
  boundary (`pager.rs`), without pretty-printing; "Show all N rows" of a table pages 500 rows at a
  time (`outline.rs` `grid_window`, which also starts under a span from above).
- Nits: the curl text uses `--form-string` for fields and `file=@"…"` with `"` and `\` escaped
  (checked against the real `curl` in `api_ref/curl.rs`; the Audio and Image labs' curl takes the
  same change); the Expert view tooltip names what it shows; the results head's filter offers
  the two routes; plain words for the model-help row, the capability notes, the stats line and
  the header labels (the wire name stays a small monospace hint); docling's `json_content` shows
  as JSON only; the usage-row, 403 and cancel → `job_cancelled` tests; the job poll stops after
  three reads that say `job_not_found` and shows "The job is gone".
- Adapter: option notes for the docling route no longer hold `,` or `;` (the header's sanitiser
  turned them into `_`: `pipeline=mapped(page-reading model_ every page)`); a test builds every
  option's note and checks the sanitiser changes none.

**Live check (2026-11-19, dev instance, CPU only).** curl on the lab API: two PDFs (one broken) →
200, summary 1 converted 1 failed (`unreadable`), markdown kept as an artifact; docling one PDF →
200 with `x-lmgw-documents: converted=1…`, `md_content` a ref; docling two files → zip kept with
`example-report.md`, `hostile.md`; async 200-page PDF → `job` at once, the run landed 996 ms later
with `location` and 200 parts; an 800-page PDF (8.9 MiB) as markdown (949 KiB) and as structure
(2.3 MiB, 8 602 blocks) in about 3.3 s each. UI drives (`scripts/ui-drive.py`, scale 1.25,
1600×1000): a hostile markdown file rendered inert (no script, iframe, `onerror`, `javascript:` or
remote image in the DOM, the page's title and background untouched, its data picture shown); the
800-page structure as 44 pages of at most 200 blocks (the next page drawn in 12 ms), its markdown
as 20 pages; an async 800-page job followed live (state, files, pool) until its run landed; the
page itself never scrolls. `scripts/webkit-check.py --backend broadway --zoom 1.25` on
`/documents-lab` at 1440×900 and 1024×700 (the one-column view): no page overflow.

### WP8

**Built (2026-11-19; ONNX Runtime 1.28 through `ort` `=2.0.0-rc.13`, poppler 26.08.0).**
- `extract/convert/layout_model.rs` (+ `files`, `prep`, `decode`, `session`, `status`): docling's
  heron layout model on the CPU. The model stage of a PDF (`stage`): one plain `pdftotext` run
  over the request's page span for the pages there are and those that have text, then per picked
  page with text a `pdftoppm` render straight to the model's 640 × 640 input
  (`extract/pdf/square.rs`, in a pool slot; batches of pages into a private folder since the
  review round) and one inference through the shared session.
- `extract/convert/pdf_layout/regions.rs` (+ `assign`, `table`): the layout pass over regions.
  `pdf_layout::layout_with` takes the stage's regions; a page with usable regions is laid out from
  them, every other page by WP3's rules, unchanged. `file/pdf.rs` calls `stage` before it takes
  the file's slot and hands the regions to the layout.
- Settings › Documents › "Layout model" (`documents/models.rs`): found / missing / downloading /
  wrong file, what it adds, size, licence, origin, **Download** (`doc_model_add`, the download
  queue), **Use it** (`documents.onnx_layout`, `settings_set`'s `documents_onnx_layout`). The
  row is `DocCaps.models` (`lmgw-api-types/src/documents/models.rs`).
- New warning code `picture_text`; `figure`, `caption`, `code` and `formula` blocks are now
  written for PDFs (figures with source `onnx`); list items the region pass marks as numbered
  are markdown's ordered items (`BlockKind::NumberedItem`, review round; the first build sniffed
  the text in `doc/render.rs`).

**Decision, by WP0a's numbers alone** (WP0b has not run yet):

| Model | Decision | Numbers |
|---|---|---|
| heron layout, docling's own ONNX export (`docling-project/docling-layout-heron-onnx` `model.onnx`) | **used**, opt-in | below: corpus PDFs 3-gram J 0.901 (off 0.857), line ratio 0.506 (off 0.270), headings 160/159, tables 28/31; 0.59 s/page |
| heron int8 (docling.rs's own quantisation, a GitHub release asset, not on Hugging Face) | not used | 0.29 s/page against 0.38 at 4 threads, 245 MiB against 430 MiB; docling.rs itself re-runs suspicious pages through fp32 because int8 scores flip detections near the 0.5 thresholds |
| TableFormer (table structure) | **not yet** | docling.rs with it: tables 30/31, cells 1413/1322. lmgw with the layout model's table regions and rows and columns from the lines: tables 28/31, cells 1468/1322 (WP3's rules alone: 39/31, 1980/1322). What is left for it: header rows over two lines and cells that wrap or span (corpus `2305.03393v1-pg9`: 17 rows where the reference has 13) |
| PP-OCR (detection, recognition, text-line orientation) | **not yet** | WP0a: docling.rs with them 0.786 on the scans against tesseract's 0.476, the gap being rotated pages (90°: 1.0 against 0.023); on rendered text PDFs only tesseract (0.637, 2.28 s/page) was measured. WP4 builds the tesseract path; these models answer what it does about orientation, and are measured against it once it exists |
| `oar-ocr` (PP-DocLayout, SLANet) | not measured | D3/D8 named it as the alternative; heron reproduces docling's own structure and runs on the pinned `ort` without it, so no new crate |

**Origins and licences** (read 2026-11-18 from the hub's API and docling.rs's `models-v1` release
notice):
- Layout: `docling-project/docling-layout-heron-onnx` at commit
  `40bde044036bb181c130ddf6c51792187268748f`, `model.onnx`, 171 220 471 bytes (163 MiB), SHA-256
  `59c81a3a2923042d85034ffc487f8f47e4854117e879aef89b2b9f728fb4922a`, card `license: apache-2.0`,
  base model `docling-project/docling-layout-heron` (RT-DETRv2). Not gated. The file docling.rs
  ships (`layout_heron.onnx`, SHA-256 `4f6125ab…`) is its own export of the same weights with other
  inputs and outputs; lmgw pins the upstream export.
- TableFormer: `docling-project/docling-models` (`model_artifacts/tableformer/accurate`),
  CDLA-Permissive-2.0 / Apache-2.0; no upstream ONNX export, docling.rs's `encoder`/`decoder`/
  `bbox` graphs are its own (GitHub release assets).
- OCR: PP-OCRv6 detection and recognition and the text-line orientation classifier from RapidOCR's
  model hub `v3.9.2` (PaddleOCR-derived), PP-OCRv3 recognition from `SWHL/RapidOCR`; all
  Apache-2.0.

**The model on `ort` `=2.0.0-rc.13`** (spike, a throwaway program outside the workspace). It
loads and runs as it is. Inputs `images` (`uint8` `[batch, 3, 640, 640]`, 0–255: its
`preprocessor_config.json` has rescale and normalize off, the scaling is in the graph) and
`orig_target_sizes` (`int64` `[batch, 2]`, width then height: found by giving it 1000 × 2000);
outputs `labels` `[batch, 300]`, `boxes` `[batch, 300, 4]` (corners, scaled to that size) and
`scores` `[batch, 300]`: RT-DETR's own post-processing (sigmoid, top 300) is in the graph, and
its answers equal docling.rs's export decoded by hand (same boxes to five decimals). 17 classes
(`config.json` `id2label`). Pre-processing as docling: the page stretched to 640 × 640 without
keeping its shape; `pdftoppm -scale-to-x 640 -scale-to-y 640` gives that directly. Against a
72 DPI render resized in the program with a triangle filter (docling renders at scale 1.0 and
resizes bilinearly) the regions differ in detail (of 1 869 kept regions on the corpus' 115 pages
1 471 are the same box, IoU ≥ 0.9, under both) and score the same or better against docling's
reference (prototype: Jaccard 0.924 both, line ratio 0.579 against 0.565), so nothing is resized
in the gateway and the render is the model's input size, no more. Post-processing as docling's
`LayoutPostprocessor`: per-label thresholds (0.45 for section header, title, code, checkboxes,
form, key-value region, document index; 0.5 for the rest), then overlap resolution where the
regions meet the lines (`regions/assign.rs`).

**Time and memory in the spike** (a release binary with only the session; the pinned file, 41
pages of three corpus PDFs, the machine under other builds' load (load average 8–12 on 32 threads)):
1.25 s a page with 1 thread, 0.37–0.40 s with 4, 0.43 s with 8, 0.36 s with 16; render 0.04 s a
page. Resident memory 364 MiB after the load, 427 MiB after the pages; load 0.45–0.7 s. Without spinning threads 0.36–0.38 s at 4 threads
(the same), so the session's threads do not spin between runs.

**Decisions:**
- **Opt-in by an explicit switch, off** (`documents.onnx_layout`, §4.4 has the key): §3 puts the
  ONNX models in tier 1 and D5 has tier 1 off until chosen. Switching it on is refused while
  the file is not there; a file that goes missing later is a `tool_missing` warning on each
  conversion, not a silent fallback.
- **The model file is not a `ToolId`.** The capability table's tools are binaries with packages
  and a version probe; a model file has a repository, a commit, a hash and a size. It is a row
  of its own kind (`DocCaps.models`), built from the same `files::locate` and the same choice a
  conversion reads, so the page and a conversion cannot disagree.
- **Where the file comes from:** the sibling pattern of `image_upscaler_add`: a pinned entry and
  an op that queues it through the existing download queue into the aux models directory (§3) at
  `<owner>/<repo>/<file>`. The op refuses when the hub's listing names another SHA-256 at the
  pinned commit; the queue checks the bytes against the hub's hash; the gateway checks them
  against the pinned hash before every load (review round; the first build did it once per
  file, by path, size and modification
  time) and loads nothing else. `hf_add` still takes only `.gguf` for `aux`.
- **The model stage runs before the file's geometry runs** and holds no pool slot while it
  waits for the model. One more plain `pdftotext` run over the request's span tells it which
  pages exist and have text, so a page without words is never rendered (WP4's recognised words
  are not yet words here), and the pages to read are those poppler wrote, not the file's claim.
  A file whose geometry run then fails has had its pages read for nothing.
- **One session, one inference at a time, its threads counted as pool slots.** The page at the
  gate takes `min(LAYOUT_THREADS, pool size)` slots (4, the knee of the timings above) and the
  blocking thread holds them and the gate until the run ends. Since the review round it takes
  them all at once at its turn (`ToolPool::acquire_many`) and holds none while it waits (the
  first build took them one by one, which a smaller pool set meanwhile could strand). A session
  per slot would be 430 MiB each; a dedicated pool beside the tool pool would run beside it on
  the same cores.
- **Idle unload after `LAYOUT_IDLE_UNLOAD` (300 s);** the next page loads it again (0.5 s).
  `LAYOUT_MEMORY_BYTES` (485 MiB) is what the page shows as its memory, from the measurement
  below.
- **Fallback per page:** more regions than the model has queries (`MAX_REGIONS` 300), no region
  left after the thresholds, or more than `ORPHAN_MAX_SHARE` (2/3) of the lines in no region:
  WP3's rules lay the page out, and a `part_failed` warning names it. On the corpus the worst
  page has 52 % of its lines in no region (`table_misidentified_as_form`), the next 5 %.
- **A line in nested regions goes to the smaller one when they score alike** (within 0.05, the
  overlap rule's own slack; docling gives it to the best-scored). The model often answers one
  region over two paragraphs and one for each: the fixture's "1 Introduction" paragraphs come
  out as one with docling's rule and as two with this one. On the corpus it joins author blocks
  and pull quotes docling's rule leaves in pieces and costs 0.002 of line ratio against
  docling's reference. "Always the smallest" was tried and splits captions and list items.
- **A text region that only wraps others is left out** (`regions/assign.rs` `wrappers`): two
  or more other regions lie in it, the innermost region of at least `WRAPPER_MIN_COVER` (0.9) of
  its lines is one of them, and no two of them share one of poppler's blocks. The fixture's
  second column is answered once whole (0.87) and once per paragraph (0.58–0.68): docling's rule
  gives one paragraph of three, this one three. The block test keeps an author block answered
  once whole and once line by line whole. On the corpus it changes two places (a newspaper
  heading, a line of three labelled fields) and +0.001 of line ratio.
- **Reading order is WP3's columns rule over the regions.** docling's rule (a graph of what lies
  above what, with dilation and the page's drawn separators) was not built: it is about 1 600
  lines in docling.rs and needs the page's rules, which poppler's text geometry does not give.
  Sorting the regions top to bottom instead costs 0.08 of line ratio (prototype: 0.453 against
  0.535).
- **Paragraphs that go on in the next column or on the next page are joined** (docling's
  `predict_merges`, with docling.rs's stricter lower-case start): +0.04 of line ratio. Across a
  page break only when both pages were laid out from regions. The joined block keeps the box and
  the page of its first part.
- **Heading levels still come from font sizes** (WP3's ranking), not docling's flat `##`.
- **Tables:** the region is the table; rows are visual lines and columns the merged x-ranges of
  the lines of rows with several cells; a line across columns spans them. WP3's finder is not
  run on a page laid out from regions.
- **Pictures** are `figure` blocks with source `onnx` and the region's box, drawn ones included
  (D22). Lines inside one are the figure block's `text` (json), left out of markdown and text
  with a `picture_text` warning, as docling leaves them out. A table region without a word is a
  figure too. **The seam to WP10:** WP10's raster pictures from poppler's image positions and
  these regions describe the same pictures where both exist; where the two meet they must
  become one block (the box that the crop takes is the region's). Nothing is cropped here.
- **Formulas and code** (D23): blocks of their kinds with the text layer's text; code keeps its
  lines and an indentation read from where they start.
- **Block source:** a block's words say where it was read (`text_layer`); only blocks that exist
  because of the model (figures) are `onnx`.
- **Typography is kept.** docling writes `-` for an en dash and `'` for a typographic
  apostrophe; lmgw keeps the PDF's characters. Folding them in the scorer alone adds 0.02 to
  the Jaccard and 0.05 to the line ratio (prototype), so part of the gap to docling.rs below is
  this.

**Measured (2026-11-19, dev instance, debug build, under other builds' load (load average 11–16 on 32 threads); poppler 26.08.0; the pinned model file, SHA-256 `59c81a3a…`, downloaded by `doc_model_add`
in that instance):** the WP0a corpus' 28 PDFs (115 pages, 18 with a reference) against docling's
ground truth, with docling.rs 2.85.0 (its own models, SHA-256 `4f6125ab…` for the layout graph,
`SHA256SUMS` of its `models-v1` release) run on the same files within the hour.

| | conv. | s/file med. | s/page | 3-gram J | line ratio | headings | tables | cells |
|---|---|---|---|---|---|---|---|---|
| lmgw, model off | 28/28 | 0.064 | 0.028 | 0.857 | 0.270 | 167/159 | 39/31 | 1980/1322 |
| lmgw, model on | 28/28 | 0.864 | 0.591 | 0.901 | 0.506 | 160/159 | 28/31 | 1468/1322 |
| docling.rs | 28/28 | 0.710 | 0.358 | 0.972 | 0.815 | 161/159 | 30/31 | 1413/1322 |

```sh
LMGW_CONTAINER_PREFIX=lmgw-wp8 LMGW_DATA_DIR=<scratch> scripts/dev-instance.sh 127.0.0.1:8885
LMGW_KEY=<owner token from its log> python3 -I scripts/convert-measure.py --corpus "$CORPUS" \
  --lmgw http://127.0.0.1:8885 --formats pdf --per-file --out off.json
curl -H "Authorization: Bearer $LMGW_KEY" -d '{"id":"layout"}' -H 'content-type: application/json' \
  http://127.0.0.1:8885/api/op/doc_model_add      # then, when the row says found:
curl -H "Authorization: Bearer $LMGW_KEY" -d '{"documents_onnx_layout":true}' \
  -H 'content-type: application/json' http://127.0.0.1:8885/api/op/settings_set
LMGW_KEY=… python3 -I scripts/convert-measure.py --corpus "$CORPUS" --lmgw http://127.0.0.1:8885 \
  --formats pdf --per-file --out on.json
python3 -I scripts/convert-measure.py --corpus "$CORPUS" --formats pdf --per-file \
  --cmd "docling-rs=env -C <dir with .models> $DRS/docling-rs --no-progress {in}" --out doclingrs.json
```

- **Model off is WP3's row exactly** (0.8569, 0.2698, 167, 39, 1980): nothing changes for
  someone without the model.
- **Per file**, model on against off: the Jaccard and the line ratio rise or stay on all 18
  files with a reference but one. `2206.01062` 0.750 → 0.954 and 0.163 → 0.688; `2203.01017v2`
  0.830 → 0.893 and 0.222 → 0.621, tables 10 → 6 of 6; `2305.03393v1` 0.893 → 0.946 and 0.164 →
  0.649; `newspaper-00` 0.868 → 0.941 and 0.033 → 0.371; `table_misidentified_as_form` 0.595 →
  0.816, tables 0 → 3 of 3; `elsevier-00` tables 9 → 3 of 3. **Worse: `acroform_sample`**, line
  ratio 0.667 → 0.000 (its Jaccard rises, 0.835 → 0.851): the model answers one text region over
  four form rows 30 pt apart, and they come out as one paragraph where docling has a line each.
  Splitting a region at wide gaps was tried in the prototype and left out: it helps this file
  and breaks paragraphs with superscripts in `normal_4pages`.
- **What is left to docling.rs** (prototype over the same regions, 15 files without the
  right-to-left ones): with the reading order free the line ratio would be 0.615 against 0.580,
  so the order rule is worth at most 0.035; with docling's typography folded in the scorer 0.636.
  Of the reference's 1 379 lines the output lacks 36 % of the table rows (the table model), 59 %
  of the list items and 32 % of the paragraphs; with the typography folded 34 % of the list items
  and 19 % of the paragraphs (the rest differ by single characters such as accents docling
  writes decomposed, or by how a paragraph is cut).
- **Time.** 0.59 s a page under that load: the plain text run, the render (0.04 s), the
  inference (0.37–0.40 s at 4 threads) and the geometry. Three 9-page conversions at once: 27
  pages in 13.1 s, the pool showing 4 slots running and the model busy; the three answers equal.
  A release-profile build was not made: the inference runs in ONNX Runtime's prebuilt library
  either way (the spike above was a release binary), and the layout pass is WP3's 0.03 s a page
  in a debug build.
- **Memory.** The gateway's resident memory was 216 MiB before the first page and 700 MiB after
  the 115 (peak 702 MiB). 325 s after the last page the session was gone (the log's "unloaded
  (idle)" 300 s after the last run, `loaded: null` on the page) and the resident memory 549 MiB:
  160 MiB went back to the system at once, the rest stays with the process's allocator, where
  the next load takes it from.
- **Live.** `doc_tools` lists the model as missing; `settings_set documents_onnx_layout=true`
  → 400 "cannot be switched on: its file is not here yet. Download it first …";
  `doc_model_add id=example` → 400 naming `layout`; `doc_model_add id=layout` → queued (download
  1), again → `already_queued`; the row goes `downloading` → `found`, the file's SHA-256 is the
  pinned one; switching on → 200. The fixture paper (2 pages) → 200 in 1.4 s with the model's
  load: title `#`, sections `##`, two paragraphs under "1 Introduction", bullet and numbered
  items, the table, `<!-- image -->` for the drawn chart, the code block with its indentation,
  the page-crossing paragraph as one; warnings `header_removed` and `picture_text`; json has the
  figure with source `onnx` and text `Apple\nPear\nPlum\nCherry`. `pages=2` → the one page, its
  running head marked by the model. The docling route (`/v1/convert/file`) → `success` with the
  same markdown.

**Not yet.**
- TableFormer and the PP-OCR models (table above).
- docling's reading-order rule; right-to-left reading order.
- A region the model answers over two paragraphs without answering the paragraphs too stays
  one paragraph (the fixture's last two paragraphs).
- A per-request switch for the model (it follows the setting); the model's name in the answer's
  `tools` (its warnings carry `tool: "layout_model"`, its figures source `onnx`).
- Regions for pages without a text layer (WP4's words): the pass takes lines with boxes from
  anywhere, the stage does not render such pages yet.
- Form and key-value regions (left out; their content is laid out as the regions inside them),
  checkboxes (text), caption-to-figure links (`caption` in a figure block is WP10/WP11's).
- Several pages in one inference (the export's batch axis is free); a quantised model.
- Giving the unloaded session's memory back to the system in full (`malloc_trim` on glibc): 325
  MiB of it stays in the process until reused.
- Windows and macOS: the code compiles there, PDFs need pdfium first (WP9).

**Unverified.** WP0b's numbers (the decision above goes by WP0a's alone). Windows (the code
has nothing of one system but a test's symlink; the Windows compile check). A long PDF
end to end with the model on (the stage holds one page's image at a time by construction; the
longest file measured has 19 pages). The time per page on an idle machine and in a release
build. The download job's sentence when the pinned commit is gone from the hub (it is the audio
catalog's wording). `ORPHAN_MAX_SHARE`, `WRAPPER_MIN_COVER`, `SHARE_SLACK` and the score slack
beyond the corpus and the fixture. The page in the app's own window (it was looked at in
headless Chrome).

**Review round (2026-11-19).** One blocker, nine should-fix and six nits, each with a test that
fails on the first build:
- **Turned pages** (`/Rotate` 90 and 270): `pdftoppm` gets `-scale-dimension-before-rotation`, so
  the square holds the whole page as displayed (without it the render was cut at the right and
  stretched; measured on the corpus' `odf/sources/odf_presentation_01.odp.pdf`, rotation 270), and
  the region shares are placed with the displayed size `pdftohtml -xml` gives per page (`fonts.rs`
  reads it beside the runs; `pdftotext -bbox-layout` gives the unturned size while its words are
  displayed coordinates). Without `pdftohtml` a page with a line outside `pdftotext`'s size is not
  placed (`Implausible::UnknownSize`, geometry). Tests: `square.rs`
  `a_turned_page_is_rendered_whole` and `pdf_layout/tests/regions_checks.rs`
  `a_turned_page_s_regions_land_on_its_lines`, both on self-made `/Rotate` 90 and 270 fixtures
  (`test_files::pdf_turned`).
- **Slots:** `ToolPool::acquire_many` (pool.rs): a step waits for its caller's round-robin turn
  holding nothing, then takes N slots at once, N clamped to the size in force each time it is
  looked at; a step whose turn it is but that does not fit keeps its turn and nothing is handed
  out past it, so it is never passed for ever (the slots that come free meanwhile stay free for
  it); a step dropped while waiting hands its turn on at once. The page at the gate takes
  `acquire_many(LAYOUT_THREADS)` and its session's threads are what it got. No path holds a slot
  while it waits for more. Tests: `pool/tests_many.rs` (sizes 1, 2, 3, 4, 8; down and up while
  waiting; a stream of single steps; cancellation) and `layout_model/session/tests.rs`.
- **The stage's bounds** (`layout_model/stage.rs`): the picked pages go to `pdftoppm` in runs of
  up to `RENDER_BATCH_PAGES` (16) neighbouring pages, written into a private folder
  (`tempfile`, in `/tmp`, so at most `RENDER_BATCH_BYTES`, 16 × 640 × 640 × 3 = 18.75 MiB, at
  once), read back one at a time and deleted; the file is piped once per batch. All renders of a
  file get `PDF_TIMEOUT` together (what one geometry run of the whole file gets): the render that
  reaches it ends the stage. With a caller's `document_timeout` the stage also ends once the
  file's clock has used `MODEL_DEADLINE_SHARE` (½) of it. Either way the pages not read are laid
  out by the geometry with a `part_failed` note saying why. Tests: `stage/tests.rs` (a `pdftoppm`
  stand-in that never finishes, the deadline share with a stand-in session, pictures deleted).
- **The file check at every load:** `files::read_checked` reads the file once (at most its size
  and a byte), hashes those bytes and the session is made from them (`commit_from_memory`); the
  remembered verdict only feeds the page's row. Test: a same-size file with the first one's
  modification time is refused at the next load.
- **Numbered items** are data: `BlockKind::NumberedItem(marker)`, set by the region pass for a
  list item without a bullet that starts with `1.`/`2)`; the renderer no longer reads text, and
  the rest is escaped like any item's. `render.rs` is `main`'s plus the new arms, so model-off,
  office and markdown output are unchanged by construction. Tests: `render.rs`
  `only_a_marked_item_is_numbered` ("24. Dezember" bullets, a markdown file's `- 1. x`),
  `regions_checks.rs` `a_list_item_is_numbered_only_without_a_bullet`.
- **The unload watcher** is armed before the blocking task is awaited, and its flag is cleared
  under the gate, so a page that loads after the watcher's last look always starts another. Tests
  through the runner seam (`session::Runner`): cancellation during the first load, during a run,
  thirty pages racing a 10 ms idle unload.
- **Pages mostly inside pictures** (a rule corrected by measurement): more than `PICTURE_TEXT_MAX_SHARE` (½) of a page's lines inside picture regions
  AND those regions covering more than `PICTURE_PAGE_MIN_AREA` (½) of the page's area makes the
  page implausible: it goes by WP3's geometry with a `part_failed` note naming the reason. The
  first form (lines only) measured worse than no rule; the area condition (`mostly_picture`) was
  measured 2026-11-19, same dev-instance recipe:

  | variant | J | line ratio | headings | tables | cells |
  |---|---|---|---|---|---|
  | lines only (review round) | 0.888 | 0.476 | 174/159 | 37/31 | 1936/1322 |
  | rule off | 0.901 | 0.506 | 160/159 | 28/31 | 1468/1322 |
  | lines and area > 0.5 (**taken**) | 0.9011 | 0.5056 | 160/159 | 28/31 | 1468/1322 |

  0.7 was not needed. The Redbook cover (`redp5110_sampled` p. 1) does not trip the rule at 0.5
  (its pictures cover under half the page); its title, subtitle and `ibm.com/redbooks` lines are
  in the markdown as text regions (`# Row and Column Access Control Support in IBM DB2 for i`),
  the figure's 13 lines left out with the `picture_text` warning. The result equals the rule-off run on every figure of the table. The goal is
  text an LLM can read; a slide taken for one picture would be one `<!-- image -->`. Below it
  nothing changed (`picture_text` warning, the text in the figure block in `json`).
- **Region tables** apply WP3's `TABLE_MIN_FILL`: a region whose lines fill less of their grid is
  text regions (one per poppler block), as orphans are.
- **Nits:** the page's row says busy with the threads the run has and not loaded while a load
  goes on or failed; one `part_failed` note per reason; `op_names`' test says 115; waiting at the
  gate (and for any slot after the first) stops the file's clock (`pool/clock.rs`, replacing the
  first-slot notify: the clock pauses while a step waits); a line a table or picture would take
  stays with a smaller regular region not 0.8 inside it (`kept_by_text`: a legend half over a
  chart stays text, as docling gives a picture the regions inside it); `square.rs` and the docs
  no longer say "whatever its shape".

**Decisions of this round:** the stage's share of a caller's
`document_timeout` is ½ (`MODEL_DEADLINE_SHARE`), so a file with a timeout converts with the
model's pages it reached instead of failing; without a timeout the stage is bounded by the
renders' `PDF_TIMEOUT` and the inferences (about 0.4 s a page); a step waiting for several slots
keeps its turn and the pool hands out nothing past it (no starvation, at the cost of slots idle
while it waits for its last one); the clock now stops for every slot wait of a file, not only
the first; a load holds the file's bytes (163 MiB) beside the session while ONNX Runtime builds
it.

**Measured after the review round (2026-11-19, dev instance on `main` + this branch, debug build,
under other builds' load; poppler 26.08.0; the pinned model file):**

| | conv. | s/file med. | s/page | 3-gram J | line ratio | headings | tables | cells |
|---|---|---|---|---|---|---|---|---|
| model off, before and after | 28/28 | 0.060 | 0.027 | 0.857 | 0.270 | 167/159 | 39/31 | 1980/1322 |
| model on, first build | 28/28 | 0.864 | 0.591 | 0.901 | 0.506 | 160/159 | 28/31 | 1468/1322 |
| model on, review round | 28/28 | 0.657 | 0.475 | 0.888 | 0.476 | 174/159 | 37/31 | 1936/1322 |
| model on, review round, picture rule off (`PICTURE_TEXT_MAX_SHARE` 2.0, measurement only) | 28/28 | 0.597 | 0.431 | 0.901 | 0.506 | 160/159 | 28/31 | 1468/1322 |

```sh
LMGW_CONTAINER_PREFIX=lmgw-wp8 scripts/dev-instance.sh 127.0.0.1:8885
LMGW_KEY=<owner token from its log> python3 -I scripts/convert-measure.py --corpus "$CORPUS" \
  --lmgw http://127.0.0.1:8885 --formats pdf --per-file --out off.json   # then switched on:
LMGW_KEY=… python3 -I scripts/convert-measure.py … --out on.json
python3 -I scripts/convert-measure.py --table off.json on.json
```

- **Model off is unchanged** (the same numbers and per-file entries as `main`).
- **What got worse is the picture rule, all of it.** Model on, only five files differ from the
  first build, and each has a new `part_failed` note naming `PICTURE_TEXT_MAX_SHARE`: 11 of the
  115 pages (`2206.01062` p. 1, `2203.01017v2` p. 5 and 8, `2305.03393v1` p. 2, 8, 10, 11,
  `redp5110_sampled` p. 1 and 10, `skipped_2pages` p. 2). On the papers these are pages with a
  figure full of small text (DocLayNet's page 1: a figure of four page thumbnails holds more of
  the page's lines than its prose, on about a third of its area); laid out by WP3's rules, the
  figure's words come back as paragraphs and WP3's finder makes false tables of them (tables 28
  → 37, the reference leaves the figure text out), and their headings are WP3's. The Redbook's
  cover is the rule's own case (its title and author lines). With the rule set out of reach the
  round's numbers are the first build's (line ratio 0.5056 against 0.5057), so the other fixes
  cost nothing measurable. The time per page fell (0.59 → 0.48 s; the 11 pages are still rendered
  and read by the model, so this is the batch render, one `pdftoppm` run per 16 pages instead of
  one per page, and the machine's load, which was lower). The area condition was built afterwards (the Pages mostly inside pictures decision above).
- **The turned corpus file** (`odf/sources/odf_presentation_01.odp.pdf`, `/Rotate` 270): its
  pictures are the whole slides now (looked at); the model answers slide 1's table of bullet
  cells as a code region, so its words come out fenced, and slide 2's columns of list items as
  three numbered items. Without the model WP3 makes a table of them; neither is docling's.
- **Live** (pool set to 2): three PDFs at once (9, 16 and 14 pages) → 200 each, in 18.4, 24.6
  and 25.6 s; the pool showed 2 running and the model reading with 2 threads, set to 4 while
  they ran → 4 running, the model with 4 threads, back to 2 → 2 and 2; each answer equal to the
  same file converted alone. A 16-page request cancelled by `curl --max-time 3` mid-stage: 2
  running at once, 0 two seconds later (the run in flight ended), no `lmgw-layout-*` folder left;
  the log's "unloaded (idle)" came 300 s after that last run, and the page's row said not in
  memory.

**Not yet / unverified after the review round.** WP3's own pass still takes a turned page's size
from `pdftotext` (the unturned one): its json `width_pt`/`height_pt` and the header/footer bands
of such a page are measured against it; the region pass does not use them. Several pages in one inference, and a release build, are still
not measured. The stage's time on a file of hundreds of pages with the model on (the batch, the
budget and the share are tested with stand-ins, not on such a file). Windows: the Windows
compile check.

### WP10

**Built (2026-11-19, poppler 26.08.0).**
- `extract/convert/pictures.rs` (+ `pdf`, `render`, `office`, `caption`, `sniff`): a file's
  pictures as one `Found` list in reading order (part, box, caption, the paragraph before, bytes),
  each named by a `figure` block (`doc::Figure` on `doc::Block`) or a picture line. Glue:
  `file/pictured.rs` (the step between reading and output), one hook each in `file/pdf.rs` (the
  font run, `assemble`) and `file.rs` (`Body::Pictured`, `read_office`).
- PDF: pictures from `pdftohtml -xml -dataurls` runs of their own beside the text runs (the font
  run stays `-i`; review round below): one `<image top left width height src="data:…">` per
  image drawn, inline, so nothing is written to disk; only a SHA-256 of each image's data is
  kept. Figures go into WP3's ordered blocks (before the first block in
  reading order that overlaps them sideways and whose middle is below theirs); crops are one
  `pdftoppm -r 72×images_scale -x -y -W -H -png -singlefile` run per page with pictures (the box
  holding them all), cut and written as PNG in a pool slot (`png` crate).
- Office: `extract/office_media.rs` (a collector the docx/pptx/odt/odp walkers call; `U+FFFC`
  markers in the part markdown where the picture is referenced; Chat and knowledge-base reads do
  not collect and are unchanged), `pictures/office.rs` (relationships, media bytes as stored,
  `picture_min_area` against the declared page/slide size, markers to lines or `figure` blocks).
- Wire: `ConvertedDocument.pictures` (`documents/pictures.rs`, `DocPicture`), `DocBlock`
  `picture`/`caption`/`description`, `images_scale` (native), `WarningCode::PicturesLeftOut`.
- Adapter: `image_export_mode`/`include_images`/`images_scale` honoured; the zip writes
  `artifacts/image_<n:06>_<sha256>.<ext>` beside the markdown (stored, each name once).
- Jobs: pictures with bytes are 0600 files at `convert/<job>/<index>/artifacts/…`, the stored entry
  keeps their names, reading puts the bytes back; counted by "Stored conversions", deleted with
  the job.

**Decisions:**
- `pdftohtml -dataurls` instead of the run without `-i` that §5.3 names: without `-i`, poppler
  writes every image as a file into its working directory (checked: also when the files cannot be
  written it drops the element), which is unbounded disk or tmpfs; `-dataurls` keeps it on stdout
  under `MAX_UNCOMPRESSED_BYTES`. The cost is poppler PNG-encoding every image it meets, which is
  why it runs only on the pages the `pdfimages -list` gate picks (review round).
- Crops from the render (as §5.3 says), not the embedded image: masks, rotation, clipping and
  JBIG2/JPEG 2000/CMYK are what poppler draws; one run per page, not per picture (poppler crops
  one box per run), and the page raster cut in-process after its declared size is checked against
  the box asked for. Crops are PNG, as docling writes them.
- Office media go out as stored (PNG, JPEG, GIF, WebP, BMP, TIFF; never decoded); docling
  re-encodes to PNG. EMF/WMF and SVG (scripts, outside links) keep placeholders, named.
- Artifact names hash the bytes (docling hashes the decoded pixels); the extension is the
  format's.
- `picture_min_area` filters the export, as §5.3's wording says, and stays so after the review.
  docling applies its `picture_area_threshold` to descriptions only and still writes small
  pictures as placeholders; lmgw leaves them out and names them in `pictures_left_out`. Alternative not taken yet: following docling (keep small pictures as placeholders, apply the area to
  WP11's descriptions; the gate could then skip only page covers).
- Left out, counted in `pictures_left_out`: the same image data in the same place (to the point)
  anywhere on the page, on at least `HEADER_MIN_SHARE` of the picked pages (WP3's header rule; a
  logo, a watermark, a background); an image covering `PAGE_COVER_SHARE` (0.9) of its page (background or scan); one drawn
  twice in one place is one picture. Identical images elsewhere keep one figure per place; their
  crops share an artifact name when the bytes match, so the zip holds them once.
- Vector figures (D22): poppler lists no paths without rendering, so the `pictures_partial` note
  keys on a numbered figure caption (`Figure 3`, `Abb. 2`, …) that no picture took as its caption.
- Captions: the block right after (else before) a figure, within `CAPTION_MAX_GAP_PT` (36),
  overlapping it, starting with a caption word and a colon or a number that ends the label (review
  round); in PDFs it becomes a `caption` block.
- Bounds: `PAGE_RASTER_MAX_BYTES` (= `MAX_UNCOMPRESSED_BYTES`, the raster replaces the dropped
  geometry outputs inside `PER_RUN_BYTES`); `PICTURES_PER_PAGE_MAX` (256: past it, placeholders at
  the page end without bytes or caption); each picture reserved at its encoded size when it gets
  its bytes and the output's takes using that reservation (review round). A picture without
  bytes keeps its placeholder; the file stays `converted`.
- `embedded` with `output: json` carries the bytes in the `pictures` list; `output: text` writes
  no pictures (`option_not_applicable`).
- Adapter: `include_images: true` without a mode is docling's default form, placeholder (WP5
  mapped it to embedded; docling's `image_export_mode` defaults to placeholder on `main` and in
  v2.142.1, read 2026-11-19). One file in the body with `referenced` keeps the links and answers
  `image_export_mode=mapped(referenced pictures are files of the zip target only)`.

**Not yet:** EPUB pictures (WP4's reader, built beside this WP: `pictures::epub_not_yet()` is the
`not_yet` note its handler adds, one call at the rebase; the `office_media` collector and markers
are the seam for exporting them), spreadsheet
drawings (`not_yet` note when the workbook has media), an image file as one picture, docx header
and footer pictures (the reader reads the body only), vector figures on pages the layout model
does not read (with it, its regions are figures with crops: WP10 on WP8 below), drawings without
a numbered caption (no note), descriptions (WP11: `Picture::before`/`caption`,
`Figure::description`).

**Review round (2026-11-19, poppler 26.08.0).** The review timed the font run with
`-dataurls`: 14× to 2500× slower on scanned and image-heavy PDFs (about 1 s per page of a 600 dpi
scan, to find an image that was then left out as a page cover), able to pass `PDF_TIMEOUT` near
300 pages, and paid by `output: text` too. Fixed as the review recommended ((b) + (a) + the text
part of (c); `placeholder` stays the default):
- The font run is `pdftohtml -xml -i` again, exactly as on `main`: the text path's time and
  failures are `main`'s whatever images a PDF holds.
- Pictures come from their own `pdftohtml -xml -dataurls -f -l` runs (`pictures/pdf/runs.rs`), in
  ranges, each range in its own pool slot (run and reading of its output in one slot), started
  beside the file's text runs; the text never waits for them. A range that fails, times out or
  passes `MAX_UNCOMPRESSED_BYTES` loses its own pages' pictures only, in a `pictures_partial`
  note naming the pages and poppler's reason ("the text was read in full").
- The gate (`pictures/pdf/gate.rs`): `pdfinfo -f -l` (page sizes) and `pdfimages -list` in one
  slot. A page gets a picture run only when one of its images may be exported; a page whose images
  are all clearly page covers (drawn size within `COVER_TOLERANCE` 5% of the page on both sides)
  or clearly below `picture_min_area` (drawn area × `SMALL_MARGIN` 2 below it) is skipped and its
  images are counted in `pictures_left_out` from the list. Unsure rows run their page: ppi below
  `PPI_MIN` (10), `[inline]` rows (one column for object and generation, parsed from the end), a
  type other than `image`/`stencil` (`mask`/`smask` rows are skipped), a page without a size, a
  short line; a line without a page number or a failed `pdfinfo`/`pdfimages` makes the whole span
  one run (`Gated::Unsure`).
- Ranges: consecutive gated pages, at most ⌈pages / pool size⌉ each, never across a skipped page;
  at most as many in flight as the pool had free slots when the file started (at least one), each
  queued through the pool like every step (fair per caller). Placement runs after both sides in a
  slot of its own (`file/pictured.rs::pdf_place`), so no file holds a slot while waiting for
  another.
- `output: text` does no picture work: no gate, no run, no placement.

Other findings:
- Rotated pages and mirrored images: `images()` takes the span of `left` and `left + width` (and
  `top`, `top + height`), and placement clamps against pdftohtml's page (as displayed) instead of
  `pdftotext`'s unrotated size. Fixture tests for `/Rotate` 0/90/180/270 and a mirroring matrix,
  each cropped through `pdftoppm` (the middle pixel is the image's colour, the size the box's).
- Per-page bound: pictures past `PICTURES_PER_PAGE_MAX` get no caption lookup and no paragraph
  before; caption lookup is one pass with each block's previous and next non-figure block found
  beforehand (`pictures/pdf/order.rs`). 100,000 distinct 160 × 160 pt draws on a page with 200
  caption-like blocks place in well under a second (debug build, the test's bound).
- Budget: a picture is reserved when it gets its bytes at `pictures::reserve` (4 · ⌈n/3⌉, its
  caption twice, `ENTRY_RESERVE` 512), with `try_take`; a crop is encoded first and dropped when its
  reservation does not fit. The file's output takes go through `budget::Spend`, which uses the
  reservation first and gives back what no take used, so a byte counts once and a picture that
  does not fit is a placeholder with the existing note while the file stays converted. Poisoning
  is now a flag, as `budget.rs`'s module doc says: a failed `take` marks the request's budget
  spent, never undone by a later `give_back` (the old counter could be brought back under the
  limit by another file's give-back); `try_take` that does not fit changes nothing. Tests: the
  referenced/embedded/json case the review describes (300,000 bytes of picture in a 600,000-byte
  budget: converted; it failed before), and a spent request staying refused after a reservation
  comes back.
- Captions: sliced at the matched word's own end (the KELVIN SIGN panic), tested with ẞ, İ, final
  sigma and K. Rule: a caption word, then a colon, or a number that ends the label (followed by
  nothing, punctuation `: . – — - | )`, or a word not starting lower case), so "Figure 3 shows …"
  and "Abbildung 4 zeigt …" are body text (no caption, no vector-figure note). A paragraph whose
  first line is a caption ends with the caption's last line, the first one shorter than
  `CAPTION_LAST_LINE_SHARE` (0.85) of the run's widest (`pdf_layout/blocks.rs`, one hook), so the
  body text poppler gave in the same block stays a paragraph.
- Watermarks: the same image in the same place on at least `HEADER_MIN_SHARE` of the pages is left
  out anywhere on the page, counted once per page in `pictures_left_out`.
- One place to merge figures: at the start of a page's placement, an existing `figure` block that
  overlaps poppler images by `MERGE_OVERLAP` (half of the smaller box) takes them: it keeps its
  place in reading order and gets poppler's box (their union) for the crop. Tested with a
  hand-built block.
- Nits: render and cut in one slot (`piped_held` + `blocking_on`); the docling answer
  (`answer::render_off`: zip, base64 decode) and a job's stored entry (decode and serialisation on
  write, parse and encode on read) run on blocking threads, and the stored entry is no longer
  cloned whole (`results::write` takes the entry); `file/output.rs` holds `output`,
  `doc_output`, `markdown_out`, `text_doc` and `shaped` (moved); the collector's token look is
  `office_media::Collector::look`; SVG office media keep a placeholder with a named note like
  EMF/WMF; the docs' scanned-page wording says text recognition is not built yet.

**Review round measured (2026-11-19, dev instance, debug build, poppler 26.08.0, a 16-core test machine,
`pictures` at its default, placeholder; `main` is `ec094809`, before is this WP as reviewed,
rebased).** Single files by `curl -w '%{time_total}'`, median of three; the corpus as all 84 of its
PDFs one after the other (10 are scans without a text layer and answer 415, as on `main`); the
scan is `ocr_test.pdf` 50 times (`pdfunite`).

| input | `main` | before | after | after, `output: text` |
|---|---|---|---|---|
| corpus, 84 PDFs | 7.2 s | 38.1 s | 15.0 s | – |
| `2203.01017v2.pdf` (16 pages, figures on 5) | 0.36 s | 4.45 s | 1.38 s | 0.36 s |
| `ocr_test.pdf` (1-page 600 dpi scan) | 0.051 s | 0.948 s | 0.047 s | 0.048 s |
| 50-page scan | 0.090 s | 45.3 s | 0.094 s | 0.092 s |

`convert-measure.py --formats pdf` (28 files): `main` 0.064 s/file median, 0.03 s/page; before
0.085, 0.098; after 0.080, 0.051. 3-gram J 0.8569, line ratio 0.2698, headings 167/159, tables
39/31, cells 1980/1322 in all three: text, headings and tables did not move. What is left of the
corpus's extra time is the picture runs on pages with figures (PNG encoding inside poppler) and
the placement step's own slot.

```sh
LMGW_CONTAINER_PREFIX=lmgw-wp10 scripts/dev-instance.sh 127.0.0.1:8887
curl -s -o /dev/null -w '%{time_total}' -H "Authorization: Bearer $KEY" -F file=@2203.01017v2.pdf \
  http://127.0.0.1:8887/v1/documents/convert
```

**Review round live check (2026-11-19, dev instance 127.0.0.1:8887).** The 3-page report PDF:
placeholder 200 in 0.10 s, embedded 200 (568×426 and 398×398 px), referenced 200 (artifact links,
bytes in `pictures`); captions now "Figure 1: Example revenue gradient" alone, the paragraph under
it a paragraph; `pictures_left_out` "1 smaller …; 2 the same picture in the same place …" (the
logo on the page the gate skipped is counted by its size). The 50-page scan: 415 (no text layer,
as on `main`) in 0.09 s. `odf_presentation_02.odp.pdf` (`/Rotate 270`): page 4's picture found,
box `[108.7, 151.3, 669.6, 482.5]` on the 792-point-wide displayed page, crop 1123×663 px. The
docling route with the report PDF and its docx, `image_export_mode=referenced`: zip with
`pictures.md`, `pictures_1.md` and four stored `artifacts/…` entries, every link resolving. Three
copies of a 30-page PDF of noise at `images_scale=4`, embedded: 200 in 151 s, 1.0 GiB answer,
all three `converted`, pages 16/17-30 as placeholders with the `pictures_partial` note naming
`MAX_UNCOMPRESSED_BYTES (1.0 GiB)`.

Decisions of the review round:
- The gate judges by drawn size only (pdfimages gives no position): a page-sized image drawn
  mostly off its page counts as a cover. An image pdfimages does not list (none seen) is not
  looked for on a page without listed images.
- A page the gate skips counts its images by size, so a repeated logo there is counted as small
  or cover, not as repeated.
- A PDF now queues two steps at first (its text's slot and the gate's), and a third for the
  placement once both are done (`documents_structure_jobs` expects two waiting with one slot).
- A spent budget fails every later take, also one fully covered by an earlier reservation.
- The caption split keys on the caption's last line being shorter than 0.85 of the block's widest:
  a one-line caption as wide as the text after it stays merged (as before).


**Measured (2026-11-19, dev instance, debug build, poppler 26.08.0, WP0a corpus at `5e600e8d`,
`pictures` at its default, placeholder):** before is `c1a4a67b`, after is this WP.

| format | | conv. | s/file med. | s/page | 3-gram J | line ratio | headings | tables | cells |
|---|---|---|---|---|---|---|---|---|---|
| pdf | before | 28/28 | 0.065 | 0.029 | 0.8569 | 0.2698 | 167/159 | 39/31 | 1980/1322 |
| pdf | after | 28/28 | 0.067 | 0.087 | 0.8569 | 0.2698 | 167/159 | 39/31 | 1980/1322 |
| docx | before | 38/38 | 0.040 | - | 0.9017 | 0.7759 | 75/77 | 26/25 | 311/361 |
| docx | after | 38/38 | 0.032 | - | 0.9017 | 0.7759 | 75/77 | 26/25 | 311/361 |
| pptx | before / after | 8/8 | 0.017 | - | 0.7926 | 0.5918 | 18/13 | 1/3 | 63/86 |

```sh
LMGW_CONTAINER_PREFIX=lmgw-wp10 scripts/dev-instance.sh 127.0.0.1:8887
LMGW_KEY=<owner token from its log> python3 -I scripts/convert-measure.py --corpus "$CORPUS" \
  --lmgw http://127.0.0.1:8887 --formats pdf,docx,pptx --per-file --out measure.json
```

Text, headings and tables did not move (the measure folds `<!-- image -->` lines away). The time
did: the font run now has poppler decode and PNG-encode every non-JPEG image. `2203.01017v2`
(16 pages, a 3690×2328 figure) goes 0.39 → 4.4 s, `2305.03393v1` 0.23 → 1.7 s, `elsevier-00`
0.59 → 1.4 s; `pdftohtml -xml -i` on the first takes 0.12 s, with `-dataurls` 3.9 s. The run
without `-i` that §5.3 names costs the same (it writes the same PNGs to files). A 20 000 × 20 000
px image shown at 300 × 300 pt (a 1.1 MiB PDF) converts in 18.8 s, its crop 600 × 600 px.

**Live check (2026-11-19, dev instance 127.0.0.1:8887; test files made with LibreOffice 26.8
from flat ODF: a 3-page report with two raster pictures, captions and a logo in the page header,
a docx of it, a 2-slide pptx, a page with a bar chart drawn as shapes).**
- PDF markdown: placeholder 200, two `<!-- image -->` lines between the text before each picture
  and its caption, `pictures_left_out` "3 the same picture repeated at the top or bottom";
  embedded 200, `![Image](data:image/png;base64,…)` 568×426 and 398×398 px; referenced 200,
  `![Image](artifacts/image_000000_758e5ef4….png)` with the bytes in `pictures`; `json`
  200, `figure` blocks `p1`/`p2` followed by `caption` blocks.
- docx and pptx, placeholder and embedded: 200, pictures in document order (`image/png`
  400×300, `image/jpeg` 300×300 as stored), slide 2's caption found.
- Vector chart: 200, no picture, `pictures_partial` "a figure caption with no picture found
  beside it on page 1".
- Bounds: `images_scale=100` 200, `pictures_partial` naming `PAGE_RASTER_MAX_BYTES (1.0 GiB)`;
  `images_scale=0` 400 `invalid_option`.
- docling route: two files, `image_export_mode=referenced` → `converted_docs.zip` with
  `pictures.md`, `pictures_1.md` and four `artifacts/…` entries (stored); every markdown link
  resolves to an entry. One file `embedded` (`images_scale=1.5`) → `ConvertDocumentResponse`
  `success` with data URIs; one file `referenced` in the body → `x-lmgw-option-notes:
  image_export_mode=mapped(referenced pictures are files of the zip target only)`. An async task
  with two files: the job directory holds the entries and `artifacts/…` files, all 0600; the
  result zip has the same entries. The instance log holds no picture bytes.

**Unverified:** pdftohtml's element for inline images, stencil masks and soft-masked images
beyond the XObjects of the test files and the corpus (the crop is poppler's render either way); a
real docling client reading the zip; the release build's timings (all above are debug builds).
WP8's figure regions through `place`: verified at the rebase (below).

**WP10 on WP8 (rebase onto `25a841d9`, 2026-11-19, poppler 26.08.0).** The two features together:
- **Order of the stages:** the model stage runs first, inside the text side (before the text's
  slot, as WP8 built it), so its `figure` blocks are in the page's reading order before
  `pictures::pdf::place` runs; `place` waits for both sides (`try_join`), so which side finishes
  first changes nothing. A model picture region over raster images takes them (`order::merge`):
  one figure, poppler's box for the crop, the region's place, `source` `onnx`, its inside text
  kept. A region with no image is a picture of its own, cut from the render by its box. An image
  the model did not mark gets its figure as without the model.
- **Left out alike:** a model figure that took no image is left out when it is alike a left-out
  image (overlap of half the larger box: the logo repeated on the pages, a watermark; counted
  once, as the image) or smaller than `picture_min_area` (counted small), as poppler's would be
  (`order::leave_out`). Without that the report's header logo came back on every page with the
  model on. Its inside text leaves with it.
- **Captions:** a caption the model marked right after (else before) a figure, within
  `CAPTION_MAX_GAP_PT` and overlapping it sideways, is taken first, once (a figure never takes
  one another figure took), unless it names a table (`caption::names_table`); the words rule
  applies to paragraphs only after that. The vector-caption note stays keyed on paragraphs: a
  caption the model marked is on a page it read.
- **`picture_text`:** unchanged and checked in placeholder, embedded, referenced, `json` and the
  docling route's `md_content` (equal to the native markdown).
- **Page size, one record:** `Part.size` is `pdftotext`'s exact size, turned when pdftohtml's
  displayed page (the font run, which WP8's regions are placed by) is the other way round
  (`pdf_layout::displayed_size`; pdftohtml gives whole points, so its own numbers would have moved
  every page's `width_pt` by up to a point). `place` clamps against it; the picture run's page only
  for a part without one. Effect on `json`: a turned page's `width_pt`/`height_pt` are as displayed,
  like its boxes (WP8's "not yet" for WP3's own size).
- **Clock lanes:** `pool::clock` counted wrong once two steps of a file ran at once (one waiting
  for a slot stopped the count of the other's work). Steps beside each other now run in lanes
  (`clock::beside`): the clock counts while any lane works. The text side, the picture side and
  each picture run are lanes. No step holds a slot while it waits for another; the model stage
  holds none while it waits for the gate.
- Budget: one API (`Spend` over `OutputBudget`); WP8's region tables are charged through
  `doc_output`'s grid reservation like WP3's.
- Lab: `images_scale` is a row of the Documents lab's Pictures group and sent on both routes.

Measured on the WP0a corpus (28 PDFs, dev instance, debug build): model off J 0.8569, line ratio
0.2698, headings 167/159, tables 39/31, cells 1980/1322 (`main`'s); model on 0.9011, 0.5056,
160/159, 28/31, 1468/1322 (`main`'s model-on row). The report PDF with the model off answers
byte for byte as before the rebase in all three modes (time aside). With the model on: 2 figures,
2 captions once each, crops in `referenced`; `2203.01017v2.pdf` 15 figures, all `onnx`, 11 with
their marked caption, none twice, 6 with inside text in `json` only.

Tests: `file/tests/pictures/model.rs` (a recorded answer through the whole conversion,
`layout_model::RECORDED`; turned pages 90/270 with regions and pictures in one space),
`file/tests/pictures/deadline.rs` (`document_timeout` with a picture run in flight: failed by
name, child killed, slots back), the clock's lanes, captions and left-out regions in
`pictures/pdf/tests.rs`.

Decisions: model figures follow `picture_min_area` and the repeat
rule as poppler's pictures do (the WP10 decision that docling applies its area to descriptions only
stands for both); on a page the gate skipped, a small region over a small image is counted
beside the image (the gate has no positions). Not yet: a model caption further than 36 pt from
its figure, or not next to it in reading order, is not linked (on `2203.01017v2` the
captions of figures 1, 2, 5, 13, 14 and 16); a region's own box is not used for the crop when it took images (vector
parts of a mixed figure outside poppler's box are not in the crop).

### WP4

**Built (2026-11-19, tesseract 5.5.4, poppler 26.08.0).** As the first pass built it; the review
round below moved every third-party parser out of the gateway and changed what is marked *(review
round)* here.
- Text recognition, `extract/convert/ocr.rs` (+ `pages`, `hocr`, `blocks`, `osd`, `langs`,
  `image`). One hook in `file/pdf.rs` after the file's own slot is given back (`pages::pdf_pages`
  on the pages `ocr` picks), one handler for image files (`image.tesseract` since the review round:
  PNG, JPEG, GIF, WebP, TIFF; the sniffer for conversions now knows TIFF). A page is one pool step:
  slot taken, `pdftoppm -gray -r 300 -singlefile` (PGM on stdout) or the image's own bytes (a TIFF
  page decoded to grey by the `tiff` crate, in lmgw's reader child since the review round),
  `tesseract stdin stdout --psm 0` for orientation when `osd` is installed (the page turned in the
  gateway, PGM in, PGM out), `tesseract stdin stdout -l <langs> -c tessedit_create_hocr=1` with
  `OMP_THREAD_LIMIT=1`, the hOCR parsed and laid out, slot given back. A page's pixels live in the
  gateway while its slot is held; nothing is written to `/tmp` (a file with several pages to read
  is spooled once under the data dir since the review round). A file's pages run through
  `buffered(pool size)` in page order.
- hOCR → blocks: the lines are written as `pdftotext -bbox-layout` XML in points and go through
  `pdf_layout::layout` unchanged (headings by size, columns, tables), then every block and the part
  are marked `ocr`. A line is split at a word gap wider than `CELL_GAP_LINES` (2) line heights, as
  poppler splits a table row into cells.
- `ocr` off/auto/force, `ocr_languages` and `stats.parts_by_source.ocr` are real; `options.rs`
  lost their `not_yet` notes, so the docling adapter's `force_ocr` and `ocr_lang` are `honoured`
  through the native option with no change to its table.
- Pure-Rust readers (in-process in the first pass; in the `lmgw-doc-reader` child since the review
  round, where `crates/lmgw-doc-reader/src/` holds them): HTML (htmd 0.6 on html5ever), EPUB (zip +
  quick-xml + the HTML path), e-mail (mail-parser 1.0.1, `full_encoding`) with attachments read
  through `file::read_at` (the table, recursively), `.xls` (calamine 0.38, BIFF5/8; the first
  pass's `xls/guard.rs` is gone); glue in `file/more.rs`. Sniffing in `sniff/more.rs`
  (`sniff_convert`; Chat and knowledge bases keep `sniff`). Caps rows `xls.calamine`,
  `html.native`, `epub.native`, `email.native`, `ocr.tesseract` (scanned PDF pages) and
  `image.tesseract` (pictures), and the page groups; the `NO_READER` rows for scans and pictures
  are gone.
- Packaging (D2): `tesseract-osd` (10.1 MiB, Apache-2.0) in Recommends (`tauri.conf.json`,
  `ci/rpm-allowlist.toml`) and in the server image. Settings › Documents lists "Page orientation for
  text recognition" under what is missing when tesseract is found without `osd`.
- New warning codes `ocr_low_confidence`, `ocr_page_reduced`; `scripts/convert-measure.py` takes
  `--lmgw-field NAME=VALUE`.

**Decisions:**
- `OCR_DPI` 300, `MAX_OCR_PIXELS` 100 million, `TESSERACT_TIMEOUT` 120 s a page,
  `TESSERACT_MEMORY_LIMIT_BYTES` 2 GiB, `HOCR_OUTPUT_BYTES` 64 MiB, `OCR_LOW_CONFIDENCE` 60,
  `OSD_MIN_CONFIDENCE` 1.0, `MAX_HTML_DEPTH` 256, `MAX_ATTACHMENT_DEPTH` 3; each with its measured
  reason in its doc comment. A PDF page past `MAX_OCR_PIXELS` at 300 DPI is rendered at the DPI
  that fits (`ocr_page_reduced`); an image or TIFF page past it is refused from its header.
- Without `osd` a turned page is not tried in other orientations (that would cost up to four reads
  a page); it gets `ocr_low_confidence`, which names `tesseract-osd`. *(Review round: a page whose
  lines read as vertical is turned and read again; only the upside-down page is left to the
  warning.)*
- The default languages: the installed packs of `LC_ALL`/`LC_MESSAGES`/`LANG` and `eng`, else every
  installed pack. The stored `documents.ocr_languages` setting (§4.4) is not built yet. A request
  whose languages are all missing reads no page (`ocr_language_missing` on a PDF, `tool_missing`
  naming the packages on a picture). docling's `mul` keeps WP5's mapping (the native default).
- `ocr: force` keeps a page's text layer where its recognition failed; `ocr: off` on a picture is
  `no_reader` that names the option.
- Each recognised page is laid out on its own: heading levels rank within the page, and repeated
  headers across recognised pages are not looked for.
- E-mail header fields in the output: Subject (heading), From, To, Cc, Date. Received,
  Return-Path, Delivered-To, Message-ID, DKIM, Bcc and X- fields stay out (privacy: they name
  servers and addresses a reader of the message does not see). Inline pictures the body shows are
  not attachments. Attachments recurse through the table (§4.2); an attachment's parts carry its
  name, its warnings are prefixed with it.
- HTML: `head`, scripts, styles, `noscript`, frames, objects, media, canvas, SVG and MathML are
  skipped; every `img`/`picture` is `<!-- image -->` (docling's placeholder), a `figure` block in
  json. Links keep their address as text. `dom_smoothie` (readability) is not used: the corpus'
  pages are documents, where main-content extraction would drop content; a "web page" mode is not
  yet. A text file that starts with another tag (`<h1>`) is HTML when its name ends `.html`,
  `.htm` or `.xhtml` (the sniffer only takes `<!doctype html`, `<html`, `<head`, `<body>`, so a
  README starting with `<p align>` stays text).
- Deep HTML: html5ever's tree builder takes time growing with the nesting depth (a page of
  100,000 nested `div`s ran past 5 minutes in a debug build), and htmd writes recursively (a 2 MiB
  stack held 500 nested `div`s, not 750, in a debug build). The first pass estimated the nesting
  from the text before parsing (`html::estimated_depth`); the review showed five ways past it, and
  the review round dropped it: the child's timeout bounds the parse, the exact depth is counted
  without recursion on the parsed tree, and past `MAX_HTML_DEPTH` (256) the page is read flat by
  `quickdoc-core`'s reducer (in the gateway: lmgw's own linear code) with a `part_failed` note.
- `.xls` reads like the xlsx reader writes (column letters, row numbers). calamine allocates
  dense grids from the file's claims when the workbook is opened. The first pass walked the
  Workbook stream first (`xls/guard.rs`) and refused by `MAX_UNCOMPRESSED_BYTES` what calamine
  would reserve; the review showed it both passable (calamine reads the compound file through its
  own parser, not the one the guard read through) and refusing ordinary workbooks (finding 10),
  and the review round dropped it for the child's memory limit. `.xlsb` is not read yet.
- Outlook `.msg` is a named `no_reader` (`SniffError::OutlookMessage`), not "legacy Office".
- `osd` stays out of the probe's language list (WP1's rule) and is named in the Languages extra's
  `absent` when missing, which is what `ocr::langs::osd_installed` and the Settings row read.

**Measured (2026-11-19, dev instance, debug build, tesseract 5.5.4 with `eng`+`deu` and
`osd.traineddata` from tessdata_fast 4.1.0 through `TESSDATA_PREFIX`, the package `tesseract-osd`
installs the same file; under other builds' load):**

| format | before (§14 WP0a, lmgw tier 0) | after WP4 | s/file med. | s/page | 3-gram J | line ratio | headings | tables | cells |
|---|---|---|---|---|---|---|---|---|---|
| ocr (10 scans, 5 with ref) | 0/10, `no_reader` 10 | 10/10 | 1.282 | 1.015 | 0.786 | 0.400 | 0/0 | 0/0 | 0/0 |
| tiff (1 file, 2 p.) | 0/1, `unsupported` | 1/1 | 1.106 | 0.553 | 0.356 | 0.041 | 22/8 | - | - |
| webp (1) | `no_reader` | 1/1 | 0.594 | 0.594 | 1.0 | 1.0 | - | - | - |
| html (33, 32 with ref) | 32/33, J 0.682, line 0.464, headings 0/68, tables 0/24 | 33/33 | 0.014 | - | 0.940 | 0.705 | 68/68 | 14/24 | 217/276 |
| epub (1) | `unsupported` | 1/1 | 0.035 | - | 0.844 | 0.645 | 19/19 | - | - |
| email (2 eml, 2 msg) | `no_reader` 2 (+2 msg) | 2/4 (the 2 `.msg`: `no_reader`) | 0.015 | - | no ref | | | | |
| xls (1) | `no_reader` | 1/1 | 0.015 | - | 0.558 | 0.0 | 1/0 | 1/2 | 27/15 |
| pdf (28, 115 p.) with `ocr: force` | WP0a tesseract alone on 300 DPI renders: J 0.637, line 0.087, 2.28 s/page one thread | 28/28 | 2.24 | 0.661 wall (pages side by side over the pool) | 0.623 | 0.214 | 388/159 | 38/31 | 1773/1322 |

On the scans tesseract alone measured 0.476 in WP0a; the difference is the orientation step (the
90° and 180° scans read as the upright one). The xls reference is docling's one table per sheet
without the column letters and row numbers lmgw's sheet tables carry, which costs the line ratio.

```sh
LMGW_CONTAINER_PREFIX=lmgw-wp4 TESSDATA_PREFIX=<dir with eng, deu, osd .traineddata> scripts/dev-instance.sh 127.0.0.1:8894
LMGW_KEY=<owner token> python3 -I scripts/convert-measure.py --corpus "$CORPUS" --lmgw http://127.0.0.1:8894 \
  --formats ocr,tiff,webp,html,epub,email,xls --per-file --out measure-wp4.json
LMGW_KEY=<owner token> python3 -I scripts/convert-measure.py --corpus "$CORPUS" --lmgw http://127.0.0.1:8894 \
  --lmgw-field ocr=force --formats pdf --out measure-wp4-force.json
```

**Live check (2026-11-19, dev instance on 127.0.0.1:8894).** A two-page image-only PDF (LibreOffice
text rendered at 200 DPI and wrapped by ImageMagick) 200, both pages `ocr`, `tools` poppler and
tesseract; a PNG 200; a two-page CCITT G4 TIFF 200, 2 pages `ocr`; a German page with
`ocr_languages=deu` 200 with its umlauts (read as `eng` by default on an `en_US` desktop, they
were lost); `deu,fra` 200 with `ocr_language_missing` naming `sudo dnf install
tesseract-langpack-fra`; `fra` alone 415 `tool_missing` naming it; the corpus' 90° and 180° scans
without `osd`: 200 with `ocr_low_confidence` naming `tesseract-osd`, with `osd`: the upright text;
an English PNG turned 90°: without `osd` tesseract read it with high confidence but its paragraphs
bottom-up (no warning), with `osd` in order; `ocr` off/auto/force on a text PDF: text layer, text
layer, `ocr` 1 page; the corpus EPUB, both `.eml` files (an attached PDF read through poppler, an
attached `.txt`), the corpus `.xls` and one LibreOffice wrote, and an HTML page with scripts, an
iframe and a remote image (none of them in the output, the picture a placeholder, a `figure` in
json).

**Not yet:** the stored `documents.ocr_languages` setting; repeated headers and cross-page heading
levels on recognised pages; heading detection on recognised pages over-counts (line boxes vary more
than font sizes: 388 headings against 159 on the forced PDFs); `pages` on a multi-page TIFF (it is
read whole, with `pages_not_applicable`); trying other orientations without `osd`; a page tesseract
reads as vertical text with high confidence is not flagged; `.xlsb`; `.msg`; EPUB pictures and
encrypted (DRM) books (an encrypted chapter reads as noise); HTML readability (`dom_smoothie`); HTML
`rowspan`/`colspan` tables (htmd writes them as text: tables 14/24 on the corpus); mbox files;
picture export of any of them (WP10).

**Unverified:** `tesseract-osd` from the RPM's Recommends and in the server image (not built here;
OSD was checked with the same 4.1.0 file through `TESSDATA_PREFIX`); a real 500-page scan (the
page-by-page path was checked with the pool and with the 115 forced pages); tesseract's own
behaviour under `TESSERACT_MEMORY_LIMIT_BYTES` on a `MAX_OCR_PIXELS` page; Windows (no tesseract run
there: `run.rs` refuses tools without the job object's memory limit); the CJK charsets of
`full_encoding` beyond the unit tests' Latin-1.

**Security review round (2026-11-19).** The review found seven blockers, five should-fix and six
nits: a small hostile upload could hang a pool slot for good, exhaust memory (Rust ends the whole
process on a failed allocation) or stall the async runtime. Almost all sat in parsers that ran
inside the gateway (htmd/html5ever, calamine, the EPUB and e-mail glue, the `image`/`tiff`
decoders). The reviewer's point: these parsers cannot be stopped once they run on a thread,
`catch_unwind` does not catch a failed allocation, and a guard per parser is one hole behind the
next file. This round closes the class instead.

*The structural fix: `lmgw-doc-reader`, a child.* Every reader that feeds an untrusted file to a third-party parser now runs in a child
process through `run.rs`, as poppler does: HTML (htmd/html5ever), EPUB (zip + quick-xml + the HTML
path), e-mail (mail-parser, with the message's own HTML body), `.xls` (calamine), and every image
decode for text recognition (`image`, `tiff`: a picture's size, a picture decoded for a turn, a
TIFF's page list and each TIFF page). The gateway sees a bounded frame on a pipe and nothing
else: markdown, cells, a page's grey pixels (a PGM), a message's fields and its attachments'
bytes.

- *Launch: a dedicated executable, not a re-exec of the running one.* `crates/lmgw-doc-reader` is
  a workspace crate with a library (the wire types and the charset decoding: `serde`, `serde_json`,
  `encoding_rs`) and the executable (the parsers). lmgw-core depends on the library only, and
  htmd, mail-parser, calamine, `image` and `tiff` left lmgw-core's manifest, so nothing in the
  gateway can call a parser by mistake, now or after a refactor: it does not link them. Weighed
  against re-executing the gateway's own binary with a hidden first argument (what §5.1 sketches
  for pdfium as `lmgw --pdf-worker`):
  - for the dedicated executable: the tests, a dev instance and production all start the same
    program through the same lookup (a test binary cannot re-exec itself as a reader, so a re-exec
    design needs a second, test-only launch path and leaves the three real `main`s untested by
    the suite); it starts in 1 to 2 ms (the desktop binary maps 161 shared objects first, 195 MiB
    of file); its address-space limit is spent on the file, not on GTK and WebKit mappings; no
    GUI library, no async runtime and no network client is in the process that reads the hostile
    bytes;
  - for the re-exec: nothing to package, and the reader can never be another version than the
    gateway. Both are answered here: the packaging is done (below), a reader that is not found is
    a named `tool_missing`, and the frame's magic line carries the wire version, so a reader left
    from another lmgw (an update under a running gateway) is refused by name ("restart lmgw").
- *Found like a tool, shown like a tool.* `ToolId::Reader` is a row of the capability table
  (`caps.rs`, label "lmgw's document reader", `bundled`), probed with `--version` and shown on
  Settings › Documents with its version and path; `xls.calamine`, `html.native`, `epub.native`
  and `email.native` need it, and pictures need it beside tesseract (`image.tesseract`; scanned
  PDF pages stay `ocr.tesseract`, which is why the page's one "scans" row became two). It is
  looked up only where lmgw installs it (`reader::own_dirs`: beside the executable, in
  `../libexec/lmgw`, and one directory up from a cargo `deps` or `examples` directory), never on
  `PATH`. Missing, its formats fail with `tool_missing` naming it and the page says "install lmgw
  again" (`Install::Bundled`); there is no in-process fallback.
- *Packaging.* RPM: `/usr/libexec/lmgw/lmgw-doc-reader` (`tauri.conf.json`, built by
  `ci/build.sh` before the bundle, in `ci/rpm-allowlist.toml` with a `built` pin like the root
  helper's, `ci/rpm_guard_test.py` extended). AppImage: the same path under its `usr/`. Server
  image: `/usr/local/bin/lmgw-doc-reader` beside `lmgw-server` (`ci/build.sh` server-only build,
  `ci/build-image.sh` context and an in-image `--version` check, the Containerfile's `COPY`, the
  packaging test). Dev instance: `scripts/dev-instance.sh` builds it before `cargo run`. Windows:
  see "Unverified/limits".
- *Protocol* (`lmgw_doc_reader::wire`). The request is the command line (`html`, `epub <budget>`,
  `email`, `xls`, `image-info`, `image-page <max pixels> [<directory offset>]`), the file is
  stdin, the answer is one frame on stdout: a magic line with the wire version, the header's
  length, the header (an `Answer` as JSON), then raw blobs (an e-mail's attachments, a page's PGM).
  A failure the reader can name travels in the frame with status 0. The wire form is the readers'
  own results as they were before (markdown text, a sheet's sparse cells, attachments, pixels):
  the block model `doc.rs` got no wire form, because none of these readers produced blocks; the
  gateway builds blocks from the markdown and the tables from the cells with its own code, as
  before. The reader gets no environment beyond what every tool gets, opens no file and no socket
  (its 84 crates hold no network client and no async runtime: `cargo tree`; the release
  executable is 3.5 MiB and links libc and libgcc), and nothing of its stderr is shown or logged
  (`Tool::quiet` in `run.rs`: a failure is its exit status alone; its panic hook prints a place,
  never a message).
- *It confines itself, so it starts without a fork.* For a foreign tool `run.rs` sets `RLIMIT_AS`
  and marks the inherited descriptors close-on-exec between fork and exec. A fork copies the
  gateway's page tables, and that was the child's real cost per file: a 2 KiB page took 7 ms of
  server time on a fresh dev instance and 52 ms once the gateway held 0.8 GiB (measured, after a
  large e-mail). The reader is lmgw's own program, so it is given the limit as its first
  argument (`--memory-limit <bytes>`, `Tool::limits_itself`) and, before it reads a byte of the
  file, sets the limit on itself and closes every descriptor beyond stdio (`confine` in its
  `main`; it exits without reading if either fails). With no hook between fork and exec, std
  starts it with `posix_spawn`. poppler and tesseract still start through a fork (not yet: a
  wrapper that sets their limit would spare it).
- *Limits and what they map to.* From `run.rs`'s existing named bounds:

  | bound | value | why | the file's error |
  |---|---|---|---|
  | `READER_TIMEOUT` | `PDF_TIMEOUT`, 300 s | the bound a file's tool already has; a 10 MiB page takes 0.49 s | `unreadable`, naming it |
  | `READER_MEMORY_LIMIT_BYTES` | `POPPLER_MEMORY_LIMIT_BYTES`, 4 GiB | what `PER_RUN_BYTES` already counts for a child, so the pool's size holds | `unreadable`: "ran out of its memory limit", naming it |
  | output | `MAX_UNCOMPRESSED_BYTES` (a page image: `MAX_OCR_PIXELS` + its header; a message: + its own size for the attachments) | the request's output bound | `output_too_large` |

  A reader that ends otherwise (a panic, a signal) is `unreadable`: "lmgw's reader stopped
  unexpectedly on this file" with its exit status. No new error code was needed.
- *A slot is held until its child is gone* (finding 4, for every tool). `pool::Slot::lease`: a run
  that is stopped (its timeout, its output bound, a dropped caller) kills the child and its group
  at once and waits for it on a task that holds a lease of the slot until the child has been
  reaped (`run.rs`, `Running`). The gateway's own work on a page (a quarter turn, the hOCR's
  read) runs on a blocking thread that owns the slot and hands it back (`pool::blocking_keep`).
- *Guards dropped as redundant or wrong:* the HTML depth estimate (five bypasses), the `.xls`
  grid guard (passable, and refusing ordinary workbooks). *Kept, because they give a better
  answer than a crash:* the exact depth count on the parsed tree (`MAX_HTML_DEPTH`, flat reading
  past it), the pixel bound from an image's header, the EPUB's budget on what is unpacked and
  kept. *Fixed in the readers as lmgw's own bugs:* htmd's table writer is replaced by lmgw's
  (`html/table.rs`: rows unpadded, each cell and caption walked once, linear in the page), the
  package document is streamed (finding 6).

*Per finding.*
1. htmd's table handler: lmgw's own table writer, in the child. The nested-caption page converts
   at once, the padded table is the size of its page.
2. html5ever: the estimate is gone; depth is counted on the tree; time and memory are the
   child's bounds (a page nested a million deep and the active-formatting amplifier each end as
   `unreadable` by a named bound).
3. `.xls`: calamine in the child, the guard gone. The shared-string bomb, the directory loop and
   the hidden `Workbook` stream each end by name; the gateway only receives cells.
4. Slots: above. A TIFF decode outlived by a 20 ms `document_timeout` keeps its slot until the
   reader has been reaped.
5. `ocr_languages`: judged when the request is planned (`langs::validated`: at most
   `MAX_OCR_LANGUAGES` (32), each a pack name by its shape, a repeat once; a 400 naming the rule
   and the count), resolved once per request (`Plan::ocr_chosen`), one warning for all missing
   codes. docling's `ocr_lang` is bounded before it is mapped (a 422 "too_long") and then goes
   through the same plan.
6. EPUB: the container, the encryption list and the package document are streamed against the
   budget, only `item`/`itemref` kept and charged, notes one per kind with a count.
7. Attachments: the child returns them as bytes; the gateway converts each through
   `file::read_at` (its own slot, its own child where needed) and charges the request's output
   budget as each is read (`OutputBudget::take_if_fits`); once it is spent the attachments not
   yet read are named and left, and the message keeps what fit. `MAX_ATTACHMENT_DEPTH` and
   mail-parser's 10,000 parts stay; no bound on the count was added, since the output budget is
   the real one.
8. What one file may cost: `document_timeout` is a native option now (seconds from the file's
   first slot; `deadline_exceeded`), with **no default** (below, Decisions). A TIFF's directories are
   walked once (`image-info` gives every page's directory offset, `image-page` goes straight to
   it). A file with several pages to recognise is spooled once (`ocr/spool.rs`: an unlinked file
   under `<data dir>/convert-spool`, each tool run its own open file description on it, opened
   through `/proc/self/fd`); `pdftoppm` is given `/dev/stdin` as its file and reads the page's
   objects by position, lmgw's reader reads its stdin by position. The spool was chosen over
   page batches because it makes each page's cost independent of the file's size with one write,
   and keeps one page one pool step.
9. The EPUB sniff unpacks at most `ZIP_MIMETYPE_BYTES` (64) of `mimetype`, and sniffing runs in
   a pool slot.
10. The grid guard is gone; a workbook of 2,000 string formulas reads.
11. Sideways pages without `osd`: `hocr::Page::sideways` (most lines of two or more words taller
    than wide, at least `SIDEWAYS_MIN_LINES`), then a quarter turn and a second read: 270° first
    when tesseract was sure of the vertical lines (it reads them top to bottom, which is right
    for a page lying on its right side: mean confidence 96 there, noise for one lying on its
    left), else 90°, and the other way if that reads vertical or unsure. The turn is an
    `ocr_page_turned` warning (new code), an unresolved page `ocr_low_confidence`. The Settings note and `docs/documents.md` say what is true (the
    upside-down page is the one left to the warning). `x_size` is the line's size for the layout
    (`blocks::sized`): headings with `ocr: force` on the corpus PDFs went from 388 to 332
    against the reference's 159 (and from 22 to 1 against 8 on the corpus TIFF). 2.1 times the
    reference is still far past the 1.3 set as the bar for keeping them, so size-ranked
    headings are off on recognised pages for now (`blocks::part` writes them as paragraphs:
    0/159) and are a "not yet".
12. The turn path is tested for real with a stand-in for the detection's answer (a script in
    tesseract's place that answers `--psm 0` and hands every other run to the real tesseract):
    the suite neither ships nor fetches `osd.traineddata`. The tests that need tesseract or
    poppler still skip without them, as the poppler tests do; `ci/check.sh` now says before the
    suite which document tools it found, so a gate that skipped them says so.

*Nits.* `PageImage::file` is the one place a picture reaches `tesseract stdin` and refuses what
leptonica does not recognise (tesseract would read it as a list of file names). The Cargo.toml
comment is corrected (htmd's html5ever 0.40 is the reader's alone now). `META-INF/encryption.xml`:
encrypted chapters are named, not read. `hidden`, `display:none` and `visibility:hidden` on an
element skip it; text hidden by a stylesheet is documented as not told apart. The memory-per-pixel
figure is corrected (`ocr.rs`: one byte a pixel in the gateway, up to ten in the reader child).
`MAX_HTML_DEPTH` is tested at the bound per handler chain (div, block quote, list, table in a
cell, inline) on the reader's 64 MiB stack. Duplicate dependencies: calamine has no feature that
leaves its xlsx/ods readers out, so zip 9 and quick-xml 0.42 stay; they are linked into the reader
only now. The reader's EPUB code shares calamine's zip 9 and uses quick-xml 0.41 (0.42 changed its
event types), so the reader has two quick-xml versions until the gateway's readers move to 0.42.
New codes in `scripts/convert-compare.py`'s lists: warnings `ocr_low_confidence`,
`ocr_page_reduced`, `ocr_page_turned`, error `invalid_option`, tool `reader`.

*Decisions of the round.*
- The dedicated executable over the re-exec (above).
- The reader sets its own memory limit and closes its inherited descriptors at the top of its
  `main` instead of having them set between fork and exec, so that it starts without a fork
  (above). For lmgw's own program, before a byte of the file is read, the two are the same
  protection.
- Size-ranked headings are off on recognised pages (finding 11): a scan's title is a paragraph
  until headings can be told there within about 1.3 times the reference.
- `document_timeout` has no default. §9 says "there is no page cap and no file-count cap", and a
  default per-file deadline is one for a long scan; every step a file takes is bounded by a named
  timeout, the pool is fair across callers, and a caller that wants a bound per file has the
  option. What stays open: a caller without a key is a caller of its own per request, so several
  keyless requests of many-page scans share the slots evenly with everyone else; that costs time,
  not memory.
- Too-deep HTML is read flat only for an HTML file (by the gateway's reducer); a too-deep EPUB
  chapter or e-mail HTML body is named and not read.
- An attachment that no longer fits the output budget is left out without failing the message
  (`take_if_fits` does not count a refusal).
- `MAX_OCR_LANGUAGES` 32 and `MAX_OCR_LANGUAGE_CHARS` 32, `SIDEWAYS_MIN_LINES` 3,
  `ZIP_MIMETYPE_BYTES` 64, the reader's 64 MiB stack.
- The compound-file sniff (`cfb`, the directory only) stays in the gateway, now in a pool slot;
  the review found it sound.

*Cost (2026-11-19, htmd 0.6.0; other builds ran at times).*
- The reader by itself (`html::tests::the_childs_overhead` in the reader crate, an ignored test
  run by hand that converts one page in its own process and through the built executable;
  release build): start and exit alone (`--version`) 0.91 ms. A 2 KiB page: 0.10 ms in-process,
  1.43 ms as a child, so 1.3 ms of overhead (the start, stdin, the frame). A 10 MiB page of
  headings, lists and tables: 431 ms in-process, 490 ms as a child, so 59 ms (14 %: 10 MiB
  through stdin, its charset decoded, the markdown back as JSON). Debug build: 1.61 ms; 1.53 →
  3.82 ms; 6.25 → 7.03 s.
- In the gateway (dev instance, debug builds of gateway and reader, `stats.time_ms`, medians of
  40 requests): a text file (no child) 0 ms; the 2 KiB page 3 ms; the corpus `.xls` 2 ms; the
  corpus EPUB 17 ms; the 10 MiB page 5.7 s. Client wall clock: 13.4, 16.2, 15.0 and 31.8 ms.
  With the fork the first build of this round still had, the same 2 KiB page was 7 ms on a fresh
  instance and 52 ms at 0.8 GiB of gateway memory; `strace` shows the reader started with
  `clone3(CLONE_VM|CLONE_VFORK)` now and poppler with a plain `clone`.
- Corpus, seconds per file (median, client wall clock) before → after: html 0.014 → 0.015, epub
  0.035 → 0.035, xls 0.015 → 0.016, email 0.015 → 0.017; server time: html 0.000 → 0.002, xls
  0.001 → 0.003, epub 0.017 → 0.017.

*Corpus, before and after the round* (dev instance, debug build, tesseract 5.5.4 with `eng`,
`deu` and `osd` through `TESSDATA_PREFIX`, as the first pass measured; "before" is its table
above):

| format | | conv. | s/file med. | s/page | 3-gram J | line ratio | headings | tables | cells |
|---|---|---|---|---|---|---|---|---|---|
| html (33, 32 with ref) | before | 33/33 | 0.014 | - | 0.940 | 0.705 | 68/68 | 14/24 | 217/276 |
| | after | 33/33 | 0.015 | - | 0.942 | 0.849 | 68/68 | 23/24 | 266/276 |
| epub (1) | before | 1/1 | 0.035 | - | 0.844 | 0.645 | 19/19 | - | - |
| | after | 1/1 | 0.035 | - | 0.844 | 0.645 | 19/19 | - | - |
| email (2 eml, 2 msg) | before | 2/4 | 0.015 | - | no ref | | | | |
| | after | 2/4 (`.msg`: `no_reader`) | 0.017 | - | no ref | | | | |
| xls (1) | before | 1/1 | 0.015 | - | 0.558 | 0.0 | 1/0 | 1/2 | 27/15 |
| | after | 1/1 | 0.016 | - | 0.558 | 0.0 | 1/0 | 1/2 | 27/15 |
| xlsx (12; lmgw's own reader, not touched) | after | 12/12 | 0.015 | - | 0.625 | 0.0 | 24/22 | 23/50 | 1271/787 |
| ocr (10 scans, 5 with ref) | before | 10/10 | 1.282 | 1.015 | 0.786 | 0.400 | 0/0 | 0/0 | 0/0 |
| | after | 10/10 | 1.250 | 0.990 | 0.786 | 0.400 | 0/0 | 0/0 | 0/0 |
| | after, without `osd` | 10/10 | 0.949 | 0.848 | 0.673 | 0.400 | 0/0 | 0/0 | 0/0 |
| tiff (1 file, 2 p.) | before | 1/1 | 1.106 | 0.553 | 0.356 | 0.041 | 22/8 | - | - |
| | after | 1/1 | 1.077 | 0.538 | 0.356 | 0.060 | 0/8 | - | - |
| webp (1) | before | 1/1 | 0.594 | 0.594 | 1.0 | 1.0 | - | - | - |
| | after | 1/1 | 0.566 | 0.566 | 1.0 | 1.0 | - | - | - |
| pdf (28, 115 p.), `ocr: force` | before | 28/28 | 2.24 | 0.661 | 0.623 | 0.214 | 388/159 | 38/31 | 1773/1322 |
| | after, `x_size` only | 28/28 | 2.53 | 0.774 | 0.624 | 0.216 | 332/159 | 40/31 | 2226/1322 |
| | after | 28/28 | 1.972 | 0.601 | 0.624 | 0.216 | 0/159 | 40/31 | 2226/1322 |

HTML tables went from 14 to 23 of 24 with lmgw's table writer (htmd's wrote a table without
header cells, or inside a cell, as text). Without `osd`, the corpus' scan turned 90° reads as the
upright one now (3-gram Jaccard 1.0, was 0.023 for tesseract alone in WP0a), the one turned 270°
0.92, and the one turned 180° stays noise (0.03) with its `ocr_low_confidence` warning. The
"`x_size` only" row was measured while two other gates ran, which is its time.

*Live check of the round (2026-11-19, dev instance on 127.0.0.1:8894, debug build).* With `osd`
through `TESSDATA_PREFIX`: an HTML page 200 (`tools`: `reader 0.7.0`, server time 2 ms, its table
unpadded, its remote image a placeholder); the corpus EPUB 200 (17 ms); an `.eml` with an attached
PDF 200 (`reader` and `poppler`, the PDF under `## Attachment:`); an `.xls` 200 (2 ms); a two-page
scanned PDF 200 (2 pages `ocr`, 1.26 s); a PNG 200; the same PNG turned 90° 200 with
`ocr_page_turned` ("turned 270° clockwise … tesseract's orientation data said so"); a two-page
G4 TIFF 200 (2 pages, 0.91 s). The nested-caption page (2,982 bytes): 200, `content` "x", server
time 4 ms, 43 ms wall with curl. An `.eml` of 6.3 MiB with four `.docx` attachments of 291 MiB of
text each: 200 `partial`, three read, `part_failed` "1 attachment(s) were not read, because the
request's output was used up (… MAX_UNCOMPRESSED_BYTES (1.0 GiB) …): long-4.docx", server time
23 s (the gateway peaked at 2.4 GiB resident for it and was back at 0.2 GiB after).
`document_timeout=0`: 400 `invalid_option`; `document_timeout=0.001` on the scan: 504
`deadline_exceeded` naming it; `ocr_languages=eng,../../x`: 400 "1 of its 2 codes are not
tesseract language codes …"; 40 codes: 400 "… at most MAX_OCR_LANGUAGES (32) …". `doc_tools`: the
reader `found 0.7.0` with its path and the five rows it reads; Settings › Documents shows it
first under "Found on this computer" (screenshot at 125 %). Without `osd`: the PNG turned 90°
200, read in order, `ocr_page_turned` "turned 270° clockwise … the lines read as vertical …
(sudo dnf install tesseract-osd)"; turned 270°: the same with 90°; turned 180°: 200, noise, with
`ocr_low_confidence` naming an upside-down page and the package; the Settings row for
`tesseract-osd` says what happens without it.

*On WP12 and WP8, which landed during the round.* The branch was rebased onto both. `pool.rs`:
WP8's counted slots (`acquire_many`) and this round's leases are one type now (`Held` carries the
count; a lease keeps all of a `Slot`'s slots). `file/pdf.rs`: the layout model's stage and the
text-recognition hook both stand. Labs › Documents: its text-recognition note reads the pictures
row beside the scans row, and its field mapping sends `document_timeout` on both routes when a
setting carries one; the form has no row for it yet. The measurements above were taken before
that rebase; the gate ran after it.

*Not yet (added by the round).* A `document_timeout` row on Labs › Documents; headings on
recognised pages (by size they come out at twice the reference; a ranking across the pages of a
scan, or tesseract's own font sizes per block, is the next thing to try); a default for
`document_timeout`; trying 180° on an unsure page without `osd`; starting poppler and tesseract
without a fork; `rowspan`/`colspan` in HTML tables (lmgw's writer puts a spanning cell in its
first place); CSS-hidden text; the spool file off Linux (`/proc/self/fd`: elsewhere each page's
tool is sent the file); the reader on Windows; the pdfium worker of §5.1 as a request of this
executable.

*Unverified/limits.*
- **Windows.** `run.rs` runs no document tool on Windows yet (no job-object memory limit), and a
  tool that limits itself is refused the same way, so the reader does not run there: HTML, EPUB,
  e-mail, `.xls` and pictures are a named "not read on Windows yet" in the capability table
  (`Install::NotYet` on the reader's row), where the first pass read the first four in-process.
  That is the weaker side of this round on Windows, taken rather than leave a parser reachable
  in the gateway. The reader is a workspace member and has to compile there (not compiled here);
  it is not in the Windows installer yet. When the job-object limit exists: the reader into the
  NSIS bundle beside `lmgw.exe` (`own_dirs`' first place), its limit through the job object
  instead of `RLIMIT_AS`, and a kill of its descendants (it starts none).
- **Packages not built here.** The RPM (`cargo tauri build` with the new file), the AppImage and
  the server image were not built; `ci/rpm_guard_test.py` passes with a stand-in for the reader,
  and the packaging test checks the Containerfile against `ci/build-image.sh`. The standalone
  build `ci/build.sh` runs (`cargo build --release -p lmgw-doc-reader`) was run here: it needed
  chrono's `std` named in the reader's manifest, which the workspace build had hidden.
- **macOS and other Unix.** The spool file needs `/proc/self/fd` and is Linux-only; elsewhere each
  page's tool is sent the file, as before. The reader's descriptor closing off Linux walks the
  descriptors (not run).
- **A reader newer or older than the gateway** is refused by the frame's version line; an update
  under a running desktop app therefore fails these formats by name until lmgw is restarted (not
  exercised with a real update; the stand-in test covers the refusal).
- The hostile files run under a 4 s timeout and a 512 MiB limit in the tests, not under the
  300 s and 4 GiB of production: the mechanism is the same, the wait and the memory are not.
  tesseract's and poppler's own behaviour at their limits is as the first pass left it.
- The first turn tried for a sideways page without `osd` follows tesseract 5.5.4's behaviour
  (sure of a page lying on its right side, unsure of one on its left); another version that
  differs costs such a page one more read, not a wrong answer.

**WP4 on WP10 (2026-11-19, rebased onto `main` at 91161cf4, WP10 and WP10 on WP8).** The first
two WP4 commits were squashed into one before the rebase. Both sides' code, tests and text are
kept; what had to be decided to make them one:

- *Slots, leases and the clock.* WP4 had already merged the layout model's counted slots into
  the one `pool::Slot` with leases, and every child WP10 added (`pdfinfo` and `pdfimages -list`
  of the gate, the `-dataurls` picture runs, the crop renders) runs through `run.rs` on its
  step's slot, so it holds a lease until it has been reaped like every tool. `acquire_many` hands
  out one `Slot` whose leases hold all its slots. No path holds a slot while it waits for another
  (sniffing, the text runs, the gate, each picture run, each OCR page and each crop take theirs in
  turn). What did not fit: WP4's OCR pages ran side by side inside the text's lane of WP10's
  per-file clock, so a page waiting for its slot paused the lane while another page worked, and a
  `document_timeout` stopped counting. Each OCR page (a PDF's and an image file's) is now a lane
  of its own (`pool::clock::beside`) and the loop that drives them waits (`paused`) while they
  run, as WP10's picture runs do. Tested: a two-page scan whose first page works while the second
  waits ends at its 2 s `document_timeout`, not after the working page's 60 s; a scanned PDF with
  a picture (`ocr: auto`, `pictures: referenced`) dropped while tesseract reads a page ends
  every child (poppler's, tesseract's, the picture runs') and gives every slot back. The
  integration test of the one-slot PDF path now sees one waiting step first (sniffing has a slot
  of its own since WP4), where WP10 saw the text's and the gate's.
- *Scans and pictures.* A scanned page is recognised; its page-sized image stays out of the
  picture export (it is the page), counted in `pictures_left_out`, whose words now say the scan's
  text is read by text recognition (and `PAGE_COVER_SHARE`'s and the docs' "not built yet" are
  gone). The gate skips such pages as before, so a scan still has no picture search. A smaller
  picture on a scanned page is exported like any other and placed by its box among the
  recognised blocks (the picture placement runs after the OCR pages replaced their parts, in
  the same point space as the displayed page); the words tesseract reads inside it stay in the
  text (*decision*: text inside a picture is only kept out where the layout model marks the
  region). `ocr: off` on a scan is `no_reader` as on `main`.
- *Image files* stay text: asked for `embedded` or `referenced` pictures, the answer has a
  `not_yet` note (option `pictures`) and an empty `pictures` list. *EPUB*: `pictures::epub_not_yet()`
  where the book has picture placeholders. *Web pages and e-mail bodies*: their `<!-- image -->`
  placeholders are `figure` blocks without `picture`, which is WP10's shape for a figure without
  a picture; a `not_yet` note names them (data-URI pictures included: not read from the reader's
  output yet; remote ones are never fetched). The notes are not tied to the `pictures` mode,
  as WP10's spreadsheet note is not; `output: text` gets none.
- *Budget.* One API: WP4's `take_if_fits` is gone for WP10's `try_take`, and what an e-mail
  charged as it read its attachments (`Read::charged`) is held by the file's `budget::Spend`
  with its pictures' reservations, so it counts once and the rest is given back. Each block is
  charged with `BLOCK_ALLOWANCE` (160 bytes, json's fields) over its text (*decision*), so a message
  that keeps an attachment also has room to write it; a message whose own text does not fit
  charges nothing and fails at its output as any file does. An attachment's pictures (a PDF's
  crops, an office file's media) are exported as it is read, as the request asks, and join the
  message's `pictures` numbered on (`p1`, `p2`, … in the attachments' order); an attachment left
  for the budget gives its reservations back. Tested with two pictured PDFs under a budget that
  holds one: the second's picture stays a placeholder, named, and the message is converted inside
  the budget; three text attachments under a small budget: the third is named and left.
- *OCR pages and the layout model.* With the recorded model answer (`layout_model::RECORDED`)
  for both pages of a PDF with a text page and a scanned page (regions on the scan too), the text
  page is laid out from its regions, the scanned page is recognised and replaces its part, and
  json marks its blocks `ocr`. Regions for recognised pages stay not yet.
- *The crop cut moved into the reader (decision).* WP10 cut a page's
  pictures out of `pdftoppm`'s PNG in the gateway with the `png` crate. That PNG is poppler's
  output, but drawn from an untrusted PDF, and the review round's rule is that decoding of
  document-derived pixels does not run in the gateway. It is now one more reader operation,
  `lmgw-doc-reader crop <width> <height> <budget> <x,y,w,h>…` (wire version 2; at most
  `MAX_CROPS`, 256, the boxes `PICTURES_PER_PAGE_MAX` already bounds): stdin is the PNG, the
  answer the crops as PNG blobs. The declared size is checked against the box asked of poppler
  before decoding, the decoder is limited to that box's raster, crops past `budget` (what the
  request's output has left, `OutputBudget::left`) are left out, and the run has the reader's
  bounds (`READER_TIMEOUT`, `READER_MEMORY_LIMIT_BYTES`, output = budget + `CROP_ANSWER_BYTES`,
  1 MiB). The gateway reserves each crop it gets with `try_take` as before. It runs in the slot
  the render had. `png` left lmgw-core's dependencies (a dev-dependency for the tests that read
  crops); the reader links the same `png` 0.18 its `image` already used. Without the reader a
  page's pictures are placeholders named in `pictures_partial` (`tool_missing` words); with it,
  the answer's `tools` name the reader beside poppler. Picture answers of three corpus PDFs
  (`picture_classification`, `normal_4pages`, `amt_handbook_sample`) in the three modes, markdown
  and json, are byte-identical to `main`'s, crops included.
- *Lists.* `scripts/convert-compare.py` knows every warning and error code of the merged wire
  enums, WP5b's `source_refused`, `source_unreachable`, `source_timeout`, `source_too_large` and
  `source_http_status` among them, and the formats and tools; `convert_compare_test.py` passes.

**Measured (dev instance, debug build, tesseract 5.5.4 with `eng`, `deu`, `osd` through
`TESSDATA_PREFIX`):** pdf 28/28, 3-gram J 0.8569, line ratio 0.2698, headings 167/159, tables
39/31 (equal to `main`); ocr 10/10, J 0.786, 1.23 s/file; html 33/33, J 0.942, line ratio
0.849; epub 1/1, J 0.844. `python3 -I scripts/convert-measure.py --corpus "$CORPUS" --lmgw
http://127.0.0.1:8894 --formats pdf,ocr,html,epub --out measure.json`.

**Live check (dev instance on 127.0.0.1:8894).** A one-page scan: 200, `ocr` 1, 1.1 s,
`pictures_left_out` "1 covering their whole page … read by text recognition", in placeholder and
referenced mode alike; the scan with a noise photo on it, `referenced`: 200, text then
`![Image](artifacts/image_000000_…)`, the cover counted; an `.eml` with the corpus'
`picture_classification.pdf` attached, `embedded`: 200, two `data:image/png` pictures `p1`
(page 1, 688×426) and `p2` (page 2, 345×459), its HTML body's remote image a placeholder with the
e-mail-body note; an HTML page, `referenced`: 200 with the web-page note; the corpus EPUB: 200
with `epub_not_yet`; an `.eml` with a PDF, an `.xls`, a PNG (`referenced`: the image-file note),
a two-page TIFF: 200 each; the nested-caption page: 200, `x`, 4 ms, the gateway up after it.

**Not yet:** an image file exported as a picture of itself; EPUB, web-page and e-mail-body
pictures (data-URI ones included); regions for recognised pages; keeping words tesseract reads
inside a picture on a scanned page out of the text.

**Unverified:** the Windows run of the crop path (the reader and poppler are not run there yet,
so a PDF's pictures stay placeholders there as WP10's crops did not run either); a crop answer
near `READER_MEMORY_LIMIT_BYTES` (a page raster near `PAGE_RASTER_MAX_BYTES` decoded with its
PNG and its crops in one 4 GiB process).

### WP6

**Built (2026-11-19).**
- `extract/convert/model_help.rs` and its children: the page-reading model as a request sees it
  (`Reader`, built by the HTTP halves; `plan` judges `model_help` into `Plan::model_help`),
  `select` (which pages), `prompts`, `session` (the route a request opened and its model-call
  pool), `budget` (`max_tokens`), `call` (one page's call), `page` (one page: policy, route,
  turn, render, call), `render` (the page image within `PAGE_MAX_PIXELS`), `deadline` (the file's
  clock in the stage), `answer` (the answer as blocks), `pdf` (the stage `file/pdf.rs` calls
  after the layout), `settings` (the save's check, the page's facts, the "scans" capability row).
- `extract/convert/doctags.rs` with `doctags/vocab.rs` and `doctags/table.rs`: the DocTags
  reader, vocabulary checked against docling-core 2.104.0 (`types/doc/tokens.py`,
  `transforms/serializer/doctags.py`, read 2026-11-19).
- Settings: `documents.page_reader` (an lmgw model name, empty = off) and
  `documents.page_reader_doctags`, saved through `settings_set` (`documents_page_reader`,
  `documents_page_reader_doctags`); Settings › Documents › Model help › "Read pages with a model"
  (`documents/model.rs`): the shared model picker over chat models that take pictures, where
  pages go (this computer, or a named service) and who answers whenever the model cannot run
  (D14), and in expert view "Answers in DocTags" with the `--special` hint. `doc_tools` answers
  it as `page_reader`. Both settings are the dashboard's to change, not `lmgw__settings_set`'s.
- Wire: `stats.models` (`documents/model_help.rs`: the answering model, `answered_for` when a
  fallback answered, calls, pages read, the bound in flight and where it comes from), the warning
  codes `model_answer_malformed`, `model_cannot_see` and `page_scaled`, the error codes
  `model_held`, `model_not_allowed` and `model_failed`, `model_help: missing`,
  `DocCaps.page_reader`.
- The docling adapter: `do_formula_enrichment`, `do_code_enrichment`, `code_formula_preset` and
  `code_formula_custom_config` are `mapped` (D23); `pipeline: vlm` and `table_mode: accurate`
  already set the native `model_help` (WP5) and now reach a model.

**How a page is read.** After the two poppler runs and the layout (the file's tool slot given
back), each picked page asks for its turn in the request's model-call pool, is rendered
(`pdftoppm -r 150`, or `-scale-to` above `PAGE_MAX_PIXELS`; a tool slot for the render only) and
sent as one user message: the picture and the prompt. The call is `gate::open` (once while files
have pages for the model) and `proxy::sample_once_dropping` per page, the path `/v1/responses`
and the knowledge bases use: the hold's swap along the fallback chain,
candidate aliases, GPU admission, the gate's per-send fit, a ladder's climb, the dead-container
retry, and one `request_logs` row per call (`ingress_proto` `documents-page`, the caller's key,
token counts, no content). The answer's blocks replace the page's, source `vision`.

**The pool rule as built (D20).** Per request (all its files share it), per opened route:
- bound = the answering model's slots. A local llama.cpp model: the slots its container was
  started with (`LlamaParams::effective_slots`: the row's `-np`, or llama-server's own 4 when the
  row leaves it to the server), `bound_from: row_slots`. Any other route (a cloud alias, the hold's
  fallback, an llama-server lmgw does not run): the tool pool's size, `bound_from: tool_pool`, as
  no upstream has a concurrency setting.
- a page takes one of the bound's turns *before* its image is rendered and gives it back when its
  answer is read, so at most `bound` page images exist at once; a waiting page holds neither an
  image nor a tool slot.
- turns are handed out first come first served (a tokio semaphore): the request's files in
  request order, a file's pages in page order. Across requests and Chat the calls queue at the
  model, like any model call.
- the route (for a local model its claim on the container) is opened by the first page that needs
  it and held only while a file has pages waiting or in flight: the request keeps a `Weak` to it,
  each file's stage holds it (`session::Keep`), and when no stage does the claim is given back,
  so the next file with pages opens again (review S9; the first build kept it until the request
  ended). A hold switched on or off makes the next page open again (below), with that route's own
  bound.
- dropping the conversion (client gone, a cancelled job) drops the calls in flight and every
  waiting page; each dropped call closes its upstream connection, leaves the in-flight gauge
  closed and writes a `499` `documents-page` row (review S8). `document_timeout` ends the stage,
  not the file (review S7, below).

**Decisions:**
- **The DocTags reader is built** (decided from the WP0a numbers; whether
  granite-docling performs best stays open, D15). Nothing in the UI names or suggests a
  model.
- **DocTags is told two ways.** By form: an answer that starts with the `<doctag>` root is read as
  DocTags whatever the setting says (markdown does not start with it). By setting ("Answers in
  DocTags"): the prompt has to be chosen before an answer exists (docling's "Convert this page to
  docling." against lmgw's page prompts), and an answer without any tag from such a model must be
  told from markdown to say what is missing (`--special`). Detection by form alone can do neither.
- **`--special` is the row's own flag**: a local model's "Extra llama-server flags"
  (`local_models.args`) already carry llama-server flags. The Settings row says so when the chosen model is a local row
  without it (read from the row, not guessed from a name); a conversion's
  `model_answer_malformed` warning says so when a DocTags model's answer came back without tags.
- **A DocTags page whose every word is a page header or footer** (WP0a saw granite do it on
  rotated scans) would come out empty in markdown, which leaves those out: its text is kept as
  paragraphs, with a `model_answer_malformed` note. Beside other text, headers and footers stay
  what they are (in json only, as for the text layer's).
- **Malformed DocTags** (an element left open, text outside any element, a box of fewer than four
  locations, no closed root) keep their text and get `model_answer_malformed` naming what was
  wrong; a `<` that opens no tag of the vocabulary stays text.
- **A model's control tokens** (`<|end_of_text|>`) in a DocTags answer are dropped: a
  llama-server started with `--special` writes them out beside the tags (seen in the live check).
- **A DocTags answer without tags** (no `--special`) still has its `<loc_N>` tokens, which are
  ordinary tokens: each run of text between them is kept as a paragraph, without the tokens, and
  the page's `model_answer_malformed` note names `--special`.
- **`model_help` absent** is `missing_or_tables` once a model is chosen (§8's row text); the
  setting is the model, not a mode.
- **"Table page"** is a page whose layout holds a table block (`pdf_layout`'s table rule, which
  uses `vision_prompts`' thresholds on geometry), not `vision_prompts::table_like` on `-layout`
  text, which the conversion no longer has.
- **The prompts** are the knowledge bases' (`vision_prompts.rs`) with their fidelity sentences
  kept and the output form changed: markdown with headings, list items and markdown tables
  instead of plain text with one line per table cell (that form serves retrieval chunks; a
  converted document wants a table block), plus D23's formulas as LaTeX between `$$` lines and
  code in fenced blocks. Not measured yet (below). `PAGE_PROMPT_VERSION` "1".
- **Temperature 0** on every page call (a transcription; what WP0a measured granite with). The
  row's or alias's own defaults apply to everything else.
- **`max_tokens`**: for a local model whose container the request holds, the slot's context
  (`/props` `default_generation_settings.n_ctx`, else the model list's) less the prompt counted on
  the running server (`/apply-template` + `/tokenize`, as the gate counts) and the projector's
  per-image bound; where lmgw has no bound for the projector (`image_token_bound` refuses), less
  the counted text alone, and the server ends an answer at its context. Any other model: its
  largest answer from its catalog; an llama-server lmgw does not run (an upstream of that kind):
  its context; anything else: none is sent (review S10: a context alone is no bound on a cloud or
  generic upstream, where prompt plus `max_tokens` passes it by construction and the request is
  refused). §9 says "the alias's real
  context minus the image's tokens": a cloud model's image tokens and tokenizer are not lmgw's to
  count, so its own maximum stands in. An answer that ends with `finish_reason: length` is a
  failed page whatever the bound; the warning names the bound and its source.
- **A failed page** is `part_failed` naming the page (a runaway, an empty or filtered answer, a
  render or upstream failure); a page with text keeps the tools' text, a page without stays
  unread, and the file is `partial`. A file none of whose pages was read fails with the model's
  own code where the model is why (review S6): `model_held` (503), `model_not_allowed` (403),
  `model_failed` (502), or `deadline_exceeded` (504) when the caller's `document_timeout` cut
  it; its message is the stage's reasons, and its `stats.model_calls`/`stats.models` keep the
  calls that were made. A render that failed (no `pdftoppm`) keeps the file's `no_reader`.
- **The key's policy**: the scope is checked before anything is opened (a key that may not use
  the model never loads it). Before each call, and already before the route is opened (review
  nit 2: an over-budget key no longer cold-loads the model), the key is read again by its id (a
  job outlives its request) and asked for its expiry, its scope, its budget and the gateway's,
  and its tokens-per-minute window (`PolicyGate::check_call`, review S2); a refusal is
  `model_not_allowed` for the pages left. The calls are not counted against the key's
  requests-per-minute window, and that window is not asked: the conversion is the request the
  key made. A route that prices nothing a budgeted key is billed in is refused once, where the
  route is opened (review nit 1). Refused pages write no row of their own (the conversion's
  answer names them). A page refused by the token window is not held back until the window
  rolls: it is `model_not_allowed` at once (fewer pages sent; this round's decision).
- **The hold mid-request**: every page asks "does the hold stand as it stood when the route was
  opened?" before its turn and again after waiting for it; a change opens again, so pages not yet
  sent take the held path (fallback, or `model_held`) and after a release go to the model again.
- **`stats.models`** names the model that answered (the fallback's name as `x-lmgw-fallback`
  would give it, with `answered_for`), which §7.5 puts in "`tools`/`stats.model_calls`"; `tools`
  stays the programs.
- **The call's deadline** is `responses_timeout_seconds`, the visible bound the knowledge bases'
  page reads use; the file's own deadline ends a call by dropping it.
- **Slots from the row, not `/props`**: §7.7 names `/props` `total_slots`; lmgw's `/props` reader
  does not keep that field, and the row the container was started with says the same number.
- **The "scans" capability row** says scanned PDF pages are read by the chosen model (quality
  partial) while no handler reads them, so the page and the route agree; it stands back as soon as
  a handler (text recognition) takes the row.

**Not yet.**
- Picture files as input: the picture handler is text recognition's (WP4); a picture's
  `no_reader` error says the page-reading model does not read picture files yet when the request
  has model help. `model_help::pdf::read_pages`' page loop is what it would call.
- `ocr: force` falling to the page-reading model when tesseract is missing (§7.4.2's `force_ocr`
  row): `ocr` is WP4's; `select::Pages` needs one more variant (every page asked as its picture
  alone).
- A page text recognition read badly: `select::PageFacts` has `read` and `table`; a quality
  fact from WP4 joins them.
- Slides and office files (WP7 renders slides to PDF, then this stage).
- Stopping a runaway before its bound: the call is unary; a streamed call could stop at a
  repeated line.
- The bound in use in a running job's status (§7.7): it is in each file's `stats.models`.
- A cloud upstream's own concurrency setting (none exists), an external llama-server's
  `total_slots`.
- A cache of page readings (the knowledge bases keep one per file, page, model and prompt
  version).
- `formulas_partial` on pages no model read: tier 0 does not find formulas yet (WP8).
- Region-level reading (a table or formula region instead of the page): WP8's layout.
- DocTags `<document_index>` is read as a table; `<chart>` as a picture followed by its data as a
  table; `<smiles>`, `<footnote>`, `<reference>`, handwritten text and key-value cells as
  paragraphs (a key-value cell's links to other cells are not kept); a picture's class token is
  dropped. A list nested inside a list item is read item by item and counted as not well-formed.
- A bounded retry of a page call: a 429 or 529 from a cloud page reader fails the page by name
  (`part_failed`) and no retry is built yet (review nit 6).
- A dropped call's prompt counted locally for its `499` row (the row has no tokens).
- A per-model limit on an image's pixels: `PAGE_MAX_PIXELS` is one named constant for every
  model.
- A chosen model whose capabilities say "takes no pictures" only after it was chosen (the save
  refuses such a choice, and the Settings row then shows the problem): a conversion still sends
  it pages, as it is the model the owner named and no fallback; only a fallback that takes no
  pictures is refused before a page is rendered.

**Review round (2026-11-19).** A reviewer read the build against lmgw's gate, policy and egress
internals: one blocker, thirteen should-fix items, six nits. All are fixed, each with a test that
fails on the first build; verified against mock upstreams and the fake GPU world only (no card
this round, the checks for it are under Unverified).
- **B1, who answers decides what is sent.** Before any page is rendered the opened route is asked
  whether it is a fallback that takes no pictures (`gate::fallback_images::blind_fallback`); such
  a route is never sent a page (the gate would have replaced the image by a text placeholder and
  the answer would have become the page's text): the pages keep tier 0 with `model_cannot_see`,
  which names the fallback. A send handed on mid-call to such a fallback is refused the same way
  (`call::OnRoute`, the page call's `PerRoute`). The prompt and the reader follow the answering
  route: docling's prompt and the DocTags reader only when the chosen model itself answers, lmgw's
  Markdown prompts for any fallback.
- **S1.** Only `stop` is a page; a refusal part, `length`, a filtered answer, a tool call and any
  other finish reason are `part_failed` naming the reason, never the refusal's or the cut
  answer's text.
- **S2, nits 1 and 2.** The key's policy as above.
- **S3.** `documents_page_reader` and `documents_page_reader_doctags` joined `OUTBOUND_SETTINGS`:
  `lmgw__settings_set` refuses them by name; the dashboard's op takes them.
- **S4.** `PageReader` carries `local`, `outside` (the upstream's name and host) and `fallback`
  (the same, and whether it takes pictures); the Settings row says where pages go, that the
  fallback answers whenever the model cannot run (not only under the hold), that a fallback
  without sight is sent nothing, and that a request that says nothing uses the chosen model.
- **S5.** A native `model_help: missing` (only pages nothing read); docling's `table_mode: fast`
  maps onto it once a model is chosen. Stated in the user docs: with a model chosen, a default
  request uses it; only "no model chosen" or `model_help: off` guarantees no call. Labs ›
  Documents (WP12, landed while this round ran) follows: its form offers "Pages without text
  only", and its docling switch sends `missing` as `table_mode: fast`; `off` has no docling
  option, so `fast` is sent as the least docling can ask for and `model_help` is named as not
  sent on that route (it used to send `fast` for `off` as if that kept every page from the
  model).
- **S6, S7, S9, S10.** As in the decisions above. The file's clock travels to the stage as a
  task-local (`model_help/deadline.rs`); once the stage has it, the batch no longer fails the
  file at the deadline.
- **S8.** `proxy::sample_once_noting` arms a guard (`proxy/in_process/dropped.rs`) that closes
  the in-flight gauge when the call's future is dropped; before, every dropped unary in-process
  call (an agent run's cancel, a `/v1/responses` request whose client left, a knowledge-base page
  read) left the gauge one too high for good. That part changes every caller. The `499` row is
  opt-in (`sample_once_dropping(…, Dropped::Row)`) and only the page call asks for it; the other
  callers still leave no row for a dropped call.
- **S11, S12.** The DocTags reader keeps text that stands where an element has none (inside a
  picture, before a table's first cell, after a joined cell) and counts it; a page whose answer
  holds only picture regions is not read. The vocabulary is docling-core 2.104.0's in full
  (`doctags/vocab.rs`): key-value cells and links, the cell labels, `<smiles>` and chart data
  inside pictures, the legacy `<chart>` class, self-closing language tokens, `<inline>` groups,
  and locations no grid has. A test feeds every token through the reader in eight contexts: none
  leaks as text and no word beside it is lost.
- **S13.** `PAGE_MAX_PIXELS` (4.4 million: an A3 sheet at 150 dpi); a larger page is rendered
  with `pdftoppm -scale-to` and gets a `page_scaled` note. lmgw has no per-model pixel limit to
  derive it from yet.
- **Nits 3 to 5.** A code block is fenced with one more backtick than the longest run that
  starts a line of it, and a formula holding `$$` is written as escaped text (`doc/render.rs`,
  for every source); a model's control tokens are dropped on the Markdown path too;
  `header_removed` counts only pages the tools read and says how many more the model read.
- **This round's decisions:** the blind fallback's pages and a
  file it leaves unread use `model_cannot_see` (warning) and `model_held` (503, error) rather
  than codes of their own for the file; a class or language token outside its element, a cell
  token outside a table and a location no grid has are dropped and counted
  (`model_answer_malformed`) rather than kept as text; out-of-range locations are no longer
  clamped; the `499` row carries `client_disconnected` whatever dropped the call; docling's
  category for a file failed by the model is `inference_failure` (`policy` for
  `model_not_allowed`); `scripts/convert-compare.py`'s fixed lists gained the model codes, the
  `source_*` codes and `missing`, and a test holds them against the wire enums.

**Live check of the review round (2026-11-19, dev instance, debug build, no card: nothing
local was loaded).** Two upstreams at `.example` addresses (unreachable by design) with aliases
whose capabilities are set by override: `example-vision` (takes pictures, chosen as the
page-reading model), `example-backup-vision` (takes pictures) and `example-text-only` (does
not) as its fallback in turn. Files: a two-page PDF (one page with text, one without) and a
one-page PDF without text.

| request | answer |
|---|---|
| no model chosen: two pages / the scan | 200 `partial` with `no_reader` naming page 2 / 415 `no_reader`; no call |
| no model chosen, `model_help: missing` | the same, plus the request-wide `model_not_chosen` |
| `example-vision`, no fallback: two pages / the scan | 200 `partial`, `part_failed` page 2 (the upstream is unreachable), `model_calls` 1 / **502 `model_failed`**, `stats.models` kept |
| the same, `model_help: off` | 200 `partial`, no call |
| fallback `example-text-only`: two pages / the scan | 200 `partial`, **`model_cannot_see`** page 2 naming the fallback (the send was handed on at the unreachable upstream and refused before anything was sent to the fallback) / **503 `model_held`** |
| fallback `example-backup-vision`: two pages | 200 `partial`, `part_failed` page 2 naming the fallback's own unreachable upstream (the send was handed on, with the page) |
| docling route, `table_mode: fast` | 200, `x-lmgw-option-notes: table_mode=mapped(lmgw tables and a chosen page-reading model on unread pages only)` |

Usage rows: one `documents-page` row per hop (502 `unreachable` on the model's upstream; 400
`unsupported` for the fallback that was refused), beside each request's `documents` row with
the file's code (`model_failed`, `model_held`). The Settings row at 125% says, for the three
choices: "example-vision is not a model lmgw runs on this computer: these pages are sent to the
service "example-cloud" (api.example)", and for the fallback either that no model reads pages
then, that pages then go to the service "example-backup" (backup.example), or that
example-text-only takes no pictures and is sent nothing (headless Chrome, `scripts/ui-drive.py
--dsf 1.25`).

**Measured, tier 0 unchanged by the round.** `python3 -I scripts/convert-measure.py --corpus
"$CORPUS" --lmgw http://127.0.0.1:<port> --formats pdf` with no model chosen, on this build:
28/28 converted, 3-gram Jaccard 0.8569, line ratio 0.2698, headings 167/159, tables 39/31, cells
1980/1322: WP3's row to the digit (the renderer's code-fence and `$$` rules touch only code and
formula blocks, which tier 0 does not make from a PDF).

**Live check (2026-11-19, dev instance, debug build, a 24 GiB test GPU, 10 page generations in all).**
granite-docling-258M Q8_0 with its mmproj as a local row of the dev instance (`-c 8192`, `-np 1`,
`-ngl 99`, extra flag `--special`, image `localhost/lmgw-llama-server:official-master`, llama.cpp
b12493), chosen as the page-reading model with "Answers in DocTags" on; pages rendered at 150 dpi;
the similarity is `scripts/convert-measure.py`'s against docling's ground truth (3-gram Jaccard /
line ratio).

| request | answer |
|---|---|
| `ocr/sources/ocr_test.pdf` (a one-page scan), the GPU hold on | 415 `no_reader`; the error names the page and that the page-reading model is paused; no container started |
| the same, hold off | 200 `converted`, `parts_by_source.vision` 1, one paragraph with its box, 2.1 to 2.6 s with the container's start; Jaccard 0.977 against the corpus' nemotron reference (measured on the first build, which still left `<\|end_of_text\|>` in the text; fixed and run again, no warning) |
| `pdf/sources/2305.03393v1-pg9.pdf`, `model_help: off` | 0.952 / 0.182, no headings, 1 table (50 cells), 53 ms |
| the same, `every_page` | 0.958 / 0.539, 2 headings, 1 table (48 cells), 2.6 s |
| `pdf/sources/code_and_formula.pdf` (2 pages), `off` | 0.887 / 0.250 |
| the same, `every_page` | 0.934 / 0.710; in `json`: headings, a `code` block (the JavaScript function), a `formula` block (`a ^ { 2 } + 8 = 1 2`), page footers, every block with its box; 4.1 s |
| `ocr/sources/nemotron_multipage.pdf`, `pages: "2"` (WP0a's rotated runaway) | 415 after 10.9 s: "its answer ran to the token limit after 7314 tokens without ending (max_tokens 8146: the model's context of 8192 tokens less the prompt's 46 (the page image's tokens are not known for this model))" |
| `ocr_test.pdf` with the row's `--special` removed | Settings row hint "started without --special…"; 200 `converted` with the page's text and `model_answer_malformed`: "the model is set to answer in DocTags, but its answer carried no DocTags structure … started with --special" (the tags were gone and the location tokens were not, which the first build took for malformed DocTags; fixed and run again) |

Usage rows of the run: one `documents-page` row per call under the caller's key (prompt 878 to
1142 tokens, completion 854 to 7314), beside each request's `documents` row. `stats.models` named
`example-doctags`, `in_flight_bound` 1, `bound_from` `row_slots`. lmgw has no per-image bound for
this projector, so the counted prompt was the text's 46 tokens and `max_tokens` 8146, 832 more
than the context really had left; the server ended the runaway at its context (`length`), which
is the failed page above. The container stopped when asked through the dev instance; nothing of
the run is left on the card.

**Measured, tier 0 unchanged.** With no page-reading model chosen (the default) the stage is not
entered (`Plan::model_help` is `None`), so the WP3 corpus numbers stand; the `off` rows above are
tier 0 on this build.

**Unverified** (not run yet; `$CORPUS` and `LMGW_KEY` as in §14 WP0a,
the dev instance on a port of its own):
- **The page prompts on a general vision model** (Gemma 4, Qwen): nothing but the mock has
  answered them. With that model chosen (DocTags off):
  `python3 -I scripts/convert-measure.py --corpus "$CORPUS" --lmgw http://127.0.0.1:<port> --formats ocr,pdf --per-file --out wp6-vlm.json`
  (about 125 pages), against the same command with `model_help` off; in particular whether the
  markdown-table sentence keeps a value with its row and column as the one-line-per-cell form did
  (88 of 88 cells, 2026-09-24).
- **granite-docling through lmgw on the whole corpus** (the run above was 9 pages): the same
  command with the granite row chosen and DocTags on, against WP0a's direct numbers (35 of 38
  pages stopped, Jaccard mean 0.808).
- **A projector with a known image bound** (Gemma 4) on a guarded row (`kv_unified`, `n_predict`):
  that `max_tokens` is the context less prompt and image exactly, and that the gate takes it
  (`POST /v1/documents/convert` with a one-page scan; the call's row shows `max_tokens_clamped`
  when `n_predict` is smaller).
- **The hold with a real fallback**: the local row with a cloud alias as fallback, hold on, a
  one-page scan: `stats.models[0]` names the cloud alias with `answered_for`, and the row is the
  cloud alias's price. (Tested against the fake GPU world and a mock cloud.)
- **A hold released mid-request** on a real card (pages not yet sent go to the model again): it
  is tested against the fake GPU world with a slow mock fallback since the review round.
- **A real fallback that takes no pictures** (`vision: false` from a real catalog, or the owner's
  capability override): the local row with a text-only cloud alias as fallback, hold on, a
  two-page PDF with one scan page: `model_cannot_see` names the fallback, the cloud alias gets no
  request, and Settings › Documents says so beforehand.
- **A refusing cloud model**: `every_page` on a text PDF through an OpenAI model that refuses a
  page (`message.refusal`): that page keeps its text layer with `part_failed`.
- **A key's tokens-per-minute window under a long scan**: a key with `tpm_limit` and a 30-page
  scan on a real model: the pages after the window fills are `model_not_allowed`, and the key's
  other requests see the same window.
- **The page of the review round's Settings row** in the app's own window, with a real cloud
  alias and a real fallback.
- **A cloud model whose catalog gives `max_output_tokens`** (`max_tokens` from it), a **candidate
  alias** and a **ladder row** as the page-reading model: one scan page each.
- **Many pages at once on a real 4-slot row** (the bound is tested against the fake world's 4
  slots and a mock's delay): a 20-page scan with `-np 4`, watching `nvidia-smi` and the
  container's `/slots`.
- **The Settings row in the app's own window** (WebKitGTK): the screenshots are headless Chrome's.
- Windows: no poppler there, so no page is rendered; the code compiles behind the same paths as
  WP3's (not compiled here).

**WP6 on WP8, WP10 and WP4 (rebased onto `main` at `ace7f022`, 2026-11-19).** The first two WP6
commits were squashed into one before the rebase. Both sides' code, tests and text are kept; what
had to be decided to make them one:

- *One clock, one deadline.* WP6's own per-file deadline (`model_help/deadline.rs`, a wall clock
  the stage took over) is gone; the stage runs on WP10's lane clock (`pool::clock`). Each page the
  model reads is a lane, as each OCR page is: opening the route (a local model's start) and the
  wait for a turn among the calls in flight are `paused`, the render's slot wait pauses as every
  slot wait does, the render and the call in flight count. A stage with pages takes the file's end
  over (`FileClock::hand_over`); `batch::within_deadline` then lets the file finish instead of
  failing it, and the steps still working at the deadline end themselves: each page through
  `FileClock::expiry` (the call dropped, its `499` row and the closed gauge as before), and each
  picture run beside the text through `FileClock::handed_expiry` (its pages' pictures stay
  placeholders, named in `pictures_partial` with `document_timeout`).
- *How the stages share `document_timeout` (the rule, also in `docs/documents.md`, "A file's
  time").* One budget of working time per file; side-by-side lanes count once; waits count
  nothing. (1) The layout model stops at `MODEL_DEADLINE_SHARE` (½), as WP8 built it. (2) Until
  the page-reading model has pages of the file, the deadline fails the file (`deadline_exceeded`)
  and stops its tools, as on `main`: the geometry, OCR pages and picture runs have no share of
  their own. (3) From then on the deadline is the model stage's: answered pages stay, the rest
  keep the tools' text with `part_failed`, picture runs still going stop, the file is `partial`.
  *Decision:* no share is reserved for the model; a long OCR can use the whole budget, and then the
  file fails by the deadline as it would without a model.
- *The order of readers (design §3, §7.1): text layer → text recognition → model.*
  `select::PageFacts` has who read a page (`ReadBy`: nothing, the text layer, recognition with
  `poor`) from the part's source and what `ocr::pages::PdfOcr` says of it (`poor`, `empty`):

  | the page | `missing` | `missing_or_tables` | `every_page` |
  |---|---|---|---|
  | nothing read it (no text layer; recognition off, missing, failed, or no word found) | picture | picture | picture |
  | recognition read it poorly | – | picture | picture |
  | recognition read it well, a table on it | – | picture | picture |
  | recognition read it well, no table | – | – | picture |
  | text layer, a table on it | – | picture and text | picture and text |
  | text layer, no table | – | – | picture and text |

  "Poorly" is the page set of recognition's `ocr_low_confidence` warnings: mean word confidence
  below `OCR_LOW_CONFIDENCE` (60), the same named threshold, or lines that read as vertical with no
  turn reading better; a page turned upright (`ocr_page_turned`) and read with confidence is read
  well. *Decisions:* a page on which tesseract found no word is a page nothing read (it goes to the
  model under `missing` too; a blank page costs a call); a recognised page is sent as its picture
  alone, never with tesseract's words to copy from (they may be wrong, which is why the model
  reads it); `missing` sends no page recognition read, however poorly (`missing` as §7.1 defines it, only pages nothing read:
  fewer pages to a model). Without tesseract every page without a text layer is "nothing read", as
  before WP4. With `ocr: force`, a recognised page is a recognised page whatever text layer it
  had. What the model reads replaces OCR's text: the part's source is `vision`, `stats` count it
  under `vision` only, and recognition's per-page notes (`ocr_low_confidence`, `ocr_page_turned`,
  `ocr_page_reduced`, its `part_failed`) are written after the model stage without the pages the
  model read (`PdfOcr::finish`); a missing language pack or tool stays said.
- *Picture files.* Not read by the model yet (named `not_yet` note as before, its seam in
  `model_help.rs`'s module doc). WP4 reads them by OCR through the reader child; sending one to a
  model needs a PNG or JPEG within `PAGE_MAX_PIXELS`, which for a TIFF, a WebP, a GIF or a larger
  picture means a scaled render in the reader child (no op for it yet), and the stage's page source
  generalised from a PDF render to a file's bytes. That did not fall out of the seam cleanly, so it
  stays not yet.
- *Rendering.* A page's render reads the file from WP4's spool: `ocr::spool::Shared` spools a
  file of more than one page once, made by the first stage that renders a page (text recognition,
  else the model stage) and read by position by every `pdftoppm` after; no stage pipes the whole
  PDF per page any more. The render takes its own leased slot (`run::run_held`), only once the
  page's turn came, so no slot is held while a page waits for the model; dropping the conversion
  kills a render in flight and drops the calls.
- *The layout model and model help on one PDF.* The layout model runs before (it decides which
  pages hold tables, which `missing_or_tables` asks about), also on the pages the model then reads;
  which pages the model answers is only known afterwards, and a page it does not answer (held,
  refused, failed, late) is laid out with the regions. *Decision:* not skipped for `every_page` either;
  on pages the model reads its regions are computed and not used.
- *Pictures on a page the model read.* The tools' figures (the layout model's picture regions with
  their inside text) are kept (`model_help::pdf::keep_figures`): one alike a figure of the answer
  (DocTags pictures have boxes; overlap of half the larger box) takes that figure's place and
  keeps the tools' box, source and text; the others follow the answer's blocks, before a page
  footer. poppler's raster pictures are placed afterwards by WP10's `place` into whatever the page
  holds: merged into a figure they overlap, else by position, and at the page's end when the
  answer has no positions (a Markdown answer).
- *Budget.* WP6 adds no output take of its own: the model's blocks go through the file's
  `budget::Spend` like the tools', and DocTags tables are `Table` blocks, which `doc_output`
  charges with the grid reservation as it does WP3's and WP8's.
- *E-mail attachments.* An attached PDF read with the model adds its calls and models to the
  message's `stats` (`Stage::absorb`), also when the attachment failed.
- *Settings and guards.* `OUTBOUND_SETTINGS` holds WP5b's two and WP6's two; WP8's
  `documents_onnx_layout` and `doc_model_add` stay the self-admin tools' (a test says so).
- *Labs › Documents.* The model help row says it is asked last and its choices say what they
  take ("Pages nothing else read", "Unread or poorly read pages, and tables", "Every page"); the
  docling switch mapping is WP6's (`missing` → `table_mode: fast`, `off` → `fast` and `model_help`
  named as not sent).
- *Lists.* `scripts/convert-compare.py` holds the union of the codes; the Rust test that holds
  its lists against the wire enums lists WP8's, WP10's and WP4's warnings too.
- *The root fix in `proxy/in_process.rs`* (the gauge closed when a sampling future is dropped)
  stands unchanged; its callers' tests ran again (below).

*Measured, no model chosen (2026-11-19, dev instance, debug build, tesseract 5.5.4 with `eng`,
`deu` and `osd` through `TESSDATA_PREFIX`, poppler 26.08.0):* pdf 28/28, 3-gram J 0.8569, line
ratio 0.2698, headings 167/159, tables 39/31, cells 1980/1322; ocr 10/10, J 0.7856, line ratio
0.400; html 33/33, J 0.9422, line ratio 0.8494, tables 23/24: `main`'s numbers, so choosing no
model changes nothing. `python3 -I scripts/convert-measure.py --corpus "$CORPUS" --lmgw
http://127.0.0.1:8886 --formats pdf,ocr,html --out measure.json`.

*Live check (2026-11-19, dev instance on 127.0.0.1:8886, no card: the page-reading model is a
loopback OpenAI-compatible stub answering a fixed Markdown page and logging each call's prompt
kind and image size; tesseract without `osd`).* A four-page PDF: a text-layer page, a clean scan
(tesseract: 18 words, mean confidence 96), the same scan turned upside down (16 words, mean
confidence 41, below `OCR_LOW_CONFIDENCE`), a blank scan (no word):

| request | the stub got | `parts_by_source` |
|---|---|---|
| no `model_help` (the setting's, `missing_or_tables`) | pages 3 and 4, as pictures | text layer 1, ocr 1, vision 2 |
| `missing` | page 4 | text layer 1, ocr 2, vision 1 (and page 3's `ocr_low_confidence`) |
| `missing_or_tables` | pages 3 and 4 | text layer 1, ocr 1, vision 2 (no `ocr_*` note left for page 3) |
| `every_page` | pages 1 (picture and text), 2, 3, 4 | vision 4 |
| `off` | nothing | text layer 1, ocr 3 |
| `missing`, `ocr: off` | pages 2, 3, 4 | text layer 1, vision 3 |

`document_timeout=5` with one tool slot (one call in flight) and the stub answering after 2 s, on
a text page and five blank scans (`ocr: off`): 200 `partial` in 5.08 s, the text page and two
model pages kept, `part_failed` for pages 4-6 naming the 5 s, the stub saw two answered calls and
the third's connection closed; the usage log has the two `documents-page` 200 rows and the dropped
call's 499. A request cancelled after 3 s (`curl --max-time 3`, `every_page` on the four pages, the
stub holding each call 30 s): the stub saw all four connections closed, `doc_tools` showed the pool
at 0 running and 0 waiting, and the usage log four `documents-page` 499 rows beside the request's
own 499. Settings › Documents and Labs › Documents at 125 % (headless Chrome, `scripts/ui-drive.py
--dsf 1.25`): "Layout model" and "Model help" sit one under the other with the same row layout; the
Labs form's "Model help" group follows "Text recognition".

*Not yet (added here):* picture files read by the model; regions for pages the model will replace
left uncomputed; a share of `document_timeout` reserved for the model; a page tesseract read
poorly given to the model with its words as a hint.

*Unverified:* the order of readers against a real vision model (every page above was answered by
the stub); `osd` and the model together on a turned scan (a page `osd` turns and tesseract then
reads well no longer goes to the model under `missing_or_tables`).

### WP11

**Built (2026-11-19).**
- `extract/convert/describe.rs` and its children: the request's description (`Describe`, judged by
  `plan` from `describe_pictures`/`picture_prompt` and the request's `model_help::Reader`), `prompt`
  (the prompt with the caption and the paragraph before as context, each bounded by
  `PICTURE_CONTEXT_MAX_BYTES`), `sendable` (which pictures a model takes; an image file as its one
  picture), `stage` (one call per picture, the notes, `stats.models`), `settings` (the save's check,
  the row's facts, the "Picture descriptions" capability row). `PROTO` `documents-picture`,
  `DEFAULT_PROMPT` docling's.
- **One machinery, extracted from WP6** (`model_help`): `Reader` carries both choices, each a
  `Chosen` with a `Purpose` (`Pages`, `Pictures`: the usage label, the model's and the image's words);
  `turn.rs` is the call's way that `page.rs` used to hold inline (the key's policy, the route, the
  turn, the policy again, the image made, the call; `By` moved there), now taken by a page's read
  (its render as the image) and a picture's description (its bytes); `call::ask` takes the image's
  type and `call::What` (a page in the form the answering model is asked for, or a picture's whole
  prompt); `judged_as` words a failed page or a failed description; `Bound::said_for`;
  `settings::facts_of`/`validate_for` tell either row. WP6's behaviour, words and tests are unchanged
  (its call test gained the image type argument; its session test reads `Reader::pages`).
- Setting `documents.picture_alias` (the design's name; empty = off), saved through `settings_set`
  (`documents_picture_alias`, the same checks as the page reader's: resolves, a chat model, not
  "cannot see"), in `OUTBOUND_SETTINGS` (refused by `lmgw__settings_set`, with a guard test).
  Settings › Documents › Model help › "Describe pictures with" (`documents/pictures.rs`): the shared
  picker over chat models that take pictures, where pictures go (this computer or a named service
  and host), who answers whenever the model cannot run and whether it can see. `doc_tools` answers
  it as `picture_describer` (the `PageReader` shape).
- Wire: `ModelCalls.pictures_described`, `WarningCode::PictureContextCut`, `DocPicture.description`
  and `DocBlock.description` filled, `DocCaps.picture_describer`, the `picture_descriptions` format
  row.
- Glue: `file/pictured.rs` (`export` describes after the bytes are made, an image file becomes its
  one picture first; `output` stamps the descriptions on the figure blocks and writes them in text),
  `pictures::line` (the description after the picture's line), `pictures::wire`,
  `office::lines` (an empty line drops its marker), `doc/render.rs` (`block_text` writes a
  figure's description), `options.rs` (`Plan::describe`, `pictures::Settings::describe`, which makes
  the bytes in every mode), `file/more.rs` (the image file's export note only without descriptions).
- Adapter: `do_picture_description` is `mapped(picture-description model)` once a model is chosen
  (`Mapped::settle`), `mapped(model_not_chosen)` without; `picture_description_api`/`_local`/
  `_custom_config` give their `prompt` and list what the model stands in for (`repo_id`,
  `generation_config`, `params`, `timeout`, `concurrency`, `batch_size`, `scale`) and that `url` and
  `headers` are not followed; `picture_description_area_threshold` is `mapped` onto
  `picture_min_area` (which also leaves smaller pictures out of the export).
- Labs › Documents: the describe switch and prompt were sent already; the docling switch now sends
  the prompt as `picture_description_local` `{"prompt"}` instead of naming it unsent; the Pictures
  group shows the capability row when nothing describes; the outline shows a figure's caption and
  description; the stats line says how many pictures were described.

**Where the description goes (checked 2026-11-19 in docling-core 2.104.0 / docling 2.142.1, the
docling-serve-cpu 1.38.2 image at `sha256:7a480a16…`, read with `podman run --network=none`).**
docling's picture description writes the text to the picture's `meta.description`
(`picture_description_base_model.py`; the old `annotations` only when kept on purpose).
`MarkdownDocSerializer` puts an item's meta *after* the item (`_meta_position` "after"),
joined with `delim="\n\n"`, without a marker (`mark_meta` false); the picture item itself is its
caption (before it, `caption_placement` standard) and its image line. So docling's markdown is
caption, `<!-- image -->` (or the image), a blank line, the description as plain text. Its plain
text (`export_to_text`, `PlainTextDocSerializer`, the same walk with an empty
`image_placeholder`) writes the description where the picture was. lmgw writes the picture's line,
a blank line, the description (its lines escaped as a paragraph's, so a description starting
with `#` or `|` cannot become a heading or a table); its caption stays where WP10 put it (after
the picture). In text the description alone. In json `figure.description` and the `pictures`
list's `description`.

**Decisions:**
- **The setting is `documents.picture_alias`** (§4.4's name; WP6's own was `page_reader`).
  (Corrected in the review round: the report found neither model; `/api/settings-full` now
  carries both and the script reads them by those names.)
- **`output: text` is described too**, as docling's text export writes descriptions: with
  `describe_pictures` a PDF's picture search runs for `output: text` (WP10 skips it otherwise).
  **`pictures: placeholder` is described too** (docling describes in every image mode): the bytes
  are made and reserved for the call and not written; the reservation not used is given back.
- **Only PNG and JPEG within `PAGE_MAX_PIXELS` are sent**; GIF, WebP, BMP, TIFF and larger
  pictures get a `not_yet` note naming why (converting or scaling an untrusted picture belongs in
  the document reader, which has no such step yet). PDF crops are PNG and within the bound unless
  `images_scale` is raised far.
- **A picture on a page the page-reading model read is described once**, by the
  picture-description model like any other; the page prompts transcribe text and describe
  nothing, and the DocTags reader keeps picture regions without text.
- **Context**: the caption, then the paragraph before, after the prompt, introduced as document
  text, not instruction; each cut at `PICTURE_CONTEXT_MAX_BYTES` (2 KiB; the caption's start, the
  paragraph's end), said in the new warning `picture_context_cut`.
- **A file is `partial`** when a picture asked for was not described for the model's reasons
  (held, cannot see, not allowed, failed, late); `not_yet` pictures and pictures that were not
  exported keep it `converted`. A file never fails for a description.
- **No model chosen** is the request-wide `model_not_chosen` (option `describe_pictures`), not a
  per-file note, as WP6's `model_help` note is. `picture_prompt` without `describe_pictures` is
  `option_not_applicable`.
- **An image file** (PNG/JPEG, read by text recognition) becomes its one picture when it is
  described: a `figure` block before its text, written as the mode says; without descriptions it
  stays WP10's `not_yet`. A TIFF/GIF/WebP file gets the description's `not_yet` note, which also
  says it is not exported. Without tesseract an image file has no handler and is not described.
- **The capability row** "Picture descriptions" is added to `doc_tools`' formats by
  `describe::settings::apply` from the chosen model (like WP6's scans row), not as a `caps.rs`
  handler: the table's handlers need tools, and this row needs a model.
- **The docling adapter**: `do_picture_description` is `mapped(picture-description model)` with a
  model chosen (it is the owner's model answering, and the callers' summary shows who asks for
  descriptions); `picture_description_area_threshold` is `mapped` onto `picture_min_area`,
  which also leaves smaller pictures out of the export (WP10's open decision; replaced in the review
  round by the native `describe_min_area`); a note's detail joins
  names with "and" (the header's value replaces commas).
- `stats.models` gained `pictures_described`; a model that both reads pages and describes is one
  entry (`calls` both, `parts_read` pages, `pictures_described` pictures).
- Temperature 0 on picture calls too (docling's local preset decodes greedily).

**Not yet:** converting or scaling a picture for the model (GIF, WebP, BMP, TIFF, more than
`PAGE_MAX_PIXELS`); pictures of EPUBs, web pages, spreadsheets and e-mail bodies (not exported,
so not described); an image file without text recognition; a cache of descriptions; a retry of a
429 from a cloud describer; a per-model pixel limit; docling's picture classification gate on
descriptions (`classification_allow`/`_deny`, §12).

**Tests** (`tests/it/documents_pictures.rs`, `documents_pictures_more.rs`; unit tests in
`describe/`, `model_help/session.rs`, `mcp/selfadmin/guards.rs`, the UI row): the call body
(picture as stored, default and caller's prompt, caption and paragraph, temperature 0, no file
name), every picture mode and json and text, the usage row, `picture_min_area`, no model chosen,
a key's scope, a tokens-per-minute window crossed mid-request, a refusal and `length`, GIF and
oversized pictures, thirty pictures on a bound of two (arrival times) and on a local row with
`-np 2` (one start, `max_tokens` 8192 − 900), `document_timeout` mid-stage, a cancelled request
(connection closed, gauge 0, `499` row, tool pool 0/0), the hold without a fallback, with a seeing
one and with a blind one (and the Settings facts), the same model for pages and pictures on one
PDF (one entry, the picture described once), the docling route's options, and the guard.

**Live check (2026-11-19, dev instance 127.0.0.1:8887, debug build, no card: the describer is a
loopback OpenAI-compatible stub logging each call's prompt line, context flags, image type and
size; files made with Python: a docx with three PNG pictures and a GIF with captions, a PDF with a
raster picture and a caption, a PNG file).**

| request | answer |
|---|---|
| docx, placeholder | 200 `converted`, `<!-- image -->`, blank line, the description, then `Figure 1: …`; p4 (GIF) `not_yet`; `stats.models` example-vision calls 3, described 3, bound 20 (tool pool) |
| docx, embedded, json | 200, `figure.description` and `pictures[].description` |
| docx, text, own prompt | 200, the description where each picture was; the stub got "Name the chart type." with caption and paragraph |
| report PDF, referenced | 200, `![Image](artifacts/image_000000_….png)`, blank line, the description; the stub got a 4958-byte PNG crop |
| PNG file | 200, `<!-- image -->` and its description, then its (empty) text |
| a 2 s stub, one tool slot, `document_timeout=3` | 200 `partial` in 3.02 s: p1 described, `part_failed` "pictures p2, p3 … document_timeout of 3 s" |
| the same, client gone after 1 s | the stub got one call; `doc_tools` pool 0 running, 0 waiting |
| docling route, model chosen / none | `do_picture_description=mapped(picture-description model)` / `mapped(model_not_chosen)`; `picture_description_api=mapped(picture-description model in place of params with its url not followed)`; the stub got "Describe the example." |

Usage rows of the run: `documents-picture` 200 ×12 and 499 ×2 (the call cut at the deadline,
the cancelled one), each under the owner key, beside the `documents` rows. Settings › Documents
and Labs › Documents at 125 % (`scripts/ui-drive.py --dsf 1.25`): "Describe pictures with" under
Model help below "Read pages with a model", saying the pictures go to the service
"example-stub" (127.0.0.1) and that without a fallback they are kept undescribed; "Who calls the
docling route" lists "picture descriptions (no model chosen), picture descriptions (lmgw's own)".

**Measured, no picture model chosen** (2026-11-19, dev instance, debug build, poppler 26.08.0,
tesseract 5.5.4): `python3 -I scripts/convert-measure.py --corpus "$CORPUS" --lmgw
http://127.0.0.1:8887 --formats pdf --out measure.json`: 28/28, 3-gram J 0.8569, line ratio
0.2698, headings 167/159, tables 39/31, cells 1980/1322: `main`'s numbers.

**Unverified (needs a real vision model):**
- descriptions from a real local vision model (Gemma 4, Qwen) on the report PDF and a docx with
  photos: `POST /v1/documents/convert` with `describe_pictures=true`, a projector with a known
  image bound so `max_tokens` is the slot's context less prompt and picture exactly;
- granite-docling as picture describer (it is a page model; whether its answer to docling's
  prompt is a description at all);
- the hold with a real cloud fallback and a real text-only fallback on pictures;
- a cloud model that refuses a picture (`message.refusal`);
- thirty pictures on a real `-np 2` row while Chat uses the model (fair queueing);
- the Settings row in the app's own window (WebKitGTK); the screenshots are headless Chrome's.

#### WP11 review round (2026-11-19)

**What changed** (review B1, S1–S8, nits):
- **B1, the prompt's memory.** `Describe::prompt` is an `Arc<str>` held once per request; a
  picture keeps only its bytes (shared) and its two bounded context pieces
  (`describe/prompt.rs::context`); its whole message is made by `call::What::Picture`'s closure
  when the call goes out, so a request holds at most one message per call in flight.
  `describe/room.rs` counts the caller's prompt once per route the request opens, against the
  answering model's context (`Open::context`): with `proxy::count_text_route` (the running
  llama-server's tokenizer, the provider's count for Anthropic and Gemini) or tiktoken in a
  tool-pool slot (OpenAI-shaped upstreams); a prompt of fewer bytes than the context's tokens is
  not counted. A prompt that fills the context sends no picture and gives one `part_failed`
  note (option `picture_prompt`) with both numbers. No size constant was added.
- **S1.** `file/more/late.rs`: once a model stage took the message's end over, attachments after
  the deadline are not read and one started after the hand-over ends at the deadline
  (`biased` on `FileClock::expiry`); each is named in one `part_failed` note (option
  `document_timeout`). An attachment left out for the output budget keeps its model calls in
  the message's stats (nit).
- **S2.** The describe stage checks the clock at its start (an expired clock opens nothing and
  names the pictures, "had passed before their turn"); each lane's `select!` is `biased;` with the
  expiry first. WP6's `page::read` is unchanged.
- **S3.** `docling_serve/options/picture.rs` writes the note from the object's own keys:
  `prompt` used; the model's settings replaced (`repo_id`, `generation_config`, `params`,
  `timeout`, `concurrency`, `batch_size`, `scale`, `engine_options`, `model_spec`,
  `provenance`, `usage_response_key`, `padding_side`, `kind`); `url`/`headers` not followed;
  `picture_area_threshold` replaced by `picture_description_area_threshold`;
  `classification_*` not yet; any other key named as not used (bounded as unknown option names).
  Key lists probed in the local docling-serve-cpu 1.38.2 image (`docling_jobkit.datamodel.convert`
  `PictureDescriptionApi`/`PictureDescriptionLocal`, docling's `PictureDescription*Options`).
- **S4.** Image, caption, description: `describe/place.rs` puts the description after the
  caption block (Doc path, `render::markdown_after`/`text_after`) or the caption paragraph
  (reader markdown, `office::lines`' `after`) when that caption follows the picture, else after
  the picture. Text output follows the same order; json keeps the field on the figure block.
- **S5.** `pictures::held`: bytes the answer does not write (`placeholder`, `output: text`) are
  reserved at their own size; after the stage they are dropped and the reservation given back;
  the out-of-budget note says "send the files in separate requests" without the placeholder
  advice there.
- **S6.** `SettingsFull` gained `documents_page_reader` and `documents_picture_alias`
  (read-only); `convert-compare.py` reads `page_reader`/`picture_alias` and no longer reports
  `vision_pages` (no build has it); the Rust script test checks the names against the wire.
- **S7.** An image file is its one picture whenever pictures are exported (`embedded`,
  `referenced`), described or not; in `placeholder`/text it stays a picture only when its
  description was made; an unknown type (or SVG) is a named `pictures_partial` note, and so is a
  file with no part to place it in. WP4's `not_yet` for it is gone.
- **Area threshold (review item 4: changed to a native option).** New native option `describe_min_area` (default 0.05,
  `DESCRIBE_MIN_AREA_DEFAULT`, docling's `picture_description_area_threshold`): a picture below it
  is exported, gets no call and no warning, and is counted in
  `stats.pictures_below_describe_min_area`. The adapter maps docling's threshold onto it
  (`honoured`) and exports with `picture_min_area` 0. This corrects how §5.3 read docling's
  option: the reviewer probed `picture_description_base_model.py` (the threshold only skips the
  description; the picture stays). §5.3 and §7.4.2 are corrected.
- **Nits done:** `pictures_described` always written; guard and session/model_help docs
  worded for both purposes ("the same name"); "one route per request" reworded; descriptions also
  escape a line starting `$$` or a link definition (in `describe::paragraph`, `render::escaped`
  unchanged); the docs say a caller's prompt is followed by the context block (decision 7).
- **Nits left:** failures grouped by exact message (a `length` failure per picture can repeat);
  text output on the office path derives plain text from markdown (a description `# Results`
  loses its `#`); mid-line markup in descriptions (as all document text); a `<think>` block left
  in `content` becomes the description (as WP6's pages).

**Tests:** `describe/tests.rs` (picture line alone, order in markdown and text, the escaping
cases through `from_md`, an expired clock opens nothing), `describe/place.rs`,
`docling_serve/options/picture.rs`, `file/tests/pictures.rs` (S5 reservation),
`file/tests/ocr_formats.rs` (image file as its picture); `tests/it/documents_pictures_review.rs`
(B1 with 40 pictures and a 1000-token catalog context: no call, no message made; markdown and
json of a marked-up answer; placeholder bytes not in the answer; the image file in three cases),
`documents_pictures_clock.rs` (moved deadline/cancel tests, the e-mail deadline),
`documents_pictures_more.rs` (a picture on a page the model read, same and different alias;
docling `headers`/`url`/`params` absent from every request the mock got),
`documents_pictures.rs` (order, `describe_min_area`).

**Live check (2026-11-19, dev instance 127.0.0.1:8887, debug build, no card: loopback
OpenAI-compatible stubs, one fast, one answering after 4 s, one listing a 1000-token
`context_length`, one answering 500; the files of the first live check plus an e-mail with a
docx and a text attachment).**

| request | answer |
|---|---|
| docx, placeholder / text | 200 `converted`: `<!-- image -->`, `Figure 1: Example chart 1`, the description; text the caption then the description |
| PNG file, `embedded`, no description | 200, `![Image](data:image/png;base64,…)`, one picture, no note |
| PNG file, placeholder, describer answers 500 | 200 `partial`, no `<!-- image -->` line, empty pictures list, `part_failed` naming p1 |
| docx, 21 KiB `picture_prompt`, 1000-token context | 200 `partial`, no call at the stub, one `part_failed` (option `picture_prompt`): "comes to 4000 tokens (counted with tiktoken's o200k_base, an estimate) … context of 1000 tokens"; docling's prompt on the same model: 3 calls |
| e-mail (docx, text), 4 s describer, `document_timeout=2` | 200 `partial` in 2.05 s: the docx's pictures `part_failed` (deadline), "1 attachment(s) were not read … second.txt" |
| docling route, `picture_description_api` with `headers`, `params`, `classification_allow`, an unknown key, threshold 0.2 / 0.1 | notes name every key ("… with its url and headers not followed with classification_allow not yet with example_key not used"); 0.2: no call (the docx's pictures are 0.19 of the page), all exported; 0.1: 3 calls, none with the caller's header |
| `GET /api/settings-full` | `documents_page_reader: ""`, `documents_picture_alias: "example-stub"` |

**Measured, no picture model chosen** (2026-11-19, dev instance, debug build, poppler 26.08.0,
tesseract 5.5.4), `convert-measure.py --formats pdf`: 28/28, 3-gram J 0.8569, line ratio 0.2698,
headings 167/159, tables 39/31, cells 1980/1322: `main`'s numbers.

**Not yet:** a Labs › Documents row for `describe_min_area`; one pool for two names of one model
(an alias and its row); the remaining nits above.

**Unverified:** the count against a real local llama-server's context (`/tokenize` on the
running container) and against Anthropic's and Gemini's count endpoints (needs the card or
credits; the code path is `proxy::count_text_route`, the counters' own); the Windows build (left to the Windows compile check at landing).

### WP7

**Built (2026-11-19, LibreOffice 26.8.2.2, pandoc 3.7.0.2, ffmpeg 9.0.2, on `main` at
`ace7f022`).**
- *Sniffing* (`extract/sniff/tools.rs`, called from `sniff_convert`): OLE2 `WordDocument` →
  `doc`, `PowerPoint Document` → `ppt`; a password-protected Office file (encrypted OOXML:
  `EncryptionInfo` + `EncryptedPackage`; a `.doc` whose FIB has `fEncrypted`; a `.ppt` with
  `EncryptedSummary`) → `SniffError::Encrypted`, the file's `unreadable` "protected by a
  password"; `{\rtf` → `rtf`; XML whose root is `FictionBook` → `fb2`, DocBook 5's namespace or a
  DocBook doctype → `docbook`; a docx whose `word/document.xml` holds `:oMath` (searched while
  unpacked, never held whole, at most `MAX_UNCOMPRESSED_BYTES`) → `docx+math`; what `sniff`
  refuses as a video container → `video`. LaTeX, RST, Org and MediaWiki are text to the bytes and
  go by their name (`Format::markup_by_name`), as a web page does.
- *Capability table* (`caps.rs`, which was split: `caps/format.rs`, `caps/table.rs`, so it stays
  near 600 lines): eleven formats, handlers `legacy.libreoffice`, `docx_math.pandoc` →
  `docx_math.native`, `rtf.pandoc` → `rtf.libreoffice`, `markup.pandoc` → `markup.text`,
  `audio.asr` (the `NO_READER` audio row is gone), `video.ffmpeg`. `resolve` walks every row of a
  format and says which tools passed over an earlier row (`Resolution::passed_over`); the page
  shows such a format half-filled and its row's words say what the tool would add. LibreOffice
  is usable for a format only with the part it needs (Writer for `.doc`/RTF, Impress for `.ppt`,
  from the probe's launchers), so `.ppt` without Impress is `tool_missing`. On Windows the three
  tools' rows say "not read on Windows yet" (`TOOLS_NOT_RUN_ON_WINDOWS`), and the lesser rows
  read there.
- *LibreOffice* (`libreoffice.rs`, `libreoffice/profile.rs`): per run a private directory
  (0700) under `<data dir>/convert-spool` holding the input `in.<ext>`, `out/`, `tmp/`
  (`TMPDIR`) and a new profile seeded with `registrymodifications.xcu`; `--headless --norestore
  --nolockcheck --nologo --nodefault --infilter=<the format's import filter> --convert-to
  <ext>:<export filter>`, `SAL_USE_VCLPLUGIN=svp`; through `run.rs` on a pool slot with
  `LIBREOFFICE_TIMEOUT`, `LIBREOFFICE_MEMORY_LIMIT_BYTES` and 64 KiB of stdout; the result read
  back by its size against `MAX_UNCOMPRESSED_BYTES` and sniffed as what was asked for. `run.rs`
  grew `Tool::keep`: what it holds (the private directory) is let go of only once the tool has
  been reaped, so the directory is never removed under a dying `soffice.bin`. A `.doc` or `.ppt`
  (or an RTF without pandoc) becomes a docx/pptx read through the table again (`file/via.rs`),
  so its pictures, `json` and a docx-with-equations' trip to pandoc come for free; the answer's
  `tools` name `libreoffice` and what read the result. The slides → PDF half is
  `libreoffice::slides_pdf`; nothing calls it yet (below).
- *pandoc* (`pandoc.rs`): `--sandbox --data-dir=/dev/null/lmgw-no-pandoc-data -f <format> -t
  html5 --wrap=none --mathjax`, stdin to stdout, then lmgw's reader's web-page path. The probe
  runs a sandbox check (a LaTeX file `\input`s a canary beside it; a pandoc that prints it, or
  has no `--sandbox`, is `ProbeFailed` and reads nothing). The reader writes pandoc's
  `span.math` as `$…$`/`$$…$$` (`html/math.rs`) and drops empty links (pandoc's code line
  anchors).
- *ffmpeg and the model* (`media.rs`, `media/ffmpeg.rs`, `media/asr.rs`): a video spooled
  (`ocr/spool.rs`'s unlinked file), `-protocol_whitelist file -f mov|matroska -i
  file:/dev/stdin -map 0:a:0 -vn -sn -dn -ac 1 -ar 16000 -c:a pcm_s16le -bitexact -f wav
  pipe:1`, the WAV's lengths written in by the gateway; then one call through
  `policy_checked_call` and `transcribe_dictation` as the request's key (`Plan::speech`, read
  when the request is planned: `documents.asr_alias` and the `RequestCtx`). Audio goes up as it
  is. New error codes `model_not_chosen` (415), `model_not_allowed` (403), `model_held` (503),
  `model_failed` (502); new source `asr` (`BlockSource::Asr`, `PartsBySource::asr`);
  `stats.model_calls` counts the transcription. The docling adapter's `pipeline: asr` is
  `mapped` onto it.
- *Settings*: `documents.asr_alias` (`documents_asr_alias` in `settings_set`, the full save and
  `settings-full`, checked by `validate_stt_alias`), shown as "Model help › Speech-to-text model"
  (a model field of the draft); the Audio and Videos rows say "not transcribed: choose a
  speech-to-text model below" while it is empty (`media::page_note`, the check the route makes).
- *Chat attachments and knowledge bases* (`own.rs`): a file `extract::sniff` refuses is sniffed
  again by `sniff_convert`; `.doc`, `.ppt`, `.xls`, EPUB (and RTF that is no UTF-8) with a
  tier-0 handler are converted by `file::convert` (markdown, lmgw's own pool caller) and stored
  as an `office` attachment / one knowledge-base part, their format in `meta.format` / `sub`;
  a missing tool, a password or an Outlook message is refused in the table's words, anything
  else keeps the old refusal. Everything those two already read stays on its path.
- `scripts/convert-compare.py`: the four error codes, the formats, and `asr` in
  `parts_by_source` (read as 0 from a server without it); it already reads
  `documents_asr_alias` from `settings-full`. `ci/check.sh` names pandoc, LibreOffice and ffmpeg
  among the suite's tools.

**What changed for Chat and knowledge bases** (every existing Chat and knowledge-base test
passes unchanged; the changed expectations are the conversion's own: `file::tests`' WAV case,
`documents_convert` ×2 and `documents_jobs` ×1 expect `model_not_chosen` where they expected
`no_reader`, because audio has a reader now and no model is chosen in those tests). Gained: old Word,
PowerPoint and Excel files and EPUB books, read as the conversion reads them (LibreOffice /
lmgw's reader child); named refusals for a missing tool, a password and `.msg`. Unchanged: text
(HTML, e-mail, RTF and the markups included: they sniff as text there and stay text), PDFs with
Chat's page images and the knowledge base's vision pages, docx/xlsx/pptx/odf through `office.rs`
(a docx with equations too), pictures, audio with Chat's own speech-to-text model. No knowledge
base is re-indexed; a file re-ingested reads what it read before.

**Decisions:**
- `LIBREOFFICE_TIMEOUT` 300 s, `LIBREOFFICE_MEMORY_LIMIT_BYTES` 4 GiB (measured: 1 GiB dies of
  `SIGSEGV` at start, 2 GiB converts a deck to PDF), `PANDOC_TIMEOUT` 300 s,
  `PANDOC_MEMORY_LIMIT_BYTES` 4 GiB (pandoc 3.7's runtime starts within 1 GiB),
  `FFMPEG_TIMEOUT` 300 s, `FFMPEG_MEMORY_LIMIT_BYTES` 2 GiB, the longest sound
  `MAX_UNCOMPRESSED_BYTES` of 16 kHz mono WAV (9 h 19 min); all fit `PER_RUN_BYTES`.
- A new profile per run, never a template copy: 0.45 s a `.doc` against 0.29 s with a reused
  profile (0.16 s of profile creation; LibreOffice 26.8.2.2, 2026-11-19). The seeded settings:
  `DisableMacrosExecution` true, `MacroSecurityLevel` 3, `DisableActiveContent` true,
  `BlockUntrustedRefererLinks` true, `DisablePythonRuntime` true, Writer link update never (0),
  field update off, Calc link update never (1); they survive LibreOffice's first start (checked
  in the written profile). Crash leftovers (`lo-*` under `convert-spool`) are removed by the
  next process's first run.
- pandoc writes HTML, not markdown: its markdown writers put `[TABLE]` in place of a table
  whose cells hold blocks when raw HTML is off (seen with LibreOffice's RTF), so the HTML goes
  through lmgw's reader, which writes such tables and drops raw HTML's scripts and frames.
- RTF: pandoc first (design §4.2), LibreOffice where pandoc is missing (an addition beyond §4.2,
  which lists pandoc only); neither: `tool_missing` naming pandoc (main read RTF as its raw text).
- The text markups and a docx with equations fall back to lesser readings, with
  `tool_missing`/`equations_dropped` (D4 allows a second row). A docx's equations without pandoc
  are kept as their characters run together (E=mc2), as `office.rs` reads `m:t`.
- `docx+math` goes to pandoc whenever pandoc is found (D11: "when pandoc is present, it has
  exactly one handler"); its pictures are placeholders then (`not_yet` when pictures are asked
  for), where lmgw's own reader would export them.
- `documents.asr_alias` is empty until chosen (D5's rule), the Chat's speech-to-text model is not
  borrowed; a fallback that answered is named in a `model_held` warning (§7.5 asks that the
  answer name it; no `tools` entry for models exists yet). The row is labelled `openai`.
- Chat/knowledge bases take only what their sniffer refuses (the smallest step).
- `Format::Video` is sniffed as `Kind::Audio`/`video` (conversion's sniffer only; Chat's
  `Kind` stays as it was).

**Measured (2026-11-19, dev instance, debug build; `measure-before` for the text formats is
main's text passthrough measured with `--cmd 'main-text=cat {in}'`, doc/ppt were `no_reader` on
main (§14 WP0a), docx "before" is this build with pandoc hidden from `PATH`, which reads as
main):**

| format | before | after | s/file med. | 3-gram J | line ratio | headings | tables | cells |
|---|---|---|---|---|---|---|---|---|
| doc (2) | 0/2, `no_reader` | 2/2 | 0.863 | 0.995 | 0.991 | 2/7 | 1/1 | 12/12 |
| ppt (1) | 0/1, `no_reader` | 1/1 | 1.081 | 0.912 | 0.750 | 3/2 | 1/1 | 63/63 |
| rtf (1) | text: J 0.421, line 0.0, 0/1 tables | 1/1 (pandoc) | 0.034 | 0.976 | 0.889 | 0/0 | 1/1 | 4/2 |
| latex (2 `.tex`) | text: J 0.600, line 0.136, headings 0/7 | 2/2 (pandoc) | 0.043 | 0.841 | 0.825 | 6/7 | 1/1 | 9/12 |
| docx (38) | J 0.902, line 0.776 | 38/38 | 0.050 | 0.910 | 0.748 | 75/77 | 26/25 | 311/361 |
| docx, the 6 with equations | J 0.742 mean | pandoc | 0.15 (0.03 before) | 0.792 mean | lower on 5 | | | |
| audio (16, mock ASR) | `no_reader` / `unsupported` | 13/16 (3 `unsupported`: raw AAC, AVI) | 0.029 | no ref | | | | |

The equation files' line ratio drops where docling writes each formula as its own display
block and pandoc keeps them inline in their paragraph; the side-by-side case (`$a$$b$`) was
fixed after this run (`html/math.rs`), not measured again. The latex folder's 9 project
directories are not files the script sends.

```sh
LMGW_KEY=<owner token> python3 -I scripts/convert-measure.py --corpus "$CORPUS" --lmgw http://127.0.0.1:8884 \
  --formats doc,ppt,rtf,latex,docx,audio --per-file --out measure-after.json
python3 -I scripts/convert-measure.py --corpus "$CORPUS" --cmd 'main-text=cat {in}' --formats rtf,latex --out before.json
```

**D13 (2026-11-19).** `podman run --rm registry.fedoraproject.org/fedora:45`, `dnf install
--setopt=install_weak_deps=False ffmpeg-free` (ffmpeg-free-9.0.2-4.fc45, libavcodec-free
9.0.2-4.fc45), `ffmpeg -hide_banner -decoders`: 200 audio decoders, among them `aac`,
`libfdk_aac`, `mp3`, `mp3float`, `opus`, `libopus`, `vorbis`, `libvorbis`, `flac`, `ac3`,
`alac`, `truehd`, `dca`; **no `eac3`**. So AAC works with Fedora's ffmpeg-free; E-AC-3 (some
films' sound) needs RPM Fusion's ffmpeg. The probe's list says so on the page.

**Live check (2026-11-19, dev instance on 127.0.0.1:8884, prefix `lmgw-wp7`, a loopback mock
speech-to-text upstream).** With the tools: the `.doc` 200 markdown and json (`libreoffice`,
0.80 s; its OLE equations are pictures left out, as LibreOffice writes them), the `.ppt` 200
(slides, the table), the `.rtf` 200 (`pandoc`, `reader`), a docx with equations 200 with
`$E = mc^{2}$` and `$$\int_{0}^{1}x^{2}\, dx = \frac{1}{3}$$`, an `.rst` 200 (heading, table,
code); a WAV 415 `model_not_chosen`, then after `settings_set documents_asr_alias` 200 with the
mock's two lines as two `asr` paragraphs, `model_calls` 1; an MP4 (its index at the end) 200,
tools `ffmpeg`, the mock received a WAV; each call its own `request_logs` row (`example-asr`,
class `audio`, the caller's key) beside the conversion's; docling's `/v1/convert/file` with
`pipeline=asr` 200 and `x-lmgw-option-notes: pipeline=mapped(speech-to-text model)`; a Chat
attachment of the `.doc`: `office`, `meta.format` `doc`. With LibreOffice, pandoc and ffmpeg
hidden from `PATH`: `.doc`/`.ppt` 415 `tool_missing` naming `libreoffice-writer
libreoffice-impress`, `.rtf` 415 naming `pandoc-cli`, the MP4 415 naming `ffmpeg-free`, the
docx with equations 200 with `equations_dropped`, the `.rst` 200 as text with `tool_missing`;
Settings › Documents (screenshot at 125 %) shows the rows and the three tools under "Not
installed" and the "Model help" row. A knowledge-base ingest of a `.doc` is covered by
`tests/it/documents_media.rs` (no embedding model on the dev instance).

**Not yet:**
- Slides → PDF → vision: `libreoffice::slides_pdf` is built and tested through `convert`; the
  hook is in `file/via.rs`'s deck reading (`via::legacy` for `.ppt`, and the `office.native`
  reading of pptx/odp), where WP6's model help, when a request asks for pages and a page-reading
  model is chosen, renders the deck with it and sends the PDF's pages through `file/pdf.rs`.
  Until then `model_help` on a slide file answers what `main` answers (`model_not_chosen`).
- Network isolation for LibreOffice (§12: a namespace or `bwrap`); a reused read-only profile
  template (0.16 s a run).
- A document with an auto-run macro was not run here (the settings were checked in the written
  profile, not exercised by a macro).
- Pictures of a file read through pandoc (placeholders, `not_yet`); `.doc` equations (LibreOffice
  Math objects come out as pictures); RTF through LibreOffice when pandoc is there.
- Timestamps in transcripts; `document_timeout` for an audio file (no tool step starts its
  clock); raw AAC (ADTS) and AVI are not sniffed (`unsupported`); E-AC-3 with ffmpeg-free.
- Chat and knowledge bases: pictures (TIFF scans), videos, and moving the formats they read as
  text or office files (HTML, e-mail, RTF, markups, docx with equations) onto the table's
  readers, which would change what existing knowledge bases were indexed from.
- Naming the answering model in `tools`; a `Speech` for Chat's own conversion path.
- The "Not installed" row of LibreOffice squeezes its words beside its long command (the page's
  layout, unchanged).

**Unverified:** Windows (the three tools are not run there: the rows say so; the workspace
compiles here, left to the Windows compile check at landing); the server image's derived image with
LibreOffice (fonts, `install_weak_deps=False`); a real speech-to-text model (the checks used a
mock upstream); a model under the GPU hold and a fallback answering (tested by the code's
mapping, not live); LibreOffice at `LIBREOFFICE_MEMORY_LIMIT_BYTES` with a huge deck.

#### WP7 review round (2026-11-19, rebased onto WP6 at `16bf0783`)

**Built.**
- *On WP6* (R1–R9): main's `model_held`/`model_not_allowed`/`model_failed` with their docs;
  `ErrorCode::ModelNotChosen` (415) added for a file that needs a model none is chosen for. One
  "Model help" group on Settings › Documents: the speech row sits beside "Read pages with a
  model" (`DocPart::Speech`, `settings/documents/speech.rs`) and says, as that row does, whether
  the audio stays on this computer or goes to a named service and who transcribes whenever the
  model cannot run (`DocCaps.speech`, in `PageReader`'s shape, from `media/settings.rs`; its
  `answer`, `hints` and the fallback's `sees` say nothing about audio). `Speech` is built beside
  `Reader` at the same call sites. The per-call key check is one function now
  (`extract/convert/key_check.rs`, `model_help/call.rs::allowed` calls it): expiry, scope,
  budget, tokens-per-minute; requests-per-minute neither asked nor counted (S3). Rows are
  `documents-asr` (`ClientProto::DocumentsAsr`). `stats.models` names who answered and
  `answered_for` (`BoundFrom::OneCall`, `in_flight_bound` 1); a failed call that reached a model
  keeps its call in the failed file's stats. docling's `pipeline: asr` stays `mapped`.
- *B1: LibreOffice runs only confined.* `lmgw-doc-reader run [--read-exec P]… [--read P]…
  [--read-write P]… [--make-sock P]… [--deny-net] [--address-space N] [--file-size N] --
  /abs/tool args…` (reader crate: `sandbox.rs` the command line both sides share, `run.rs` the
  work; no parser on its path): close every descriptor above stdio (`close_range`), set
  `RLIMIT_AS` and `RLIMIT_FSIZE`, build a Landlock ruleset over every filesystem right of the
  kernel's ABI (refer ≥2, truncate ≥3, ioctl_dev ≥5, resolve_unix ≥9), TCP bind/connect ≥4 and
  UDP bind/send ≥10 with `--deny-net`, the abstract-socket and signal scopes ≥6;
  `PR_SET_NO_NEW_PRIVS`, `landlock_restrict_self`, then `execv` (same pid: the runner's process
  group and kill reach `soffice.bin`; tested). Exit 125 = not confined, tool not started; 126/127
  = not started/not found. `run --probe` applies an empty ruleset to itself and prints
  `landlock-abi N`. The gateway starts it through `run.rs` like the reader (`Tool::confine`,
  `run/confined.rs`, no `pre_exec`, `env_clear` + `PATH=/usr/bin:/bin` + the caller's variables),
  so every tool can move behind it; LibreOffice, pandoc and ffmpeg have. ABI on the test machine: **11**
  (Linux 7.3, LSM list `…,landlock,…`).
- *LibreOffice's allowlist* (`confine.rs`): read+execute `/usr` (and the install's top directory
  when `soffice` lives elsewhere: its own directory, plus its parent when that is `program`);
  read `/etc/ld.so.cache`, `/etc/fonts`, `/etc/passwd`, `/etc/group`, `/etc/nsswitch.conf`,
  `/dev/urandom`; read+write (no execute) the run directory and `/dev/null`; `/tmp`: make a socket
  only; TCP and UDP denied. Found by running it under `strace` with Landlock (it also asks for
  `/proc/*`, `/sys/*`, `/etc/dconf`, `/dev/tty`, Java's `/tmp/hsperfdata_*`: denied, harmless).
  The `/tmp` socket is LibreOffice's single-instance pipe (`osl/unx/pipe.cxx` picks `/tmp`
  whenever `access("/tmp", W_OK)` succeeds, which Landlock does not hook; `OSL_SOCKET_PATH` is
  read only when neither `/tmp` nor `/var/tmp` is writable; without the pipe it exits 1). A
  confined run cannot remove it, so the gateway removes it after reaping
  (`libreoffice/scratch.rs`: `/tmp/OSL_PIPE_<uid>_SingleOfficeIPC_<md5 of the profile URL in
  UTF-16, hex without leading zeros>`, checked against LibreOffice's own `bind`). A new profile
  makes LibreOffice restart once and the second start finds the first one's socket, so every
  run copies a **profile template** made once per process by a confined
  `--terminate_after_init` run (`lo-<pid>-profile`; 0.6 s a `.doc` against 0.86 s before).
  Without Landlock or without the reader, LibreOffice's probe fails naming it (`NEEDS_LANDLOCK`),
  so the table's rows and the route say `tool_missing` with that sentence; never an unconfined
  run.
- *pandoc and ffmpeg* run confined where the probe found the confinement working
  (`confine::helper`, the reader's probe at each tool check): `/usr`, the loader cache,
  `/dev/null`, no network; ffmpeg its run directory. Where it does not work they run as before.
- *S1*: ffmpeg writes the WAV into a private run directory (`ff-<pid>-…`, 0700, files 0600,
  removed on every path after reaping); `-fs` stops it at the smaller of
  `MAX_UNCOMPRESSED_BYTES` and the request's output budget left, `RLIMIT_FSIZE` 1 MiB above is
  the hard stop; the WAV is mapped (`media/ffmpeg/mapped.rs`), never copied into gateway
  memory, and counted against the output budget until the transcription returns.
- *S2/S4*: the transcription opens the gate itself in `proxy/transcribe.rs` (the file clock is
  paused while it waits), starts the file's clock at the call (`clock::started`), and a drop
  before its row writes the `499 client_disconnected` row from a task of its own with the route
  once the gate opened (`proxy/transcribe/dropped.rs`); `batch::within_deadline` cuts it.
- *S5* `audio.<sub>`; *S6* `RLIMIT_FSIZE` for LibreOffice (`LoError::FileTooLarge`, found by a
  file in the run directory at exactly the limit, since `oosplash` reports `soffice.bin`'s
  `SIGXFSZ` as its own exit status); *S8* `documents_asr_alias` in `OUTBOUND_SETTINGS`.
- Nits: N1 VBA `Load=false` for Writer, Impress and Calc; N2 directories named by pid, the sweep
  removes only those of processes that are gone (and their sockets); N4 MIDI `unsupported`; N5
  said in docs; N6 a knowledge-base job's cancel drops a running conversion; N7 `it-map.toml`;
  N9 by R4. The "Not installed" LibreOffice row: the words take at least 280 px and the command
  wraps below them.
- Tests: `confine/tests.rs` (outside file denied, inside read, write outside denied, a written
  file not executable, a non-CLOEXEC descriptor not inherited, TCP to a loopback listener denied
  with an unconfined control, `SIGXFSZ` → `FileTooLarge`, a missing program, the group kill
  through the reader), `libreoffice/tests.rs` (the RTF `INCLUDEPICTURE` probe embeds the file
  unconfined and gets nothing confined, for a file in another directory and one beside the run in
  the spool; a `.doc` with a picture link made before the file existed; the file-size limit; two
  real runs and the template socket gone; the stand-ins under the helper), `tools/tests.rs` (a
  LibreOffice that cannot be confined is not used), the reader crate's command-line tests,
  `tests/it/documents_media_policy.rs` (scope and budget 403 with 0 calls, rpm 1 converts, the
  upload's name, the hold with and without a fallback, `model_failed` with its call kept, a call
  cut by `document_timeout` → 504 and one `499` row), the guard test.

**Measured (2026-11-19, dev instance, debug build, `measure-review.json` against the build's
`measure-after.json`):** doc 2/2 J 0.9946 line 0.9906 tables 1 cells 12 (unchanged; median
0.63 s against 0.86 s), ppt 1/1 J 0.9121 line 0.75 cells 63 (unchanged; 0.57 s against 1.08 s),
rtf 1/1 J 0.9758 line 0.8889 (unchanged, pandoc now confined), docx 38/38 J 0.9103 (0.9096)
line 0.7477. The six docx with equations after the side-by-side formula fix: J mean 0.7923 →
0.7966 (`omml_multi_equation_paragraph` 0.7435 → 0.7699, the others equal), line ratio mean
0.4212 unchanged (docling writes each formula as its own block; pandoc keeps them inline).
Inside a rootless `podman run --rm registry.fedoraproject.org/fedora:45` with
`libreoffice-writer` installed (26.8.3.1, `install_weak_deps=False`): `run --probe` →
`landlock-abi 11`, a confined LibreOffice converts the RTF probe, a confined `cat` of a file
outside its rules is denied.

**Decisions.**
- Landlock through the reader executable (following the performance review's helper rule), not bubblewrap: `bwrap --unshare-all` cannot mount `/proc` or devpts in
  a rootless Podman container, where the server image runs.
- `/tmp` keeps one right for LibreOffice, making a socket (above); the gateway removes the socket.
- The profile template per process (made without a document) replaces "a new profile per run".
- pandoc and ffmpeg run unconfined where Landlock is missing (LibreOffice does not run there).
- The WAV is counted against the output budget while it exists, as planned.
- `DocCaps.speech` reuses `PageReader`'s shape rather than a type of its own.

**Not yet.**
- UDP is blocked from Landlock ABI 10 only, pathname Unix sockets from ABI 9; before that they
  are not. poppler, tesseract and the reader itself are not behind the helper yet (they keep
  `run.rs`'s `pre_exec` limit or their own); the later round moves them.
- Slides → PDF → the page-reading model (R10): `libreoffice::slides_pdf` is confined and tested,
  not wired. Reading a deck as its PDF changes what every deck converts to once a model is
  chosen (the default `missing_or_tables`), not only which pages a model sees, and WP11 is
  reshaping `model_help/` now; the hook stays `model_help/pdf.rs::read_pages` fed by a PDF the
  deck path renders.
- Fonts in a home directory are not visible to a confined LibreOffice; the container check did
  not reproduce the RTF leak unconfined (that LibreOffice without weak dependencies did not embed
  the picture either), so there it shows the confinement applies, not the leak closed.
- Streaming a WAV to a cloud upload without mapping it (the multipart path takes `Bytes`).

**Unverified.** Windows (no confinement there; LibreOffice, pandoc and ffmpeg are not run there
anyway; left to the Windows compile check at landing); the server image itself (no LibreOffice in it; the check used
a plain `fedora:45`); a kernel without Landlock (only the probe's error path through a missing
reader is tested); a real speech-to-text model (mock upstreams and the fake GPU world).

#### WP7 on WP11 (2026-11-19, rebased onto WP11's review round at `d6887b5e`)

**What changed.**
- **One key check.** `key_check.rs::may_call` is the per-call check of every model call a
  conversion makes: `model_help/call.rs::allowed` (asked by `turn.rs` for a page's read and a
  picture's description, and by `describe/room.rs`) and `media/asr.rs`. WP11's `turn.rs::By`
  maps `BoundFrom::OneCall` like the tool pool (a page or picture call is never one).
- **Model help** on Settings › Documents is one group of three rows in the same pattern: "Read
  pages with a model", "Describe pictures with", "Transcribe audio with" (WP7's row was
  "Speech-to-text model"; the label, the search terms, the `model_not_chosen` message and the
  docs now say "Transcribe audio with"). Its `where_said` uses `model.rs`'s `outside_said`.
- **Settings.** `/api/settings-full` carries `documents_page_reader`, `documents_picture_alias`
  and `documents_asr_alias`; `convert-compare.py` reads the three by these names and its Rust
  test checks all three against the wire, with the union of the codes (18 error codes).
  `OUTBOUND_SETTINGS` holds the six outbound settings; the picture and speech-to-text models
  share one guard test (the page reader keeps WP6's), and the refusal names all three models.
- **Usage labels.** `documents-page`, `documents-picture`, `documents-asr` are listed together
  where the docs introduce the route's `documents` row. No page of the dashboard lists labels
  by name (Traffic shows the row's own); the callers summary reads the docling options, not
  labels, so it needed nothing.
- **Pictures through LibreOffice.** An old `.doc`/`.ppt` is read through the docx/pptx
  LibreOffice makes, so its pictures are exported, described and below `describe_min_area`
  counted as a docx's or pptx's (no code change; a test with the real LibreOffice, skipped by
  name without it or without Landlock). A file read through pandoc with picture placeholders
  gets a `not_yet` note (option `describe_pictures`) in every output, text included; an audio
  file `option_not_applicable`, a video `not_yet` (its frames).
- **Stats.** `stats.models[]` counts each source of model work: `parts_read` (pages),
  `pictures_described` (pictures), `files_transcribed` (new, a transcription; WP7 counted it as
  `parts_read`). Main's rule for zero counts is "always written" (`pictures_described`,
  `pictures_below_describe_min_area`), so `parts_by_source.asr` is now always written too and
  `files_transcribed` is; WP6's two tests that compare `parts_by_source` whole gained `"asr": 0`.
- **The docling table**: WP11's `options/picture.rs` and WP7's `pipeline: asr` (`mapped`,
  speech-to-text model) both as they were.

**Decisions.**
- An e-mail keeps one clock: a transcription does not take the message's end over
  (`FileClock::hand_over`), since it keeps nothing partial. Before a picture or page stage took
  it over, the batch ends the message at its deadline as before (the call dropped, a `499` row);
  after, `file/more/late.rs` ends a recording still being heard at the deadline and starts no
  attachment after it, each named in the one `document_timeout` note, the call's `499` row
  written.
- The changed WP6 test expectations above (`"asr": 0`) are the rule's consequence, not WP6
  behaviour.

**Found on main, not changed here:** a pptx whose slide has a title shape and a picture (as
LibreOffice writes one, and so every `.ppt` read through it) keeps the picture's marker inside
the title's paragraph in `output: json` (the text ends in U+FFFC, `0`, U+FFFC; no `figure`
block); markdown writes `<!-- image -->` on its own line and the picture and its description
are in the `pictures` list (WP10's office reader).

**Not yet.** An attachment ended at the deadline does not add its call in flight to the
message's `stats` (its `499` row is written), for pictures as for recordings; descriptions of
pictures in web pages, EPUBs and files read through pandoc; a video's frames.

**Tests:** `tests/it/documents_media_pictures.rs` (a `.doc` and a `.ppt` made here by
LibreOffice, described with the mock describer and `describe_min_area` 0.9 leaving both
pictures undescribed; an audio file and a LaTeX file through pandoc with `describe_pictures`;
an e-mail with a pictured docx and a recording, its stats per model; the same with a recording
heard for 4 s after the picture stage and a third attachment, `document_timeout` 1.5: partial
in under 3.5 s, both named, one `499` `documents-asr` row), the merged guard test, the
script test over the three settings.

**Measured, no model chosen** (2026-11-19, dev instance 127.0.0.1:8884, debug build, poppler
26.08.0, LibreOffice 26.8.2.2, pandoc 3.7.0.2): `convert-measure.py --formats
pdf,doc,ppt,rtf`: pdf 28/28 J 0.8569 line 0.2698 headings 167/159 tables 39/31 (main's); doc
2/2 J 0.9946 line 0.9906 tables 1 cells 12; ppt 1/1 J 0.9121 line 0.75 cells 63; rtf 1/1 J
0.9758 line 0.8889 (WP7's review-round numbers). Settings › Documents at 125 % (headless
Chrome, `ui-drive.py --dsf 1.25`, 1440 and 1000 px wide) with a cloud vision and a cloud
speech-to-text alias chosen: the three rows in one "Model help" group, nothing overlapping.

**Unverified.** The Windows build (checked at the end of this round, see its commit); a real
vision and a real speech-to-text model together on one e-mail.

### Performance round (2026-11-19)

**Built (2026-11-19, on `main` at `f1919ece`), from the performance review's eleven findings.**
Measured on the release build (`lto = true`, `codegen-units = 1`) of `examples/headless` and
`lmgw-doc-reader`, started as `scripts/dev-instance.sh` starts the debug one (`LMGW_DEV=1`,
`127.0.0.1:8888`) in a cgroup scope of its own, its data dir on a real disk (not
`/tmp`); a 16-core test machine with 128 GiB of memory, Linux 7.3 (Landlock ABI 11),
glibc 2.44, poppler 26.08.0, tesseract 5.5.4 (`eng`, `deu`), GPU idle, model help off. "Before"
is `main` (`f1919ece`) built the same way, measured in the same session; the review's own
baseline (`16bf0783`, tmpfs data dir) is kept below for reference.

How to reproduce: `scripts/convert-perf/` (`gw.sh start <binary> [16G]` starts the release
headless example with its data dir on the disk, `seq.sh TAG` runs the fixed sequence below,
`m16.sh TAG` the 16 GiB rows, `mem.sh TAG` the memory rows, `jobs.py` one async job, `table.py`
puts runs side by side). The stress files are the review's (`gen_md.py`, `gen_pics.py`,
`gen_dense_pdf.py`, `gen_misc.py`, `img2pdf.py` beside them made them), kept where `PERF_IN` points (`~/.local/share/lmgw-corpora/convert-perf-2026-11-19/` by default; `scripts/convert-perf/gen_*.py` and `img2pdf.py` recreate them); answers checked with
`scripts/convert-same.py` (every answer of the 20 corpus PDFs in `pdf/sources`, the 8 in `pdf/`,
`pics100.pdf` and `text50.pdf`, in `text`, `markdown` and `json` and every picture mode, hashed
without `time_ms`): **270 of 270 identical** to `main` after item 1 and after items 1–5.

**The sequence** (one instance from fresh, rows in this order; each cell
wall s / gateway CPU s / children CPU s / gateway peak RSS over start MiB / cgroup peak over start
MiB; the cgroup's peak includes the spool's page cache since item 1):

| request | main | item 1 (+ spool) | items 1–5 |
|---|---|---|---|
| text5 | 0.10/0.02/0.19/5/13, 0.07/0.02/0.08/0/29 | 0.09/0.01/0.21/5/13, 0.06/0.02/0.08/0/28 | 0.07/0.02/0.20/4/8, 0.04/0.00/0.07/1/10 |
| text50 | 0.25/0.03/0.75/4/48 | 0.25/0.03/0.69/4/52 | 0.14/0.03/0.31/4/66 |
| small50 | 0.82/0.60/4.42/15/62, 0.97/0.67/3.99/14/60 | 0.29/0.24/4.85/22/91, 0.29/0.24/4.77/14/75 | 0.29/0.26/5.19/29/86, 0.29/0.24/5.06/9/82 |
| small50 x8 callers | 8.18/12.08/34.87/71/115 | 2.10/1.95/35.90/77/137 | 2.14/2.06/36.50/78/130 |
| corpus 20 pdfs one req | 4.74/1.14/9.82/55/184 | 2.40/0.25/9.92/47/455 | 1.40/0.28/10.62/51/465 |
| text500 | 2.16/0.24/2.57/42/52 | 2.15/0.20/2.57/34/50 | 1.18/0.20/2.64/42/68 |
| text500 text | 2.13/0.21/1.97/36/50 | 2.13/0.20/1.99/41/58 | 1.16/0.20/2.02/40/68 |
| text5k | 15.29/1.45/18.78/306/311 | 15.37/1.40/18.83/295/312 | 8.39/1.42/19.21/304/324 |
| dense200 | 3.55/0.74/3.29/149/180 | 3.50/0.66/3.27/169/213 | 3.09/0.70/3.33/212/230 |
| bigtable | 1.65/0.27/1.98/20/66 | 1.62/0.21/2.05/24/98 | 0.98/0.21/2.01/39/122 |
| pics100 md | 1.22/1.07/4.03/54/619 | 0.62/0.13/5.03/6/621 | 0.53/0.15/5.29/1/618 |
| pics100 embedded | 11.82/3.15/13.60/199/610 | 9.47/0.39/13.71/174/675 | 1.36/0.41/15.71/175/739 |
| scan20 | 3.20/1.34/33.30/160/1386 | 2.39/0.64/36.45/171/1422 | 2.39/0.66/37.20/234/1486 |
| big200 ocr off | 1.41/0.68/1.97/209/693 | 1.30/0.21/1.95/240/978 | 0.95/0.18/1.83/238/1233 |
| html200 | 0.07/0.05/0.27/1/15 | 0.06/0.05/0.25/0/14 | 0.06/0.05/0.25/0/17 |
| big.xlsx json (inflate) | 2.94/2.79/0.09/553/559, 2.94/2.81/0.00/550/556 | 2.94/2.80/0.00/547/549, 2.77/2.65/0.00/492/499 | 2.78/2.64/0.00/487/490, 2.80/2.67/0.00/527/533 |
| text5 @high rss | 0.25/0.11/0.17/0/9, 0.24/0.10/0.17/0/9 | 0.06/0.00/0.08/0/6, 0.06/0.01/0.06/0/7 | 0.04/0.01/0.08/0/11, 0.04/0.00/0.07/0/10 |
| small50 @high rss | 9.83/4.98/8.84/1/54, 9.74/4.93/8.78/0/52 | 0.28/0.21/4.40/0/66, 0.28/0.22/4.42/0/68 | 0.29/0.22/4.52/0/75, 0.28/0.22/4.51/0/71 |
| scan20 @high rss | 4.28/1.98/31.54/162/1160 | 2.41/0.66/36.46/165/1416 | 2.32/0.63/36.52/165/1413 |

The "@high rss" rows run after the two `big.xlsx` rows raised the gateway's RSS (to 991 MiB on
`main`): `small50` there went from 9.7–9.8 s to 0.28 s, the fork's cost growing with the
gateway's memory. Item 1 alone: `small50` 0.82–0.97 → 0.29 s, eight callers at once 8.2 → 2.1 s
(gateway CPU 12.1 → 2.0 s), the corpus' 20 PDFs in one request 4.7 → 2.4 s, `pics100.pdf` as
placeholders 1.22 → 0.62 s. Items 3 and 4 on top: `pics100.pdf` embedded 9.5 → 1.4 s (11.8 s on
`main`), `text500.pdf` 2.15 → 1.18 s, `text5k.pdf` 15.4 → 8.4 s, the corpus 2.4 → 1.4 s,
`big200.pdf` 1.30 → 0.95 s.

**Item 2, the pool derived for 16 GiB** (`MemoryMax=16G` on the scope, a 32-thread test machine): `main` derived 2
slots (16 GiB over 6 GiB); now 32 steps of which 7 pages of text recognition, 2 PDFs or 3 reader
runs fit at once (Settings › Documents says so).

| request (16 GiB) | `main` | now |
|---|---|---|
| `scan20.pdf`, OCR, 20 pages | 13.6 s, 13.6 s | 4.5 s, 4.5 s |
| `pics100.pdf`, placeholders | 2.53 s | 1.60 s |
| `pics100.pdf`, embedded | 12.1 s | 5.0 s |
| `small50` (50 PDFs) | 3.27 s | 1.84 s |
| `text500.pdf` | 2.14 s | 1.17 s |

Settings › Documents › Parallel tool runs (expert view, checked at 125 % on a dev instance): "32 at
once: 32 from 32 cores, as far as 126 GiB of memory holds them. Each step counts the most memory
its tools may use: a PDF's text 6.0 GiB (20 at once), a page of text recognition 2.2 GiB (32 at
once), a web page, e-mail or picture read by lmgw's reader 5.0 GiB (25 at once)."

The cgroup's peak for `scan20.pdf` rose from 236 to 592 MiB over start (seven pages at once against
two), well inside the 16 GiB the charges hold it to.

**Item 5, an async job of 200 HTML files** (data dir on the disk): 1.22–1.24 s → 0.13–0.18 s to
its end (the same request in sync mode is 0.07 s); polls 1–1.5 ms.

**Decisions:**
- *Every foreign tool starts through lmgw's reader* (`run/start.rs`, `lmgw-doc-reader run`), also
  where nothing confines it: the reader's new `--limits-only` closes the descriptors and sets the
  limits, so no `pre_exec` hook is left on the conversion path and std spawns with
  `clone3(CLONE_VM|CLONE_VFORK)` (checked with `strace -f -e trace=clone,clone3,fork,vfork` on the
  release gateway: four `CLONE_VFORK` clones for a 9-page PDF, no fork). Without the reader no tool
  starts: every handler with a tool needs `ToolId::Reader` in `caps.rs` (one named state on
  Settings › Documents and the route, `tool_missing`, "install lmgw again"), and a run that still
  gets there is `RunError::NoReader`. The tool probes (`probe.rs`, once per tool change, not per
  conversion) still start their tools directly, with the hooks: not moved yet.
- *A cleared environment* for every helper-started tool (the security review's L4): `PATH`
  (`/usr/bin:/bin`), what the caller sets, and `TESSDATA_PREFIX` when the gateway has it
  (`start::PASSED_ON`). The corpus answers are unchanged with it.
- *poppler and tesseract run confined* where Landlock works (`confine::poppler`, `confine::tesseract`:
  `/usr` read+execute, `/etc/ld.so.cache`, `/etc/fonts` for poppler, tesseract's language data when
  outside `/usr`, `/dev/null`, a layout run's private directory read+write, the tool's own install
  prefix when it is outside `/usr`; no home, no data dir, no network). Their tests and the corpus
  answers stayed identical, so it applies. A PDF that does not embed a font is rendered with the
  system's fonts only: fonts a user installed in a home directory are not seen. The reader's own
  parsing runs apply a ruleset that allows nothing (no file, no network) after their limits, where
  the kernel has Landlock. Where Landlock is unavailable all of them run as before (limits, own
  group, closed descriptors); only LibreOffice refuses.
- *poppler is told `fd://0`*, never `/dev/stdin`, when its stdin is a spool file: poppler reads the
  descriptor by position, and a confined run may not open the spool's path again (the security
  review proved `/dev/stdin` refused under the rules). **Wrong, corrected in the review round
  below:** poppler 26.08 reads an `fd://` input to its end into memory, as it reads a pipe; it is
  `/dev/stdin` again, with a read rule on that one file (`--read-stdin`).
- *A PDF is spooled once, before its first run, whatever its size* (a file under the data dir,
  not a memfd: 26–36 ms against 79–87 ms for 215 MiB on the test machine's disk, the runs read both alike, and a
  memfd would be a second copy of the upload in memory). Picture export spools the file once more
  for its crop renders (the reader's spool went with its text): one write, not a pipe a page.
- *The pool's size is the cores; memory bounds the steps by their charges* (`pool/charge.rs`), the
  heaviest step's charge for every step that names none. `documents.tool_slots` keeps its old
  meaning: that many steps at once, memory not counted (the owner's figure replaces both inputs).
- *pdftoppm's OCR render limit lowered to 2 GiB* (`OCR_RENDER_MEMORY_LIMIT_BYTES`, named in its
  warning) on the review's measurement (a page peaks at 27–54 MiB) and one of this round (a
  100-million-pixel page, the largest recognition renders, fits 384 MiB of address space), so a page
  of text recognition is charged 2.2 GiB, not 4.2.
- *Crops side by side keep the one-after-the-other result* (`pictures/render/side.rs`): a page's
  crops are kept when the reader made all of them and they fit what is left at the page's turn,
  else the page is cut again in its turn with that; so which crops fit the output depends on the
  document's order alone. Crop renders are PPM (the reader reads both; the crops are the same bytes).
- *Geometry runs side by side only when two slots are free at once* (`try_acquire_charged`); a file
  never waits for a second slot.
- *A job's batch* is every file done by the time the last batch was recorded (no count of its own).
- *glibc's thresholds fixed at 1 MiB* (`alloc_tune.rs`, an explicit `MALLOC_*_THRESHOLD_` wins);
  no trim after a request yet.

**Confined now:** LibreOffice (only confined, as WP7), pandoc, ffmpeg, the five poppler tools,
tesseract, and lmgw's reader's own parsing runs (a ruleset that allows nothing), each where the
kernel has Landlock. Not confined yet: the tool probes (`probe.rs`, which also still fork; once per
tool change).

**Corpus with no model chosen** (`python3 -I scripts/convert-measure.py --corpus "$CORPUS" --lmgw
http://127.0.0.1:8888 --formats pdf,html` and `ocr,doc,ppt,rtf` against the release build of
items 1–5): pdf 28/28, J 0.8569, line ratio 0.2698, headings 167/159, tables 39/31; html 33/33,
J 0.9422, line ratio 0.8494; doc 0.9946/0.9906, ppt 0.9121/0.75, rtf 0.9758/0.8889; ocr 10/10,
J 0.7856, line ratio 0.400 with `osd` through a `TESSDATA_PREFIX` outside `/usr` (the confined
tesseract reads it), 0.6728 without `osd` on `main` and now alike. All equal `main`. The median
time per PDF of the corpus was 0.038 s (the review's 0.191 s had the fork's cost in it).

**The gateway's memory after large conversions** (`mem.sh`: `text5k.pdf`, `pics100.pdf`
embedded, `scan20.pdf`, `big200.pdf`, `big.xlsx` as json twice, then idle). No conversion keeps
anything: malloc's own count (`malloc_info` through gdb) after the sequence is 7–8 MiB in use, and
the rest of what the process holds is free memory in glibc's 32 arenas (628 MiB of 636 on
`main`). glibc raises its mmap and trim thresholds whenever a large block is freed, so later
blocks up to 32 MiB come from the arenas and stay there; fixing both at 1 MiB (`alloc_tune.rs`)
lets large blocks go back when they are freed:

| after the sequence | idle RSS | malloc: held / free / in use | after `malloc_trim(0)` |
|---|---|---|---|
| `main` (two runs) | 511, 632 MiB | 506 / 498 / 7, 636 / 628 / 8 MiB | 292, 252 MiB |
| `main` with `MALLOC_MMAP_THRESHOLD_` and `MALLOC_TRIM_THRESHOLD_` at 1 MiB | 330 MiB | 259 / 251 / 8 MiB | 86 MiB |
| this round without the thresholds, with them | 643, 526 MiB | 633 / 625 / 7, 455 / 447 / 7 MiB | 402, 85 MiB |

Run to run the idle figure moves by about 100 MiB (which thread's arena a block lands in); the
fixed thresholds take 100 to 180 MiB of it, with no conversion measurably slower. What stays is free
memory between live blocks, which only `malloc_trim(0)` gives back; a trim after a large request
is not done yet (measured above what it would return). The review's own figures (77 MiB fresh,
0.4–1.2 GiB after ordinary conversions) were this retention, not a cache or a leak.

**Left, and why:**
- *Finding 6* (placeholder picture runs spanning pages without images, hashing the runs' output
  as it streams): not built yet. After items 1 and 5 `pics100.pdf` with placeholders is 0.53 s
  (1.22 s on `main`); its 37 runs now read the spool instead of 27 MiB each through a pipe.
- *Finding 7* (several layout-model sessions): not built yet. A file's pages are inferred one
  after the other in the model stage (`layout_model/stage.rs`), so more sessions help only several
  files at once; for one file the stage's per-page deadline bookkeeping would have to run pages
  side by side first. Not measured.
- *Finding 8* (a JSON body held three times): not built yet; it touches the wire type
  (`ConvertFile` borrowed) and the route, more than a local change.
- *Finding 9's zip* (the docling `zip` built in memory, about 4.8 times its size at the peak):
  not streamed yet; only the job's recording and kept outcomes changed.
- *Finding 10* (an e-mail's attachments side by side): not built yet. A PDF, an old Office file
  or a video attached charges the request's budget inside its read, and an attachment's model
  stage takes the message's clock over (`file/more/late.rs`); reading them side by side would need
  a budget of each attachment's own to keep both rules order-stable. 40 attachments took 1.01 s.
- *Finding 11* (spreadsheets read inside the gateway, `output: json` nine times the markdown):
  a note, as asked. A 10.7 MiB xlsx (100,000 × 20 cells) is 2.8–2.9 s, a 158 MiB json answer,
  gateway peak +490–550 MiB over start; office readers have no memory limit of their own beyond
  `MAX_UNCOMPRESSED_BYTES`, and their working memory now goes back to the system (above).
- No item was reverted: each gain reproduced.

**Unverified:** the server image and the RPM were not built in this round (the reader they ship
is the one the tests and the dev instance start); Windows is checked to compile, its spawn path
unchanged (no tool runs there yet).

**The review's baseline** (`16bf0783`, tmpfs data dir, 20 slots; kept for reference):

| file, request | wall | gateway CPU | children CPU | gateway peak RSS over start | children |
|---|---|---|---|---|---|
| `text5.pdf` 9 p, markdown | 0.07 s | 0.01 s | 0.07 s | 3 MiB | 4 |
| `text50.pdf` 72 p, markdown | 0.26 s | 0.03 s | 0.31 s | 4 MiB | 4 |
| `text500.pdf` 715 p, markdown | 2.2 s (3.1 ms/p) | 0.24–0.27 s | 2.65–2.70 s | 47–50 MiB | 4 |
| `text500.pdf`, text / json | 2.2 / 2.3 s | 0.24 / 0.25 s | 2.0 / 2.7 s | | 2 / 4 |
| `text5k.pdf` 5005 p, markdown | 15.7–15.8 s | 1.5 s | 19.2 s | 279–308 MiB | 4 |
| `dense20.pdf` / `dense200.pdf` | 0.43 / 3.9 s | 0.09 / 0.82 s | 0.39 / 3.6 s | 20 / 141 MiB | 4 |
| `bigtable.pdf` 295 p | 1.7 s | 0.28 s | 2.05 s | 41 MiB | 4 |
| `pics100.pdf` text / placeholders | 0.56 / 0.9–1.0 s | 0.08 / 1.0–1.1 s | 0.5 / 4.0–4.7 s | 24 / 44 MiB | 2 / 41 |
| `pics100.pdf` embedded or referenced | 11.6–11.9 s | 3.0–3.2 s | 13.7–14.4 s | 204–210 MiB | 188 |
| `scan20.pdf` OCR (20 p) | 3.1–3.9 s | 1.1–1.4 s | 37–49 s | 173–205 MiB | 43 |
| `scan200.pdf` OCR (200 p) | 32.8 s (6.1 p/s) | 15.9 s | 390 s | 288 MiB | |
| `big200.pdf` 215 MiB, multipart, `ocr: off` | 1.5–1.7 s | 0.7–0.9 s | 2.0–2.4 s | 211 MiB | 4 |
| same as JSON base64 | 2.0–2.4 s | 1.2–1.5 s | 2.3 s | 570–670 MiB | 4 |
| 50 × `text5.pdf`, one request, fresh gateway | 0.74–1.2 s | 0.56–0.83 s | 4.0–5.2 s | 13–17 MiB | 200 |
| 200 HTML files, one request | 0.07–0.13 s | 0.06–0.08 s | 0.25–0.37 s | 0 | 200 |
| corpus, 20 PDFs (105 p), one request | 4.7 s | 1.0 s | 9.9 s | 46 MiB | |
| layout model on, `text50.pdf` | 32.3 s (0.45 s/p) | 111 s | 1.4 s | 450 MiB | |

#### Review round (2026-11-19)

**Built** on the performance round's branch from its review (blockers B1, B2, should-fix S1–S7, nits), each with a test that fails without it (B1, B2 and S3
checked by undoing the fix: the crop test hangs to its 120 s timeout, the spool test runs out of
memory, the geometry test waits for the 60 s font run).

- **B1, side-by-side crops hung the request and the pool.** `pictures/render/side.rs`: a page cut
  again in its turn is cut on the slot it already holds (`render_cut_on`), never on a new one; at
  most as many pages are in hand (running, or done and not taken) as the pool admits PDF steps by
  memory (`pdf_file.fit`, the size where memory is not counted); and while a page is cut again the
  pages after it are still driven (`drive`, their results kept in order), so none of them stalls
  on a full pipe while its clock runs. N2 with it: a page cut with exactly what is left at its turn
  is not cut again, a failed cut with a larger budget is. The same shape was looked for in every
  side-by-side path of the round: the geometry pair takes its second slot only with
  `try_acquire_charged` (no wait while holding); OCR pages and picture runs acquire inside their
  own futures and their consumers do synchronous work only; the layout render gives its slot back
  before the model's inference waits for its slots; a job's batch takes no slot. A test pool
  (`pool::test_pool::small`: 8 slots, memory for 1 or 2 PDF steps) runs the crop path (a recut at
  the second page), four picture PDFs at once, and three scanned PDFs' recognition at once.
  *Live* (`scripts/convert-perf/hang.sh`, release build in a `MemoryMax=16G` scope: 32 slots, 2
  PDF steps fit): `pics100.pdf` with `pictures=embedded` and `images_scale=12`, whose 1 GiB output
  runs out mid-file, answered 200 in 24.6 s (1023.8 MiB of answer; before the fix it never
  answered and the pool stayed at `running 2, waiting 4` with no child); another caller's one-page
  PDF sent 5 s into it answered 200 in 0.73 s (the pool meanwhile `running 2, waiting 0`, 12 GiB
  charged), behind the PDF steps at the queue's head (N9).
- **B2, `fd://0` made poppler read the whole file.** poppler is told `/dev/stdin` again; a confined
  run's rules carry `--read-stdin`, which the reader turns into a Landlock file rule on the inode
  descriptor 0 is (after `fstat`: a regular file, else the run is refused), read right only. Each
  run still gets an open file description of its own (`Spool::input`), so side-by-side runs share
  no offset; a confined `sh` cannot read another file in the spool directory, nor open its stdin
  for writing (test `a_confined_run_reaches_its_own_spool_alone_and_only_to_read`). The spool is
  made `0400` once written (N5), so a tool without Landlock cannot write it through `/dev/stdin`
  either. One run of `pdftoppm -gray -r 300 -f 7 -l 7` on `big200.pdf` (215 MiB) through the
  reader with poppler's rules: 34–35 MiB resident and 0.03–0.04 s by `/dev/stdin`, 272 MiB and
  0.30–0.33 s by `fd://0` (three runs each, `/usr/bin/time`). The test
  `a_confined_poppler_reads_its_spool_by_position` reads a 96 MiB padded PDF under an 80 MiB
  address space. The module doc (`ocr/spool.rs`), the call above and `docs/documents.md` say so.
  *Rows of the round's tables that read large PDFs, re-measured* (release build, same machine,
  same scripts; cells as in the sequence table: wall s / gateway CPU s / children CPU s / gateway
  peak RSS over start MiB / cgroup peak over start MiB):

  | request | items 1–5 | review round |
  |---|---|---|
  | pics100 md | 0.53/0.15/5.29/1/618 | 0.28/0.14/1.88/36/158 |
  | pics100 embedded | 1.36/0.41/15.71/175/739 | 0.76/0.46/6.54/128/233 |
  | scan20 | 2.39/0.66/37.20/234/1486 | 2.25/0.58/36.45/163/1414 |
  | big200 ocr off | 0.95/0.18/1.83/238/1233 | 0.30/0.17/0.30/212/440 |
  | scan20 @high rss | 2.32/0.63/36.52/165/1413 | 2.30/0.65/36.55/92/1345 |

  The other rows of the sequence moved within their noise (`text5k` 8.39 → 7.77 s, the corpus'
  20 PDFs 1.40 → 1.32 s, `small50` 0.29 → 0.31 s). At 16 GiB (`m16.sh`): `scan20.pdf` 4.5 s
  (unchanged), `pics100.pdf` placeholders 1.60 → 0.78 s, embedded 5.0 → 2.98 s, `small50`
  1.84 → 1.95 s, `text500.pdf` 1.17 → 1.18 s.
- **B2's one changed answer, a decision.** `scripts/convert-same.py` over the corpus' 28 PDFs,
  `pics100.pdf` and `text50.pdf` (every output and picture mode, debug builds of `main` and of
  this round): **261 of 270 identical**; the 9 that differ are all of
  `pdf/sources/2203.01017v2.pdf`. poppler 26.08 reads that file from a pipe (or `fd://0`) short:
  `Syntax Error (7139328): Unexpected end of file in flate stream` (the file is 7,215,177 bytes),
  and page 1 loses its rotated arXiv line and more. By path, as `/dev/stdin` is now, it reads the
  whole file, as `pdftotext file.pdf` does. Every other corpus PDF gives the same bytes either way
  (`pdftotext -bbox-layout` and `pdftohtml -xml` compared by pipe and by `/dev/stdin`). So the
  corpus moves by that file alone: pdf 28/28, J 0.8569 → 0.8570, line ratio 0.2698 → 0.2701,
  headings 167/159, tables 39 → 38 (of 31 in the reference); html 33/33, 0.9422 / 0.8494; doc
  0.9946/0.9906, ppt 0.9121/0.75, rtf 0.9758/0.8889; ocr 10/10, J 0.7856, line ratio 0.400 with
  `eng`, `deu` and `osd` through a `TESSDATA_PREFIX` outside `/usr` (confined, the directory
  passing S2's check). All equal the round's figures but pdf's. *Decision:* the whole file read is
  kept, being the right reading; `main`'s answer for that file was poppler's short read.
- **S1, a tool's install prefix.** `confine/reach.rs`: a tool outside `/usr` gets its binary's
  directory and, for a `bin` directory, the prefix's `lib`, `lib64`, `libexec` and `share`, each
  only when it is not `/`, not a home directory or one holding a home (`$HOME`, `/root`, anything
  one level under `/home` or `/var/home`), and not a directory holding lmgw's data dir (the one it
  runs with, registered at `AppState` start, and the installed one), `lmgw`/`lmgw-dev` settings or
  cache dirs; never the prefix itself. So `~/.local/bin/pdftotext` gets `~/.local/bin` and
  `~/.local/lib`, not `~/.local/share`. A binary whose own directory fails the check (one directly
  in a home) runs with its limits alone, named on Settings › Documents. LibreOffice keeps WP7's
  rule (refused by name when it cannot be confined). *Decision:* a binary in a Nix-style store
  (`/nix/store/…`) gets `/nix/store` read and execute (its libraries and loader are other store
  paths; the store holds packages, readable by every user), so the exec can work; not run on a Nix
  system (none here): if it still fails, its confined check run (S5) names it and it runs not
  confined.
- **S2, `TESSDATA_PREFIX`.** tesseract's language data outside `/usr` is a read rule only when the
  directory holds `*.traineddata` and passes the same check; otherwise tesseract runs not confined,
  named with the reason. *Decision:* the directory, not a rule per `.traineddata` file (tesseract also
  reads `configs/` and `tessconfigs/` under it).
- **S3.** `file/pdf/runs.rs`: the bbox and font runs side by side under `select`; a failed bbox run
  drops the font run at once (its group killed, its slot kept by its lease until it is reaped),
  with the same `unreadable` failure. N3 with it: after the side-by-side runs the 2-slot lease is
  cut to one slot charged `PDF_FILE` for the layout pass (`Slot::shrink`, only when no lease of it
  lives).
- **S4.** A confinement the reader cannot apply at run time (exit 125) is `RunError::NotStarted`
  and the file's new code `tool_not_started` (500), worded "lmgw's document reader could not confine
  pdftotext or set its limits, so pdftotext was not started (tool_not_started); the gateway's log
  says why": the tool by its file name, no path; the reader's line with its paths goes to the log.
  The wire list (`documents_model_review.rs`) and `convert-compare.py`'s codes have it.
- **S5.** The Landlock probe's answer stands for the process only when it is the kernel's (an ABI,
  or the reader's own 125 line); a timeout, a failed start or a stray answer is asked again at the
  next tool check (start, "Check again", a changed tool: `confine::recheck` from the reader's
  gate). At their probes poppler (`-v`), tesseract (`--list-langs`, which must list as many
  language packs as unconfined), pandoc (`--version`) and ffmpeg (`-version`) run once through the
  reader under the rules their conversions get (`confine/verdict.rs`); one that fails only there
  runs with its limits alone until its next probe. Settings › Documents shows "Programs run
  confined: Yes/No" above the found tools, with each tool that runs without it and why
  (`DocCaps.confinement`, `ToolRow.not_confined`). The tool probes themselves (`probe.rs`) still
  start their tools directly, with the fork: not moved to the reader yet.
- **S6.** `alloc_tune.rs`'s module doc and `docs/documents.md` ("lmgw's own memory") say the
  thresholds are the whole gateway's. Measured 2026-11-19 on the release build in its own scope, a
  fresh instance each, with the thresholds and without (the same binary with
  `scripts/convert-perf/no-mallopt.c` preloaded, so glibc keeps its own); `chat.py` against
  `chat-mock.py` on loopback, a 64 MiB answer, ten runs after the first five; the layout model on
  (`docling-layout-heron-onnx`, CPU, GPU idle):

  | path | with the thresholds | without |
  |---|---|---|
  | chat, 64 MiB streamed (SSE, 16 KiB chunks) | 0.29–0.33 s, CPU 0.33–0.36 s, peak 142–146 MiB |
0.28–0.33 s, CPU 0.32–0.35 s, peak 145–149 MiB |
  | chat, 64 MiB in one JSON body | 0.50–0.53 s, CPU 0.33–0.35 s | 0.52–0.56 s, CPU 0.35–0.40 s |
  | layout model, `text50.pdf` (72 pages) | 24.9–25.9 s (median 25.4), CPU 89.6–91.1 s | 24.9–25.2 s
(median 25.0), CPU 89.5–90.4 s |
  | layout model, the corpus' 20 PDFs, one request | 35.3, 35.6 s | 36.5 s |

  None of them is measurably slower: the layout model's 1 % on `text50.pdf` is inside either
  side's spread and reverses on the corpus. *Decision:* the thresholds stay process-wide.
- **S7.** A job records at most the pool's size of files a batch and writes their entries at most
  that many at once (`buffer_unordered`); their content is bounded besides by the request's output
  (`MAX_UNCOMPRESSED_BYTES` for all of them).
- **Nits.** N1: every reader line about its own failure starts `lmgw-doc-reader run <token>: `,
  the token a run's own (`--status-token`, 8 random bytes), read off its argument line before the
  rest is parsed, so a parse error has it too; a tool's stderr cannot forge it (test: a tool
  writing the reader's old words and exiting 125–127 stays the tool's failure). N4:
  `Confined::argv` forces `limits_only: false`. N6: poppler reads `/var/cache/fontconfig`. N7: a
  `SIGXFSZ` without a limit of the run's own names the inherited limit. N8, N9:
  `docs/documents.md` says the programs' memory, and that a heavy step at the queue's head keeps
  its turn. N11: the scripts' usage lines say `-e` is left out on purpose. N12: `confine.rs`'s
  table has the install directories, the spool's file rule, the font cache and the tessdata rule.
- **Not yet:** N10 (`ocr/pages.rs` is 617 lines, `caps.rs` 623: not split in this round); fonts a
  user installed under a home directory stay unseen by confined poppler (fontconfig would need its
  per-user cache as well); the tool probes are not started through the reader; the security
  review's findings are the next round's.

### Security round A — resource bounds (2026-11-19)

**Built (2026-11-19, on `main` at `52468847`)** from the security review of `main` at `f1919ece`
(its lane A: "who can hold how much, for how long"; lane B, confinement, messages and output,
is a block of its own). Each finding was first re-read or re-probed against the code after the
performance round, then fixed.

- **H1, answers held in memory without a bound.** Still held after the performance round: a
  sync answer was built whole and written by axum with no deadline. Now every conversion holds
  a share of one gateway-wide bound (`extract/convert/budget/answers.rs`): the bound is the
  memory lmgw may use, read as the tool pool reads it (`pool_size::read`, a cgroup limit or the
  machine's memory; not read on Windows and macOS, then not bounded); a share is `SHARE_BYTES`
  (2 GiB: the answer and its body, each at most `MAX_UNCOMPRESSED_BYTES`), taken before the
  request's body is read, set to the body's real size once the answer is made
  (`Share::settle`, never a wait) and held until the body has been written or the client went
  away (`server/documents/send.rs`). The body goes out through a task that hands 64 KiB copies
  to the connection within a write deadline, `ANSWER_WRITE_GRACE_S` (60 s) plus 1 s per
  `ANSWER_MIN_RATE_BYTES_PER_S` (1 MiB); past it the connection is ended, the log line names
  both constants and how much was read, and the share and the body's memory go back. Shares
  wait first come, first served, before any file is read, so waiting is no file's time (the
  file's clock starts at its first slot). A machine with less memory than one share refuses
  every conversion with 503 `answer_memory`, naming both figures. Covered: the native route,
  the docling sync routes (JSON and zip), a job while it converts and records (taken in its
  task before its first file), the native result routes (whole and `?index=`) and docling's
  `/v1/result`. Settings › Documents › "Memory for answers" shows the bound, what is held and
  what waits; a running job's state has the same under `pool.answers`.
- **M1, one caller can hold every slot.** Still held (the pool's fairness was round robin over
  waiting callers only). Now:
  - *A caller's share* (`pool/share.rs`, always on): one caller holds at most the size less
    one slot, and its charges together leave the charge of the step it asks for free; a caller
    that holds nothing may always start one step. A caller at its share passes its turn on
    (`hand_out`), and a newcomer's enqueue hands out what a blocked caller left free. Callers
    are `pool::Caller { queue, share }`: a key is one queue and one share; every keyless
    request is a queue of its own in the one `keyless` share; lmgw's own Chat and
    knowledge-base reads are a caller like the others. Settings › Documents › "Parallel tool
    runs" says it in one sentence with the per-caller counts (`PoolFacts.per_caller_slots`,
    `StepCharge.per_caller`); the side-by-side crops take the per-caller count as their width.
  - *Nesting bounds before heavy work.* The reader child parses a page first with a sink that
    keeps only each node's links (`lmgw-doc-reader/src/html/depth.rs`, html5ever 0.40, the
    version htmd uses), 16 KiB at a time, and stops once a node lies deeper than `STOP_DEPTH`
    (4 × `MAX_HTML_DEPTH`, 1,024): the page is answered too deep and the gateway reads it flat
    with its existing named warning. pandoc's DocBook and FictionBook input is scanned by
    quick-xml (`pandoc/depth.rs`) and past `PANDOC_MAX_XML_DEPTH` (`MAX_HTML_DEPTH`, 256)
    fails with the new code `too_deep` (422) before pandoc runs. Re-probed with pandoc 3.7.0.2:
    only its XML readers grow with the square of the nesting (DocBook: 5,000 nested sections
    0.55 s, 20,000 nested emphases 6.0 s, the review's 50,000 sections still running at a 20 s
    cap, 111 s in the review; FictionBook: 20,000 sections 22.8 s); the review's `deep.tex`,
    `nest.tex`, `nest.org`, `nest.rst`, `emph.rst`, `nest.wiki` and a 50,000-deep RTF stayed
    at 0.01 to 0.7 s, so they are not scanned.
  - *A cooperative stop in the in-gateway readers* (`pool/stop.rs`): every blocking pool step
    gets a flag that the future waiting for it sets when it is dropped (the file's
    `document_timeout`, a client gone, a job cancelled); the office readers' XML walk asks it
    every 4,096 events and their entry reads every MiB, and end with `OfficeError::Stopped`, so
    the step's slot comes back at once.
- **M2, async jobs have no quota.** Still held. Two settings with derived defaults, on Settings
  › Documents › "How much conversions may hold" (`jobs/quota.rs`): "Storage for conversion
  jobs" (`documents.job_storage_mib`; 0 is half the space free on the data directory's file
  system when lmgw started; checked at accept against the uploads, 507 `job_storage_full`, and
  before each result is written, the file then failed with a message naming the setting) and
  "Jobs per key" (`documents.jobs_per_key`; 0 is the tool pool's size; 429 `too_many_jobs`).
  Both refusals name the setting, its value and where the derived value comes from. **This
  revises WP5's decision** "No admission bound was added" (§14 WP5): a queue and the disk are
  bounded now.
- **L7, Labs jobs are keyless jobs.** The lab's in-process requests carry the request's own
  context (the owner who runs the lab) instead of `RequestCtx::default()`, and the job it follows
  is read as that principal: a lab job is the owner's, never a keyless one.
- **Hardening 4, 5, 10.** The status WebSocket closes with 1008 past `WS_CLIENT_MESSAGES_PER_S`
  (10) client messages a second, the reason naming it. `poll?wait=` waits at most
  `POLL_WAIT_MAX_S` (120 s); a longer wait is cut, and the answer carries
  `x-lmgw-poll-wait: 120 (POLL_WAIT_MAX_S)` (in `LMGW_HEADERS`). `jobs::sweep::boot` sweeps
  `convert-spool` of gateways that no longer run (`scratch::sweep_once`, the same rule as
  before: dead pids only), and the layout model's page renders (`lay-<pid>-…`) and poppler's
  working directory of a geometry run (`pg-<pid>-…`) moved from `/tmp` into `convert-spool`
  (`Spool::private_dir`), so they are swept too and stay off a RAM-backed `/tmp`. The
  performance round had moved the spool files themselves (unlinked at creation) but not these.

**The order in which a request takes what it waits for, and why no wait closes a cycle:**
1. its answer share (`answers::Answers::hold`), holding nothing else: no slot, no upload read;
2. then, per step of its files, a tool slot within its caller's share (`ToolPool::acquire*`),
   holding the answer share and the slots of its steps that already run.

Nothing that holds a tool slot ever waits for an answer share (the share is taken once, before
the files; `Share::settle` never waits; a job takes its own in its task, after the accepting
request gave its back with the 202), so an answer-share waiter holds nothing a slot holder
needs. Among slot waiters the share changes nothing about the earlier rule (no step holds a slot
while it waits for another): a caller at its share waits only for its own running steps, which
wait for nothing of it, and a caller holding nothing may always start a step, as an empty pool
always admitted one. The crops' side-by-side width is the per-caller count, so no page in hand
waits for a slot its own caller's share keeps from it. Tested with six callers at once, two
answer shares and a pool of three slots admitting one PDF step
(`budget::answers::tests::callers_with_fewer_shares_and_slots_than_they_need_all_finish`, four
docx files each), and the pool's share tests on `pool::test_pool`-sized pools.

**Decisions** (the first five as planned):
- *The answers' bound is the whole memory lmgw may use* (one derivation with the pool,
  `pool_size::read`), counted apart from the pool's charges: those bound what the tools'
  address spaces may reach, the shares what the gateway itself holds. A fraction of it, or one
  count for both, would be the alternatives.
- *(revised below)* *A share is 2 × `MAX_UNCOMPRESSED_BYTES` up front*, the answer and its JSON body
  side by side,
  then the body's real size. A body larger than its share (JSON escapes in the text) is counted
  as it is, never waited for, so no holder waits on the bound. Shares are first come, first
  served: all are the same size until settled, so nothing is lost to the order.
- *(revised below)* *Too little memory for one share is 503 `answer_memory`* (below 2 GiB), not
  "runs alone" as an
  over-heavy pool step does: such a machine cannot hold one full answer.
- *An async request holds a share while it reads and accepts*, the job takes its own in its
  task (and holds it while its entries are recorded); a job waiting for one shows it in
  `pool.answers` of its state. The native result routes and docling's `/v1/result` hold one while
  they send a stored answer.
- *The write deadline is one deadline for the whole body* (60 s plus 1 s per MiB, a rolling rate
  is not built), its chunks copied at 64 KiB so a stalled connection keeps one chunk, never the
  answer; `content-length` stays as before.
- *A caller's memory share leaves its next step's own charge free*, not the heaviest step's: on a
  16 GiB machine one caller then reads 6 pages of text recognition at once (7 fit), with the
  heaviest step's 6 GiB kept free it would be 4. Another caller's heavier step can then wait for
  one of the first caller's steps to end, at the head of the queue (it keeps its turn).
- *(revised below)* *The share is always on, also for a size set by hand* ("Parallel tool runs" 2
  gives one caller
  1) and for lmgw's own Chat and knowledge-base reads; the owner's bearer is a key like any
  other (its own share). The layout model's inference takes up to four slots, at most its
  caller's share (on a four-core machine three).
- *The web-source fetch pool (the tool pool's twin) has no caller share yet*: each fetch is
  bounded by `max_body_mb` and `SOURCE_FETCH_TIMEOUT` already.
- *The HTML parse stops at four times `MAX_HTML_DEPTH`*, not at the bound itself: a node is
  counted where it is appended, and html5ever may move it up later, so stopping at the bound
  could read a page flat that the tree's own count takes; with the margin no corpus or test page
  moved. A page within the stop is parsed twice (the counting parse, then htmd's).
- *pandoc's nesting bound is `MAX_HTML_DEPTH` and only for DocBook and FictionBook*, with a wire
  code of its own, `too_deep` (422); the scan takes a slot of its own before pandoc's.
- *The in-gateway readers get no timeout of their own*: their work is bounded by
  `MAX_UNCOMPRESSED_BYTES` and grows with it, and the stop ends them at the file's
  `document_timeout`, a client gone or a cancel.
- *(revised below)* *"Storage for conversion jobs" derives from the free space read once per
  process* (at its
  first use), half of it; uploads are checked at accept, results before each is written, and a
  file's upload deleted while its job runs is counted until the job ends. Where the free space
  cannot be read, storage is not bounded unless set (said on the page).
- *"Jobs per key" counts jobs running or waiting in this process*, all keyless requests as one
  key; derived from the pool's size.
- *L7: the lab runs as the owner who runs it*, which also makes its usage rows and model calls
  the owner's.
- *`poll?wait=` is cut, not refused*: a docling client sending a long wait keeps working, and
  the header names the bound.
- *The boot sweep reuses LibreOffice's scratch rule* (`scratch::sweep_once`, dead pids only) with
  two prefixes more; `libreoffice/scratch.rs` was touched for the prefixes alone.
- `scripts/convert-perf/gw.sh` takes its port from `LMGW_PERF_PORT` (8888 by default), and
  `scripts/convert-perf/sec.sh TAG [RUNS]` runs this round's rows.

**Not yet:** Chat attachments and knowledge-base reads hold no answer share; the Labs page's job
follower reads a stored result without one; large answers are not streamed from the spool (the
review's fix option 2); a default per-key request concurrency on the conversion routes (option
4); a rolling minimum send rate; re-reading the data disk's free space after start; the fetch
pool's caller share; nesting bounds for the in-gateway office readers (their tables flatten and
their work is bounded by the budget already).

**Numbers** (2026-11-19, release builds of `examples/headless` and `lmgw-doc-reader` started by
`scripts/convert-perf/gw.sh` on port 8890, data dir on the disk; a 16-core test machine with 128 GiB of memory,
Linux 7.3, poppler 26.08.0, pandoc 3.7.0.2; "main" is `main` at `52468847` built the same way,
measured in the same session; other builds ran on the machine meanwhile, load
average about 10). `scripts/convert-perf/sec.sh TAG 3`, cells wall s / gateway CPU s:

| request | main (3 runs) | now (3 runs) |
|---|---|---|
| `small50` (50 PDFs, one request) | 0.31/0.25, 0.29/0.23, 0.29/0.22 | 0.81/0.27 (first run, beside
a build), 0.29/0.23, 0.30/0.22 |
| `small50` from 8 callers at once | 2.24/2.21, 2.25/2.05, 2.75/1.98 | 3.52/1.88 (first run),
2.20/2.01, 2.26/2.07 |
| the corpus' 20 PDFs, one request | 1.33/0.28, 1.34/0.29, 1.64/0.29 | 1.34/0.29, 1.37/0.29,
1.31/0.29 |
| `pics100.pdf`, pictures embedded | 0.80/0.47, 0.80/0.47, 1.08/0.56 | 0.81/0.49, 0.77/0.45,
0.82/0.50 |
| a job of 200 HTML files, to its end | 0.15, 0.16, 0.28 | 0.15, 0.15, 0.15 |

None is measurably slower: the two slow first runs of "now" fell on another build, and
its other runs match `main`'s. One caller on the test machine runs 19 PDF steps at once instead of 20
(`per_caller`), which `small50` does not show.

`scripts/convert-same.py` against `main`'s answers: **270 of 270 identical** (the corpus' 28
PDFs, `pics100.pdf`, `text50.pdf`, every output and picture mode), and **684 of 684** for the
corpus' HTML, EPUB, e-mail and docx sources (76 files), which run through the reader's new
parse bound and the office readers' stop checks.

Live probes on the same release build:
- `deep.html` (200,000 nested `div`s): read flat with its named warning in 0.27 s (the review:
  300 s and `unreadable`). `deep.dbk`: `too_deep` in 0.02 s (the review: pandoc 111 s).
  `deeptables.docx` unchanged (0.07 s).
- `sizelie.docx` (400 MiB of XML): 8.2 s to convert whole; with `document_timeout=1`,
  `deadline_exceeded` after 1.06 s and the pool's running count 0 at once after it (before: the
  read held its slot to its end).
- With "Parallel tool runs" set to 2: one key sends four `sizelie.docx`, a keyless caller's
  one-page PDF one second later answers in 0.06 s (main: 16.6 s). The key's own request took
  30.5 s (main 19.2 s): it has one of the two slots.
- The review's slow readers (`slowread.py`: six `bomb300.docx` requests, 0.9 MiB each, whose
  304 MiB answers are read at 1 KiB/s) against an instance in a `MemoryMax=8G` scope (bound
  8 GiB, four shares while converting): after 45 s six shares held 1.70 GiB together, settled to
  their bodies, gateway RSS 2.1 GiB (peak 2.8 GiB; the review saw a 5.4 GiB peak and 2.1 GiB
  held until the clients closed). At 350 s (60 s + 290 s for 290 MiB) the six write deadlines
  fired, each logged with both constants and the 4.4 MiB handed to the connection; shares 0,
  RSS 90 MiB, while the clients were still reading.
- Settings › Documents at 125 % (`scripts/ui-drive.py --dsf 1.25` on a dev instance): the new
  group "How much conversions may hold" with "Memory for answers" (126 GiB on the test machine, 2.0 GiB a
  conversion, the write deadline in words), "Storage for conversion jobs" (half of the free
  space, with a GiB field and Set) and "Jobs per key" (32, from the 32 parallel tool runs), and
  the expert row "Parallel tool runs" with the per-caller sentence (31 slots; a PDF's text 19 at
  once, a page of text recognition 31, a reader run 24).

**Unverified:** Windows and macOS run nothing of this yet beyond compiling (no tool runs there;
the answers' bound is off there as memory is not read); the server image and the RPM were not
built; a machine below 2 GiB (the `answer_memory` refusal) was tested by unit test only; the
fetch pool and Chat/knowledge-base reads are as before.

#### Review round (2026-11-19)

From the review of this block's branch (2 blockers, 7 should-fix, 7 nits; each re-probed live,
numbers below). The decisions above that this round changes are marked *(revised below)* where they
stand.

- **B1, trickling uploads held the answer shares.** A request's body now has a read deadline
  (`budget/upload.rs`), the mirror of the write deadline: `UPLOAD_READ_GRACE_S` (60 s) plus 1 s per
  `UPLOAD_MIN_RATE_BYTES_PER_S` (256 KiB) of its declared length, or of `max_body_mb` for a body
  without one; past it the route answers 408 `read_deadline` naming both (native, docling form and
  JSON routes), the row and one log line say so, and the share goes back. A share is reserved
  smaller: the declared length (twice it for JSON) plus the request's output budget, not 2 ×
  `MAX_UNCOMPRESSED_BYTES` (`SHARE_BYTES` is gone); a chunked body reserves `max_body_mb`'s bytes in
  place of its length. For that to hold, an answer whose body is more than half its share is no
  longer made whole beside it: its length is counted, then it is written as the client reads it
  (`server/documents/send.rs`, `streamed`), so answer and body never sit side by side. A docling
  request's downloaded sources grow its share without a wait (`Share::grow`). On an 8 GiB machine
  (bound 8 GiB, output 1 GiB): before, 4 conversions at once (8 / 2 GiB); now 7 small ones (8 / (1
  GiB + a few KiB)), one caller 6 of them where the shares apply (6 × 1 GiB + 1 GiB ≤ 8 GiB − 1
  GiB). A chunked JSON upload at the default `max_body_mb` (64 MiB) reserves 1.13 GiB: still 7.
- **B2, one key held every answer share.** Answer shares follow the pool's per-caller rule
  (`budget/answers/holders.rs`): a caller holding shares takes another only while one like it stays
  free beside its own, and a waiting caller holding none goes first (the one served longest ago
  first). A job counts against the key that sent it (`Caller::share`) for as long as it holds its
  share, which is now its uploads plus the output budget. Test: four jobs of one key at four shares
  hold three, another key's request gets the fourth at once (`budget::answers::tests`, and over HTTP
  `documents_security_a::one_keys_jobs_leave_an_answer_share_for_another_key`).
- **S1, two keys defeated the pool share.** A waiting caller that holds no slot goes before the
  round robin, the one given a slot longest ago first (`pool::share::idle_first`,
  `Shares::idle_rank`), so a caller whose step just ended does not take its slot back before a
  newcomer. Test: two keys at their share in a pool of two and a newcomer; the slot the first key
  gives back goes to the newcomer (`pool::share::tests`).
- **S2.** `results::remove_input` returns the bytes it deleted and the job's spool takes them off
  the count (`quota::Stored`, shared with the spool). Test: the count equals what is on disk after
  four jobs (`documents_security_a::the_storage_count_equals_the_disk_after_jobs`).
- **S3.** The derived storage bound is read at each admission and each result: half of (what the
  jobs store now + what is free now on the data directory's file system); a size set by hand is used
  as it is. Settings › Documents' row says the rule and the figures.
- **S4.** A request's output budget is the smaller of `MAX_UNCOMPRESSED_BYTES` and half the answers'
  bound (`Answers::output_budget`, used by `OutputBudget::request`), shown on "Memory for answers"
  and named by `output_too_large` and the pictures' warning ("768 MiB (MAX_UNCOMPRESSED_BYTES, or
  half the memory for answers where that is less)"). `answer_memory` 503 stays for a request whose
  upload and output can never fit.
- **S5, the reserve cost the single user.** The per-caller rules of the pool and of the answers
  apply only where a second caller can exist (`extract/convert/callers.rs`): "Require API key" on
  and more than one caller among the enabled, unexpired keys that hold `/v1` (client keys, agent
  tokens, devices; all owner keys of one account count once). It is read at every snapshot publish
  (a key added or switched off counts from the next request), never from traffic. Otherwise one
  caller may use the whole pool and every answer share, as on `main`, and lmgw's own Chat and
  knowledge-base reads wait behind its batch. "Parallel tool runs" says which case is in force and
  why; at one slot it says no room can be kept.
- **S6.** Every paced answer carries `x-lmgw-write-deadline: <s> (ANSWER_WRITE_GRACE_S + 1 s per
  ANSWER_MIN_RATE_BYTES_PER_S)` (in `LMGW_HEADERS`, the native route, the job result route, the
  docling sync routes and `/v1/result`), named in `docs/documents.md`.
- **S7.** The deadlock test drives `server::documents::convert` itself
  (`file/tests/route_order.rs`): six keys, two answer shares, `test_pool::small(3, 1)`, each request
  two PDFs with embedded pictures (the geometry pair, crops side by side and cut again) and two docx
  files. The reviewer's "could not judge" probes that are cheap are tests now: the status
  WebSocket's 1008 close past `WS_CLIENT_MESSAGES_PER_S` (`documents_security_a`), and the 408 at
  the read deadline (`server::documents::tests`, paused time).
- **Nits.** N1: uploads and results are reserved in the count in one step at their check
  (`Stored::reserve`) and given back when their write fails. N2: a page deeper than the reader's
  stop says "at least N deep (the count stops past STOP_DEPTH, 1024)". N3: the stale comment went
  with the new `caller`. N4: `Refused` answers `rate_limit_error` for 429, `api_error` for 5xx (503
  `answer_memory`, 507), `invalid_request_error` for 408. N6: a request waiting more than a second
  for its share logs one debug line with the figures.

**The order, after this round:** a request waits first for its answer share (holding nothing), then
reads its body under the read deadline (holding the share), then its steps wait for pool slots
(holding the share and its running steps' slots). No path waits for a share while it holds a slot:
the share is taken once before the body; `settle` and `grow` never wait; a job takes its own in its
task before its first file, after the accepting request gave its own back with the 202; the result
routes take theirs before reading a stored answer and hold no slot; the streamed body's thread holds
the share and the answer, no slot.

**Decisions:**
- *A share is the upload plus the output budget*, the upload counted twice for a JSON body (its text
  and its parsed value side by side while it is read). This replaces "a share is 2 ×
  `MAX_UNCOMPRESSED_BYTES`".
- *A body larger than half its share is written as the client reads it*, from a blocking thread that
  holds the answer and hands 64 KiB chunks to the connection under the write deadline; its length is
  counted by a first serialisation (no copy). Such answers hold a blocking thread each while they
  are read; their number is bounded by the shares (at most twice the bound over the output budget:
  about 250 on a 125 GiB machine). Smaller bodies are made whole as before. *(revised below,
  verification round)*
- *The read deadline's rate is 256 KiB/s*, a quarter of the write rate, as uploads are often the
  slower half of a link (a 64 MiB upload: 316 s); the grace is the write deadline's 60 s.
- *A docling request's downloaded sources grow its share without waiting*: they are counted, so
  later shares wait, but the request does not wait while holding its share (that would be a wait
  that can close a cycle).
- *A caller for the per-caller rules is a key, or an account for owner keys*: the dashboard's and
  self-admin's keys are one person. Keyless requests stay one caller. *Revises* "the owner's bearer
  is a key like any other (its own share)".
- *Callers are counted from the configuration, not from who called*: a device key or agent token
  that never converts still makes the shares apply. A key's expiry is judged at each publish: one
  that expires while lmgw runs counts until the next publish.
- *The storage bound is half of stored + free now*. *Revises* "derives from the free space read once
  per process".
- *The output budget below 2 GiB is half the bound*, the share then at most the bound with a small
  upload; a chunked upload whose `max_body_mb` does not fit beside it is refused with
  `answer_memory`.
- *Refusals' error types*: 429 is `rate_limit_error`, 5xx `api_error`, 408 `invalid_request_error`
  for every `Refused`, not only the conversions'. *(revised below, verification round: the
  OpenAI shape says `server_error` for 5xx)*

**Not yet:** "Jobs per key" still derives from the pool's size (N5: at "Parallel tool runs" 1 the
owner gets one async job at a time); a bound that is an exact multiple of a share no longer exists,
but a settled remainder can still keep the next share waiting until a body is read (N7); a rolling
minimum rate for the read and write deadlines; the docling zip answer is built whole beside its
documents; a poll on a task that runs past `POLL_WAIT_MAX_S` is not a test (it needs 120 s; probed
live below); the header read of a request has no deadline of its own (hyper's defaults).

**Numbers** (2026-11-19, release builds of `examples/headless` and `lmgw-doc-reader`
through `scripts/convert-perf/gw.sh` on port 8890, data dir on the disk; a 16-core test machine with 128 GiB of memory,
Linux 7.3, poppler 26.08.0; load average about 1; "main" is `main` at `3f092344` (lane B, without
this block's round) built and measured the same way in the same session).
`scripts/convert-perf/sec.sh TAG 3`, cells wall s / gateway CPU s:

| request | main (3 runs) | now (3 runs) |
|---|---|---|
| `small50` (50 PDFs, one request) | 0.31/0.28, 0.29/0.25, 0.29/0.25 | 0.49/0.28 (first, cold), 0.29/0.26, 0.29/0.24 |
| `small50` from 8 callers at once | 2.23/2.48, 2.16/2.23, 2.24/2.25 | 2.18/2.44, 2.21/2.27, 2.14/2.17 |
| the corpus' 20 PDFs, one request | 1.32/0.30, 1.32/0.29, 1.33/0.31 | 1.30/0.29, 1.31/0.30, 1.31/0.31 |
| `pics100.pdf`, pictures embedded | 0.84/0.52, 0.82/0.51, 0.77/0.48 | 0.82/0.54, 0.76/0.45, 0.80/0.55 |
| a job of 200 HTML files, to its end | 0.15, 0.15, 0.16 | 0.15, 0.17, 0.18 |

`scripts/convert-same.py` over the same 270 answers (the corpus' 28 PDFs, `pics100.pdf`,
`text50.pdf`, every output and picture mode): **270 of 270 identical** to `main`'s.

S5's case, "Parallel tool runs" 2, the owner's request of four `sizelie.docx` (a 1.2 MiB docx of
400 MiB of XML, made by the review's `mkdocx.py`; its output passes the request's budget at the
fourth file in both builds), two runs each: main 5.3 s and 4.5 s; now with one caller ("Require
API key" on, only the owner's keys) 5.4 s and 4.7 s, as main; with a second key added while running
(the row then says two callers, the shares on) 8.1 s and 7.9 s, one slot of the two. The block's own
earlier figures (main 19.2 s, the branch 30.5 s) were taken with the review's heavier file.

**Live probes** (the same release build, `gw.sh` in a `MemoryMax=8G` scope, auth on, keys `hogA`,
`hogA2`, `victimB` beside the owner, so four callers):
- B1: seven uploads from `hogA` announcing 1 MiB and sending one byte every 4 s held six shares
  (6.0 GiB; the seventh waited, one caller's rule); `victimB`'s 12-byte text answered 200 in
  0.013 s and a one-page PDF in 0.046 s meanwhile (the review: no answer after 30 s and 20 s). At
  65 s the six uploads got 408 `read_deadline` ("within its read deadline of 64 s
  (UPLOAD_READ_GRACE_S 60 s + 1 s per UPLOAD_MIN_RATE_BYTES_PER_S 256 KiB of 1.0 MiB, its declared
  length)"), six log lines, and their shares went back.
- B2: eight async jobs of two `sizelie.docx` each from `hogA`: six held shares, two waited;
  `victimB`'s 12-byte text answered 200 in 0.56 s (the review: 46.5 s).
- S2: after those jobs, `stored_bytes` 6,510,859,964 and the files under `convert/` 6,510,859,964
  bytes, no upload left; the storage bound equalled half of what was stored
  plus the space free then (checked to the byte).
- S1: "Parallel tool runs" 2, `hogA` and `hogA2` four `sizelie.docx` each, `victimB`'s one-page PDF
  1.5 s later: 200 in 5.1 s, one step's end (the review: 23.3 s); the hogs' requests 8.9 s and
  8.8 s.
- S4: in a `MemoryMax=1536M` scope (bound 1.5 GiB, output 768 MiB, one caller): the 12-byte text
  200 in 0.012 s, the one-page PDF 200, `sizelie.docx` converted (the review: 503 for all); a
  chunked JSON body at `gw.sh`'s `max_body_mb` 512 is 503 `answer_memory` (2 × 512 MiB + 768 MiB
  can never fit), as the decision above says.

**Unverified:** the streamed path for a body larger than half its share ran in a unit test and
the route tests only, not live with a 300 MiB answer; the slow reader probe (`slowread.py`) was not
re-run; a poll on a task running past `POLL_WAIT_MAX_S` was not probed live; the UI rows were
checked by their unit tests, not by a screenshot; Windows compiles it
(`scripts/windows-remote-check.sh`, clippy) and runs nothing of it.

#### Verification round (2026-11-20)

The verification of the review round (0 blockers, 3 should-fix, 7 nits) and what was built of it,
rebased onto `main` at `200db8a1` (lane B, the device-ledger merge, migrations 0114 and 0115).

- **SF1, the streamed path still built half its share.** `send::made` counts the body first (no
  allocation), then makes it whole into one buffer of exactly its size only when it is at most
  half the share; a larger one is never made, only written chunk by chunk as the client reads it.
  Test: `send::tests::the_body_is_counted_first_and_made_whole_only_within_half_the_room`.
- **SF2, the connection outlived the write deadline.** Every listener's stream (plain, main TLS,
  device) is a `Cuttable` (`server/conn/cut.rs`) whose `WriteCut` rides on `Conn`. At the deadline
  the answer cuts it: the cut wakes the connection's task, its stalled write fails, hyper ends the
  connection and the socket closes with a reset (`SO_LINGER` 0), so neither the descriptor nor the
  kernel's unsent buffer stays. An answer handed over whole whose last bytes still wait unsent at
  its deadline is ended then too (`WriteCut::check_tail`, judged by the connection itself at its
  next poll, woken for it): when it has written at most the answer since the answer's mark (plus
  `HEAD_SLACK`, 16 KiB, for its head) and either waits to write (the rest in hyper's buffer) or,
  on Linux, still holds bytes in the kernel's send queue (`SIOCOUTQ`). The first live run found
  that second case: a 1.6 MiB answer taken whole by a 2.8 MiB send queue, hyper done and the
  connection waiting for a next request, stayed open for 40 minutes; the kernel's queue is read
  since. A connection whose client read the answer, or that wrote a later one, is left alone.
  Tests: `conn::cut::tests` (a stalled write woken, failed and reset; a read connection left
  alone; an answer left in the kernel's queue ended, a later answer's connection not),
  `send::tests::connection` (axum's listener, a client with a 4 KiB receive buffer that reads the
  head and stops: a 16 MiB answer heard as `Late`, a 256 KiB one the kernel took whole heard as
  `Whole`; both sockets reset at the deadline, shares back). With the kernel check switched off
  locally, the two kernel tests fail.
- **SF3.** The B2 test asserts, half a second after the shares stand at four held and one
  waiting, that key A's fourth request has not been answered. With the rule switched off locally
  (first come, first served) the test fails ("key A's fourth request took the share key B was
  owed"); restored, it passes.
- **Error types (item 4).** A `Refused` 5xx is `server_error` in the OpenAI shape, as realtime and
  Responses already said; the Anthropic shape keeps `api_error`; 429 stays `rate_limit_error`, 408
  `invalid_request_error`. This reaches every OpenAI-shaped route that answers a `Refused`: the
  images routes (`image_queue_full` stays 429, `unchanged_image` 502, `upscale_needs_local` 503),
  quickdoc's `fallback_refused`, and lmgw's own `/v1/documents/*` (OpenAI's envelope, so the same
  rule: `answer_memory` 503, `job_storage_full` 507). Test: `error::tests::a_refusals_type_follows_its_status_in_each_shape`.
- **N-a.** A result's reservation is a guard (`quota::Reservation`): dropped, written or not, it
  gives back what was reserved and did not reach the disk (`results::Landed`, each file counted
  once its write finished), so a cancel during a result write releases it at once. Test:
  `quota::tests::a_write_dropped_by_a_cancel_gives_its_reservation_back_at_once`.
- **N-b.** The row of an answer is written once its body ended (`send::To::then`,
  `Row::when_sent`): a client that left midway is `499 client_disconnected`, one that missed the
  deadline `499 write_deadline`, each saying how much was handed over and what the answer's status
  was. Test: `documents_security_a::an_answer_whose_client_left_midway_is_a_499_row`.
- **N-c, N-d.** With sign-in off, "Parallel tool runs" says one caller may use every slot because
  Require API key is off, and that with it on and more than one key each caller keeps room for the
  others; "Memory for answers" says who may hold all of it and why, in each of the two cases.
- **N-e.** The storage field shows the figure its sentence says: `units::gib_figure`, the shared
  formatter's precision in GiB ("217" beside "217 GiB").
- **N-f.** A streamed answer is written on a thread of its own (`lmgw-answer`), not the runtime's
  blocking pool, so `spawn_blocking` never queues behind slow readers (test: the streamed body's
  serialisation runs on that thread).
- **N-g.** Without poppler the route-order test runs with its office files only and says so
  ("partly skipped"), as the other poppler tests say their skips.

- **Found by the gate.** `convert_compare_test.py`'s interruption test failed in a gate started in
  the background (`scripts/long-run.sh`): such a command inherits an ignored SIGINT, and
  `_thread.interrupt_main` then does nothing. The test sets Python's own handler for its length;
  it passes with SIGINT ignored and without.

**Decisions:**
- *A whole answer's row is written just before its last chunk is handed over, and an answer of
  one chunk (64 KiB) before its head*, so a client holding the whole answer finds its row, as when
  the row came before the body; a client that leaves during the last chunk is recorded as
  answered. (Revised by the connection cut review below: the row is written once
  the connection has room for the last chunk, so a deadline missed before it is a 499.)
- *A client that missed the write deadline is a 499 row with the kind `write_deadline`*, beside
  `client_disconnected`: neither got its answer; the kind says which.
- *The row's time now includes the write* (the row is written when the body ends).
- *A cut ends the whole connection*: on HTTP/2 every stream of it (they are the one client's,
  which stopped reading); on HTTP/1.1 keep-alive the connection. (Revised by the connection cut
  review below: HTTP/2 connections are never cut, only the answer's stream is reset. The premise
  was wrong: HTTP/2 flow control is per stream, so a client can pause one stream and read the
  others.)
- *The streamed answers' threads are bounded by the answers' shares*, not by a count of their own:
  about 2 × the memory for answers / the output budget (16 at 8 GiB, 256 at 256 GiB), more only
  where answers settled above their room; unbounded where the memory is not read, as the answers'
  memory is. A thread that cannot start falls back to the blocking pool, said in the log.
- *A `Refused` 5xx is `server_error` in the OpenAI shape* (revises the round's N4 decision). OpenAI's
  own 429 and 5xx bodies are not recorded in the repo (`scripts/compat_live`, the fixtures), so
  `rate_limit_error` for a 429 and `server_error` for a 5xx are unverified against the platform.
- *A cancelled write's partial file*: counted once finished, so the deletion at a cancel can take
  up to that file's partial bytes more off the count than was counted, until the hourly resync.
- *An answer's tail is judged by bytes*: the connection is ended when it wrote at most the answer
  plus `HEAD_SLACK` since the answer began and still holds some of it; a later answer of fewer
  than 16 KiB on the same keep-alive connection, itself unread, is ended with it. (Wrong, and
  replaced by the connection cut review below: `SIOCOUTQ` counts unacknowledged bytes of later
  traffic too, so a later stream that was being read was reset; "itself unread" did not hold. The
  check now concerns only the answer's own bytes, and only while the connection has done nothing
  since.)

**Not yet:** the other `GatewayError` kinds' 5xx (`GpuHold`, `Upstream`, `Internal`, ...) still say
`api_error` in the OpenAI shape; a `compat-live.sh` scenario that records a real OpenAI 429 body;
the kernel's send queue is read on Linux only (Windows and macOS judge an answer's tail by a
waiting write alone; macOS reads it since the connection cut review); the result routes read a stored answer whole before writing it
(`jobs::answer::full`), and an `output: json` answer's block model takes about 2.2 to 3 times its
bytes once parsed, so its peak passes its share (numbers below); reading the entries one at a
time while the answer is written is not built yet.

**Numbers** (2026-11-20; release builds of `examples/headless` and `lmgw-doc-reader`,
`main` at `200db8a1` and this branch, through `scripts/convert-perf/gw.sh` on port 8890 in a
`MemoryMax=8G` scope (bound 8 GiB, output budget 1 GiB, `max_body_mb` 512, sign-in off); a 16-core test machine with 128 GiB of memory,
Linux 7.3, poppler 26.08.0). `scripts/convert-perf/sec.sh TAG 3`, wall s /
gateway CPU s, main then this branch:

| request | main (3 runs) | now (3 runs) |
|---|---|---|
| `small50` | 3.90/0.24, 3.75/0.20, 3.76/0.20 | 3.70/0.21, 3.71/0.22, 3.84/0.22 |
| `small50` from 8 callers | 30.12/1.71, 30.07/1.71, 29.93/1.70 | 29.54/1.74, 29.95/1.77, 29.89/1.73 |
| the corpus' 20 PDFs | 8.41/0.26, 8.40/0.28, 8.45/0.29 | 8.36/0.28, 8.52/0.27, 8.41/0.26 |
| `pics100.pdf`, embedded | 5.39/0.49, 5.45/0.51, 5.48/0.50 | 5.36/0.50, 5.44/0.46, 5.40/0.51 |
| a job of 200 HTML files | 0.36, 0.36, 0.36 | 0.36, 0.37, 0.34 |

These rows ran in a `MemoryMax=8G` scope and compare only with other 8G rows, not with the
review round's figures of 2026-11-19, which ran with no memory limit. That is not a regression: a
diagnosis on 2026-11-20 measured `200db8a1` with no memory limit at 0.28–0.30 s for `small50`,
2.11 s for 8 callers, 1.29 s for the corpus' 20 PDFs and 0.75 s for `pics100.pdf` embedded, all
matching 2026-11-19, and with `MemoryMax=8G` at 3.63–3.71 s for `small50`. At 8 GiB the pool
admits one PDF at a time: a PDF file is charged `PDF_FILE` (`pool/charge.rs`), the 4 GiB poppler
limit plus 2 × 1 GiB, 6 GiB, and 8 / 6 = 1. The tools' own CPU did not grow (`kids_cpu` 4.4 s
against 3.5–4.3 s), and the confinement is not the cause.
`scripts/convert-same.py` over the corpus' 20 PDFs, `pics100.pdf` and `text50.pdf` in every
output and picture mode: **198 of 198 answers identical** to `main`'s.

SF1, the cgroup's peak above the idle gateway (anonymous memory, `memory.stat`; the cgroup's
`memory.current` beside it counts the data dir's page cache too), before the answer's head and
while its body is read:
- `pics100.pdf`, `pictures=embedded images_scale=8`, a 613,925,047-byte answer with its length
  declared (share 28.5 MiB + 1 GiB, half of it less than the answer: streamed): 958 MiB before the
  head (the conversion itself), 576 MiB while read (the review: 1,202 and 732 MiB of
  `memory.current`; now 1,018 and 636). The same request chunked (share 512 MiB + 1 GiB, made
  whole): 1,167 MiB before the head, 588 MiB while read. Both within their share.
- A job's stored result (4 × `sizelie.docx`, markdown, 637,535,779 bytes, share the output
  budget, 1 GiB, streamed): 770 MiB before the head, 543 MiB while read (the review: a 1,560 MiB
  peak of `memory.current` for its 814 MB result). Within the share.
- The same through `output: json` (2 × `sizelie.docx`, 637,535,376 bytes): 1,955 MiB before the
  head, 1,408 MiB while read, above its share: the parsed block model, not the body (Not yet).

SF2, live, a client with a 4 KiB receive buffer that reads the head and stops (`ss` on the
gateway's side, each probe once):
- streamed, the 585 MiB answer: the deadline's log line at 645 s ("3.3 MiB of it handed over"),
  the gateway's socket gone at 645.6 s, the client's next read a reset;
- paced, a 19.6 MiB answer (`images_scale=1`): gone at 79.6 s (deadline 79 s), reset;
- a 1.6 MiB answer (`text500.md`) the kernel took whole: on the first build open for 40 minutes
  (the probe was stopped), after the kernel check gone at 61.6 s (deadline 61 s), reset, the log
  saying "handed over whole, but its client had not read it ... 1.6 MiB still in the connection's
  buffers".

Settings › Documents at 125 % (`scripts/ui-drive.py --dsf 1.25`, Expert view on), looked at with
sign-in off ("One caller may use all 32 at once: Require API key is off, so calls need no key.
... Once Require API key is on and more than one key may call the conversions, each caller keeps
room for the others."; "one caller may hold all of it, as Require API key is off; ..."; the
storage sentence "216 GiB at most now" beside the field's "216") and with sign-in on and two keys
("3 callers may call the conversions, so one caller ... runs at most 31 of them at once ..."; "one
caller leaves room for one more conversion like its own ...").

**Unverified:** the kernel-queue check on Windows and macOS (not built there yet); an HTTP/2
connection's cut, by reading only; the job-result route's stored answer in `output: json` stays
above its share (above).

#### Connection cut review (2026-11-20)

The review of `e18972f3` (1 blocker, 1 should-fix, 7 nits) and what was built of it, on `main` at
`ab96bfd0`.

- **B1, a reused keep-alive connection was reset for later traffic.** The tail check now concerns
  only the answer's own bytes, and only while the connection has done nothing since. Each answer
  takes a `Mark` as it begins (`WriteCut::begin`: which answer it is, the bytes the connection had
  written and read). Its end is counted, not guessed: the answer's body tells the connection when
  its last frame goes to hyper (`WriteCut::last_frame`), and the next flush that finishes (hyper
  flushes its inner stream only once its own buffer is written) records where the answer ended
  among the connection's written bytes, head and body as written. At the deadline the check is
  disarmed, on every system, when the connection has read any byte since the mark (an HTTP/1
  client sends its next request, or an upgrade, only once it has taken the answer), has written
  past the answer's end (another response) or a later answer began on it. Still armed, it cuts
  when the answer's rest still waits to be written (no last frame flushed, the write waiting for
  room) or, where the kernel's queue is read, when some of the answer's own bytes are still
  queued: the queue is first in, first out, so that is `queued > written since the mark − the
  answer`, which while armed is the queue itself. `HEAD_SLACK` and the `since` gate are gone
  (nit 6). Every poll reads one atomic flag first and takes the lock only with a check waiting
  (nit 4). Tests: `conn::cut::tests` (a later response still queued after an answer that was
  read is not cut; a byte read after the mark disarms; a later answer begun disarms the earlier
  one's check; an answer left unread in the kernel's queue is cut; the judgement's rules by
  counts), `send::tests::reuse` through axum's listener: the reviewer's case (a 524 B conversion
  read whole, then on the same connection a streamed chat across its deadline whose client is a
  little behind, 14 KiB still in the gateway's send queue at the deadline, read to its end, never
  cut) and the true case (a 256 KiB answer whose client never reads it, reset at its deadline).
  With the two disarming rules switched off locally, the reviewer's case and both disarm tests
  fail; the routed one is reset, with 14,436 B in the queue at the deadline, all of them the
  chat's.
- **S1.** The log line names the answer's own still-queued bytes and the answer's head and body
  as written ("… N of its M (head and body) still unacknowledged in the connection's send queue,
  and the connection had read and written nothing since"), or that its rest still waited to be
  written. As the check is disarmed by anything later, the line can no longer describe another
  response's bytes.
- **Nit 1, HTTP/2.** An HTTP/2 request's answer carries no `WriteCut` (`send::To::of` and
  `To::on` read the request's version): a missed deadline ends the answer's body with its error,
  which resets that stream alone, and the share goes back at the deadline as before. Test:
  `send::tests::reuse::on_http2_a_stalled_document_stream_is_reset_and_the_connection_serves_on`
  (an h2c client with a 64 KiB stream window that does not read a 16 MiB answer: `Late` at the
  deadline, the stream's body ends in an error, a second stream on the same connection is
  answered whole and the connection stays up).
- **Nits 2, 3.** The whole answer's row is told once the connection has taken every chunk before
  the last and has room for it (its slot in the channel reserved within the deadline), then the
  last chunk goes; a deadline missed while the last chunk waits is `Late`, a 499
  `write_deadline` row, never a 200. Tests: `send::tests::a_deadline_missed_during_the_last_chunk_is_heard_as_late`,
  `row::tests::an_answer_that_missed_its_write_deadline_is_a_499_write_deadline_row` (the row's
  status, kind and message through the store).
- **Nit 5.** macOS reads its queue with `getsockopt(SOL_SOCKET, SO_NWRITE)`, unsent plus
  unacknowledged bytes like Linux's `SIOCOUTQ` (`cut.rs`, `cfg(target_os = "macos")`). Windows
  still judges the waiting write alone.
- **Nit 7.** The stalled-write test is two now, named for what they drive:
  `a_cut_wakes_a_waiting_write_fails_it_and_resets_the_socket` (`WriteCut::cut`) and
  `a_tail_check_ends_an_answer_whose_rest_waits_to_be_written` (`check_tail`).
- **Re-review nits (2026-11-20).** A late `check_tail` of an answer that a later one followed
  returns at once (the answer count read under the tail's lock), so it never takes the place of
  the later answer's waiting check (test
  `a_late_check_of_an_earlier_answer_never_replaces_a_later_one_s`); `poll_flush` loads the
  `ending` flag before it swaps it, so a flush outside an answer's end is one relaxed load.

**Decisions:**
- *A byte read after an answer disarms its check*, also a pipelined next request sent before the
  answer was taken: such a client's unread answer stays until the client goes, bounded by one send
  buffer per connection, as on Windows. Pipelining HTTP/1 clients are rare; reading a request is
  the only sign a server has that the answer was taken.
- *An answer handed over whole and cut by its tail check keeps its 200 row*: the row is written
  as the last chunk goes, before the deadline; the log line says the connection was ended.
- *On HTTP/2 a stalled stream holds up to two chunks (128 KiB) after its deadline* (the one hyper
  holds and the one in the channel) until the client reads that stream again or the connection
  ends: hyper resets the stream only when it next polls its body, which it does once the client
  gives the stream room. The answer's share goes back at the deadline.
- *The h2c listener stays*: the plain listener still speaks HTTP/2 with prior knowledge (axum's
  auto builder); only the cut is left out there.

**Not yet:** Windows does not read its send queue (the reviewer's `SIO_TCP_INFO` question is
unprobed). A client can avoid the tail check's cut by sending one byte after the answer (the
first decision above): its unread answer then stays until it goes, bounded by one send buffer per
connection; nothing closes that yet. Over TLS, if rustls still holds some of the answer's bytes
when hyper's buffer has drained, the flush that records the answer's end comes later and the end
is counted late (theoretical, not seen; not handled yet). No test drives HTTP/1 pipelining through
hyper itself yet (the disarm by a read is tested on the stream).

**Numbers** (2026-11-20; release builds of `examples/headless` and `lmgw-doc-reader`,
`main` at `ab96bfd0` and this branch, `scripts/convert-perf/gw.sh` on port 8890 with no memory
limit; the same test machine, Linux 7.3). Hot path, the two builds alternated twice
(main, branch, main, branch):
- `chat.py` against `chat-mock.py` (64 MiB streamed as SSE, 16 KiB chunks), ten runs after five
  warm-up runs each: main 0.27–0.32 s wall, CPU 0.31–0.35 s; branch 0.28–0.32 s, CPU 0.32–0.36 s.
- 16,000 `GET /v1/models` on 8 keep-alive connections (2,000 each), three runs per build: main
  9,721–10,055 requests/s, 84–87 µs gateway CPU per request; branch 9,811–10,169 requests/s,
  79–84 µs.
- 4,000 streamed chat completions of a few events (`chat-mock.py` with 0 MiB) on 8 keep-alive
  connections, three runs per build: main 112–129 requests/s, 947–1,005 µs CPU per request;
  branch 118–130 requests/s, 922–1,010 µs.
- Not measurably slower. The gateway's peak during the 64 MiB chat is 144–146 MiB on main and
  162–166 MiB on the branch, and so is its idle size (82 against 101 MiB, five seconds after
  start): the request's own growth is the same, about 62 MiB on both. Where lane A's 19 MiB at
  idle come from is not looked into yet.

`scripts/convert-same.py` over the corpus' 20 PDFs, `pics100.pdf` and `text50.pdf` in every output
and picture mode, no memory limit: **198 of 198 answers identical** to `main`'s.

B1 live, the reviewer's probe again on this branch's release build: the gateway (`gw.sh`) in a
network namespace of its own (`unshare -rn`) with `tc netem` 25 ms each way on its loopback (RTT
50 ms), a mock upstream streaming one ~220 B SSE event per 100 ms for 10 s, the owner's bearer
on every request. 16 connections at once, each a conversion (an 895 B answer) read whole, then on
the same connection a streamed chat starting 56.0 to 59.5 s later, across the conversion's 60 s
deadline, read line by line: **16 of 16 read to their end** (103 events, 25,713 B, 200 each), and
the gateway logged no cut. The reviewer's run on `e18972f3` reset 1 of 8 at the same RTT.

**Unverified:** macOS's `SO_NWRITE` read was not compiled for macOS (no Apple target on this
machine): its body was type-checked on Linux against `libc` 0.2.186 with `SO_NWRITE` as that crate's
Apple module defines it (`0x1024`), and `getsockopt`, `SOL_SOCKET` and `socklen_t` are the same
items on both. The netem probe is not a gate test (it needs user and network namespaces and `tc`,
which the gate's CI container is not known to allow); the gate's routed test reproduces the same
state, later bytes still queued at the deadline, on plain loopback.

#### PDF charge after the runs (2026-11-20)

**Built** on `main` at `f8ed03a2`. Once a PDF's poppler runs have ended, its slot is shrunk
(`Slot::shrink`) to what it still holds: `pool::charge::after_runs`, the runs' output buffers as
allocated plus `READ_PER_OUTPUT_BYTE` (2) per byte of them for their in-process reading. Both
geometry paths (`file/pdf/runs.rs`: the pair side by side, cut from two slots to one; one after
the other) and a picture run before its images are read (`pictures/pdf/runs.rs`). Nothing in
those slots runs poppler again (text recognition and the page-reading model take slots of their
own), so the bound is never undercut. `PDF_FILE` stays the admission bound.
The factor's derivation: the reads are linear in their input; with every block rounded up as
glibc's malloc rounds it, the layout pass's peak above its inputs was 0.25–0.50 times their bytes
on real files (`doc01.pdf` of `small50`, `2203.01017v2.pdf`, `text500.pdf`, `text5k.pdf`,
`bigtable.pdf`, `dense200.pdf`) and 1.04 at most on synthetic output with the most structure per
byte poppler writes (one one-letter word a line, a block each, `%f` coordinates). Tests:
`pool::tests_read` holds the layout pass and `pictures::pdf::images` to the factor on those shapes
under a counting test allocator (`pool/peak.rs`, test binary only); `tests_charge` shows the next
PDF admitted on 8 and 16 GiB pools once the first's runs end, and that a shrunk slot gives back
exactly the difference; `runs::tests` shows both geometry paths hand back one slot charged
`after_runs`.

**Numbers** (release builds of `examples/headless` and `lmgw-doc-reader`, `main` `f8ed03a2` and
this branch, `scripts/convert-perf/memrow.sh` on port 8893: `gw.sh`, `sec.sh TAG 3`, then
`convert-measure.py --formats pdf`; the same test machine, Linux 7.3, poppler 26.08.0, sign-in off).
Wall s, three runs each:

| request | main, `MemoryMax=8G` | branch, 8G | main, no limit | branch, no limit |
|---|---|---|---|---|
| `small50` | 3.67, 3.67, 3.66 | 3.63, 3.58, 3.58 | 0.30, 0.27, 0.28 | 0.48, 0.28, 0.28 |
| `small50` from 8 callers | 29.17, 29.20, 29.01 | 28.55, 28.58, 28.49 | 2.12, 2.07, 2.06 | 2.13, 2.09, 2.07 |
| the corpus' 20 PDFs | 8.27, 8.24, 8.26 | 8.20, 8.19, 8.18 | 1.32, 1.33, 1.26 | 1.31, 1.30, 1.32 |
| `pics100.pdf`, embedded | 5.43, 5.36, 5.30 | 5.26, 5.21, 5.30 | 0.79, 0.74, 0.81 | 0.77, 0.81, 0.78 |
| a job of 200 HTML files | 0.38, 0.38, 0.38 | 0.36, 0.38, 0.36 | 0.15, 0.17, 0.15 | 0.15, 0.16, 0.18 |
| corpus `pdf/` folder, one file at a time | 8.77 | 8.71 | 4.32 | 4.39 |

The corpus' `pdf/` folder scores the same on all four (28/28, J 0.857, line ratio 0.2701,
headings 167/159, tables 38/31). **At 8 GiB the gain is 1–2 %, not the parallelism the item
aimed at:** the layout pass is a few milliseconds a file; the time is in poppler's runs, which
still go one file at a time (8G `small50`: children's CPU 3.5 s against 3.6 s wall). Each run is
charged poppler's 4 GiB address-space limit, so 8 GiB admits one file's runs at a time, and a
lone file's pair side by side (2 × `POPPLER_RUN`, 10 GiB) is admitted only when nothing else runs
(the "heavier than all the memory runs alone" rule), so nothing else starts beside it.

**Decisions:**
- *`READ_PER_OUTPUT_BYTE` is 2*, twice the worst measured shape, held by a test rather than
  proved from every stage's structures.
- *The crop pages and the picture gate keep `PDF_FILE`*: a crop page may be rendered again with
  poppler on its own slot (`side.rs`, cut again in its turn), so shrinking it would need the
  charge back first; the gate's judge after its two runs takes milliseconds, and a non-UTF-8
  output would be copied lossily at up to 3 times its size.

**Not yet:** parallel PDFs at 8 GiB need a smaller charge for the runs themselves: a lower
`POPPLER_MEMORY_LIMIT_BYTES` from measured peaks (as the OCR render's 2 GiB was set), or the pair
side by side only where it fits the memory; neither is decided yet (the owner's decision, as it
changes a named limit). The picture runs' and the layout's `Slot::shrink` keeps the charge as it
was while a lease of the slot lives (a stopped tool not reaped yet).

#### Poppler limit from measured peaks (2026-11-20)

The owner's decision (2026-11-20): "Lowering the 4 GiB poppler limit to a figure based on measured
peaks is approved."

**Built** on `main` at `a569b483`. `POPPLER_MEMORY_LIMIT_BYTES` (4 GiB on every poppler child) is
two limits now, by what a run needs (`extract/pdf.rs`, `memory_limit_of`):

| limit | value | runs |
|---|---|---|
| `POPPLER_TEXT_MEMORY_LIMIT_BYTES` | 512 MiB | `pdftotext` (Chat/KB text, the layout's words, the layout model's pages with words), `pdftohtml -i` (the layout's fonts), `pdfinfo` and `pdfimages -list` (the picture gate) |
| `POPPLER_IMAGE_MEMORY_LIMIT_BYTES` | 2 GiB | `pdftoppm` (Chat/KB page renders, the page-reading model's pages, the layout model's squares, picture crops) and `pdftohtml -dataurls` (picture runs, said as that tool) |
| `POPPLER_PIPE_READ_PER_BYTE` | 3 | added per byte of a file sent through a pipe, to the run's limit (`pdf::reading`, in `run::poppler_held`, the font/picture runs and the OCR render) and to its step's charge (`charge::piped`, every PDF step; `charge::run` for `run::poppler`) |

The OCR render keeps `OCR_RENDER_MEMORY_LIMIT_BYTES` (2 GiB). `LIBREOFFICE_MEMORY_LIMIT_BYTES`,
`PANDOC_MEMORY_LIMIT_BYTES` and `READER_MEMORY_LIMIT_BYTES` were defined as poppler's; each is a
literal 4 GiB of its own now, unchanged in value. The confinement check (`confine/verdict.rs`) runs
each tool's version run within that tool's own limit (it used poppler's for all four). Charges
(`pool/charge.rs`): `PDF_FILE` 6 → 2.5 GiB (text limit + two layout outputs; also the picture
gate), `POPPLER_TEXT_RUN` 1.5 GiB (the layout pair side by side 2 × 1.5 GiB, was 10), `POPPLER_IMAGE_RUN`
3 GiB, `PICTURE_RUN` 4 GiB, `PAGE_CROPS` 6 GiB (the reader's 4 GiB and two outputs, which
`PDF_FILE` covered before; `PoolCharges.page_crops`, shown on Settings › Documents, drives how many
pages are cropped at once), `HEAVIEST` still 6 GiB. Each `OutOfMemory` names its tool's limit, and
the message says a piped file's share. Tests (`pdf/tests_limit.rs`): a two-million-line path is
refused by name under the text limit (one tenth reads), a 7 814 dpi render (raster within poppler's
own 2^31-byte bound) under the image limit (1 000 dpi renders), the tool → limit table and the
pipe term; `pool/tests_charge.rs` and the Settings test with the new figures.

**Method** (as the OCR render's 2 GiB was set): every poppler run kind lmgw starts, with lmgw's
arguments and input mode (`/dev/stdin` on the file, or a pipe where lmgw pipes), over the corpus'
85 PDFs, the performance inputs (`small50`'s first five, `mixed50`'s first ten, `big200`,
`bigscan`, `scan200`, `text5k`, `bigtable`, `dense200`, `pics100`) and 20 generated files; `VmPeak`
read at exit (an `LD_PRELOAD` destructor), and for the deciding cases the smallest `RLIMIT_AS`
(`prlimit --as`, 4 MiB steps) at which the run gives the unlimited run's output byte for byte and
says nothing of memory. poppler 26.08.0 (`pdftotext -v`), Linux 7.3.

**Peaks** (MiB, `VmPeak` of the run without a limit; **bold** the smallest `RLIMIT_AS` that still
gives the same output). Columns are the runs as lmgw starts them: `pdftotext -layout` piped (Chat,
knowledge bases), `pdftotext` plain and `-bbox-layout` (layout model's words, the layout),
`pdftohtml -xml -i` (fonts), `pdfinfo`, `pdfimages -list` (gate), `pdftohtml -dataurls` (pictures),
`pdftoppm -r 150` piped (Chat/KB page), the page-reading model's render (≤ `PAGE_MAX_PIXELS`), the
layout model's 640² squares (first 20 pages), a crop render of the largest page at 144 dpi:

| input | text piped | text | bbox | fonts | info | list | pictures | Chat render | page model | squares | crop |
|---|---|---|---|---|---|---|---|---|---|---|---|
| the corpus' 85 PDFs and the other perf inputs, largest | 232 (`scan200`, 94 MiB) | 51 | 51 | 50 | 50 | 50 | 62 | 232 | 67 | 51 | 70 |
| `big200.pdf` (215 MiB) | 424, **428** | 41, **44** | 41 | 41 | 40 | 40 | 53 | 424 | 60 | 42 | 59 |
| `text5k.pdf` (5 000 pages) | 88, **92** | 56 | 56, **60** | 58, **60** | 55 | 56 | 58 | 88 | 51 | 44 | 50 |
| 20 000 pages of text | 129 | 97 | 97, **100** | 97, **100** | 96 | 96 | 97 | 90 | 57 | 51 | 57 |
| one page, 100 000 words | 52 | 50 | 50 | 78, **80** | 40 | 40 | 78 | 52 | 52 | 45 | 51 |
| one page, a million words | – | 699 at the 300 s timeout | – | – | – | – | – | – | – | – | – |
| one path of a million lines, 14 400² pt | 415 | 399, **400** | 399, **400** | 399, **400** | 40 | 399, **400** | 399 | 1 573 (raster refused) | 44 415 in 256 s | 14 533 | – |
| 1 000 embedded fonts / 1 000 font names on a page | 64 / 52 | 44 / 52 | 44 / 52 | 43 / 47 | 43 / 40 | 43 / 47 | 43 / 47 | 65 / 66 | 49 / 66 | 43 / 59 | 48 / 65 |
| page tree 1 000 deep / arrays 100 000 deep | 42 / 49 | 42 / 49 | 42 / 49 | 41 / 49 | 41 / 48 | 41 / 48 | 41 / 49 | 53 / 60 | 53 / 60 | 46 / 53 | 52 / 59 |
| trailer claiming 50 million objects | ≤ 52 in every run |
| 40 nested transparency groups, A4 | 41 | 42 | 42 | 42 | 40 | 42 | 42 | 383 | 384 | 107 | 357 |
| 14 400² pt page (text, vectors, a picture) / with 4 groups | 52 / 40 | 42 / 40 | 42 / 40 | 40 / 40 | 40 / 40 | 40 / 40 | 64 / 40 | 52 / 43 (raster refused by poppler) | 60 / 125 | 45 / 48 | – |
| 14 400 × 9 900 pt page (30 000 × 20 625 px at 150 dpi) | 40 | 40 | 40 | 40 | 40 | 40 | 40 | 2 406, **2 408** | 60 | 45 | – |
| 32 000² grey Flate image | 41 | 40 | 40 | 40 | 40 | 40 | 53 | 56 | 55 | 45 | 54 |
| 10 000 × 14 000 JPEG, baseline / progressive | 64 / 64 | 40 / 40 | 40 / 40 | 40 / 40, **44** | 40 / 40 | 40 / 40, **44** | 108 / 106 | 73 / 472 | 55 / 456, **460** | 45 / 444 | 54 / 454 |
| 10 000² RGB Flate image with a soft mask | 232 | 40 | 40 | 40, **44** | 40 | 40, **44** | 249 | 232 | 57 | 45 | 56 |
| JPEG 2000, A4 at 600 dpi (4 960 × 7 016) | 46 | 40 | 40 | 455, **60** | 40 | 455, **60** | 471 | 467 | 463 | 456 | 462 |
| JPEG 2000, A3 at 600 dpi (7 016 × 9 920) | 52 | 40 | 40 | 868, **72** | 40 | 868, **72** | 900, **900** | 884 | 877 | 870 | 876 |
| JPEG 2000, 10 000 × 14 000 | 64 | 40 | 40 | 1 705, **104** | 40 | 1 705, **104** | 1 767 | 1 729 | 1 713 | 1 706 | 1 713 |

Crop renders at `PAGE_RASTER_MAX_BYTES` (an A4 page whole at 1 890 dpi, 15 619 × 22 102 pixels,
0.96 GiB of raster): **2 348** (the baseline JPEG page), **2 676** (the soft-masked image),
**3 160** (the A3 JPEG 2000 scan). A render takes 4 bytes a pixel (raster and alpha: the 7 814 dpi
render of the test is refused at once under 2 GiB, 2.7 GiB resident under 4 GiB), an image
scaled up to the page another 3. A piped file: `big200` 384 MiB above the same run by position
(1.8 times the file), `text5k` 36 MiB (2.2 times). `pdftohtml -i` and `pdfimages -list` decode a
JPEG 2000 image whole although neither uses its pixels; under 512 MiB the decode fails
("Size of tile data exceeds system limits"), the output is the same byte for byte and the stderr
names no memory, so lmgw reads it as a success.

**Numbers** (release builds of `examples/headless` and `lmgw-doc-reader`, `main` `a569b483` and this
branch, `scripts/convert-perf/memrow.sh` on port 8895 in a `systemd-run --user --scope` with
`MemoryMax=8G` and without a limit, and `small50` alone at `MemoryMax=16G`; the same test machine, Linux
7.3, poppler 26.08.0, sign-in off). Wall s, three runs each:

| request | main, 8G | branch, 8G | main, no limit | branch, no limit |
|---|---|---|---|---|
| `small50` | 3.76, 3.66, 3.65 | 1.30, 1.28, 1.28 | 0.35, 0.28, 0.28 | 0.30, 0.26, 0.26 |
| `small50` from 8 callers | 29.49, 29.08, 29.23 | 10.31, 10.50, 10.30 | 2.27, 2.11, 2.15 | 1.97, 1.90, 1.95 |
| the corpus' 20 PDFs | 8.39, 8.34, 8.32 | 4.22, 4.22, 4.06 | 1.34, 1.29, 1.33 | 1.31, 1.31, 1.32 |
| `pics100.pdf`, embedded | 5.42, 5.35, 5.31 | 4.72, 4.72, 4.73 | 0.82, 0.81, 0.78 | 0.82, 0.82, 0.81 |
| a job of 200 HTML files | 0.38, 0.36, 0.38 | 0.36, 0.37, 0.36 | 0.15, 0.17, 0.16 | 0.16, 0.15, 0.18 |
| corpus `pdf/` folder, one file at a time | 8.85 | 5.73 | 4.37 | 4.36 |

`small50` at `MemoryMax=16G`: main 1.90, 1.87, 1.88; branch 0.69, 0.67, 0.67. The corpus' `pdf/`
folder scores the same on all four (28/28, J 0.857, line ratio 0.2701, headings 167/159, tables
38/31, the same warnings). At 8 GiB three PDFs' runs now go at once (`PDF_FILE` 2.5 GiB, was 6):
`small50` 2.9 times faster, from 8 callers 2.8 times, the 20 PDFs 2.0 times; `pics100` takes 12 %
less (its picture runs are `PICTURE_RUN`, 4 GiB). Without a limit nothing changes but the 8
callers' 11 % (the layout pair side by side is admitted more often: children's CPU 51 s against
38 s for the same wall).

**Decisions:**
- *Two limits, not one lower one*: the runs that need no image's pixels peak at 40–100 MiB on
  ordinary files and decide what a PDF's text is charged; the runs that decode images peak with
  the image (a JPEG 2000 scan 12 bytes a pixel) or the render (4 bytes a pixel). One figure would
  keep the text runs at the image runs' size.
- *512 MiB for the text runs*: five times the largest ordinary file (the 20 000-page book, 100
  MiB), rounded up to a power of two. A page drawing one path of more than about 1.2 million
  pieces is run again under the wide limit with a note (review round below; the million-line page
  needs 400 MiB); a page of a million words was stopped by `PDF_TIMEOUT` before it was by memory.
- *`pdftohtml -i` and `pdfimages -list` are text runs*: both decode a JPEG 2000 image whole and
  throw the pixels away; under 512 MiB the decode fails, their output stays the same byte for byte
  and nothing about memory is said (checked on the three JPEG 2000 inputs and the progressive
  JPEG). Under the old 4 GiB a 10 000 × 14 000 JPEG 2000 scan cost the layout's `pdftohtml` 1.7
  GiB; it costs 104 MiB now.
- *2 GiB for the image runs*: 2.2 times the largest ordinary scan measured (A3 at 600 dpi as JPEG
  2000, 0.9 GiB); the 10 000 × 14 000 JPEG 2000 image fits (1.7 GiB). Run again under
  `POPPLER_WIDE_MEMORY_LIMIT_BYTES` (4 GiB, the old figure) with a `memory_limit_widened` note,
  as they read under 4 GiB before (review round below): a render of more than about 500 million
  pixels (a Chat/KB page of about 15 m² at 150 dpi; the page-reading and layout models scale
  their pages down, so they never get there), a JPEG 2000 image of more than about 170 million
  pixels, and crops near `PAGE_RASTER_MAX_BYTES` (`images_scale` of about 20 and more on a full
  A4 picture: 2.3–3.2 GiB at the bound).
- *A piped file adds 3 × its size* (`POPPLER_PIPE_READ_PER_BYTE`) to the run's limit and its
  charge, the bound of a buffer that doubles (old and new at once), rather than a fixed figure: with
  a fixed 512 MiB, Chat could not read a PDF above about 230 MiB.
- *The confinement check runs each tool within its own limit* (poppler's text limit, tesseract's,
  pandoc's, ffmpeg's), not poppler's for all four.
- *`PAGE_CROPS` is a charge of its own* (6 GiB: the reader's 4 GiB, the raster and the crops), as
  `PDF_FILE` no longer covers it, and `PoolCharges.page_crops` is a new field of the pool's facts
  (Settings › Documents says it, "a page's pictures cut out"); the crops' width per caller follows
  it.

**Not yet:** `PAGE_RASTER_MAX_BYTES` (1 GiB) is not derived from the image limit (a render needs
4–7 bytes a pixel of raster, so near the bound a crop is refused by the limit instead, with its
name); the OCR render's 2 GiB was not measured again on JPEG 2000 scans (it decodes them like any
render: an A3 600 dpi scan fits, a 170-million-pixel one does not); LibreOffice's, pandoc's and the
reader's 4 GiB were not measured in this round; Chat and knowledge bases still pipe a PDF (a spool
would drop the 3 × file term).

**Unverified:** the corpus' other folders (only `pdf/` was scored); Windows (no limit is set there
yet, unchanged); poppler versions other than 26.08.0 (the figures are per version; a new poppler
needs this measurement again: `VmPeak` and `prlimit` over the same inputs); the rows ran beside
another corpus measurement on the same machine.

##### Review round (2026-11-20)

A review of the branch found one blocker and three should-fix items; the wide fallback was added
following the owner's rule that what works is never blocked (2026-11-11). **Built** on the branch at
`4a58284c` (rebased onto `main` `9a51cf08`):

- **B1, the layout read under-charged.** `after_runs` of two outputs of about 1 GiB is up to
  6 GiB, and `Slot::shrink` silently kept it at the 2.5 GiB (3 GiB for the pair) the runs were
  charged, so the "never undercut" of "PDF charge after the runs" no longer held. `Slot::fit`
  (`pool/fit.rs`) shrinks as before, or grows the slot at once where the pool has the memory free
  now; otherwise the file gives its slot and outputs back and runs both again, one after the
  other, on a slot charged `max(PDF_FILE, what the outputs showed)` (`file/pdf/runs.rs`), so no
  step waits for memory while it holds a slot (the crops' ordered group holds finished slots, and
  a grow that waited could deadlock against it). `shrink` asked for more than it holds logs it. The
  picture gate's read is fitted the same way (its two outputs, their reading and lossy UTF-8 copies
  at 3 bytes a byte), or the gate is unsure. Test: `pool/fit/tests.rs`, two zeroed 1 GiB outputs on
  an 8 GiB pool: grown to 6 GiB in an empty pool, refused (slot unchanged) beside another PDF.
- **The wide fallback** (`pdf/wide.rs`, `POPPLER_WIDE_MEMORY_LIMIT_BYTES` 4 GiB, the old figure):
  a run that ran out of its measured limit, or an image run (`pdftoppm`, `pdftohtml -dataurls`)
  whose stderr says `Did no succeed opening JPX Stream.`, is run once more under it. Never a
  timeout, never twice, never while holding a slot: own-slot runs give theirs back first
  (`wide::own`, charged the wide limit + pipe share + output, 5 GiB: Chat/KB text and page renders,
  which pages have words, the page-reading model's page, the layout squares); the layout pair
  gives everything back and takes `PDF_FILE_WIDE` (6 GiB) for both runs one after the other;
  a picture run gives its slot back and takes `picture_run(Wide)` (6 GiB); a crop runs again in
  its own slot (`PAGE_CROPS`, now `max(POPPLER_WIDE_RUN 5 GiB, the reader's 6 GiB)`, still 6
  GiB); the picture gate is not run again (unsure costs picture runs, loses nothing). The run
  again gets the run's timeout within what the file's clock has left; with none left the first
  outcome stands, and an out-of-memory error says "not run again … deadline". The wide run is the
  run lmgw made before the limits were split, so what it refuses was refused then too.
- **Said.** Each widened run is a `memory_limit_widened` warning (tool `poppler`, the program,
  both limits, the pages; alike notes merged), collected per file by a task-local scope opened in
  `file::convert` (`wide::notes`) and written by `file/memory.rs`; Chat and knowledge bases read
  the notes a run hands back (`PdfText.notes`, `pdf::Rendered.notes`) into their notes. An image
  poppler still drew blank under the wide limit (above about 340 million pixels, or damaged), or
  under its own when no time was left, is `image_not_decoded`. A run past the wide limit too
  names both (`PdfError::OutOfMemory` carries its `Limit`, whether the file was piped, and
  `Retry`), the pipe note only for a piped file.
- **S1, JPEG 2000 probed** (poppler 26.08.0, openjpeg 2.5.4, `prlimit --as`, a set of hostile
  test PDFs and two made with PIL at 14 000² and
  20 000² pixels, kept out of the repository): `pdftoppm` (Chat render, page model, squares,
  crops) and `pdftohtml -dataurls` do not fail on an image whose decode does not fit: they exit 0
  with the image blank (a 1.5 KiB PNG for a page), stderr ending in "Did no succeed opening JPX
  Stream." (before it "Size of tile data exceeds system limits" for the single-tile image; the
  tiled ones say only "Failed to decode the codestream"). 10 000 × 14 000 decodes under 2 GiB;
  14 000² is blank under 2 GiB and whole under 4 GiB; 20 000² is blank under 4 GiB too, as it
  was before this branch, silently then. The docs' "refused by name" is corrected.
- **S2.** A layout run out of memory is run again wide; past that the file fails with the new
  error code `memory_limit` (422; api-types, the compare script's list, the Documents lab's words,
  the API description), not `unreadable`.
- **S3.** A confinement check run that ran out of its limit (`run::ran_out_of_memory`) records
  nothing against the confinement, which stays on, and logs the limit.
- **Nits:** the exhaustive `PopplerTool` (name, limit, whether it draws) replaces
  `memory_limit_of`'s catch-all; the limits moved to `pdf/limit.rs`; Settings › Documents' share
  sentence names `page_crops`; `scripts/convert_compare_test.py`'s mock charges are today's.

Tests: `pdf/wide/tests.rs` (when a run is run again, what it says, the deadline);
`pdf/tests_limit.rs` on real poppler under the real limits (a two-million-line path read wide with
the note) and under limits of the test's own (`limit::with_limits`: a path past both limits
refused naming both, a 1 000 dpi render read wide, a 2 000² JPEG 2000 image made by
`opj_compress` decoded wide, and left blank and named with the wide limit low too);
`file/pdf/runs/tests.rs` (stand-ins reading `ulimit -v`: the layout read wide with its note, and
refused naming both); `file/tests/memory.rs` (the conversion's warning, and `memory_limit`);
`confine/verdict/tests.rs` (S3). Live (debug `examples/headless` on port 8895, `pictures:
embedded`, the two made images on an A4 page with a line of text): the 14 000² page converted
with two `memory_limit_widened` warnings (`pdftohtml -dataurls` and the crop's `pdftoppm`, page 1,
both limits named) and its picture; the 20 000² page converted with those and two
`image_not_decoded` warnings naming the wide limit; a corpus PDF with none of them.

**Decisions:**
- *The JPEG 2000 line is the trigger for image runs only*, and a damaged image triggers it too:
  one more run, then `image_not_decoded`, which says "larger than about 340 million pixels or
  damaged".
- *A file that cannot grow its slot runs its layout runs again* rather than waiting with its
  outputs held outside any charge; it costs the runs' time again on files with about 0.65 GiB of
  layout output and more, only while the pool is full.
- *A grow does not ask the caller's share*: it is part of a step the caller already started.
- *`memory_limit` is a new error code* rather than `unreadable` with the limit in its message.
- *Notes are per file*, not per attachment of an e-mail (the innermost conversion's scope keeps
  them).
- *A widened run leaves the file's status as it was* (nothing was lost); an `image_not_decoded`
  image does not make it `partial` either.

**Not yet:** the OCR render (its own `OCR_RENDER_MEMORY_LIMIT_BYTES`, unchanged by this branch)
has no JPEG 2000 check: a scanned page whose image does not decode within 2 GiB is recognised
blank, silently, as before; a knowledge base's text run widened says so in the base's notes, a
Chat attachment's text only in the log (Chat stores its text at upload and has no note channel
there yet); only the first 64 KiB of stderr are read for the JPEG 2000 line.

**Unverified:** the before/after rows were not run again: nothing on an ordinary file's path
changed its charge or limit (`PDF_FILE`, `PICTURE_RUN`, `PAGE_CROPS` keep their values), and the
added work per run is one look through its kept stderr; Windows (compile check only).

##### Review round 2 (2026-11-20)

A second review of the round above (`4a58284c..4952eab3`) found no blocker and four
should-fix items. **Built** on the branch `poppler-fix2` from `main` at `4952eab3`:

- **S1, the JPEG 2000 trigger is memory only.** poppler ends every JPEG 2000 image it could not
  decode with "Did no succeed opening JPX Stream.", damaged or not. A watch reads its lines as they
  come (`pdf/wide/jpx.rs`): run again wide only where openjpeg said an allocation failed (its
  2.5.4 lines: "exceeds system limits", "Not enough memory", "Cannot allocate", "memory error",
  "allocation fail", "Failed to allocate", "Error allocating"), or where a decode failed with no
  openjpeg line saying why (probed: a tiled image whose whole picture openjpeg cannot allocate says
  nothing but the JP2 wrapper's "Failed to decode the codestream", a raw tiled codestream nothing
  at all). A damaged image always has a line naming the damage ("Stream too short", "Tile part
  length size inconsistent…", "A marker ID was expected…", "segment too long…"), under every
  limit: `image_not_decoded` (worded "damaged or in a form openjpeg does not read") at the run's
  own limit, not run again. Probed with poppler 26.08.0 / openjpeg 2.5.4 under `prlimit --as` on
  the three JPEG 2000 scans of the hostile set and images made with `opj_compress`
  (single-tile, 512² and 256² tiles, raw codestreams; whole, cut to 50 % and 60 %, zeroed,
  bit-flipped): every memory case and every damage case told apart (`jpx.rs`'s test holds the
  lines word for word). Counts are per image, so a run with one image of each is both.
- **S2, the whole stream.** `run/stderr/scan.rs`: the out-of-memory words are matched over every
  byte with a window of the longest word less one carried across chunk borders, and the JPEG 2000
  watch is handed each line (cut to `LINE_KEEP_BYTES`, 1 KiB) as it arrives; nothing grows with
  the stream, the 64 KiB kept is display only. `run::Tool::watch` is a `Watcher` now; the
  confinement check's whole-read stderr goes through the same words (`Said::of`).
- **S3, a grow waits its turn.** `Slot::fit` grows only when no step waits and, where shares
  apply, when the caller's charges with it still leave one more step like this one's first charge
  free (share.rs's rule); otherwise `Err`, and the file's runs go again on a new slot in the
  queue. Nits: the only step running grows past the memory (as it is admitted alone), and a grown
  pair keeps one slot (`hand_out` runs). Tests: `pool/fit/tests.rs` (refused past a waiter and
  past the share, allowed with neither; alone; the pair).
- **S4, the OCR render** (`ocr/pages/render.rs`) runs through `pdf::wide` as
  `PopplerTool::OcrRender` under `Limit::OcrRender` (`OCR_RENDER_MEMORY_LIMIT_BYTES`): the same
  watch, its page's `image_not_decoded`, and once more wide on a new slot charged
  `OCR_PAGE_WIDE` (4.25 GiB) after giving its own back; nothing waits holding a slot. An
  out-of-memory render is run again wide too (it fell out of `settle`).
- **Nits.** The floor rerun gets its time from the file's clock (`again_within`), and with none
  left the outputs are read on the slot they have, logged (`file/pdf/runs/tests.rs`, end to end
  with a 900 MiB stand-in output). A blank-image first run is kept until its run again answers;
  one that ends without an answer leaves it standing (`Blank::AgainFailed`), its bytes charged:
  the first slot shrinks to them with no slot count (`shrink(0, kept)`) while the wide one is
  waited for, and the wide slot carries them. `memory_limit_widened` means the wide run's output
  was used (it fit, or decoded at least one more image); a run again that decoded nothing more is
  `image_not_decoded` alone. A note of a run over several pages says "in the run over pages
  a–b". `docs/documents.md`'s retry time matches the code (the program's own timeout, the
  squares' what is left, within the file's clock). Chat: a PDF attachment's text notes go to its
  `meta.notes`; a page with notes is not cached, so it is said on every send. docling adapter:
  `memory_limit` and `output_too_large` are both `policy` on a `failure` (docling's word for a
  document past its own limits); `image_not_decoded` is a `backend_failure` entry with its page,
  the status unchanged.
- **Probed (the review's unverified point):** `pdftohtml -xml -stdout -i` (and `pdftotext`,
  piped, plain and `-bbox-layout`) under 512 MiB on the A4 and A3 600 dpi JPEG 2000 scans and the
  10 000 × 14 000 one give the same output byte for byte as without a limit; A4 decodes, A3 and
  10 000 × 14 000 fail their decode with "exceeds system limits" and no out-of-memory word, so
  nothing is run again for them.

**Decisions:** *a decode that fails without openjpeg saying why is memory*
(the tiled case; a damaged image without any line would be run again once, needlessly); *the
first run's kept output is charged without a slot while the wide one is waited for* (a zero-count
slot cannot deadlock: with nothing running the head waiter runs alone); *the OCR render's
out-of-memory is run again wide too*; *a Chat page with notes is not cached* (the cache has no
notes column yet; such a page is rendered again on each send); *`image_not_decoded` is a lost
part for the docling answer* but does not make it partial, as the native answer.

**Not yet:** the Chat page cache keeping notes (a migration); a run again of the layout model's
squares that ends early leaves the page it was writing cut (its read fails by name), the others
whole; Letter/Legal 600 dpi scans were not among the probe inputs (A4 and A3 bracket them).

**Unverified:** openjpeg versions other than 2.5.4 (its lines are what the watch reads; a new
openjpeg needs the probe again: `prlimit --as` over the same inputs); Windows (compile check only).

##### Review round 3 (2026-11-20)

A third review (`9d9ed62a..3562a88d`, no blockers) found two should-fix items. **Built** on
`poppler-fix3` from `main` at `3562a88d`:

- **S1, out of memory means a failed allocation.** `run/stderr/scan.rs` no longer matches "memory
  alloc", which also read poppler's "Bogus memory allocation size" (a damaged object's overflowing
  size; poppler skips it and exits 0, so a long file could end as `memory_limit`) and openjpeg's
  "Memory allocation failure in opj_jp2_apply_pclr()". The words are "bad_alloc", "out of
  memory", "memory allocation failed" and "memory allocation error" (`strings` on libpoppler
  26.08.0, libopenjp2 2.5.4, libavcodec.so.63, libpng16, libtiff.so.6 and pandoc; a Rust tool's
  "memory allocation of N bytes failed" is caught by its abort). The palette line goes to the
  JPEG 2000 watch, whose "allocation fail" reads it as memory. Tests: the words
  (`scan.rs`), an exit-0 stand-in pdftotext saying the bogus size is used as it is, not run wide,
  no note (`file/pdf/runs/tests.rs`), the palette line counted memory (`jpx.rs`). The gate
  showed the line's other meaning: a page bitmap past what poppler allocates at all (300 × 200 pt
  at 20 000 dpi) says only "Bogus memory allocation size" under 4 GiB (under 512 MiB also "Out of
  memory" from `prlimit`, not from the confined run) and pdftoppm writes a 1 × 1 stub, exit 0. So
  the line with a 1 × 1 PNG or PNM on stdout is out of memory as before (`run/stub.rs`): run again
  wide, then refused naming both limits, never a silent one-pixel page.
- **S2, only an openjpeg error says why.** Probed: poppler prints openjpeg's warnings as
  "Syntax Warning: …<0a>" (an unknown marker segment after SIZ: "Syntax Warning: Unknown
  marker<0a>"); a tiled 8000² image with one under `prlimit --as` 512 MiB says that warning, then
  only the wrapper's line, and decodes at 4 GiB. Before, the warning marked it damaged. `jpx.rs`
  counts only "Syntax Error: " lines; that run's lines are its test, word for word.
- **Nits.** `Slot::shrink` asserts (debug) and warns when a leased slot is asked to shrink to 0.
  The model-stage deadline test takes the tools' time as the shorter of two one-page runs.
- **A fourth load flake:** `a_pdfs_one_slot_starts_the_clock_and_a_late_one_is_stopped_and_frees_it`
  slept a fixed 900 ms and then expected the request waiting in the pool; under load it had not
  got there (`scripts/flake-hunt.sh copies`, 32 copies × 3: 75 of 96 failed, all on that assert).
  It waits for the waiter now, then holds the slot 900 ms past it: 0 of 96, and 0 of 32 with 16
  copies on four cores. `flake-hunt.sh` puts its frozen copy beside the test binary, so the
  copy finds lmgw's document reader (before, every reader-using test failed there).

**Decisions:** *a render stub after "Bogus memory allocation size" is worded out of memory* (as
before this round), though more memory does not help it: the wide run says the same and the page
is refused naming both limits.

**Not yet:** an error of its own for a page past poppler's bitmap size (it is `memory_limit` /
the render warning's out-of-memory wording); the pool's alone rule with keeps (zero-count slots, `Slot::shrink(0, kept)`): a step
admitted alone may pass the memory by the other waiters' keeps (each the output its first run
kept, under 1 GiB), and a waiter's own keep counts beside its new charge until it drops it, so it
may start later than it needs, at the latest alone (said at `pool.rs`'s `fits`); the OCR render's
wide slot keeps the first PGM's charge into tesseract's read; a grow refused whenever a step waits
(S3) runs a batch PDF's pair again past about 0.85 GiB of outputs, not said in
`docs/documents.md` yet.

### Security round B — confinement, messages and output (2026-11-19)

**Built** on `main` at `52468847` from the security review of `f1919ece` (findings M3, M4, L1–L6,
hardening 1–3 and 6–9) and the carried bugs, each re-read or re-probed against the code after the
performance round first; with the verification of the performance fix round (below). Lane A (answer bytes, the pool's share per caller, job quotas) was built beside it. Every
item has a test that fails without it.

- **M3, no minimum Landlock ABI.** The reader's rule building is pure numbers now
  (`lmgw-doc-reader::sandbox::abi`: the rights, network rights and scopes each ABI handles, the
  attribute size, and `lacking(abi)`), tested for ABI 1 to 11 on any kernel. LibreOffice's probe
  fails by name below ABI 4 (`LIBREOFFICE_MIN_ABI`, the first that denies TCP), as without
  Landlock, and so does its run (`confine::required`). Each protection the running kernel lacks
  (TCP below 4, abstract Unix sockets and signals below 6, pathname Unix sockets below 9, UDP
  below 10) is named with the ABI found once in the log (`confine::available`) and on Settings ›
  Documents' "Programs run confined" (`Confinement.lacks`, "For every tool, this kernel leaves
  open: …"). `confine.rs`'s module doc, `docs/server.md` and `docs/documents.md` say so. Not run
  on such a kernel (this one is ABI 11): the rule building, the floor and the row's sentence are
  tested by number.
- **M4, refused values echoed without bound.** One helper (`convert::quoted`: `quoted`, and
  `quotes_bounded` for serde's messages, whose list of expected values stays whole) quotes at most
  `MAX_QUOTED_BYTES` (64) of a caller's value and says `… (cut at 64 bytes, MAX_QUOTED_BYTES)`:
  `pages`, the form's true/false and number fields, a field given twice, serde's unknown field or
  variant, an `http` source's scheme, docling's target kind, the docling table's recorded values
  and details (`image_export_mode`, `ocr_engine`, `ocr_lang`, `pipeline`, `to_formats`) and
  `to_formats`' error items. Every usage row's `error_msg` is cut at `ERROR_MSG_MAX_BYTES`
  (4 KiB) where all rows are written (`store::request_logs`), saying so. Probe: the review's 5 MiB
  `pages` field is a 400 of under 2 KiB, its row under 2 KiB.
- **L1, tool stderr with paths.** `run::first_lines` cuts every absolute path in a tool's stderr
  to its file name behind `…/` and drops heap addresses (`run/scrub.rs`); the runner names a tool
  by its file name in what the caller reads (also the file-size limit's message, N-e below); the
  log line "document tool … failed (status) in <run dir>" keeps the path and the run directory.
  The review's `bogus.mkv` line becomes `[in#0 @ 0x…] … Error opening input file file:…/in.media.`
  `tool_not_started` already covered the reader's own lines; this covers ffmpeg, pandoc,
  LibreOffice, tesseract and poppler.
- **L2, markdown carries live markup.** `docs/documents.md` ("The markdown is the document's, not
  sanitised") says the output is untrusted and must be sanitised by whoever renders it. The Labs
  page's renderer is tested on lmgw's own answers for the review's `xss.html` and `xss.pdf`
  (`documents_lab/rendered.rs`: no script, no handler attribute, no `javascript:` or `data:`
  link, no remote picture).
- **L3, the cheap ones.** A run's process group is killed after the tool ended by itself too:
  its end is seen through a pidfd that does not reap it, so the group id cannot be reused before
  the signal (`child::group::end_group`; off Linux not yet). Its stderr is read for at most
  `STDERR_DRAIN_TIMEOUT` (5 s) after that, and the message says so. A `--read-write` rule makes
  no symbolic link and no cross-directory link or move (`abi::WRITE` without `make_sym` and
  `refer`), and the gateway opens what a tool wrote without following one, each step `openat`
  with `O_NOFOLLOW`, a regular file required (`child::beneath`): LibreOffice's result, ffmpeg's
  WAV, the layout model's squares (`square::rendered` skips anything but a regular file). The
  group kill ends what stays in the group; a process that calls `setsid()` survives, still under
  its Landlock rules and limits, and ending it needs a supervising parent, not built yet (the
  review round below: the gateway maps a copy of the WAV, and every run has a CPU time bound).
- **L4.** Closed by the performance round: poppler, tesseract and the reader's parsing runs are
  confined, with a cleared environment. Still started unconfined, with the gateway's fork: the
  tool probes (`probe.rs`, once per tool change: `--version`-style runs on no document) and the
  test gate `pdf::available`. Where Landlock is missing every tool runs with its limits alone
  (LibreOffice not at all).
- **L5 and hardening 6, LibreOffice's socket.** The profile template's directory is
  `lo-<pid>-profile-<12 hex>`, the random part drawn once per process, so its socket's name cannot
  be worked out from the data dir and the pid. A socket already at a run's socket path before it
  starts, or another user's there after it ended, fails the file as `tool_not_started` (500),
  "LibreOffice could not run here: its single-instance socket in /tmp is held by another
  process…", the log naming the socket. A run that ends by itself without a result and without
  "could not be loaded" (which a damaged or locked file says, with exit 1) is `tool_not_started`
  too, worded as lmgw's (`LoError::NoResult`).
- **L6, the outbound guard.** `mcp/selfadmin/guards/outbound.rs` refuses `lmgw__model_set`,
  `lmgw__candidate_alias_set`, `lmgw__local_model_set`, `lmgw__aux_model_set`,
  `lmgw__audio_model_set` and `lmgw__upstream_set` when they name the page-reading,
  picture-description or speech-to-text model, any hop of its fallback chain, or the upstream any
  of them routes to, with the same "change it in the dashboard, nothing was changed" wording. A
  candidate alias `preview` passes.
- **Hardening 1.** Reviewed, not changed (the review round below): `GET /version` names lmgw's
  version to anyone, as `GET /api/version` does.
- **Hardening 2.** A zip entry stem that starts with `-` or is a Windows device name (`CON`,
  `NUL`, `COM1`…`COM9`, `LPT1`…, with any extension after it) gets `_` in front and counts as
  renamed (`docling_serve/zip/guard.rs`).
- **Hardening 3, not done:** `script-src 'self' 'wasm-unsafe-eval'` on the dashboard breaks the
  Chat's HTML preview, whose sandboxed `srcdoc` frame inherits the page's policy and runs the
  previewed page's inline scripts. Checked 2026-11-19 with headless Chrome: a page with that
  policy and a `sandbox="allow-scripts"` `srcdoc` frame logs "Executing inline script violates
  the following Content Security Policy directive", the same page without it runs the script.
  The dashboard's preview would have to move to a page of its own with its own policy first (as
  the extension's does), so the CSP is unchanged.
- **Hardening 7, 9.** `docs/documents.md`: serve exported pictures with `nosniff`; the layout
  model runs in ONNX Runtime from the `ort` crate's download at build time, pinned by the crate
  version.
- **Hardening 8.** LibreOffice reads `/usr` and executes only its install directory, its start
  script's shell and helpers and the dynamic loader (`confine::LIBREOFFICE_EXECS`, from
  `strace -f -e trace=execve` on LibreOffice 26.8.2.2). It also probes for Java, which is now
  refused; the corpus' doc, ppt and rtf answers are unchanged (below).
- **Carried bugs.** (a) A pptx slide's title and picture: the office reader writes a slide's
  shapes one line apart, so the picture's marker joined the title's paragraph in `output: json`;
  `doc/from_md.rs` makes a marker line a block of its own, so it becomes a `figure` block (the
  markdown is unchanged). (b) A transcription takes the file's clock over as a page stage does
  (`FileClock::hand_over`) and ends itself at the deadline (`deadline_exceeded`, its call kept in
  `stats`): an e-mail with a slow audio attachment keeps its body and earlier attachments and
  names the attachment with those the deadline ended, where the batch used to fail the whole
  message. (c) Every model call records itself where it is sent (`convert::sent`, a task-local
  ledger per attachment read, nested ledgers counting too), so an attachment the message's
  deadline cuts mid-call adds its calls to `stats.models`.

#### From the verification of the performance fix round

- **SF1.** Confined-check verdicts are kept by tool and directory (`verdict::key(ToolId, dir)`),
  so poppler, tesseract, pandoc and ffmpeg in one `/usr/bin` keep one each; `confine::helper_for`
  takes the tool.
- **SF2.** A `bin` whose prefix is a home (`~/bin`, `/home/<user>/bin`, `/root/bin`) gets no
  `lib`/`share`/`libexec` siblings (`reach::siblings`).
- **SF3.** `/home`, `/var/home` and what is above them never pass `reach::too_wide`, whatever
  `HOME` says.
- **N-a.** `confine.rs`'s module doc: no home directory itself or above one, a tool's own install
  directories inside a home pass `reach`'s check.
- **N-e.** The inherited file-size limit's message names the tool by its file name.
- **N-f.** The S4 test is skipped when run as root (a mode-0000 directory opens anyway).
  A `0400` spool does not stop a tool that runs unconfined as root from opening it for writing;
  Landlock does stop a confined one.
- **N-g.** A path inside lmgw's data, settings or cache directories does not pass either.
- **N-c.** LibreOffice's gate asks the reader again (`recheck`) when its last answer was not the
  kernel's.
- **Not yet:** N-d (any exit 125 with the reader's prefix counts as the kernel's answer, a
  transient failure inside the probe too; it stays until restart); N-b (the tessdata rule opens
  the whole named directory, not its `*.traineddata` files and `configs`); N-h (`/nix/store` is
  read and executed as a whole).

**Decisions:**
- *LibreOffice's floor is ABI 4* (TCP), as planned: on ABI 4 to 9 it runs with UDP or
  pathname Unix sockets still open, each named; a network namespace in the reader instead is not
  built.
- *The lacking protections are said once for all tools* (one sentence on "Programs run confined"
  and one log line), not on each tool's row: they are the kernel's, the same for every tool.
- *`MAX_QUOTED_BYTES` is 64*, the docling table's `MAX_NAME_BYTES` for unknown names;
  *`ERROR_MSG_MAX_BYTES` is 4 KiB* and applies to every route's usage rows, cut where rows are
  written (`store::request_logs`), not per route.
- *serde's messages keep their words*: only their quoted runs are bounded, so "expected one of …"
  stays whole.
- *L1: the caller keeps a tool's stderr lines with the paths cut*, not the exit status alone:
  pandoc's and ffmpeg's reasons are the caller's answer about its own file. The log gets the
  tool's path, its status and its run directory, not the stderr text (the project's rule on logs).
- *The group is killed through a pidfd on Linux only*; elsewhere a normal end still disarms the
  kill, as before. `STDERR_DRAIN_TIMEOUT` is 5 s.
- *`refer` is dropped from the write class beside `make_sym`*: a hard link into another
  directory of the run is refused too. LibreOffice's, ffmpeg's and poppler's runs, their tests
  and the corpus were unchanged by it.
- *No new error code*: a taken socket and a LibreOffice run without a result are
  `tool_not_started` (500), with their own words.
- *The template's random part is drawn once per process* (each run copies the same template).
- *L6 refuses by name and by the chain as it is when the call is made*, the six tools above;
  the dashboard's own ops (`/api/op/*`) are the owner's and are not guarded.
- *Zip prefix `_`*, counted with `entry_names_renamed`.
- *Java is not allowed to LibreOffice* (hardening 8): its start script probes for it; the corpus'
  doc, ppt and rtf converted the same without it.
- *The pptx fix is in `doc/from_md.rs`*: any marker line is a block of its own in `json`, for
  every office format; markdown is unchanged.
- *Every transcription hands the clock over*, not only one inside an e-mail (as every page stage
  does): a lone audio file past its deadline fails `deadline_exceeded` with the call's own words
  ("had not answered when the caller's document_timeout of N s passed") instead of the batch's.
- *A cut attachment's calls are counted with the answering route's name* (`answered_by`, and
  `answered_for` when a fallback answered), calls only, nothing read.
- *The new wire fields' doc texts are short*: the admin OpenAPI document reached 2,200,311 bytes
  with the first wording, past `openapi_structure`'s `ADMIN_MAX` (2,200,000); the texts were
  trimmed rather than the bound raised, so the document is just under it now.
- *The ONNX Runtime line went into Settings › Documents' layout model paragraph* of
  `docs/documents.md`, where the docs name the model; no list of linked libraries exists yet.

**Not yet:** hardening 3 (the dashboard's `script-src`, above); a squat on a run's own socket
in the moment between the reader's command line and LibreOffice's bind (its directory has a random
name, which is on that command line); the group kill after a normal end off Linux; L3 item 4 (a
system-call filter in the reader); the tool probes started through the reader (still forked,
unconfined); L6 for changes that reroute a chosen model without naming it (an upstream's catalog
that changes, a fallback chain as it would be after the call); the three nits above (N-b, N-d,
N-h); `file/more.rs` is 631 lines (13 more): not split in this round.

**Unverified:** a kernel below Landlock ABI 10 (rule building, floor and wording tested by
number; the test machine is ABI 11); ffmpeg's path-free message through the route (no speech-to-text
model on the dev instance: the review's captured `bogus.mkv` text is the unit test); the server
image, the RPM and Windows (the Windows compile check is the integration step's); a Nix system for
S1's store rule; the Labs page and the HTML preview in the app's WebKitGTK window (the renderer
was tested, and the CSP effect in headless Chrome).

**Note (2026-11-20):** LibreOffice 26.8.3 links an RTF's `INCLUDEPICTURE` target (`r:link`, a
relationship with `TargetMode="External"`, no `word/media/` entry) instead of embedding it (measured
2026-11-20); confinement still covers the older versions that embed, and lmgw's readers never follow
an external picture link (`a_linked_picture_naming_a_local_file_is_never_read_from_the_machine`, the
docx and pptx half; the answer keeps the placeholder with a `pictures_partial` note).

**Corpus** (debug builds of `main` at `52468847` and of this round, the same dev instance setup
on `127.0.0.1:8891`, Landlock ABI 11, LibreOffice 26.8.2.2, poppler 26.08.0; `python3 -I
scripts/convert-measure.py --corpus "$CORPUS" --lmgw http://127.0.0.1:8891 --formats
pdf,html,doc,ppt,rtf`): unchanged, pdf 28/28 J 0.857 line ratio 0.2701 headings 167/159 tables
38/31; html 33/33 0.9422/0.8494; doc 0.9946/0.9906; ppt 0.9121/0.75; rtf 0.9758/0.8889, on both.
`scripts/convert-same.py` over the 30 PDFs of the performance round (the corpus' 28,
`pics100.pdf`, `text50.pdf`) and the corpus' doc, ppt, pptx, rtf and html files (74 files, every
output and picture mode): **660 of 666 answers identical**; the 6 that differ are the `json`
answers of `pptx/powerpoint_with_image.pptx` and `pptx/powerpoint_malformed_pictures.pptx` in
the three picture modes, the carried pptx bug's fix: they now have 1 and 2 `figure` blocks and no
U+FFFC in any block (their pictures' markers were joined to the slide text before). The ppt's
answers did not change (its slides put no picture one line below text).

#### Review round (2026-11-19)

From the review of `972fd193` (6 should-fix, 7 nits), each item with a test that fails without
it.

- **S1 (M4), every refusal bounded at its outlet.** The documents route's, its jobs' and the
  docling routes' errors become a body in one place each (`server/documents.rs::refusal`, which
  the job routes use too, and `server/docling.rs::error`); there the message is cut at
  `ERROR_MSG_MAX_BYTES` (4 KiB) on a character boundary, saying `(cut at 4096 bytes,
  ERROR_MSG_MAX_BYTES)` (`convert::quoted::outlet`, `outlet_openai`, `outlet_detail`: each string
  of docling's `detail`, and a list kept while it stays within the bound, the rest named by one
  last entry of docling's shape). A job's stored error and each file's are cut the same where
  they are written (`store::convert_jobs`). `quotes_bounded` also bounds the runs outside quotes
  at `MAX_UNQUOTED_RUN_BYTES` (256), so a value holding a backtick or `"` no longer escapes it;
  `files: {e}` and docling's body error go through it. Probe (dev instance, 5 MiB values): `output`
  with a backtick 400 of 481 bytes, `ocr_languages` with `"` 400 of 468, an unknown field name
  400 of 687, `files` as a string 400 of 300, docling `sources` as a string 422 of 291, a 60 KiB
  job id 404 of 4,226 bytes; their usage rows 196 to 593 bytes.
- **S2, the protections below ABI 3 and 5.** `abi::lacking` names "emptying files by truncating
  them is not blocked, so a tool running hostile code could empty any file this user may write,
  lmgw's database included" below ABI 3 (`TRUNCATE_ABI`) and "ioctl calls on devices are not
  limited" below ABI 5 (`IOCTL_DEV_ABI`), on the Settings row and in the log as the others; no new
  refusal (LibreOffice's floor stays ABI 4).
- **S3, a process that leaves its group.** The gateway maps no file a tool can still write:
  ffmpeg's WAV is copied into an unnamed file in the spool's root (`O_TMPFILE`, else created and
  unlinked; no tool's rules reach that directory, only their run directories below it), a clone of
  its blocks (`FICLONE`) where the filesystem shares them, else `copy_file_range`, at most the
  WAV's bound and one byte; the copy is mapped (`media/ffmpeg/mapped.rs`). The other reads of a
  tool's file after it ended were checked: LibreOffice's result and the layout squares are read,
  not mapped, and no further than they can be (`read_bounded`'s `MAX_UNCOMPRESSED_BYTES`;
  `square::open_rendered` now at most `ppm_max_bytes(side)`, a square's header and pixels); page
  renders and crops come over stdout under the output bound. Every tool started through the
  reader gets `--cpu-seconds` (`RLIMIT_CPU`, hard limit one second above: `SIGXCPU`, then
  `SIGKILL`) of its wall timeout × the cores the gateway may use (`run::cpu::CpuBound`,
  `available_parallelism`; ffmpeg's 300 s on a 32-thread machine: 9600 s), which a process that
  left the group keeps; a tool ended by it fails "<tool> used its CPU time bound (N s: its timeout
  of T s × C cores, RLIMIT_CPU) and was stopped". The corpus converts the same with it (below), so
  it stays. `mapped.rs`, the L3 bullet above and `docs/documents.md` now say: the group kill ends
  what stays in the group; a process that calls `setsid()` survives, still under its Landlock
  rules and limits; ending it needs a supervising parent, not built yet. Probe: the review's
  reader run (`setsid bash -c 'sleep 2; truncate -s 0 $RUN/out.wav'`) still empties the run's file
  after the leader exited 0, and that process has `ulimit -t` 300 soft, 301 hard; the test
  `media/ffmpeg/tests_escaped.rs` (a stand-in ffmpeg whose `setsid` child empties the WAV through
  a held descriptor after the run) gets the whole WAV, no signal.
- **S4 (L6), the guard checks a row as it is and as it would be.** A local, aux or audio row is
  refused when the row its `id` picks or the name it gives (`model_id`, every public name that
  name would have) is protected, so the rename and the create under the old name are both
  refused; `lmgw__model_set` and `lmgw__candidate_alias_set` already checked both. A candidate
  alias's candidates count as protected rows. An upstream call is refused when it would answer a
  protected name by its passthrough prefix (a new or changed upstream exposing its catalog under
  the prefix the name starts with, or under none while a protected name is a bare passthrough one
  or answers to nothing). The refusal names the setting and the model ("where the page-reading
  model 'example-vl', chosen on Settings › Documents, sends callers' documents, through its
  fallback 'x'") and says the owner changes it in the dashboard. Probe (dev instance, `/mcp/admin`):
  the rename of local row 1 `example-vl` refused; after the owner renamed it in the dashboard, the
  create of `example-vl` with a cloud fallback refused; the cloud alias's upstream: key rotation
  and `timeout_ms` saved, `base_url` refused, disable saved; a new `expose_all` upstream without a
  prefix refused (the page reader's name then answered to nothing).
- **S5, LibreOffice's install directory through `reach`.** `reach::libreoffice_install` puts it
  through `too_wide_in`; too wide fails LibreOffice's probe by name with the reason, and its run
  (`confine::libreoffice` returns `Err`, the run `tool_not_started`), never widened, never
  unconfined. Probe: an instance whose data dir is `<prefix>/.local/share/lmgw` with a `soffice`
  wrapper first on `PATH` in `<prefix>/.local/bin`: "LibreOffice's soffice at …/.local/bin/soffice
  makes …/.local its install directory, and it holds lmgw's data, settings or cache, …", a `.doc`
  then `tool_missing`. `~/bin` is tested with an injected home.
- **S6, hardening 1 reviewed, not changed.** `GET /api/version` is public by design and names the
  version, so hiding it on `/version` protected nothing; `/version` answers lmgw's version to
  anyone again (docling compatibility), the docs' claim is gone.
- **Nits.** N1: `run::scrub` cuts the run's directory, lmgw's data dirs (as given and resolved),
  the home and the temp directory word for word first (a path with a space, a Windows path), then
  by shape. N2: a drain given up at `STDERR_DRAIN_TIMEOUT` keeps what it read (the message starts
  with those lines). N3: a conversion's transcription counts itself where `proxy::transcribe`
  has opened its route, under the model that answers (`answered_for` the alias asked for), so a
  refusal before the send is no call. N4: zip stems get the prefix before the 200-byte cut, and
  `CONIN$`, `CONOUT$`, `CLOCK$` count as device names. N5: the upstream lock is narrowed (calls
  below). N6: the SF2 and SF3 tests take the home and lmgw's directories injected
  (`reach::Places`), a home with `lib` and `share`, one outside `/home` and none; nothing sets
  `HOME` in the test binary. N7: the job id goes through the outlet (S1).

**Decisions:**
- *One bound for the answer and the row*: `ERROR_MSG_MAX_BYTES` (4 KiB) serves both, so a row
  keeps what the answer said; serde's words outside quotes get `MAX_UNQUOTED_RUN_BYTES` (256) of
  their own.
- *The lock is narrowed for upstreams only*: a protected upstream keeps its key and `timeout_ms`
  free, and may be disabled unless a protected name is one of its passthrough models (another
  upstream with the same prefix could answer it then); enabling it, and every other field, stay
  locked. Local, aux and audio rows, aliases and candidate aliases stay locked whole: a row's
  container image, run arguments and server arguments can each send what it reads elsewhere,
  and disabling a row or alias lets a model of the same spelling answer its name, so narrowing
  them is not small.
- *The copy is an unnamed file in the spool's root*, not a named one in a directory of its own:
  nothing can open it by path.
- *The CPU bound counts all cores the gateway may use for every tool*, tesseract too (its
  `OMP_THREAD_LIMIT` limits OpenMP only); the reader's own parsing runs (`limits_itself`) get no
  CPU bound yet: they are lmgw's code, never `exec` another program, and end with their group.
- *The upstream's prefix check is by prefix alone*, case folded: it refuses a prefix that could
  catch a protected name without working out whether it would win the longest-prefix order.

**Not yet:** a supervising parent (a subreaper reader or a PID namespace) that ends what left the
group; a CPU bound over a process's children together (each process has its own); the profile
template, which a LibreOffice run wrote, is copied into every run (`scratch::copy_tree`) with
no size bound of its own; a transcription that its own deadline
ends names the alias asked for in `stats.models`, not a fallback that had it; the zip stem's
`_<index>` suffix for a taken name can still pass 200 bytes; `run.rs` is 608 lines.

**Unverified:** a kernel below ABI 10 (the new rows are tested by number); the CPU bound ending a
real tool (no corpus file comes near it; the test ends a `setsid` loop at a 1 s bound); a
filesystem without `FICLONE` on the test machine (it uses btrfs; the `copy_file_range` path is tested
by the bounded copy); the WAV path through the route (no speech-to-text model on the dev
instance; the stand-in ffmpeg test runs `media::ffmpeg::sound`); Windows (compile check is the
integration step's).

**Corpus** (debug builds of `972fd193` and of this round, dev instance on `127.0.0.1:8891`,
Landlock ABI 11, LibreOffice 26.8.2.2, poppler 26.08.0): `scripts/convert-same.py` over the 74
files of the round above, **666 of 666 answers identical**; `scripts/convert-measure.py`
`--formats pdf,html,doc,ppt,rtf` on this round: pdf 28/28 J 0.857 line ratio 0.2701 headings
167/159 tables 38/31; html 33/33 0.9422/0.8494; doc 0.9946/0.9906; ppt 0.9121/0.75; rtf
0.9758/0.8889, as recorded above.

#### Final measurement (2026-11-20)

**Built.** Nothing; a measurement of `main` at `a569b483` with no memory limit. Release build
(`cargo build --release` of `examples/headless` and `lmgw-doc-reader` through the build slot),
started by `scripts/convert-perf/gw.sh start` (a scope with no `MemoryMax`) on 127.0.0.1:8894, its
data dir on a real disk; the same test machine (32 threads), Linux 7.3, poppler 26.08.0, tesseract 5.5.4
(`eng`, `deu`; `osd` not reachable: no `TESSDATA_PREFIX` outside `/usr` set), LibreOffice 26.8.2.2,
pandoc 3.7.0.2, GPU idle, no model chosen (no layout model, picture description or page reading).
Default options, markdown output, one file at a time: `python3 -I scripts/convert-measure.py --corpus
"$CORPUS" --lmgw http://127.0.0.1:8894 --per-file --out final-md.json` (270 files of 39 folders, 25.1 s
in all). The wall time of a folder is the sum of its files' client seconds.

| format | files | converted | failed (codes) | s/file med. | s/page | with ref | 3-gram J | line ratio | headings out/ref | tables out/ref | cells out/ref | wall s (folder) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| afp | 1 | 0 | unsupported 1 | - | - | 0 | - | - | - | - | - | 0.14 |
| asciidoc | 6 | 6 | - | 0.013 | 0.111 | 5 | 0.8062 | 0.4544 | 0/17 | 0/10 | 0/110 | 0.17 |
| audio | 16 | 0 | model_not_chosen 13, unsupported 3 | - | - | 0 | - | - | - | - | - | 0.24 |
| boxnote | 1 | 1 | - | 0.012 | - | 1 | 0.2484 | 0.0 | 0/2 | 0/1 | 0/4 | 0.01 |
| calendar | 4 | 4 | - | 0.013 | - | 4 | 0.2731 | 0.0 | 0/24 | 0/0 | 0/0 | 0.05 |
| csv | 9 | 9 | - | 0.013 | - | 9 | 0.489 | 0.2405 | 0/0 | 0/9 | 0/380 | 0.12 |
| doc | 2 | 2 | - | 0.467 | - | 2 | 0.9946 | 0.9906 | 2/7 | 1/1 | 12/12 | 0.93 |
| docx | 38 | 38 | - | 0.016 | - | 38 | 0.9103 | 0.7477 | 75/77 | 26/25 | 311/361 | 1.22 |
| ebcdic | 7 | 4 | unsupported 3 | 0.012 | - | 0 | - | - | - | - | - | 0.08 |
| email | 4 | 2 | no_reader 2 | 0.013 | - | 0 | - | - | - | - | - | 0.05 |
| epub | 1 | 1 | - | 0.018 | - | 1 | 0.8435 | 0.6452 | 19/19 | 0/0 | 0/0 | 0.02 |
| html | 33 | 33 | - | 0.015 | 0.201 | 32 | 0.9422 | 0.8494 | 68/68 | 23/24 | 266/276 | 0.73 |
| html_chandra | 7 | 7 | - | 0.014 | - | 0 | - | - | - | - | - | 0.11 |
| jats | 7 | 7 | - | 0.012 | - | 5 | 0.798 | 0.0796 | 0/95 | 0/9 | 0/896 | 0.10 |
| json_dots | 4 | 4 | - | 0.012 | - | 0 | - | - | - | - | - | 0.05 |
| json_paddleocr_vl | 3 | 3 | - | 0.012 | 0.283 | 0 | - | - | - | - | - | 0.31 |
| keynote | 4 | 0 | unsupported 4 | - | - | 0 | - | - | - | - | - | 0.05 |
| latex | 2 | 2 | - | 0.027 | - | 2 | 0.8413 | 0.8249 | 6/7 | 1/1 | 9/12 | 0.05 |
| md | 11 | 11 | - | 0.012 | - | 11 | 0.9718 | 0.858 | 35/37 | 4/5 | 62/82 | 0.14 |
| md_deepseek | 3 | 3 | - | 0.012 | - | 3 | 0.8382 | 0.5507 | 9/9 | 0/1 | 0/80 | 0.04 |
| mets_gbs | 2 | 0 | unsupported 2 | - | - | 0 | - | - | - | - | - | 0.03 |
| mhtml | 1 | 1 | - | 0.015 | - | 0 | - | - | - | - | - | 0.01 |
| numbers | 7 | 0 | unsupported 7 | - | - | 0 | - | - | - | - | - | 0.09 |
| ocr | 10 | 10 | - | 0.8 | 0.712 | 5 | 0.6728 | 0.4 | 0/0 | 0/0 | 0/0 | 9.25 |
| odf | 12 | 12 | - | 0.037 | 0.262 | 12 | 0.8353 | 0.5059 | 60/48 | 17/30 | 277/408 | 3.78 |
| pages | 4 | 0 | unsupported 4 | - | - | 0 | - | - | - | - | - | 0.05 |
| pdf | 28 | 28 | - | 0.048 | 0.044 | 18 | 0.857 | 0.2701 | 167/159 | 38/31 | 1948/1322 | 5.09 |
| pdf_password | 2 | 1 | unreadable 1 | 0.012 | - | 0 | - | - | - | - | - | 0.04 |
| ppt | 1 | 1 | - | 0.45 | - | 1 | 0.9121 | 0.75 | 3/2 | 1/1 | 63/63 | 0.45 |
| pptx | 8 | 8 | - | 0.013 | - | 8 | 0.7926 | 0.5918 | 18/13 | 1/3 | 63/86 | 0.10 |
| rtf | 1 | 1 | - | 0.021 | - | 1 | 0.9758 | 0.8889 | 0/0 | 1/1 | 4/2 | 0.02 |
| tiff | 1 | 1 | - | 0.842 | 0.421 | 1 | 0.3562 | 0.0597 | 0/8 | 0/0 | 0/0 | 0.84 |
| uspto | 10 | 10 | - | 0.013 | - | 5 | 0.7655 | 0.1228 | 0/69 | 0/2 | 0/148 | 0.15 |
| webp | 1 | 1 | - | 0.302 | 0.302 | 1 | 1.0 | 1.0 | 0/0 | 0/0 | 0/0 | 0.30 |
| webvtt | 4 | 4 | - | 0.013 | - | 4 | 0.6006 | 0.5519 | 0/0 | 0/0 | 0/0 | 0.05 |
| xbrl | 2 | 2 | - | 0.014 | - | 2 | 0.6296 | 0.0184 | 0/2 | 0/25 | 0/1612 | 0.03 |
| xls | 1 | 1 | - | 0.015 | - | 1 | 0.5581 | 0.0 | 1/0 | 1/2 | 27/15 | 0.01 |
| xlsx | 12 | 12 | - | 0.013 | - | 12 | 0.6246 | 0.0 | 24/22 | 23/50 | 1271/787 | 0.16 |

**Differences against the earlier rows.** Folders with earlier rows in this section are equal to
them (doc, ppt, rtf, latex, docx, html, epub, md, csv, pptx, xlsx, xls, webp, tiff after its
heading fix, ocr without `osd` at J 0.6728 / line 0.400) except:
- **pdf, slightly worse than the WP6-era row:** tables 38/31 and cells 1948/1322 (was 39/31 and
  1980/1322), J 0.8570 and line ratio 0.2701 (was 0.8569 / 0.2698). The performance round's block
  already records 38/31 and 0.2701, so the step happened before that round, not in the
  later ones. Headings 167/159 equal.
- **odf, better than the first tier-0 row:** J 0.8353 (0.832), line ratio 0.5059 (0.447), tables
  17/30 (11/30), cells 277/408 (152/408); headings now 60/48 (were 22/48: over-counted by 12).
- Time: pdf median 0.048 s/file (0.044 s/page), against 0.038 s in the item 1–5 block (a release
  build, run alone, with the corpus' 28 files one after the other); ocr 0.800 s/file (0.712 s/page)
  without `osd`, against 0.949 s in the earlier no-`osd` row (faster); doc 0.467 s and ppt 0.45 s
  (0.63/0.57 s in the review round, 0.86/1.08 s at WP7).
- Folders with no earlier row, for the record: jats J 0.798 (headings 0/95, tables 0/9), uspto 0.7655
  (0/69), xbrl 0.6296 (tables 0/25), calendar 0.2731 (0/24 headings), webvtt 0.6006, boxnote 0.2484.
  Their output has no headings or tables where docling's reference has them.

**Unverified.** The `json` output: `convert-measure.py` sends `output=markdown` itself and has no
option for another; `--lmgw-field output=json` adds a second `output` field and every request
answered `invalid_option`, so no json pass was run (not yet; the script scores markdown against
docling's markdown only). The `osd` row of ocr (J 0.7856) was not re-run. Model-backed options
(layout model, picture description, page reading) were not measured, the GPU was idle.
