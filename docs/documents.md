# Documents: what lmgw turns into text

lmgw reads the files you attach in the Chat and the files you add to a
knowledge base, and turns them into text a model can read. Some file types it
reads by itself; others need an optional program installed on the computer.
**Settings → Documents** shows which is which on your computer.

## The Documents page

- **What you can convert** lists each kind of file and what you get from it
  here. A green dot means lmgw reads it. An empty dot means it doesn't, and the
  line says why: a program to install, or a reader lmgw doesn't have yet.
- **Found on this computer** lists the optional programs lmgw found, with
  their versions and what lmgw uses each one for. A program lmgw found but
  doesn't use yet says so. **lmgw's document reader** is in this list too: it
  is a small program of lmgw's own, installed with lmgw (see
  [lmgw's document reader](#lmgws-document-reader)). If it is ever missing,
  it shows under **Not installed** and the file types it reads say so.
- **Not installed** lists the optional programs that are missing, what
  installing each would let lmgw read, and the command that installs it (with
  a **Copy** button).
- **Check again** looks for the programs again. You rarely need it: lmgw
  checks at start, and every time the page is opened it notices a program
  that was installed, removed or updated since, without a restart. That
  includes a language pack added to tesseract and a part (Writer, Calc,
  Impress) added to or removed from LibreOffice.
- **Expert view** adds the details to the same rows: which reader handles each
  file type, where each program is, its licence, tesseract's language packs,
  ffmpeg's audio decoders and LibreOffice's installed parts. When a program
  is installed but doesn't answer lmgw's check, the expert view shows its
  error output; the plain view only says it did not answer. A check that
  takes longer than 10 seconds is stopped, with everything it started (on
  Linux and macOS; stopping a Windows program's helper processes is not built
  yet), and the message names the limit (`TOOL_PROBE_TIMEOUT`).

- **Parallel tool runs** (expert view) is how many conversion steps run at
  once across the whole gateway: each PDF that poppler reads, each page of
  text recognition and each file lmgw's own reader opens takes one. lmgw
  sizes it from the computer: as many steps as the processor cores it may
  use, while the memory they may use together fits the computer's. Each step
  counts the most its programs can use, their memory limits plus the output
  lmgw holds for them: a PDF's text 2.5 GiB (poppler's 512 MiB limit plus
  the two layout outputs, up to 1 GiB each), a page's pictures cut out 6 GiB
  (lmgw's reader cutting the page's render), a page of text recognition
  about 2.2 GiB (its 2 GiB programs, the page and its text), a file read by
  lmgw's reader 5 GiB. So a 16 GiB server reads six PDFs, seven scanned
  pages or three web pages at once. Once poppler has finished with a PDF, its step
  counts only the layout outputs it really holds and twice their size for
  reading them, so the next PDF starts while the layout is read (on an 8 GiB
  computer poppler reads three PDFs at a time, beside the layout of the ones it
  finished). Layout outputs that need more than the step counted (two of
  about 1 GiB need up to 6 GiB) are counted in full: at once where the
  memory is free, otherwise the PDF gives its step back and reads its pages
  again in a step that counts that much. A program run again under
  poppler's wide limit (below) counts that limit in a step of its own. No
  step ever waits for more memory while it holds some. The programs it
  starts never need more memory
  together than it has, whatever the files are (lmgw's own readers inside
  the gateway, spreadsheets and the office zip readers, count the same, but
  have no memory limit of their own yet). Steps wait their turn in order:
  a heavy step at the front of the queue (a PDF) keeps its turn, and a
  lighter step behind it (another caller's scanned page) waits for it even
  when it would fit beside the steps running, so no PDF waits for ever
  behind a stream of small steps. Where a second caller can exist (**Require
  API key** on and more than one key may call the conversions; all owner keys
  of one account count as one), one caller (an API key, all requests without
  a key together, or lmgw's own Chat and knowledge-base reads) never takes the
  last step's worth: it holds at most one slot less than the size, and its
  steps together leave memory free for one more step like its own, so another
  caller's next step can always start beside a long batch. A caller that holds
  nothing may always start one step, and goes first when a slot comes free
  (the one served longest ago first), so it waits for at most one step to end.
  On a 16 GiB server one caller then reads five PDFs at a time (six fit),
  on a 126 GiB one thirty-one of thirty-two. With **Require API key** off, or one such
  key, one caller uses the whole pool, and lmgw's own reads wait behind its
  batch. A pool of one slot keeps no room for anyone. The row says which case
  is in force, why, and how many one caller runs of each kind. The row shows the size, the memory considered, what each
  kind of step counts and how many of each fit, and what runs and waits
  right now. Type a number and **Set** to choose it yourself: then that many
  steps run at once and their memory is not counted (your number replaces
  the computer's); **Use this computer's size** goes back. In the server
  image the memory is the container's limit when it has one. On Windows and
  macOS lmgw does not read the memory yet, so the size follows the cores.

- **Programs run confined** (above the found programs) says whether lmgw's
  document reader can confine the programs it starts here (Landlock), and
  names each found program that runs without it, with the reason: poppler or
  tesseract installed where its confinement would have to reach a home
  folder or lmgw's own folders (for example right in your home folder),
  tesseract's language data outside `/usr` in a folder without
  `*.traineddata` files, or a program that failed its check run under its
  rules. Such a program still runs, with its memory limit and closed
  descriptors, as everywhere without Landlock; LibreOffice never runs
  without the confinement. A check that could not finish (a slow start) is
  tried again at the next check (**Check again**, or the next start). On a
  kernel whose Landlock is older than ABI 10 the row also names, for every
  program, what that kernel cannot block, each with the ABI found: emptying
  files by truncating them (ABI 3; below it a program running hostile code
  could empty any file your user may write, lmgw's database included), TCP
  connections (ABI 4), `ioctl` calls on devices (ABI 5), abstract Unix
  sockets and signals to other processes (ABI 6), Unix sockets by path
  (ABI 9), UDP (ABI 10). Such programs still run, with these gaps named. The
  same is said once in the log. LibreOffice needs ABI 4 at least (its
  network denied); below that it is not used, as without Landlock. A
  LibreOffice whose install folder would reach a home folder or lmgw's own
  folders (a `soffice` script in `~/.local/bin` or `~/bin` that comes first
  on `PATH`) is not used either, and the row says so.

- **lmgw's own memory.** At start lmgw fixes two thresholds of the system
  allocator (glibc on Linux) at 1 MiB for the whole gateway, not only for
  conversions: a block of 1 MiB or more gets memory of its own that goes
  back to the system when it is freed, and free memory at the end of the
  allocator's areas goes back past 1 MiB. Without it, after a few large
  conversions the gateway kept 0.5 to 0.6 GiB it no longer used (measured
  2026-11-19). Setting `MALLOC_MMAP_THRESHOLD_` or `MALLOC_TRIM_THRESHOLD_`
  in lmgw's environment keeps your value instead.

- **Layout model** is a model that looks at each PDF page as a picture and
  finds what is on it: paragraphs, headings, list items, tables, pictures and
  drawn charts, captions, footnotes, formulas, code, and the page's header
  and footer. With it, a PDF's text is cut into paragraphs and put in order
  the way a reader sees the page ("With the layout model" below). It runs on
  the processor, so it needs no graphics card and works while the GPU hold
  is on. It runs in ONNX Runtime, which lmgw links: the library comes from
  the `ort` crate's download when lmgw is built, pinned by that crate's
  version (`=2.0.0-rc.13`). The row says whether its file is on this computer. lmgw does not
  ship the file and never fetches it by itself: **Download** queues it (163
  MiB, from Hugging Face, Apache-2.0; the Downloads page shows its progress),
  and **Use it** switches it on once the file is here. It stays off until you
  switch it on. The file goes into the folder for embedding and rerank
  models (Settings → Runtimes); if that folder is not set, the row says so.
  You can also put the file there by hand: the expert view shows the exact
  place, where the file comes from (the repository, the commit and the
  file's SHA-256, which lmgw checks before it first loads the file) and what
  the loaded model takes: about 485 MiB of memory, up to 4 processor threads
  while it reads a page, and it is dropped again 5 minutes after its last
  page (about a third of that memory goes back to the system at once; the
  rest stays reserved by lmgw until it is used again or lmgw is restarted).
  Each of those threads is one of the **Parallel tool runs** while a page is
  read: where that pool is 4 or smaller (a computer with 16 GiB of memory
  gets 2), reading one page takes the whole pool for about half a second,
  and other conversions wait that long.
- **Read pages with a model** (under **Model help**) chooses the model that
  reads the pages lmgw's tools can't read well: pages without text (scans),
  and pages with tables. It is **off until you choose one**, and while it is
  off no conversion ever asks a model. The list offers your chat models that
  take pictures; lmgw suggests none. A request can turn it off, keep it to
  pages nothing else read, or ask for every page (`model_help`, below).
  **Once a model is chosen, a request that says nothing about `model_help`
  uses it** for pages without text and pages with tables, on the docling
  route too: only **Off** here, or `model_help: off` in the request,
  guarantees that no page is sent to a model.
  - The row says where pages go, from what lmgw knows about the model you
    chose: that it **runs on this computer** (one of your local models), or
    that it does not and **these pages are sent to** a service, named with
    its host.
  - It says the same about the model's **fallback**, the model that answers
    whenever the chosen one can't run. For a local model that is while the
    GPU hold is on, while the graphics card is full or in use by a
    benchmark; for a model at a service, while that service is out of quota
    or can't be reached. The row names the fallback and says whether it runs
    on this computer or which service the pages are then sent to. Without a
    fallback no model reads pages then, and the answer names the pages that
    kept the tools' text.
  - A fallback that takes no pictures can't read a page. lmgw sends it
    nothing: the pages keep the tools' text, with a `model_cannot_see`
    warning that names the fallback, and the row says so beforehand.
  - **Expert view** adds **Answers in DocTags**, for a model trained to
    answer in docling's page format (the granite-docling models are): lmgw
    then asks it with docling's own prompt and reads its answer with
    positions and tables. Such a model's llama-server must be started with
    `--special` (add it to **Extra llama-server flags** on the model's page
    under Models), or the tags are left out of its answers; the row says so
    when the model's row lacks it, and a conversion's warning says so when an
    answer came back without tags.
- **Describe pictures with** (under **Model help**, below the page-reading
  model) chooses the model that describes the pictures of a converted file
  when a request asks for it (`describe_pictures`, docling's
  `do_picture_description`): one short description per picture. It is
  **off until you choose one**; while it is off a request that asks for
  descriptions gets its pictures without one and a `model_not_chosen`
  warning, and no picture is sent anywhere. The list offers your chat models
  that take pictures; it may be the same model as the page-reading one
  (then a request shares that model's calls in flight between pages and
  pictures). The row says, like the page-reading row, whether the model
  **runs on this computer** or **the pictures it describes are sent to** a
  named service, and what its fallback does whenever it can't run: which
  model answers and where it is, that a fallback that takes no pictures is
  sent nothing, or that without a fallback pictures are kept without a
  description. Only requests that ask for descriptions send pictures to it.
- **Transcribe audio with** (under **Model help**, below the
  picture-description model) chooses the speech-to-text model audio files
  and the sound of videos are transcribed with ([Audio and
  video](#audio-and-video)). It is **Off** until you choose one; while it is,
  the Audio and Videos rows say so. Once one is chosen, the row says in plain
  words whether it runs on this computer (the audio stays here) or the audio
  is sent to an outside service, which it names, and which model transcribes
  whenever the chosen one cannot run (the GPU hold, for a local one) and
  where that one runs.
- **Who calls the docling route** lists each key that called lmgw's
  docling-serve routes (below): how many requests, when it last called, and in
  plain words what it asked for that lmgw handles differently ("picture
  descriptions (lmgw's own)", "the OCR engine "easyocr" (lmgw's own)", web
  addresses it was refused). The expert view adds the routes and every option
  with its value, outcome and count. It is summed from the usage log, so it
  follows the log's retention.

Through the API the page's answer is the `doc_tools` op (and the
`lmgw__doc_tools` tool on the self-admin MCP plane); **Check again** is
`doc_tools_check`. The pool's size is `pool` in that answer, and
`documents_tool_slots` of the `settings_set` op sets it (`0` = this
computer's size). "Who calls the docling route" is the `doc_adapter_usage` op
(`lmgw__doc_adapter_usage`). The layout model's row is `models` in the
`doc_tools` answer; `doc_model_add` with `id: "layout"` (`lmgw__doc_model_add`)
queues its download, and `documents_onnx_layout` of `settings_set` switches
it on or off (switching it on is refused while the file is not there). The
page-reading model is `page_reader` in that answer (`local`, `outside` with
the service's name and host, and `fallback` with the same for the model that
answers in its place); `documents_page_reader` (a model's name, `""` = off)
and `documents_page_reader_doctags` of the dashboard's `settings_set` op set
it. The admin tools (`lmgw__settings_set`) can't: choosing where document
pages are sent is yours alone, and a call that names either setting is
refused. The speech-to-text model is `speech` in that answer (the same
fields), and `documents_asr_alias` of the dashboard's `settings_set` op sets
it; the admin tools can't, for the same reason: it decides where your
callers' audio is sent.

## Converting files through the API

`POST /v1/documents/convert` turns one file or several into text. It is an
inference route: it takes the same API keys as `/v1/chat/completions`, and
each request writes one row on the Usage page (labelled `documents`, alias
`documents/convert`, with its status and time). The models a conversion
calls write rows of their own beside it, under the same key, one per call:
`documents-page` (the page-reading model), `documents-picture` (the
picture-description model) and `documents-asr` (the speech-to-text model),
each with token counts or seconds and never the file's content; a call cut
off before its answer is a row with status 499. The file's type is read from
its bytes, never from its name, and it is read by whatever Settings →
Documents says reads it here: the page and the route use the same table.

Send the files as `multipart/form-data`, one part named `file` per file:

```sh
curl -s http://127.0.0.1:8787/v1/documents/convert \
  -H "Authorization: Bearer $LMGW_KEY" \
  -F file=@example-report.pdf -F file=@example-notes.docx -F output=markdown
```

or as JSON, each file base64-encoded:

```json
{ "files": [ { "name": "example-notes.txt", "data_base64": "SGVsbG8=" } ],
  "output": "text" }
```

The request is bounded by **Max request body** (`max_body_mb`, Settings →
Network & access), and that is the only bound on how many files one request
carries. base64 makes a file about a third larger, so the same files fit
better as multipart; the 413 says so.

The files' converted output is bounded too: all of one request's files
together may produce at most `MAX_UNCOMPRESSED_BYTES` (1 GiB, the bound the
readers hold their own output to). The file whose output crosses it fails
with `output_too_large` (HTTP 413 when it is the only file), naming the
constant and its value; files already converted stay converted, and files
that finish after it fail the same way. Files convert side by side, so which
file crosses it can vary from one run to the next; send the files in separate
requests to keep them apart.

### Options

| Field | Default | What it does |
| --- | --- | --- |
| `output` | `markdown` | `markdown` keeps headings, lists and tables; `text` leaves the markup out (headings without `#`, table cells separated by tabs); `json` gives the structure (below). |
| `pages` | every page | `"3-7,10"`, 1-based, for each PDF. A file without pages is read whole, with a `pages_not_applicable` warning; pages a PDF doesn't have are named in a `pages_out_of_range` warning (a spec that repeats or overlaps itself is merged first) (a PDF with none of them fails with that code). |
| `tables` | `true` | `false` writes each table row as a line of cells separated by tabs. |
| `compact_tables` | `false` | `true` writes markdown tables as `\|a\|b\|`, without the padding. With `output: text`, `output: json` or `tables: false` no markdown table is written, so a request-wide `option_not_applicable` warning says it changed nothing. |
| `page_break` | none | a line written between two PDF pages. A file without pages gets an `option_not_applicable` warning (nothing was written), and so does `output: json`, where each page is a part of its own. |
| `pictures` | `placeholder` | what each picture becomes: `placeholder` writes docling's `<!-- image -->` line where the picture was, `embedded` the picture itself as `![Image](data:image/png;base64,…)`, `referenced` a link `![Image](artifacts/image_000000_<sha256>.png)` to its entry in the file's `pictures` list, which carries the bytes. See [Pictures](#pictures). `output: text` writes no pictures (an `option_not_applicable` warning says so). |
| `picture_min_area` | `0.05` | a picture smaller than this share of its page is left out (docling's `picture_area_threshold` default); `0` keeps every one. |
| `images_scale` | `2.0` | PDF pictures are cut out of the page rendered at `72 × images_scale` dpi (2.0: 144 dpi), as docling's `images_scale`. Above 0. |
| `describe_pictures` | `false` | `true` sends each exported picture to the picture-description model chosen on the Documents page, one call a picture, and writes its description beside it. With no model chosen, a request-wide `model_not_chosen` warning says the pictures were kept without one. See [Pictures described by a model](#pictures-described-by-a-model). |
| `picture_prompt` | docling's "Describe this image in a few sentences." | the prompt each picture is described with, followed by the picture's caption and the paragraph before it. A prompt that by itself fills the model's context sends no picture (one `part_failed` warning). Without `describe_pictures` it changes nothing, and an `option_not_applicable` warning says so. |
| `describe_min_area` | `0.05` | a picture smaller than this share of its page is exported but not described (docling's `picture_description_area_threshold` default), counted in `stats.pictures_below_describe_min_area`; `0` describes every one. Without `describe_pictures` another value changes nothing (`option_not_applicable`). |
| `ocr` | `auto` | Text recognition (tesseract): `auto` reads PDF pages that have no text layer and pictures; `force` reads every PDF page from its picture, replacing the text layer (a page whose recognition fails keeps it); `off` reads none (a scanned page is named in a `no_reader` warning, a picture fails with `no_reader`). See [Scans and pictures](#scans-and-pictures). |
| `ocr_languages` | the installed packs of the desktop's language, and English | tesseract language codes (`eng`, `deu`, `chi_sim`, `script/Latin`): at most `MAX_OCR_LANGUAGES` (32), each letters, digits and `_` up to 32 characters, optionally after `script/`; a repeat counts once. A list that breaks a rule is a 400 that names the rule and how many codes broke it. Codes that are not installed are named in one `ocr_language_missing` warning with the packages that add them, and the others are read; when none of them is installed, nothing is recognised, by name. |
| `document_timeout` | none | The most seconds one file may take (a number above 0), counted from its first step in **Parallel tool runs**, so waiting for a slot does not count. A file past it fails with `deadline_exceeded` (504 for a one-file request), its programs are stopped, and the other files go on; a file the page-reading model was reading is finished from what was read instead (`partial`). See [A file's time](#a-files-time). Without it there is no deadline per file: each program run still has its own named timeout. |
| `async` | `false` | `true` answers `202 {"job": "cj_…"}` at once (and a `Location` header) instead of waiting; see [Background conversions](#background-conversions). |
| `model_help` | the Documents page's choice | which PDF pages the page-reading model reads: `missing_or_tables` (the default once a model is chosen: pages nothing read, pages text recognition read poorly, and pages with a table), `missing` (only pages nothing read: no text layer, and text recognition read nothing), `every_page`, or `off`. With no model chosen, a value other than `off` adds a request-wide `model_not_chosen` warning and no model is called. With one chosen, only `off` guarantees that none is. See [Pages read by a model](#pages-read-by-a-model). |

A field the route doesn't know is a 400 that names it. In a multipart form
the switches are `true` or `false`, `document_timeout` is a number, and
`ocr_languages` takes codes separated by commas, `+` or spaces. A refusal
that repeats what you sent repeats at most `MAX_QUOTED_BYTES` (64 bytes) of
it, and says `(cut at 64 bytes, MAX_QUOTED_BYTES)` when it cut (a run of
a library's message outside quotes at most `MAX_UNQUOTED_RUN_BYTES`, 256
bytes). Every refusal of this route, its jobs' routes and the
docling-compatible ones is at most `ERROR_MSG_MAX_BYTES` (4 KiB) in all,
whatever it repeats, saying `(cut at 4096 bytes, ERROR_MSG_MAX_BYTES)` when
it cut; a usage row and a job's stored error keep at most the same of any
error message, every route's, saying so the same way.

### How PDFs are read

A PDF's text layer is read with its geometry (poppler's `pdftotext
-bbox-layout`, and `pdftohtml -xml` for font sizes and bold). poppler reads
the span from the first to the last page `pages` asks for (`pages: "1,800"`
reads 800 pages) and the answer keeps the pages asked for:

- **Headings**: lines clearly larger than the body text (1.15 times the most
  common size), or a short bold line opening a paragraph. The largest size is
  `#`, the next `##`, and so on (a size only one heading has, such as a
  label in a figure, takes the level above it); bold headings at body size
  come after. Levels are ranked over the pages read.
- **Columns**: a page whose text sits in columns side by side is read column
  by column. A title or abstract across the columns, and a centred line with
  nothing beside it (an author line, an "Abstract" heading, a caption under a
  figure across the page), is read where it is, between the columns above it
  and those below.
- **Side by side but no columns**: labels beside their values (a CV, an
  invoice) or other short blocks side by side are read row by row, a line
  per row.
- **Repeated headers and footers**: a line in the top or bottom 13 % of the
  page, set apart from the text by space, that repeats on at least a third
  of the pages read is left out of markdown and text. It counts as repeated
  when its text is the same, or differs only in a page number (a number that
  counts up with the page, such as "Page 3 of 9"); a line that changes in any
  other way (an amount, a slide title) stays. Each one is named in a
  `header_removed` warning with the number of pages asked for that it left,
  and `json` keeps it as a `page_header` or `page_footer` block. A table's
  lines are never taken for headers, so a long table's header row repeated
  on each page stays.
- **Tables**: three or more rows of four or more cells that line up become a
  markdown table, and so do three or more rows of two or three short cells on
  one baseline (a small table without borders, a label/value list; cells of
  five words or more on half the rows are sentences, not a table). In those,
  a value that wraps onto the next line goes on in its cell; in tables of
  four or more columns such a line becomes a row of its own, not handled yet.
  A table beside a column of text takes that column in as an extra column,
  also not handled yet. A "table" whose cells fill less than a quarter of
  its rows times its columns is none.
- A word broken by a hyphen at the end of a line is joined again; a line
  starting with a bullet is a list item.
- Text that markdown would read as markup at the start of a line (`#`, a
  code fence, `<`, `>`, a list marker, `1.`) is escaped with a backslash.
- A PDF's layout output is about 17 times its text. One that passes 1 GiB
  (MAX_UNCOMPRESSED_BYTES) fails with `output_too_large`; ask for fewer
  pages at a time.

Without `pdftohtml` (it is part of poppler-utils) headings are told by size
alone, with a warning; so are pages its output did not cover, or did not
cover readably (a `part_failed` warning names them). Chat attachments and knowledge bases still read PDFs
as plain layout text; they move to this reading later.

Each poppler program runs with a memory limit of its own kind, set from the
most each needed on real and made-up files (measured with poppler 26.08):

| limit | value | for |
|---|---|---|
| `POPPLER_TEXT_MEMORY_LIMIT_BYTES` | 512 MiB | `pdftotext`, `pdftohtml` for the layout's fonts, `pdfinfo` and `pdfimages -list`, which need no image's pixels (ordinary files under 100 MiB; a page drawing one path of a million lines 400 MiB) |
| `POPPLER_IMAGE_MEMORY_LIMIT_BYTES` | 2 GiB | `pdftoppm` and `pdftohtml` for pictures, which decode images (an A3 page scanned at 600 dpi as JPEG 2000 about 0.9 GiB; a page's render takes 4 bytes a pixel) |

A file sent to poppler through a pipe (Chat and knowledge-base reads) is read
whole into memory first, so the limit grows by three times its size
(`POPPLER_PIPE_READ_PER_BYTE`).

A program that needs more than its limit is run once more under
`POPPLER_WIDE_MEMORY_LIMIT_BYTES` (4 GiB, the one limit every poppler
program had before), so nothing that read then is refused now: a page
drawing one path of more than about 1.2 million pieces, a render of more
than about 500 million pixels (about 15 m² at the 150 dpi Chat renders at),
a JPEG 2000 image of more than about 170 million pixels. Each such run is a
`memory_limit_widened` warning (tool `poppler`) naming the program, both
limits and the pages (a page, or "the run over pages 1–64" for a run that
renders several); Chat says it beside the page it sends and in a PDF
attachment's notes, knowledge bases in their notes. The run again gets the
program's own timeout (`PDF_TIMEOUT`; the layout model's squares what is
left of theirs) within what is left of the file's `document_timeout`; with
none left it is not run again, and the error or warning says so. A program
that needs more than the wide limit too is stopped, and the error names both
limits: a PDF whose layout cannot be read this way fails with `memory_limit`
(422), a page or picture that cannot be rendered is a warning that names
them.

A JPEG 2000 image whose decoding does not fit the limit does not stop
poppler: it draws the image blank and finishes (checked with poppler 26.08
and openjpeg 2.5.4, renders and picture runs alike). lmgw reads poppler's
messages as they come, all of them, for the programs that draw pages and
pictures and for text recognition's page render. Where openjpeg says it had
no memory for the image (or a tiled image's decode fails without an
openjpeg error saying why, which is how openjpeg says it; its warnings say
nothing of why), the program is run again under the wide
limit. An image poppler cannot decode there either (more than about 340
million pixels) stays blank, and an `image_not_decoded` warning names the
pages and the limit. A damaged image, or one in a form openjpeg does not
read, is blank under every limit: it is an `image_not_decoded` warning at
once, and nothing is run again. Where the run again ends without an answer
(out of time, or out of the wide limit), the first run's page or pictures
are kept, the image in them blank, and the warning says so. `pdftotext` and
the layout's runs decode such an image too, but never use its pixels: their
text is the same either way, and they are not run again for it (checked
2026-11-20 on A4 and A3 pages scanned at 600 dpi and a 10 000 × 14 000
image: the same output under 512 MiB as without a limit).

A Chat page with such a warning is not kept in the attachment's page cache
yet: each send renders it again and says it again.

### With the layout model

When the layout model is switched on (Settings → Documents), each page of a
PDF that has text is also rendered as a small picture (640 × 640 pixels, all
the model takes) and the model marks the page's regions. The words still come
from the PDF's text layer; the regions decide what they are and how they are
cut:

- **Paragraphs** are the model's text regions: the lines in one region are
  one paragraph. A paragraph that goes on in the next column or on the next
  page is one paragraph (it ended in a lower-case letter, a comma or a
  hyphen and goes on in lower case); in `json` it is one block with the box
  of its first part.
- **Headings** are the regions the model marks as title or section header.
  Their levels still come from the font sizes, ranked across the pages read.
- **List items** are the model's; a bullet is taken off, and an item that
  starts with a number instead (`1.`, `2)`) is written as markdown's own
  numbered item (`1. First`). An item with a bullet whose text starts with a
  number ("• 24. December") stays a bullet item, as without the model.
- **Tables** are where the model sees one. Their rows and columns are read
  from where the lines sit: each visual line of the table is a row, so a
  cell that wraps onto a second line becomes a second row (a model that
  reads a table's structure is not built yet). A table without any text (a
  picture of a table) is a figure; one whose lines fill less than a quarter
  of the grid they would make (`TABLE_MIN_FILL`, as without the model) is
  paragraphs.
- **Pictures**, drawn figures and charts included, are `figure` blocks with
  the region's box, written as the request's `pictures` asks (see
  [Pictures](#pictures): a placeholder, or the region cut out of the
  rendered page). A region over a raster image is one figure with the
  image's box. Text inside one, such as a
  chart's labels, is left out of markdown and text and kept in the figure
  block's `text` in `json`; a `picture_text` warning says how many lines. A
  line that lies as much in a text region reaching out of the picture (a
  legend half over a chart) stays text.
- **Captions**, **footnotes**, **formulas** and **code** are blocks of those
  kinds (a footnote is a paragraph). A formula keeps the text the PDF has for
  it; code keeps its lines and their indentation.
- **Page headers and footers** the model marks are left out of markdown and
  text like the repeated ones, on a single page too, and named in a
  `header_removed` warning.
- **Reading order** is by columns, as without the model, over the regions.
- Lines in no region stay in the text as paragraphs of their own.

A page falls back to the reading above, with a `part_failed` warning naming
it and why, when its picture could not be rendered or read, when the model
finds nothing on it, when two thirds of its lines lie in no region
(`ORPHAN_MAX_SHARE`), or when more than half of its lines lie inside what the
model takes for pictures and those pictures cover more than half of the page
(`PICTURE_TEXT_MAX_SHARE` and `PICTURE_PAGE_MIN_AREA`: a slide read as one
picture would otherwise lose its text; a paper's text-heavy figure on a
quarter of the page keeps its place, its text left out of the markdown with
the `picture_text` warning and kept in the figure block of `json`). Pages turned by the PDF (`/Rotate`)
are read as they are displayed. If the model is switched on and its file is
gone, the file converts as without it and a `tool_missing` warning says so.
Pages without text are not sent to the model.

The pictures are rendered up to 16 pages at a time (18.75 MiB,
`RENDER_BATCH_BYTES`) into a private folder in the system's temporary folder
and deleted as each page is read. The renders of one file get
`PDF_TIMEOUT` (300 s) together, as one poppler run over the whole file does;
the render that reaches it ends the model's work on that file. With
docling's `document_timeout`, the model also stops once the file has used
half of it (`MODEL_DEADLINE_SHARE`), so the rest of the reading has the other
half. Either way the pages it did not read are read as without the model and
a `part_failed` warning names them.

Each page costs about 0.4 s on four threads (measured on a 32-core desktop;
1.3 s with one thread). The model reads one page at a time for the whole
gateway. The threads it uses are **Parallel tool runs**: a page waits for its
turn and then takes them all at once, up to 4 or the whole pool when it is
smaller, and while it waits for them, runs that end leave their slots free
for it rather than to the conversions queued behind it. On a computer whose
pool is 2 (16 GiB of memory), one page's reading has the whole pool for its
half second. Waiting for the model, like waiting for a slot, does not count
against a file's `document_timeout`. A request can't switch the model on or
off for itself yet.

### Pictures

Each file's entry has a `pictures` list, one entry per picture in reading
order: `id` (`p1`, `p2`, …, what the markdown's line and the `figure` block
name), `part` (the page or slide), `bbox` (PDFs), `caption`, `mime`,
`width_px`/`height_px`, `bytes`, and `file` with `data_base64` when the entry
carries the bytes: `referenced` always, `embedded` only with `output: json`
(in markdown the picture is in the text). `stats.pictures` counts them.

- **PDF:** every raster image the page draws, found with poppler
  (`pdftohtml`), cut out of the page as poppler renders it (`pdftoppm`, one
  run per page with pictures) and written as PNG by
  [lmgw's document reader](#lmgws-document-reader), on rotated pages and for
  mirrored images too. Finding them costs time on pages with large images
  (poppler encodes each one), so `pdfimages -list` looks first and only
  pages with an image that may be a picture are searched: a scanned PDF,
  whose pages are one page-sized image each, has no picture search at all
  (its pages' text is recognised, [Scans and pictures](#scans-and-pictures)). The pictures are searched beside the text, in parallel over the
  pages; when that fails for some pages, those pages lose their pictures
  (a `pictures_partial` warning names them), never their text.
  `output: text` does no picture work. The picture's place in the text
  follows the page's reading order: before the block below it in its
  column. A line right below (or above) it that starts like a caption
  (`Figure 1:`, `Fig. 2 Example`, `Abbildung 3 –`, `Chart:`) is its
  `caption`, and a `caption` block in `json`; a sentence of the text such
  as "Figure 3 shows …" is not. When poppler gives a caption and the
  paragraph under it as one block, the paragraph stays a paragraph of its
  own.
- **PDF with the layout model** (Settings → Documents → Layout model): each
  region the model marks as a picture is one figure, drawings and charts
  made of lines and shapes included. A region over raster images takes
  them: one figure, with poppler's box for the crop and the region's place
  in the reading order (`source` stays `onnx`). A region with no raster
  image in it is cut out of the rendered page by its own box. A raster
  image the model did not mark keeps its figure as without the model. A
  caption the model marked right below or above a figure is that figure's
  `caption` before any paragraph is (unless it names a table); the words
  rule above applies to the rest. Text inside a picture region stays out of
  markdown and text in every `pictures` mode and on the docling routes, and
  is the figure block's `text` in `json`.
- **Word, PowerPoint, OpenDocument text and presentations:** the pictures
  where the document places them, as stored in the file (PNG, JPEG, GIF,
  …; not converted). A picture's size against the page or slide size the
  document declares decides `picture_min_area`.
- **Left out**, counted in a `pictures_left_out` warning: pictures smaller
  than `picture_min_area`, the same picture in the same place on at least a
  third of the pages (a logo in the running header, a watermark, a
  background), and an image that covers its whole page (its background, or
  a scanned page: the scan is the page, whose text recognition reads, not a
  picture on it). A smaller picture on a scanned page (a photo pasted on
  it) is exported like any other, placed among the recognised text by where
  it sits on the page; the words tesseract finds inside it stay in the text.
- **Placeholders without their picture**, named in a `pictures_partial`
  warning: drawings and charts made of lines and shapes on pages the layout
  model did not read (poppler lists raster images only; a page with a
  `Figure N` caption and no picture beside it is named), a page whose pictures would render to more than 1 GiB
  (`PAGE_RASTER_MAX_BYTES`; lower `images_scale`), a page with more than 256
  pictures (`PICTURES_PER_PAGE_MAX`; the rest go to the page's end), pictures
  past the request's 1 GiB output (`MAX_UNCOMPRESSED_BYTES`; the file stays
  converted), an office picture linked from outside the file, in a format
  browsers don't show (EMF, WMF), or in SVG (it can carry scripts and links
  to outside the file; exporting it as a picture isn't built yet).
- Pictures in spreadsheets, EPUBs, web pages and e-mail bodies aren't
  exported yet: each is a placeholder (a `figure` block without `picture` in
  json), the `pictures` list stays empty, and a `not_yet` warning says so
  (pictures on other servers are never fetched, pictures inside the page as
  `data:` addresses are placeholders too).
- **An image file** is read by text recognition, and asked for `embedded` or
  `referenced` pictures it is also the one picture it is, as stored, before
  its text. With `placeholder` (or `output: text`) it is a picture of the
  answer only when it was described (see below), never a placeholder alone.
- **E-mail attachments:** an attachment's pictures (a PDF's, a Word file's)
  are exported with it as the request asks and numbered on through the
  message (`p1`, `p2`, … in the attachments' order); each counts against the
  request's output as its attachment is read.

A picture's bytes count against the request's 1 GiB output like its text,
once, at the size the answer writes them in (base64); a picture that does
not fit any more stays a placeholder, and the file stays converted. Bytes
made only to be described (`placeholder`, `output: text`) count at their
own size while the calls run and are given back after them.
A background conversion keeps them as private files beside its result and
deletes them with it. Picture bytes never go into a log or a usage row.

### Pictures described by a model

With `describe_pictures: true` and a picture-description model chosen (the
Documents page, **Describe pictures with**), every picture the export above
yields is sent to that model, one call a picture, after lmgw's tools have
read the file: a PDF's pictures and the layout model's figures, an office
file's pictures (an old `.doc` or `.ppt` too, through the Word or
PowerPoint file LibreOffice makes of it), an e-mail attachment's, and an
image file as the one picture it is. A picture smaller than `describe_min_area` of its page
(default 0.05, docling's `picture_description_area_threshold`) stays in
the answer without a description, without a warning, and is counted in
`stats.pictures_below_describe_min_area`. Pictures in EPUBs, web pages,
spreadsheets and e-mail bodies are not exported yet, so they are not
described either; in a file read through pandoc (RTF, LaTeX and the other
markups) they aren't either, and a `not_yet` warning says so. An audio
file has no pictures (an `option_not_applicable` warning says so), and a
video's frames are not described yet (`not_yet`).

- **What is sent:** the picture, as stored in the file or as cut out of the
  PDF page, and one message: `picture_prompt` (docling's default, "Describe
  this image in a few sentences.", when it is not set), then the picture's
  caption and the paragraph before it, marked as the document's own text.
  Each of the two is cut at `PICTURE_CONTEXT_MAX_BYTES` (2 KiB: the
  caption's start and the paragraph's end are kept), and a
  `picture_context_cut` warning names the pictures. Nothing about the
  request goes in: no file name, no key. Your own `picture_prompt` is
  followed by that context block too, so a prompt that asks for a strict
  form ("answer with one word") still gets the document's text after it.
  The prompt is held once for the request and each picture's message is
  made only when its call goes out. Where the answering model's context is
  known, your prompt is counted against it once: a prompt that by itself
  fills it sends no picture, and one `part_failed` warning (option
  `picture_prompt`) gives both numbers.
- **Where the description goes:** in markdown a paragraph of its own after
  the picture and its caption, in every `pictures` mode (docling writes a
  picture's description after the picture item, which holds its caption):
  `<!-- image -->`, the caption below it, then the description, each with a
  blank line between. A picture whose caption is above it, or that has
  none, has its description right after its line. Every line of the
  description is written as paragraph text, so it cannot start a heading,
  a table, a fence, a comment or a formula block. In `output: text` the
  description stands in the same place (docling's text export does the
  same), so `output: text` with `describe_pictures` looks for pictures too.
  In `json` it is the `figure` block's `description`, and in every output
  the picture's `description` in the file's `pictures` list.
  `pictures: placeholder` is described too: the picture's bytes are made for
  the call and not written.
- **Pictures a model can't take** are not sent, and a `not_yet` warning
  names them: anything but PNG and JPEG (a GIF, WebP, BMP or TIFF would need
  converting), and a picture of more than `PAGE_MAX_PIXELS` (4.4 million
  pixels; scaling it down isn't built yet). A picture that was not exported
  (a `pictures_partial` warning says why) is not described either.
- **On a page the page-reading model read**, its pictures are described
  once, by the picture-description model, like any other: the page's
  prompt transcribes text and describes nothing.
- **The calls are made the page-reading model's way**
  ([Pages read by a model](#pages-read-by-a-model)): as your key, asked
  before each call (scope, budget, tokens-per-minute window, expiry); one
  usage row each (`documents-picture`, token counts, no picture or text; a
  call cut off is a 499 row); at most as many in flight as the model has
  slots, shared with the page-reading model's when both are the same model;
  `max_tokens` from the model's own bound; only an answer the model ended
  itself is a description. `stats.models` counts them as
  `pictures_described` (a model that also read pages shows both), and
  `stats.model_calls` includes them.
- **A picture that is not described keeps its picture, never fails the
  file.** A warning names the pictures (`p1`, `p2`, … with their pages) and
  why: `model_held` (the GPU hold, no fallback), `model_cannot_see` (the
  fallback takes no pictures; nothing is sent to it), `model_not_allowed`
  (your key's policy), `part_failed` (a refusal, an answer that ran to its
  limit, an upstream error, or `document_timeout`). The file is then
  `partial`. A fallback that sees answers like the chosen model, and
  `stats.models` names it with `answered_for`.
- **`document_timeout`** ends the descriptions still going, like the
  page-reading model's pages: descriptions already answered are kept, each
  call in flight counts against the file's time and waiting for a turn does
  not. Pictures whose turn comes after the deadline are not sent. In an
  e-mail, the attachments after the deadline are not read (and one being
  read there ends), each named in a `part_failed` warning.

### Scans and pictures

Text recognition is tesseract, an optional program (`sudo dnf install
tesseract`; the RPM recommends it). It reads:

- PDF pages without a text layer (`ocr: auto`), or every page (`ocr:
  force`): each page is rendered at `OCR_DPI` (300 DPI) by poppler's
  `pdftoppm` and read;
- pictures: PNG, JPEG, GIF, WebP and TIFF files, a multi-page TIFF page by
  page. lmgw's document reader reads a picture's size and decodes each TIFF
  page before tesseract sees it, so pictures need both.

The recognised words go through the same layout pass as a PDF's text layer,
so columns, the reading order and tables come out as they do for a text PDF,
and `output: json` marks each block `"source": "ocr"`. Headings are not told
on recognised pages yet: telling them by the size of their lines gave about
twice as many as there are, so a heading of a scan is written as a paragraph
for now. Each page is laid out on its own, and repeated page headers of a
scan are not removed yet.

A page takes one slot of **Parallel tool runs** for its render and its
reading (about 0.5 to 2.5 s on one core); a long scan goes page by page, at
most as many pages at once as there are slots, so other callers get their
turns, and it stops with the request. A slot stays taken until the program
that used it has really ended, also when the request went away first. A
page's pixels are handed to tesseract in memory. A file with several pages to
recognise is written once to a nameless, read-only spool file under lmgw's
data folder (`convert-spool`, gone when the conversion ends), and each page's
program reads only the parts it needs of it, by position (poppler opens it
as `/dev/stdin`, and its confinement lets it read that one file and nothing
else in the folder); lmgw does not send the whole file once per page.

- **Turned pages.** With tesseract's orientation data installed (`sudo dnf
  install tesseract-osd`, recommended by the RPM, in the server image), each
  page is checked and turned upright before it is read. Without it:
  - a page scanned **on its side** is still read. tesseract reads such a page
    as vertical lines, and when the page lies on its right side it is sure of
    every word, so its confidence gives no warning. lmgw looks at the shape
    of the lines instead: when most lines of two or more words are taller
    than wide, it turns the page a quarter turn and reads it again, and the
    other way if that reads no better. This costs such a page one more
    reading, sometimes two;
  - a page scanned **upside down** is read as it is, which gives wrong text:
    nothing in the result says which way up it is. tesseract is unsure of
    such text, so the page gets an `ocr_low_confidence` warning (mean word
    confidence under `OCR_LOW_CONFIDENCE`, 60 of 100) that names the package.

  Every turn is said in an `ocr_page_turned` warning with the pages, the
  angle and whether the orientation data or the lines' shape decided. A page
  whose lines read as vertical and that no turn read better (vertical text,
  or a poor scan on its side) gets `ocr_low_confidence` too.
- **Languages.** `ocr_languages`, or the installed packs of the desktop's
  language and English. Settings → Documents shows the installed packs in
  its expert view.
- **Large pages.** A page may have at most `MAX_OCR_PIXELS` (100 million
  pixels). A PDF page larger than that at 300 DPI is rendered at the
  resolution that fits, with an `ocr_page_reduced` warning; a picture or TIFF
  page larger than that is not read, and its warning or error names the
  bound. tesseract runs with `TESSERACT_TIMEOUT` (120 s a page) and
  `TESSERACT_MEMORY_LIMIT_BYTES` (2 GiB).

### Web pages, e-books, e-mail and old Excel files

These need no program of your own to install. They are read by
[lmgw's document reader](#lmgws-document-reader), which comes with lmgw:

- **HTML**, by its first bytes (`<!DOCTYPE html>`, `<html>`, `<head>`,
  `<body>`) or, for text starting with another tag, by a name ending `.html`,
  `.htm` or `.xhtml`. Headings, lists, links, emphasis, code and tables are
  kept. Scripts, styles, the page's head, frames, embedded objects and media
  are left out, nothing is fetched, and each picture becomes `<!-- image -->`
  (as docling writes it; exporting a page's pictures is not built yet, see
  [Pictures](#pictures)). The charset comes
  from a byte-order mark or the page's `charset`, else UTF-8, else
  windows-1252. A table is written as a markdown table, one line per row,
  without padding; a cell that spans rows or columns (`rowspan`, `colspan`)
  is written once, in its first place (spans are not read yet), and a table
  inside a cell becomes that cell's text. An element that hides itself (the
  `hidden` attribute, or `display:none` or `visibility:hidden` in its own
  `style`) is left out with its content. Text hidden another way (by a
  stylesheet or a class, `font-size:0`, white on white) is **not** told
  apart and is written like visible text: lmgw reads no CSS yet. A page
  nested deeper than `MAX_HTML_DEPTH` (256 elements, counted on the parsed
  page) is read flat (headings, paragraphs and code), with a warning. The
  parse itself stops as soon as the page nests deeper than four times that
  (1,024 levels): the parser's time grows with the square of the nesting, and
  a 2 MiB page of 200,000 nested elements used to run the reader's whole five
  minutes; it is now read flat in well under a second.
- **EPUB**: the chapters in the order of the book's spine, through the HTML
  reader, as one document. Everything unpacked counts against
  `MAX_UNCOMPRESSED_BYTES`, as for office files. A chapter that is missing,
  outside the book (never fetched) or too deeply nested is said in a warning,
  one per kind of gap with how many chapters it is about and the first of
  them. A book with DRM lists its encrypted files in
  `META-INF/encryption.xml`; those chapters are named as encrypted and not
  read (they would be noise).
- **E-mail** (`.eml`): the subject as the heading, then From, To, Cc and Date
  (the routing fields, message ids, `Bcc` and `X-` fields stay out), then the
  text body, or the HTML body when there is no text one. Each attachment is
  read like a file of its own, under `## Attachment: name`, and one that can't
  be read is a `part_failed` warning; a message attached to a message is read
  the same way, at most `MAX_ATTACHMENT_DEPTH` (3) deep. What each attachment
  converts to counts against the request's output (`MAX_UNCOMPRESSED_BYTES`)
  as it is read: once that is used up, the attachments not yet read are named
  in a warning and left, and the message keeps what fit. Outlook `.msg` files
  are not read yet (`no_reader`, which says to save the message as `.eml`).
- **Old Excel files** (`.xls`, Excel 95 to 2003): each sheet as a table, as
  the `.xlsx` reader writes one. A file whose sheets claim more than the
  reader's memory limit allows (a few megabytes can claim a grid of billions
  of cells) fails by that limit's name, alone.

#### lmgw's document reader

Web pages, e-books, e-mail, old Excel files and pictures are files from
anywhere, read by libraries lmgw did not write (and a PDF page's render,
which poppler draws from such a file, is cut into its pictures there too). A file made to trip such a
library can make it run for hours or ask for more memory than the computer
has. So lmgw does not read these files inside the gateway. It starts a small
program of its own for each file, `lmgw-doc-reader`, hands it the file, and
reads back the result: text, table cells, a page's pixels or an e-mail's
attachments. The reader runs like poppler and tesseract do: in one slot of
**Parallel tool runs**, with nothing of lmgw's open in it, no network use,
and three limits. Whatever a file does to the reader, the gateway, its keys
and the other files are untouched, and the file fails with the limit's name:

| Limit | Value | The file's error |
| --- | --- | --- |
| `READER_TIMEOUT` | 300 s (the same as `PDF_TIMEOUT`) | `unreadable`: "did not finish within READER_TIMEOUT" |
| `READER_MEMORY_LIMIT_BYTES` | 4 GiB | `unreadable`: "ran out of its memory limit" |
| the reader's answer | `MAX_UNCOMPRESSED_BYTES` (1 GiB) | `output_too_large` |

The reader is installed with lmgw (`/usr/libexec/lmgw/lmgw-doc-reader` from
the RPM, beside `lmgw-server` in the server image) and is looked for only
there, never on `PATH`. It shows on the Documents page with its version, and
in an answer's `tools` as `reader`. If it is missing (a damaged install), its
file types fail with `tool_missing` naming it, and the page says to install
lmgw again; lmgw never falls back to reading such a file itself. Starting it
costs a few milliseconds per file.

On Windows the reader is not used yet: lmgw cannot give a program a memory
limit there yet, and it runs no document program without one. Web pages,
e-books, e-mail, old Excel files and pictures are therefore not read on
Windows yet, and the page says so.
### Pages read by a model

With a page-reading model chosen (the Documents page), a PDF's pages that
`model_help` selects are rendered (150 dpi) and sent to it, one call a page,
after lmgw's tools have read the file. **A request that does not set
`model_help` uses the chosen model** for pages nothing read, pages text
recognition read poorly, and pages with a table. Only "no model chosen" or
`model_help: off` guarantees that no page goes to a model.

**Who reads a page.** A page is read by its text layer first; a page without
one by text recognition (tesseract, `ocr: auto`); the model is asked last,
about what those left:

| The page | `missing` | `missing_or_tables` (the default) | `every_page` |
| --- | --- | --- | --- |
| nothing read it: no text layer, and text recognition is off, not installed, failed, or found no word | its picture | its picture | its picture |
| text recognition read it poorly | – | its picture | its picture |
| text recognition read it well, with a table | – | its picture | its picture |
| text recognition read it well, no table | – | – | its picture |
| its text layer, with a table | – | picture and text | picture and text |
| its text layer, no table | – | – | picture and text |

"Poorly" is what the `ocr_low_confidence` warning says: the recognised words'
mean confidence is below `OCR_LOW_CONFIDENCE` (60 of 100), or the lines read
as vertical and no turn read better. A page that was turned upright first
(`ocr_page_turned`) and then read with confidence is read well. Without
tesseract every page without a text layer is a page nothing read. With
`ocr: force` a recognised page counts as recognised, whatever text layer it
had.

- A page without a text layer is sent as its picture alone, also when text
  recognition read it: recognised text may be wrong, which is why the model
  reads the page. A page with a text layer is sent with that text, and the
  model is told to copy from it and use the picture for the layout only.
- What the model reads **replaces** what the tools read on that page:
  `stats.parts_by_source` counts the page under `vision`, not under `ocr`,
  and text recognition's warnings no longer name it. Pictures on the page
  stay: the figures the layout model found keep their place where the
  model's answer has a picture in the same place (a DocTags answer) and
  otherwise follow the model's text, and poppler's pictures are placed into
  the page as on any page (at its end when the answer has no positions, as a
  Markdown answer has none). When the layout model is on it still reads the
  pages the model will read too: which pages the model answers is only known
  afterwards (a page it doesn't answer keeps the tools' text, laid out with
  the layout model's help), so on pages the model reads its regions are
  read and then not used.
- The model is asked for Markdown: headings, lists, tables, formulas as
  LaTeX between `$$` lines and program code in fenced blocks. Its answer
  replaces the page's blocks, with the source `vision`. Whatever the answer
  says, it only becomes text: nothing in it is followed, and a code or
  formula block is written so its own text can't end it.
- **Only a whole answer is a page.** An answer the model didn't end itself
  is a failed page (`part_failed`, naming why) and the page keeps the tools'
  text: one that ran to its token limit (a model repeating itself), a
  refusal, a filtered answer, a tool call, any other finish reason, an empty
  answer, or one that holds only picture regions.
- **The calls are yours.** They are made as the key that sent the request.
  Before each call lmgw asks again whether the key may make it: the key
  still exists and isn't disabled, rotated or expired, its scope allows the
  model, its budget and the gateway's aren't spent, and its
  tokens-per-minute window isn't full. A refusal leaves the pages not yet
  sent with the tools' text and a `model_not_allowed` warning; nothing is
  loaded or rendered for a key that may not use the model. The calls aren't
  counted as requests in the key's requests-per-minute window: the
  conversion is the one request. Each call is its own row in the usage log
  (`documents-page`), with token counts and no page content; a call cut off
  before its answer (you went away, `document_timeout` passed, the job was
  cancelled) is a row with status 499.
- **How far a budget can be passed.** A call's cost is known when it ends,
  so a budget stops the *next* call. A conversion keeps several calls in
  flight, so a budget can be passed by at most that many calls
  (`in_flight_bound`), each up to the model's largest answer; a
  tokens-per-minute window likewise.
- **How many at once.** A request keeps at most as many calls in flight as
  the model has slots: a local model's parallel slots, or for any other model
  the size of **Parallel tool runs**. A page is rendered only when its call
  can start. `stats.models` shows the bound in use (`in_flight_bound`,
  `bound_from`).
- **The page's picture** is at most `PAGE_MAX_PIXELS` (4.4 million pixels,
  an A3 sheet at 150 dpi). A larger page is sent scaled down to that, with a
  `page_scaled` warning that names both numbers.
- **The answer's length** is bounded by the model itself: for a local model
  its context less the prompt lmgw counted on it, otherwise the model's
  largest answer where its catalog says one. Where lmgw knows neither it
  sends no limit of its own. An answer that runs to its bound without ending
  is a failed page, never half a page.
- **When the model can't run, its fallback answers** if it has one: under
  the GPU hold, when the graphics card is full or held by a benchmark, when a
  service is out of quota. `stats.models` then names the model that answered
  and, as `answered_for`, the one it answered for. The fallback gets the
  page's picture and lmgw's Markdown prompt, whatever format the chosen
  model answers in. A fallback that takes no pictures gets nothing: the
  pages keep the tools' text, with a `model_cannot_see` warning naming it.
  Without a fallback the pages keep the tools' text with a `model_held`
  warning; the same in a background conversion, which doesn't wait for the
  hold to end. lmgw holds the model only while a file has pages for it, so a
  long job doesn't keep it from being held or unloaded.
- **`document_timeout`** ends the model's work on a file, not the file:
  pages already read and the tools' text are kept, the pages not answered
  get a `part_failed` warning naming the timeout, and the file is
  `partial`. Every page is counted like a page of text recognition: the
  render and the call in flight count against the file's time, waiting for
  the model to start, for a turn among the calls in flight or for a slot of
  **Parallel tool runs** does not. See [A file's time](#a-files-time).
- A DocTags model's answer that is not well-formed, or came back without
  its tags, keeps its text; the page is read, and a `model_answer_malformed`
  warning says what was wrong (and names `--special` when the tags were
  missing).
- A page the model could not read keeps what the tools read: a page with
  text keeps it, a page without stays unread, and a warning names the page
  and why. `stats.model_calls` counts the calls, `stats.parts_by_source.vision`
  the pages a model read.
- **A file nothing could read** (every page a scan, and the model read
  none) fails with the model's reason instead of `no_reader`: `model_held`
  (503: the model is paused, or can't run and its fallback takes no
  pictures), `model_not_allowed` (403: the key's policy) or `model_failed`
  (502: the model or its service failed). Its `stats` still lists the calls
  that were made.
- Repeated page headers and footers: the `header_removed` warning counts
  only pages the tools read. A page the model read is written as the model
  saw it, so such a line may be part of its text there.

Not yet: picture files (PNG, JPEG, TIFF, …) are read by text recognition
only, not by the model (a picture would need converting or scaling to a
size and format every model takes, in lmgw's document reader); slides and
office files; stopping a runaway before it reaches its bound; a retry when a
model's service answers "too many requests" (the page fails by name); and
the bound in use in a running job's status.

### A file's time

`document_timeout` is one budget of working time per file, counted on one
clock for all its steps: the PDF's poppler runs, the layout model, each page
of text recognition, the picture search beside the text, and each page the
model reads. Steps that run side by side count once (the clock runs while
any of them works); waiting counts nothing: for a slot of **Parallel tool
runs**, for the layout model, for the page-reading model to start or for a
turn among its calls in flight. The steps share it like this:

1. The layout model stops once the file has used half of it
   (`MODEL_DEADLINE_SHARE`); its pages left are laid out without it.
2. Until the page-reading model starts on the file, the deadline fails the
   file (`deadline_exceeded`) and stops its tools: nothing was finished yet.
3. Once the page-reading model has pages of the file, the deadline is its:
   its calls in flight are dropped, the pages answered are kept, the rest
   keep what the tools read with a `part_failed` warning, picture searches
   still going are stopped (their pages' pictures stay placeholders, named
   in a `pictures_partial` warning), and the file is `partial`.

### Old Word and PowerPoint files, RTF, LaTeX and other markup

These are read by turning them into something lmgw already reads, with an
optional program:

- **Old Word and PowerPoint files** (`.doc`, `.ppt`, Office 97 to 2003) need
  **LibreOffice** (`sudo dnf install libreoffice-writer libreoffice-impress`;
  Writer reads `.doc`, Impress `.ppt`). LibreOffice saves the file as a
  `.docx` or `.pptx`, and lmgw reads that with its own reader, so you get the
  same headings, lists, tables, pictures and `json` as from a newer file. A
  Word file with equations goes on to pandoc, below. The answer's `tools`
  name `libreoffice` and the reader that read its result.
- **RTF** needs **pandoc**, or LibreOffice where pandoc is missing.
- **LaTeX** (`.tex`), **reStructuredText** (`.rst`), **Org** (`.org`) and
  **MediaWiki** (`.wiki`) are plain text to look at, so lmgw goes by the
  file's name. **DocBook** and **FictionBook** (`.fb2`) are XML, told apart
  by their root element. All of them need **pandoc** (`sudo dnf install
  pandoc-cli`) to be read with their headings, lists, tables and code.
  Without it they come back as the plain text they are, markup included,
  with a `tool_missing` warning that names pandoc.
- **Word files with equations** (`.docx` whose text holds Office Math): lmgw's
  own reader keeps an equation only as its characters run together (E = mc²
  as `E=mc2`). With **pandoc** installed, such a file is read by pandoc, and
  its equations come out as LaTeX: `$E = mc^{2}$` inline, `$$…$$` on a line
  of their own. Without pandoc, lmgw's reader reads it, with an
  `equations_dropped` warning that names pandoc. Pictures in a file read
  through pandoc are placeholders for now (`<!-- image -->`, with a `not_yet`
  note when you ask for embedded or referenced pictures).
- **DocBook and FictionBook files nested deeper than
  `PANDOC_MAX_XML_DEPTH`** (256 elements) fail with `too_deep` (422) before
  pandoc runs: pandoc's XML readers take time growing with the square of
  the nesting (pandoc 3.7: 5,000 levels half a second, 50,000 almost two
  minutes), and lmgw's markdown writer reads no deeper anyway. The other
  formats pandoc reads for lmgw stayed under a second on such files and
  are not counted.

pandoc writes HTML, which [lmgw's document reader](#lmgws-document-reader)
turns into markdown as it does a web page, so a table whose cells hold lists
or several paragraphs is still a table, and raw HTML a document carries
(scripts, frames) is left out. Each program runs like poppler: in a slot of
**Parallel tool runs**, in a process group of its own that is stopped whole,
with a time, a memory and (for LibreOffice) a file-size limit whose names
the file's error quotes. Every one of these programs, poppler and tesseract
included, is started by lmgw's reader, which closes what it inherited from
lmgw, sets the limits and, where the kernel has Landlock, confines it to the
system's files before the program starts; lmgw itself is never copied to
start a program, so starting one stays fast however much memory lmgw holds.
When the reader cannot apply a program's confinement or limits as it starts
it (the kernel refuses a rule, a resource runs out), the program is not
started and the file fails with `tool_not_started` (500): lmgw's own
failure, not the file's, and the gateway's log has the reason. A program's
own error lines reach you as it wrote them, with every path in them cut to
its file name (`…/in.media`) and memory addresses left out (`0x…`); the
gateway's log keeps where the program is and the folder it ran in.
Without the reader no program starts, and every format that needs one says
so on this page and in its error (`tool_missing`, "install lmgw again"):

| Limit | Value | Error |
| --- | --- | --- |
| `LIBREOFFICE_TIMEOUT` | 300 s | `unreadable` |
| `LIBREOFFICE_MEMORY_LIMIT_BYTES` | 4 GiB | `unreadable` (it ran out, or LibreOffice crashed) |
| `PANDOC_TIMEOUT` | 300 s | `unreadable` |
| `PANDOC_MEMORY_LIMIT_BYTES` | 4 GiB | `unreadable` |
| the result, and every file LibreOffice writes | `MAX_UNCOMPRESSED_BYTES` (1 GiB), or what is left of the request's output budget when that is less | `output_too_large` |

**What a document can and cannot make them do.**

- **LibreOffice runs only inside a confinement the kernel enforces**
  (Landlock, applied by lmgw's document reader before LibreOffice starts).
  It can read the system's programs and libraries (`/usr`), run only its
  own folder's programs, its start script's shell and helpers (`sh`,
  `basename`, `dirname`, `grep`, `ls`, `paperconf`, `sed`, `uname`) and the
  dynamic loader, read the loader cache,
  the font configuration and the user database (`/etc/ld.so.cache`,
  `/etc/fonts`, `/etc/passwd`, `/etc/group`, `/etc/nsswitch.conf`,
  `/dev/urandom`), read and write its own private folder (but not run
  anything written there) and `/dev/null`, and create its one
  single-instance socket in `/tmp` (nothing else there: it cannot read,
  write or delete anything in `/tmp`; lmgw removes the socket afterwards).
  Nothing else: not your home folder, not the rest of lmgw's data folder,
  not `/run/user`. So a document that names a file (an RTF's
  `INCLUDEPICTURE "/home/…/photo.png"`, a linked picture or section) gets
  nothing of it. It has no TCP connections (Landlock ABI 4, Linux 6.7 or
  newer, which LibreOffice needs) and no UDP (Landlock ABI 10; on an older
  kernel UDP is not blocked yet). Its environment is its own (none of
  lmgw's). Where the confinement cannot be applied (a kernel older than
  5.13, Landlock not enabled among the kernel's security modules, a
  Landlock older than ABI 4, lmgw's document reader missing, a system other
  than Linux, an install folder that would reach your home folder or lmgw's
  own folders), LibreOffice is **not used**: Settings → Documents shows it as
  failing lmgw's check, saying why, and old Word and PowerPoint files fail
  with `tool_missing` saying the same. pandoc and ffmpeg run in the same confinement where it is
  available (stdin to stdout for pandoc, its own folder for ffmpeg), and as
  before where it is not.

- pandoc runs with `--sandbox`: a document cannot make it read a file (LaTeX's
  `\input`, reStructuredText's `include`, Org's `#+INCLUDE` get nothing) or
  fetch anything. No filters, no Lua, no templates or defaults files are
  used, and pandoc's data folder in your home is never read. lmgw checks
  that the pandoc it finds honours `--sandbox`; one that does not (older
  than 2.15) is shown as failing lmgw's check and is not used.
- LibreOffice gets a private profile for every file (a copy of one lmgw
  made without any document, never your own, so a LibreOffice you have open
  is not touched, and two files at once never wait for each other), in a
  private folder under lmgw's data folder with the file and LibreOffice's
  temporary files. The folder is deleted afterwards, whatever happened, and
  only once LibreOffice and everything it started have been stopped. The
  profile switches off macros (all kinds, at the highest security level,
  and VBA is not even loaded), active content (OLE objects, DDE) and the
  Python runtime, and never updates a document's links or fields when it
  opens it. These settings are not what keeps your files out of reach (an
  RTF's linked picture is followed whatever they say): the confinement
  above is. The file is opened with its format named (a `.doc` as Word 97),
  not guessed. LibreOffice runs without a display.
- **Not prevented yet:** on a kernel before Landlock ABI 10, LibreOffice can
  still send UDP; before ABI 9 it can still connect to a Unix socket by its
  path. Fonts installed in your home folder are not visible to it (it uses
  the system's).
- What a program writes into its private folder is read back without
  following a link: the confinement lets it make no symbolic link there,
  and lmgw opens its result refusing one anyway, and reads no more of it
  than such a result can be. When a program ends, what it started is
  stopped with it, also after a normal end, as long as it stayed in the
  program's process group; its error output is read for at most
  `STDERR_DRAIN_TIMEOUT` (5 s) more (what came before is kept). A process
  that leaves the group (`setsid`) survives that, still under the
  program's confinement and limits: ending it needs a supervising parent,
  which lmgw does not have yet. So that such a process can do no harm to
  lmgw itself, lmgw keeps no file a program can still write open as
  memory (ffmpeg's sound is copied, a clone of its blocks where the file
  system shares them, into a file of lmgw's own before it is read), and
  every program has a CPU time bound that a process it started keeps: its
  timeout times the cores lmgw may use (`RLIMIT_CPU`, 300 s × 32 = 9600 s
  for ffmpeg on a machine of 32 cores). A run that keeps to its timeout
  never reaches it; a process past it is ended by the kernel, and a
  program ended so fails saying "used its CPU time bound".
- A password-protected Office file (an encrypted `.docx`, `.xlsx` or
  `.pptx`, or a `.doc` or `.ppt` that says it is encrypted) fails with
  `unreadable`, saying it is protected by a password; one LibreOffice cannot
  open fails saying it is damaged or protected by a password. When
  LibreOffice could not run here (its single-instance socket in `/tmp` held
  by another process, or it ended without a result and without saying the
  file could not be loaded), the file fails with `tool_not_started` (500),
  worded as lmgw's failure, not the file's; the log names the socket.

Slides whose content is a picture (a chart pasted as an image) need a model
that looks at the slide. lmgw can have LibreOffice render a deck to a PDF for
that, but the page-reading model reads PDF pages only for now, so a deck is
read for its text and pictures as above.

### Audio and video

An audio file (WAV, MP3, Ogg, FLAC, M4A, WebM) is transcribed by the
**speech-to-text model** you choose under **Settings → Documents → Model
help** (`documents_asr_alias` of the dashboard's `settings_set` op). None is
chosen at first,
and then audio and videos fail with `model_not_chosen` (415), which says
where to choose one: a conversion never calls a model you did not pick. The
Chat's own speech-to-text model is not used for conversions.

A video (MP4, QuickTime, Matroska) needs **ffmpeg** too (`sudo dnf install
ffmpeg-free`, or `ffmpeg` from RPM Fusion; the server image has it). ffmpeg
takes out the first sound track as 16 kHz mono WAV, which then goes to the
model. The video's picture, subtitles and attachments are not decoded.
ffmpeg reads the container lmgw recognised from the file's bytes and nothing
else (no playlists, no network: it may open files only), from a copy in a
private folder under lmgw's data folder, since many MP4 files keep their
index at the end and cannot be read from a stream. The sound is written to
that folder too and stays there, on disk, until the model has it (lmgw maps
the file instead of loading it into its memory); the folder is deleted
afterwards. The sound may be at most 9 h 19 min long (`MAX_UNCOMPRESSED_BYTES`
of 16 kHz mono sound), and no longer than what is left of the request's
output budget, which it counts against while it exists; a longer video
fails with `output_too_large`, naming the bound. `FFMPEG_TIMEOUT` (300 s) and `FFMPEG_MEMORY_LIMIT_BYTES`
(2 GiB) bound the run. Fedora's `ffmpeg-free` lacks some decoders (the
expert view lists the audio decoders found); a video whose sound it cannot
decode fails with `tool_missing`, naming the decoder.

The upload is named `audio.<type>` (`audio.mp3`, `audio.wav`), never by your
file's name: a cloud model reads the format from the name, and a file's name
is yours. Cloud speech-to-text services limit uploads (OpenAI's is 25 MiB,
about 13 minutes of 16 kHz WAV); a longer file fails with `model_failed`,
quoting the service's answer.

The model is called as the key that sent the file: before the call, the
key's expiry, scope, budget and tokens-per-minute window are asked, as for
the page-reading model (`model_not_allowed`, 403, when the key may not use
the model). The call is not counted as a request of its own in the key's
requests-per-minute window: the conversion is the request. The call has its
own row on the Usage page, labelled `documents-asr`, and the GPU hold
applies. A local model under
the hold with no fallback fails the file with `model_held` (503). One with a
fallback is answered by the fallback, as configured, and the file's
warnings say which model answered (`model_held`, naming both). The
transcript is one part of paragraphs (the model's own line breaks), marked
`asr` in `json` and in `stats.parts_by_source`; `stats.model_calls` is 1 and
`stats.models` names the model that answered (with `answered_for` when a
fallback answered for the chosen one, and `files_transcribed` 1 beside the
page-reading model's `parts_read` and the picture-description model's
`pictures_described`); a file whose call failed after it was
sent keeps them too. A model that hears nothing gives an empty part with a
`part_failed` warning. Timestamps are not given yet. `document_timeout`
covers the transcription: the file's time runs from the call's start (from
ffmpeg's for a video), not while it waits for the model's turn, and at the
deadline the call is stopped and the file fails with `deadline_exceeded`.
A call stopped before its answer (that, a client that went away, a cancelled
job) still leaves a row (status 499) under your key on the Usage page.

A WebM file is taken as audio, even one with a picture: it goes to the model
whole, picture included (ffmpeg does not take its sound out yet). MIDI files
are no speech and are refused as `unsupported`.

### Chat attachments and knowledge bases

The Chat and knowledge bases read the files they always read the way they
always did. A file they used to refuse is now read through the same
conversion, where it can be: old Word, PowerPoint and Excel files and EPUB
books (an attachment of kind `office`, its own format in its details). One
it still cannot read says why: the program to install, a password, an
Outlook message. Existing knowledge bases keep the text they were indexed
from; nothing is read again by itself. Pictures read by text recognition
and videos are not taken there yet.

### `output: json`

Each file's `content` is `null` and `structure` holds its parts in order: a
PDF's pages (`kind: "page"`, `number` as in the file, `width_pt`,
`height_pt` as the page is displayed, so a page turned by `/Rotate 90` of a
portrait sheet is landscape, as its boxes are), a deck's slides, a workbook's sheets (with `name`), or one
`document` for Word, OpenDocument text and text files. A markdown file
(named `.md`, `.markdown`, `.mdown`, `.mkd`, `.mkdn`) is read into
headings, paragraphs, list items, tables and code; any other text file
(code, YAML, CSV, plain text) is one `paragraph` block with its text exactly
as it is. A part's `source` says
how it was read (`text_layer`, `native`), or is `null` for a PDF page that
has no text layer. Each part has `blocks` in reading order:

```json
{ "kind": "heading", "level": 1, "text": "Example Report",
  "bbox": [56.8, 66.3, 197.2, 88.4], "source": "text_layer" }
```

Block kinds are `heading`, `paragraph`, `list_item`, `table` (with `table:
{ rows, cols, cells: [ { row, col, row_span, col_span, header, text } ] }`),
`page_header`, `page_footer`, `figure` (with `picture`, its id in the
`pictures` list, and `caption`) and `caption`. With the layout model on, a
PDF's pages also get `code` and `formula` blocks, and a `figure` block the
model found has `source` `onnx` and the text inside the picture as its
`text`; otherwise `code` and `formula` are in the schema for readers that
are not built yet (office files' fenced code is `code`). A figure's
`description` is its picture's description when the request asked for
descriptions ([Pictures described by a model](#pictures-described-by-a-model)). `bbox` is
`[x0, y0, x1, y1]` in points from the page's top
left corner, `null` where the file has no geometry. `tables: false` turns
tables into paragraphs of tab-separated lines. `stats` counts the
structure's headings and tables, and a slide's or sheet's own title as the
`##` heading markdown gives it, so the counts are the same in every output.

### The answer

The answer always has a `documents` list with one entry per file, in the
order sent, even for one file:

- `status`: `converted`, `partial` (some parts were left out; `warnings` says
  which) or `failed` (with `error.code` and `error.message`).
- `format` as read from the bytes, `parts` (a PDF's pages, a deck's slides, a
  workbook's sheets, 1 for a document), and `content` (or `structure` for
  `output: json`).
- `stats`: how many parts each source read (`text_layer` for a PDF page's
  own text, `ocr` for a page or picture tesseract read, `vision` for a page
  the page-reading model read, `native` for lmgw's readers), the headings per
  level, tables and their cells as the markdown has them, the model calls
  made (`model_calls`, and `models` with each answering model's calls), and
  the file's time.
- `warnings`, each with a `code` and a sentence. A PDF page without a text
  layer that nothing read (`ocr: off`, tesseract not installed, and no
  page-reading model read it) is `no_reader` or `tool_missing`. The pages are
  named as ranges ("3-900, 902"), and `parts` lists them. Warnings about the
  layout model carry `tool: "layout_model"`.

`summary` counts the statuses, `tools` names the programs that ran with their
versions, and the top-level `warnings` are the option notes above.

One bad file never fails the others. The status is 200 when at least one file
was converted or partly converted. When none was, a one-file request answers
that file's code (415 for `tool_missing`, `no_reader` and `unsupported`, 413
for `output_too_large`, 422 for a damaged file, `invalid_base64` or
`memory_limit`; 503
`model_held`, 403 `model_not_allowed` or 502 `model_failed` when only the
page-reading model could have read it) and a request of several files 422;
the body has the same shape either way. A PDF without poppler-utils installed
fails with `tool_missing`, naming the package and the command that installs
it.

If the client goes away, the conversion stops and the programs it started are
stopped with everything they started; the Usage row says 499. lmgw's own
office readers (docx, pptx, xlsx, OpenDocument), which run inside the
gateway, stop too: they ask between pieces of their work whether their file's
`document_timeout` has passed or its request has gone, and give their slot
back then, instead of reading on to the end of a file that unpacks to
hundreds of MiB. A stopped
program's slot is given back only once the program has really ended. Conversions
from different keys take turns in **Parallel tool runs**, so a large batch
doesn't hold back someone else's single file. A request without a key (auth
off) takes turns as a caller of its own, and all of them together are one
caller for the share each caller may hold. A request's body must arrive
within `UPLOAD_READ_GRACE_S` (60 s) plus one second for every
`UPLOAD_MIN_RATE_BYTES_PER_S` (256 KiB) of its declared length (of
`max_body_mb` when it declares none, a chunked upload): a body still arriving
then is refused with 408 `read_deadline`, naming both. A file's
reading and shaping run in those slots too, never on the server's async
threads; a reader that crashes fails its file with `unreadable` and the others
convert. Telling a file's type apart runs in a slot as well, and reads only a
container's list of contents (and at most 64 bytes of a ZIP's `mimetype`
entry, `ZIP_MIMETYPE_BYTES`).

**The markdown is the document's, not sanitised.** lmgw writes a document's
text and markup into the markdown faithfully: inline HTML, `javascript:` and
`data:` links and remote pictures a document or a web page carries stay in
the text (only a line-opening `<`, `#` or similar is escaped, so a line
does not become a block it was not), and a page-reading or
picture-description model's answer goes in as it answered, which a document
can steer by what it says. Treat the output as untrusted: a program that
renders it (a chat front end, a wiki, a RAG viewer) must sanitise it, as
lmgw's own Documents lab does (raw HTML shown as text, only http, https,
mailto and relative links kept, remote pictures shown as links, nothing
loaded from another site). Serve exported pictures (`artifacts/…` in a zip,
`pictures` in the answer) with `X-Content-Type-Options: nosniff` and the
type lmgw gives them: lmgw tells a picture's type by its first bytes, and a
file that is also something else keeps its picture name.

The request's usage row records a failed request's codes by file position
("2 of 2 files failed (file 0: unreadable, …)") and never a file's own error
text, which can quote the document; the answer carries the messages. Chat attachments and knowledge
bases share the same pool.

### How much answers hold

A conversion's answer lives in lmgw's memory from its first converted file
until the client has read all of it, and one request's output may be up to
`MAX_UNCOMPRESSED_BYTES` (1 GiB): a 1 MiB docx can unpack to a 300 MiB
answer. So the answers in flight share one bound, the memory lmgw may use
(the same figure **Parallel tool runs** counts with: a cgroup limit below
the computer's memory, else the computer's memory; not read on Windows and
macOS yet, and then not bounded). One request's files may produce up to
`MAX_UNCOMPRESSED_BYTES` together, or half that bound where that is less (so a
1.5 GiB container converts up to 768 MiB a request; `output_too_large` names
the figure in force). Each conversion holds a share of the bound: its upload
(the body's declared length, twice it for JSON; the most `max_body_mb` allows
for a body sent without a length) plus that output, from before its body is
read until its answer is made, and from then on the body's real size until
the body has been sent. On an 8 GiB machine that is seven small conversions
at once (four before the review round, which held 2 GiB each). An answer too
large to sit beside itself in the share is written to the client as it is
read, never made whole in memory. A request that finds the bound held waits
for its turn, holding nothing else: no upload read, no tool slot, and the wait
is not part of its `document_timeout`. Where a second caller can exist (as for
**Parallel tool runs**), one caller takes another share only while one like it
stays free beside its own, and a caller holding none goes first; with one
caller, shares go first come, first served. A request whose share is larger
than the whole bound is refused with 503 `answer_memory`, which names the
figures. A job holds a share while it converts (its uploads and its output,
counted against the key that sent it), and so do the result routes while they
send a stored answer. **Settings → Documents → Memory for answers** shows the
bound, the output a request may make, what is held and what waits; a running
job's state shows the same under `pool.answers`.

A client gets `ANSWER_WRITE_GRACE_S` (60 s) plus one second for every
`ANSWER_MIN_RATE_BYTES_PER_S` (1 MiB) of an answer to read it (a 300 MiB
answer: 6 minutes); the answer says so in `x-lmgw-write-deadline: <seconds>
(ANSWER_WRITE_GRACE_S + 1 s per ANSWER_MIN_RATE_BYTES_PER_S)`. One that reads
slower has its connection ended there, closed with a reset, also when the
answer's last bytes were handed over in time and some of the answer's own bytes
still wait unread, as long as the connection has done nothing since (a client
that sent its next request has taken the answer, and later responses on the
same keep-alive connection are never judged); the gateway's log names both
limits and how much was handed over, and the answer's memory goes back. Over
HTTP/2 only the answer's own stream is reset, never the connection. The request's Usage row is written once its answer has been
handed over: a client that left midway is a 499 `client_disconnected` row, one
that missed the deadline a 499 `write_deadline` row, each saying how much of
the answer went out. Chat attachments and knowledge-base reads hold no share
yet.

### Background conversions

With `async: true` the request becomes a job:

- `GET /v1/documents/jobs/{id}` is its state: `running`, `succeeded` (every
  file has an outcome and one was converted), `failed` (none was),
  `cancelled` or `interrupted`; per file its state, parts and error code;
  while it runs, how many conversion steps of all callers run and wait in
  **Parallel tool runs**. A file's parts are known once it is read, so the
  counts grow file by file (progress inside one file isn't reported yet).
- `GET /v1/documents/jobs/{id}/result` is the same answer the request would
  have had without `async`, once the job has ended (409 `job_running` before).
  `?index=2` answers one file's entry as soon as that file is done. With
  `output: json` the stored entries carry `structure` (and `content: null`)
  exactly as the sync answer does, so a job's result and its sync answer for the
  same files are the same JSON.
- `DELETE /v1/documents/jobs/{id}` cancels a running job: its programs are
  stopped and its converted text deleted. On an ended job it deletes the job
  and its text.

A job belongs to the key that made it; another key gets a 404, the owner sees
every job. A job's uploads wait on disk (`convert/<job>/input/<file>`, readable
by you only) until their file is converted, so a long queue holds no uploads in
memory; at most as many files of a job are read at once as there are
**Parallel tool runs**. The converted text is kept in lmgw's data folder
(`convert/<job>/<file>/entry.json`, also readable by you only), never in the
usage log, for **Settings → Retention → Conversion jobs** (1 day by default,
`0` keeps them); then the job and its text are deleted. **Stored conversions**,
right below that setting, shows how many jobs have files on disk, how much space
they take and how many files still wait (`doc_jobs_stored`,
`lmgw__doc_jobs_stored`).

Two settings under **Settings → Documents → How much conversions may hold**
bound the jobs, each derived until you set it (`settings_set`'s
`documents_job_storage_mib` and `documents_jobs_per_key`, 0 goes back):

- **Storage for conversion jobs**: what the jobs store together, uploads and
  results. Derived, read again at each new job and each result: half of what
  the jobs store now and what is free now on the data folder's disk together,
  so the other half of the free space stays for everything else on that disk,
  also after a model download took some of it (where the free space cannot be
  read, nothing is bounded unless you set it). A size you set is used as it is. A job whose uploads do not fit beside what is stored is refused
  with 507 `job_storage_full`; a file whose result does not fit is failed
  with a message naming the setting, and the others go on. The message says
  what is stored and the bound.
- **Jobs per key**: how many jobs one key has running or waiting at once
  (all requests without a key count as one). Derived: the size of
  **Parallel tool runs**. One more answers 429 `too_many_jobs`, naming the
  setting, until one of them has ended.

A key's **concurrent requests** limit counts a job as one request for as long
as it runs, so a key limited to 2 has two jobs at once and a third `async`
request answers 429 until one ends. Looking at a job (its state, its result,
cancelling it, docling's poll, WebSocket and result routes) takes no slot and
counts no request against the key's requests per minute. If lmgw stops while a job
runs, the job is `interrupted` after the restart: the files that had finished
keep their text, the others say `interrupted` and need sending again. The
request's usage row is written when the job is accepted (status 202).

## Trying it in the dashboard: the Documents lab

**Labs → Documents** sends files through the conversion API from the
dashboard, as the Audio and Image labs do for theirs:

- **Files**: drop files on the box or pick them; the list shows each file's
  type (from its first bytes, as the route reads it), its size and its place:
  the files are sent in that order, ▲ and ▼ move one.
- **Options**: every option of `/v1/documents/convert` in plain words, with
  the field's name beside it: the output (markdown, plain text or the
  structure), pages, page breaks, tables, text recognition, model help and
  pictures. None is hidden because its feature isn't built yet or its program
  is missing here: the group says what **Settings → Documents** reports about
  it, and after a run it shows the route's own note on that option
  (`not_yet`, `tool_missing`, `model_not_chosen`). **Background job** sends
  `async: true`; the card then follows the job (each file's state, the parts
  done, the tool pool's running and waiting steps) and **Cancel job** cancels
  it.
- **Convert** or **As docling-serve** (the menu after the title): the second
  sends the same files and options to `/v1/convert/file`, each option as the
  docling option lmgw maps it onto, to show what a docling client gets: the
  docling answer for one file, `converted_docs.zip` for several (kept whole and
  entry by entry), and the `x-lmgw-option-notes` and `x-lmgw-documents`
  headers decoded. Docling's form has one span of pages (`page_range`) and no
  field for the picture prompt; the lab says so before sending.
- **Results**: each run is a card: the route's notes on the options first,
  then the files, a list on the left and the chosen file's result on the
  right: its status (a failed file with its code and message), how it was read
  (parts by how they were read, headings, tables, pictures and how many were
  described, time), its warnings with
  their codes in plain words, and its text as rendered markdown, as it is
  (**Raw**), and for `output: json` as an outline of parts and blocks (kind,
  level, page, how it was read; tables as tables; a picture's caption and
  description; the boxes in the **Expert view**) and as its JSON (on the docling route, whose `json_content` is
  docling's own document, only the JSON). Each form has **Copy** and
  **Save…**. A long text is shown a page at a time (about 48 KiB of text, or
  200 blocks, a page; a line longer than 192 KiB, such as the structure's
  compact JSON, is cut inside itself; a table opens 50 rows, and "Show all"
  pages it 500 rows at a time), so an 800-page PDF stays quick to look
  through, and switching between the forms of a result does not fetch or
  parse it again. **The request as curl** is the run's
  request to copy, with `$LMGW_KEY` where the key goes.
- The rendered markdown comes from the documents and is shown the way the Chat
  shows a model's reply: HTML in a document is shown as text, a link opens in
  your browser only for http(s) and mail addresses, and pictures from the web
  are never loaded (only pictures embedded in the text show).
- **History**: the runs are kept in the lab store like the other labs': the
  files you sent and the texts that came back, until you delete a run or
  **Clear** the lab; the results head says how much they take. **Open in
  form** puts a run's options and files back into the form (the files as kept
  files, so changing an option and converting again uploads nothing), **Run
  again** sends the same files and options again, **Save texts…** saves a
  run's texts as a zip, and **Option presets** keep named sets of options.

The lab sends its requests through the routes' own handlers inside lmgw, with
the routes' bounds. A request past **Max request body** gets the route's own
413 while the files are still being added: the bytes are counted as they
arrive, nothing is kept, and no file is held in memory whole by the lab (each
goes to disk as it arrives, and a run streams them from there). Each run is a
request on the Usage page like any other. A run whose job is still going when
lmgw stops is shown stopped after the restart (the job itself is
`interrupted`); a job the gateway no longer has shows as gone after three
reads that say so. The results head's filter offers the two routes.

## Calling lmgw as docling-serve

Tools that call [docling-serve](https://github.com/docling-project/docling-serve)
can call lmgw instead: point them at lmgw's address and give them an lmgw key
(docling's `X-Api-Key` header works, and so does `Authorization: Bearer`).
lmgw answers docling-serve's routes (release 1.38) with its own conversion,
the same one as `/v1/documents/convert`:

| docling-serve route | lmgw |
| --- | --- |
| `POST /v1/convert/file` (multipart `files`), `POST /v1/convert/source` (JSON `sources` of `kind: "file"` with `base64_string`; the older `file_sources` too; `kind: "http"` web addresses when you allow them, below) | converts now |
| `POST /v1/convert/file/async`, `/v1/convert/source/async`, `/v1/convert/source/batch` | a task, as a background job above |
| `GET /v1/status/poll/{task_id}` (with `wait`), `WS /v1/status/ws/{task_id}` (the key as `?api_key=` works), `GET /v1/result/{task_id}` | the task's state and result |
| `GET /health`, `/ready`, `/version` | answered, no key needed |
| `/v1/chunk/*`, `/v1/capabilities`, `/v1/clear/*` | not built yet: 501 `not_yet` |

`md_content` is the native `output: markdown` of the file and `text_content`
the native `output: text`, both written from the same conversion (a PDF is
read once), not one derived from the other. The answers are docling's: one file is `{"document": {"md_content",
"text_content", …}, "status", "errors", "processing_time", …}`; several files,
or `target: zip`, are `converted_docs.zip` with one `<name>.md` (and `.txt`)
per converted file at the top of the archive, as docling-serve's current
code and its 1.38.2 release write it (two files with the same name, ignoring
case: the later gets `_<index>`, then `_<index>_<n>` if that is taken too; a
name that starts with `-` or is a Windows device name such as `CON`, `NUL`
or `COM1` gets `_` in front; a name longer than 200 bytes is cut;
`x-lmgw-documents` then ends with
`entry_names_renamed=N` or `entry_names_shortened=N`). A
file that failed is missing from the zip, as in docling; the header
`x-lmgw-documents: converted=2, partial=0, failed=1, skipped=0` says so. A
task's result is deleted 5 minutes after its first fetch, as docling-serve
does by default. A task that failed as a whole answers `TaskFailureResult` with
status 200 on `/v1/result`, as docling does (the sync routes answer 422).
A failed file (docling's `failure`) has one `errors` entry in docling's
categories: `timeout` for a deadline; `policy` for one of lmgw's named
bounds (`output_too_large`, `memory_limit`, a source refused or too large, a
model the key may not use), as docling files a document past its own limits
(`max_num_pages`, `max_file_size`); `inference_failure` for the page-reading
model; `backend_failure` for a file the tools could not read. Warnings that
lose text, or may (a part not read, a JPEG 2000 image drawn blank:
`image_not_decoded`), are `backend_failure` entries with their page; the
file's status stays what lmgw's own answer says.
`wait` on the poll route is the longest time to wait for the task to end, at
most `POLL_WAIT_MAX_S` (120 s): a longer `wait` waits that long, and the
answer says so in `x-lmgw-poll-wait: 120 (POLL_WAIT_MAX_S)`; poll again for
longer. The status WebSocket takes at most `WS_CLIENT_MESSAGES_PER_S` (10)
messages a second from its client (each reads the task's state again); a
client that sends more has the socket closed with 1008 and that name. A
`document_timeout` counts from the moment a file gets its first tool slot and
stops while the file waits for another slot or for the layout model, and
one larger than a duration can hold is a 422.

Every docling option a request sets gets an outcome. Most are done as docling
does them (`to_formats` md and text, `from_formats`, `do_ocr`,
`do_table_structure`, `page_range`, `document_timeout`, `abort_on_error`,
`md_page_break_placeholder`, `md_compact_tables`). Others are served by
lmgw's own equivalent, for example any `ocr_engine` (lmgw's text recognition)
or `pipeline: vlm` (the page-reading model) and `pipeline: asr` (audio and
videos are transcribed by the speech-to-text model chosen under Settings →
Documents, whatever the pipeline). Some aren't built yet:
`to_formats` json/html/doctags (their fields stay empty and `status` is
`partial_success`; lmgw's own `structure` is not a DoclingDocument, so
`to_formats: json` has no equivalent yet),
picture classification, chart extraction,
chunking. `image_export_mode` (`placeholder`, the default, `embedded`,
`referenced`), `include_images` (`false` keeps placeholders) and
`images_scale` are the native `pictures` and `images_scale`; with
`referenced`, the zip holds each picture under `artifacts/` with the name the
markdown links, so the links work after unzipping. One file answered in the
body has no place for picture files: its markdown links them as the zip
would, and the header says
`image_export_mode=mapped(referenced pictures are files of the zip target only)`. `table_mode: accurate` asks for the
page-reading model on table pages and `pipeline: vlm` on every page; both answer
`mapped(model_not_chosen)` until one is chosen. `table_mode: fast` keeps tables
with lmgw's own reader: once a page-reading model is chosen it reads only the
pages nothing else read (the native `model_help: missing`), and the note says
so. A docling request that sets neither still uses a chosen model for
unread pages and table pages, as a native request does. Formula and code enrichment
(`do_formula_enrichment`, `do_code_enrichment`, `code_formula_preset`) are
`mapped`: the page-reading model writes formulas as LaTeX and code as fenced
blocks on the pages it reads, and no option picks pages for them.
`do_picture_description` is the native `describe_pictures`, answered by the
picture-description model you chose (`mapped(picture-description model)`;
none chosen: `mapped(model_not_chosen)` and the pictures without
descriptions). The prompt in `picture_description_api`,
`picture_description_local` or `picture_description_custom_config` is used,
and the note names every other key the object has and what became of it:
the model's own settings (`repo_id`, `generation_config`, `params`,
`timeout`, `concurrency`, `batch_size`, `scale`, `engine_options`,
`model_spec`, …) are replaced by your model, `url` and `headers` are never
followed, `picture_area_threshold` is replaced by the top-level
`picture_description_area_threshold` (as docling-serve does), the
`classification_*` keys are not built yet, and any other key is named as
not used. `picture_description_area_threshold` is the native
`describe_min_area`: a smaller picture is kept without a description, as
docling keeps it. The docling route exports every picture whatever its size
(`picture_min_area` 0), as docling does. An option lmgw doesn't know is a note, never an
error. The header `x-lmgw-option-notes` lists every option that wasn't done
as docling does, for example
`ocr_engine=mapped(easyocr->lmgw-ocr); do_picture_classification=not_yet`
(past 4096 bytes the unknown options collapse to
`unknown_options=not_yet(N names; see the request record)`; only the first 32
unknown names, cut to 64 bytes each, are kept in the record);
the full table is on the API page under "docling-serve compatible".

lmgw calls no address the caller chose except one, the web address of an
`http` source, and only when you allow it: `callbacks` aren't called (poll or
use the WebSocket); `s3` and other storage sources and the `put`, `s3` and
`presigned_url` targets are refused by name.

### Web addresses (`http` sources)

A docling caller can name a web address instead of sending the file
(`sources: [{"kind": "http", "url": …, "headers": {…}}]`, or the older
`http_sources`). lmgw downloads it only with **Settings › Documents › Web
addresses › Let callers send web addresses** on, and only from the **Allowed
sites** listed under it (in the page's expert view). Both are off and empty at
first. While it is off, such a request is a 400 `http_source_refused` that asks
for the file itself (as a `kind: "file"` source with `base64_string`) and names
the setting. Every source's host shows under **Who calls the docling route**,
with whether it was downloaded, failed or refused (also when a download stopped
at a refused address or redirect), so you can see which sites your callers use.

**Only callers with a key.** Once it's on, any caller that sends an API key
(`Authorization: Bearer …` or docling's `X-Api-Key`) can name web addresses. A
request without a key is a 401 `http_source_needs_key`, also while **Require
gateway API keys on /v1** is off: `/v1` answers any web page, so a page open in
a browser on this computer could otherwise read your allowed sites through
lmgw. Uploads without a key keep working as before.

The request's options are checked first: a request that would be a 422 downloads
nothing.

How a site is matched:

- An entry is a host name or an address, with `:port` when it isn't the usual
  web port: `docs.example` allows `http://docs.example/…` on port 80 and
  `https://docs.example/…` on port 443 (not `http://docs.example:443/`),
  `docs.example:8443` that port only, under either scheme. No wildcards yet:
  `docs.example` doesn't allow `www.docs.example`.
- Names are compared as lmgw reads them: case doesn't matter, a trailing dot is
  ignored, international names are compared in their `xn--` form, and an
  address written as `2130706433` or `0x7f.1` is the address `127.0.0.1`.
- A web address with a user name or password in it, or any scheme but `http`
  and `https`, is refused.
- **Private addresses need their own entry.** lmgw looks up every address a
  listed name points to before it connects, and connects only to those. If one
  of them is on this computer or a private network (127.0.0.0/8, 10.0.0.0/8,
  172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16, 100.64.0.0/10, `::1`,
  `fc00::/7`, `fe80::/10`, documentation and other special ranges, also as
  IPv4-mapped IPv6), the download is refused unless that address is listed too,
  with the port: `intranet.example` on `172.16.0.5` needs both `intranet.example`
  and `172.16.0.5`. `0.0.0.0`, multicast and broadcast addresses are never
  fetched.
- **This computer's own addresses count as private too:** every address of its
  network interfaces, every address in their local networks (a global IPv6
  `/64` at home, a server's public subnet), and the addresses lmgw's own
  listeners are bound to. If the interfaces can't be read, the log says so once
  and the fixed ranges above apply alone.
- Redirects are followed one at a time, at most 10 (`SOURCE_FETCH_MAX_REDIRECTS`),
  and each one is checked like the first address; a redirect from `https` to
  plain `http` is refused.

Bounds: each download, and a request's downloads together, at most
`max_body_mb` (the request size limit under Settings › Network & access, as an
upload would be); 10 s to connect (`SOURCE_FETCH_CONNECT_TIMEOUT`) and 300 s
for the whole download (`SOURCE_FETCH_TIMEOUT`). lmgw asks for no compression
and unpacks none, over HTTP/1.1. With `max_body_mb` at 0 (no limit) web
addresses are refused (400 `http_source_needs_limit`): a download needs a set
limit.

Across all requests, downloads run in a pool of their own with the same size as
**Parallel tool runs** (derived from the cores and memory, or set by hand), so
slow downloads never hold up conversions. At most that size × `max_body_mb` of
downloads is held in memory at once (8 × 64 MiB = 512 MiB with 8 slots and the
default limit); the expert view shows the size, the downloads running and
waiting, and that product. A download waiting for a slot isn't timed yet (its
300 s start with the slot); meanwhile the caller's request stays open, and an
`/async` or `/batch` request answers with its task once its downloads have
run. A caller that gives up ends its wait.

What is sent: the request, plus the source's own `headers` (for example an
`Authorization` the site needs), to that site only: the same scheme, host and
port. Once a redirect has left it, they are not sent again, not even when a
later redirect comes back. lmgw sends no key, cookie or user agent of its own and
uses no proxy (`HTTP_PROXY` and the like are ignored). Headers lmgw sets itself
(`Host`, `Content-Length`, `Transfer-Encoding`, `Connection`, `Keep-Alive`,
`Upgrade`, `TE`, `Trailer`, `Expect`, `Proxy-Authorization`,
`Proxy-Authenticate`, `Proxy-Connection` and `Accept-Encoding`) are left out, and `x-lmgw-option-notes` says
`headers=mapped(…)` with their names. A header name or value HTTP doesn't
allow, or more headers than one request can carry, is docling's 422
`invalid_source`. Header values are never logged or recorded.

The downloaded file is converted like an upload: its type comes from its
bytes, not from the address or what the site says it is, and its name is the
last part of the address's path (`file` when there is none), as docling names
it. A background request (`/async`, `/batch`) downloads before it answers with
its task, so the task holds no address or header. A download that fails fails
its own file, with a named error at the start of its message, and the other
files go on:

| Error | When |
| --- | --- |
| 401 `http_source_needs_key` | the request has no API key; the whole request is refused |
| 400 `http_source_needs_limit` | `max_body_mb` is 0 (no limit) |
| 400 `http_source_host_refused` | the host (with its port) isn't an allowed site; the whole request is refused before anything is downloaded |
| 400 `http_source_scheme_refused`, `http_source_userinfo_refused`, `http_source_invalid_url` | not an `http`/`https` address, a user name or password in it, or not an address |
| `source_address_refused` | the name points to an address in a refused range, or one of this computer's own, that the list doesn't name |
| `source_redirect_refused` | a redirect to a site that isn't allowed, to a refused address, from `https` to `http`, or more than 10 |
| `source_too_large` | past `max_body_mb` |
| `source_timeout` | past `SOURCE_FETCH_CONNECT_TIMEOUT` or `SOURCE_FETCH_TIMEOUT` |
| `source_http_status` | the site answered an error (only the status is given) |
| `source_unreachable` | the name didn't resolve, or the connection or its certificate failed |

The errors name the source's host, never what the site answered or where it
redirected to. The admin tools (`lmgw__settings_set`) can't change these two
settings; they change on Settings only.

Each request writes a usage row (alias `docling/v1/convert/…`) with a record
of its route, file count, target and options. Option values are kept only
where they are one of a few fixed words (`easyocr`, `md`); prompts,
addresses, headers, file names and the documents' text never.

## Comparing lmgw with docling-serve on your own documents

Before lmgw takes a docling-serve's place, `scripts/convert-compare.py` measures
both on a folder of your real documents. It is made for a place the documents
may not leave: it writes a report of numbers only, and stops without a report
if anything else could be in it.

**Copy** one file there, `scripts/convert-compare.py`. It needs `python3` (3.6
or newer) with its standard library and nothing else.

**Set** the keys in the environment, never on the command line:

```sh
export DOCLING_KEY=...   # docling-serve's X-Api-Key; leave unset if it asks for none
export LMGW_KEY=...      # an lmgw key; with the owner's bearer the report also
                         # holds lmgw's tools and the models chosen for documents
```

(`read -rs LMGW_KEY; export LMGW_KEY` takes a key without leaving it in the
shell's history.)

**Run** it in four steps. The first sends nothing; the second measures three
files, so you see it works and what a report looks like:

```sh
python3 -I convert-compare.py --dir FOLDER --dry-run
python3 -I convert-compare.py --docling https://docling.example --lmgw https://lmgw.example \
    --dir FOLDER --out report.json --also-adapter --limit 3
python3 -I convert-compare.py --show report.json
python3 -I convert-compare.py --docling https://docling.example --lmgw https://lmgw.example \
    --dir FOLDER --out report.json --also-adapter --resume --overwrite
```

- Every file goes to docling-serve and then to lmgw, one at a time, under a
  neutral name (`doc000012.pdf`); `--also-adapter` also sends it to lmgw's
  docling route and records only whether the answer has docling's shape.
  Files are read and sent in 1 MiB pieces, so a large file takes no more of
  the script's memory than a small one; `--dry-run` shows the largest.
- Before the first document both systems get a built-in one-page PDF. If one
  answers it with http 200 but does not convert it (a login page at that
  address, say), the run stops before any document is sent;
  `--accept-test-page` measures anyway.
- A run that is interrupted (Ctrl-C, a closed terminal) keeps what it measured;
  the same command with `--resume` continues.
- After three files in a row without an answer (no connection, a timeout, a
  502 or 503), the test page is sent again. If both systems still answer it,
  those files were too slow or too large for the connection, and the run goes
  on. If not, it stops. Once the server is back, plain `--resume` continues
  and keeps those files' results; `--resume --retry-failed` measures them again.
- A file whose measurement fails inside the script (no memory left for an
  answer, an archive that does not unpack) is recorded as `failed`, the error's
  type and line are printed, and the run goes on with the next file.
- `--fresh` starts over and moves the earlier salt and state files aside
  (`.old`); nothing is deleted.
- A private certificate authority goes in `--ca-file`; `--insecure` switches
  the certificate check off, and the report says so. Proxy variables in the
  environment are ignored unless you pass `--use-env-proxy`.
- docling-serve answers 504 when a file takes longer than its sync wait limit
  (120 s by default); the script then sends that file through docling's async
  route by itself.
- `python3 -I convert-compare.py --help` has the rest: timeouts, symlinks,
  where the two files below go.

**Read before copying the report out.** The run ends with a short summary; read
it, then read the report in full with `--show`.

- The report holds numbers, yes/no values, fixed code words and one 12-digit
  hash per file. It has no file names, folder names, document text, error
  messages, addresses or keys: the script checks every key and value against a
  fixed list, then searches the report for every file and folder name and for
  the words of the converted texts.
- The only text taken from the servers is their version strings and the alias
  names of the models lmgw uses for documents. The summary lists them; an alias
  name is a name someone chose, so look at it. `--withhold-aliases` or
  `--withhold-strings` writes the report without them. A version string that
  holds a word of a converted text is written `withheld` by itself, and the
  run goes on.
- A file's entry has its characters and lines rounded to two significant
  digits, and keeps its small counts (headings, tables, cells, pictures, pages,
  times) exact. Someone who holds a document and runs it through the same lmgw
  version could still tell from those counts whether it was in the folder.
- If the check finds something, the run stops, names the rule and writes no
  report. It prints what to run next. A file or folder name inside a version
  string or an alias name stops it before any document is sent.
- Two files stay where the documents are: `convert-compare.salt` and
  `convert-compare.state`, written next to the folder (or in `--state-dir`).
  With the salt, `--identify HASH` tells you there which file a hash in the
  report is. Never copy them out.

## What lmgw reads today

| Files | Needs |
| --- | --- |
| Plain text, Markdown, CSV and other UTF-8 text | nothing |
| Word, PowerPoint, Excel and OpenDocument files (docx, pptx, xlsx, odt, odp, ods) | nothing: lmgw's own reader keeps headings, lists and tables |
| PDF | `poppler-utils` |
| Old Excel files (`.xls`), web pages (HTML), e-books (EPUB), e-mail (`.eml`) | nothing to install: lmgw's document reader, which comes with lmgw (not on Windows yet) |
| Scanned PDF pages | `tesseract` and `poppler-utils`; `tesseract-osd` tells which way up a page is |
| Pictures of text (PNG, JPEG, GIF, WebP, TIFF) | `tesseract` (and lmgw's document reader); `tesseract-osd` as above |
| Old Word and PowerPoint files (`.doc`, `.ppt`) | LibreOffice (`libreoffice-writer`, `libreoffice-impress`) |
| RTF | `pandoc-cli`, or LibreOffice |
| LaTeX, reStructuredText, Org, MediaWiki, DocBook, FictionBook | `pandoc-cli` (without it: as plain text) |
| Word files with equations, the equations as LaTeX | `pandoc-cli` (without it: lmgw's own reader, equations as plain characters) |
| Audio | a speech-to-text model chosen under Settings → Documents |
| Video (its sound) | `ffmpeg-free` and a speech-to-text model |

A page-reading model chosen on the Documents page reads the scanned pages and
pictures text recognition can't, or reads poorly ([Pages read by a
model](#pages-read-by-a-model)).

On Windows, pandoc, LibreOffice and ffmpeg are not used yet (lmgw cannot
give a program a memory limit there yet): their file types are said to be
not read on Windows yet, and the text markups and Word files with equations
are read without pandoc.

## The optional programs

| Program | Fedora package | What lmgw does with it |
| --- | --- | --- |
| poppler | `poppler-utils` | reads the text of PDFs with their headings, columns and tables, and renders PDF pages for a knowledge base's vision model |
| tesseract | `tesseract` (your language's pack comes with it), `tesseract-osd` | reads scanned PDF pages and pictures ([Scans and pictures](#scans-and-pictures)) |
| Pandoc | `pandoc-cli` | reads RTF, LaTeX, reStructuredText, Org, MediaWiki, DocBook and FictionBook, and Word files with equations, the equations as LaTeX |
| ffmpeg | `ffmpeg-free`, or `ffmpeg` from RPM Fusion | takes the sound out of videos for the speech-to-text model |
| LibreOffice | `libreoffice-writer libreoffice-impress` | reads old Word (Writer) and PowerPoint (Impress) files, and RTF where pandoc is missing |

Fedora 45's `ffmpeg-free` (9.0.2) decodes AAC, MP3, Opus, Vorbis, FLAC and
AC-3 sound, not E-AC-3 (checked 2026-11-19); RPM Fusion's `ffmpeg` decodes
all of them.

### Other Linux distributions

The `sudo dnf install` line is for Fedora and its relatives: lmgw reads
`ID` and `ID_LIKE` in `/etc/os-release` and shows the command when one of
them is `fedora`, `rhel`, `centos` or a clone of those (Rocky, AlmaLinux).
On Debian, Ubuntu, Arch, openSUSE and the rest, the page names the program
("Install poppler with your distribution's package manager") and shows no
command to copy, because lmgw has no package names for them yet. The page
looks for the programs on `PATH` the same way.

### Desktop (RPM)

The RPM **recommends** `poppler-utils`, `tesseract` and `tesseract-osd`
(tesseract's orientation data, 10 MiB). dnf installs recommended packages by
default, so most installs have them. They are not
required: if you remove poppler-utils, lmgw keeps running, and a PDF you
attach in the Chat or add to a knowledge base fails with the message "install
poppler-utils (e.g. `sudo dnf install poppler-utils`) to read PDFs".

The other programs are large (Pandoc about 200 MiB, LibreOffice about
300 MiB) and many desktops already have LibreOffice, so the RPM doesn't pull
them in. Install them with the command the page shows.

The RPM installs lmgw's document reader itself, as
`/usr/libexec/lmgw/lmgw-doc-reader`.

The AppImage has no dependencies of its own; it carries lmgw's document
reader inside it. lmgw finds the other programs the same way, and the page
shows what is missing.

### Server image

The server image ([docs/server.md](server.md)) ships lmgw's document reader
(beside `lmgw-server`), `poppler-utils`, `tesseract` with the English and
German language packs and its orientation data (`tesseract-osd`), and
`ffmpeg-free`.
Building the image yourself, set `TESSERACT_LANGS` to other tesseract codes
(for example `TESSERACT_LANGS="eng fra spa" bash ci/build-image.sh`).

Pandoc and LibreOffice aren't in the image yet (about 500 MiB together).
A `dnf install` inside the running container would be lost when the
container is recreated, so the page shows a derived image instead:

```dockerfile
FROM localhost/lmgw-server:current
RUN dnf -y install --setopt=install_weak_deps=False pandoc-cli \
      libreoffice-writer libreoffice-impress && dnf clean all
```

Build it as the service account, tag it so the unit runs it, and restart the
service:

```sh
podman build -t localhost/lmgw-server:current-docs -f Containerfile.docs .
podman tag localhost/lmgw-server:current-docs localhost/lmgw-server:current
systemctl --user restart lmgw
```

Derive again after each update, since an update replaces `:current`.

### Windows and macOS

lmgw ships none of the programs there yet. The page links each program's
installer, and lmgw also looks in the usual install folders (for example
`C:\Program Files\LibreOffice\program`, and for Pandoc
`%LOCALAPPDATA%\Pandoc` and `C:\Program Files\Pandoc`).

On Windows an installer changes the system's `Path`, not the `PATH` of a
program that is already running. lmgw reads the machine's and your user's
`Path` from the registry again each time it looks, so a program you install
while lmgw is open shows without a restart (the registry read is not
verified on a Windows machine yet). PDF reading on Windows isn't
available yet: it is planned through pdfium, which isn't built yet. A
`pdftotext` found on the `Path` is not used (the runner cannot limit a
tool's memory on Windows yet), so the page and the route agree: a PDF there
fails with `no_reader`, saying so.
